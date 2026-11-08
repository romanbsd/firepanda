//! JSON-RPC MCP server: framing contract, protocol negotiation, and method
//! dispatch. Validation order, error codes, and legacy/modern negotiation are
//! pinned by `tests/protocol.py`.

use crate::browser::{Browser, Options, Stop};
use crate::tools::{self, CallError};
use serde_json::{Value, json};
use std::time::Instant;

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
/// Modern revision first; legacy initialize still negotiates the 2025-11-25
/// fallback.
const PROTOCOL_VERSION: &str = "2026-07-28";
const FALLBACK_PROTOCOL_VERSION: &str = "2025-11-25";
const VERSIONS: [&str; 5] = [
    PROTOCOL_VERSION,
    FALLBACK_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const MAX_STRUCTURED_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_RESULT_BYTES: usize = 8 * 1024 * 1024;

fn identity() -> Value {
    json!({"name": "firepanda", "version": env!("CARGO_PKG_VERSION")})
}

pub fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn is_integer(value: &Value) -> bool {
    crate::json::as_integer(value).is_some()
}

pub struct Server {
    browser: Browser,
    initialized: bool,
}

impl Server {
    pub fn new(options: Options, stop: Stop) -> Self {
        Self {
            browser: Browser::new(options, stop),
            initialized: false,
        }
    }

    pub async fn idle_deadline(&self) -> Option<Instant> {
        self.browser.expire_if_idle().await
    }

    /// Tear the browser down when the service exits (C++ destructor path).
    pub async fn shutdown(&self) {
        self.browser.shutdown().await;
    }

    fn discover() -> Value {
        json!({"supportedVersions": VERSIONS, "capabilities": {"tools": {}}})
    }

    fn initialize(&mut self, params: &Value) -> Result<Value, String> {
        let well_formed = params.get("protocolVersion").is_some_and(Value::is_string)
            && params.get("capabilities").is_some_and(Value::is_object)
            && params.get("clientInfo").is_some_and(Value::is_object);
        if !well_formed {
            return Err(
                "initialize requires protocolVersion, capabilities, and clientInfo".to_string(),
            );
        }
        let requested = params["protocolVersion"].as_str().unwrap_or_default();
        // Same negotiation as C++: the modern revision itself and any unknown
        // version fall back to the legacy initialize handshake.
        let negotiated = if requested == PROTOCOL_VERSION || !VERSIONS.contains(&requested) {
            FALLBACK_PROTOCOL_VERSION
        } else {
            requested
        };
        self.initialized = true;
        Ok(json!({
            "protocolVersion": negotiated,
            "capabilities": {"tools": {}},
            "serverInfo": identity(),
        }))
    }

    fn tools_list(params: &Value) -> Result<Value, String> {
        if params.get("cursor").is_some() {
            return Err("No pagination cursor is supported".to_string());
        }
        Ok(json!({"tools": tools::tools_list()}))
    }

    async fn tools_call(&self, params: &Value) -> Result<Value, String> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err("Tool name must be a string".to_string());
        };
        let default = Value::Object(serde_json::Map::new());
        let arguments = params.get("arguments").unwrap_or(&default);
        match tools::execute(&self.browser, name, arguments).await {
            Ok(data) => {
                let text = serde_json::to_string(&data)
                    .map_err(|error| format!("Cannot serialize tool result: {error}"))?;
                if text.len() > MAX_TOOL_RESULT_BYTES {
                    return Ok(json!({
                        "content": [{"type": "text", "text": "Tool result exceeds 8 MiB; select fewer output formats or pages"}],
                        "isError": true,
                    }));
                }
                // structuredContent repeats the whole payload. Keep it for normal
                // results; large crawls and maps would otherwise serialize twice.
                let mut result =
                    json!({"content": [{"type": "text", "text": text}], "isError": false});
                if text.len() <= MAX_STRUCTURED_CONTENT_BYTES {
                    result["structuredContent"] = data;
                }
                Ok(result)
            }
            Err(CallError::Invalid(message)) => Err(message),
            Err(CallError::Tool(message)) => Ok(json!({
                "content": [{"type": "text", "text": message}],
                "isError": true,
            })),
        }
    }

    /// Handle one validated JSON-RPC message; `None` means no reply
    /// (notifications). Error-code precedence mirrors `src/main.cpp`.
    pub async fn handle(&mut self, request: &Value) -> Option<Value> {
        let id = if request.is_object() {
            request.get("id").cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let envelope_ok = request.is_object()
            && request.get("jsonrpc").is_some_and(|v| v == "2.0")
            && request.get("method").is_some_and(Value::is_string)
            && (request.get("id").is_none() || id.is_string() || is_integer(&id))
            && request.get("params").is_none_or(Value::is_object);
        if !envelope_ok {
            return Some(rpc_error(&Value::Null, -32600, "Invalid JSON-RPC request"));
        }
        // Notifications (no id) get no reply.
        request.get("id")?;
        let empty = Value::Object(serde_json::Map::new());
        let params = request.get("params").unwrap_or(&empty);
        let meta = params.get("_meta").unwrap_or(&empty);
        if !meta.is_object() {
            return Some(rpc_error(&id, -32602, "_meta must be an object"));
        }
        let null = Value::Null;
        let version = meta.get(VERSION_KEY).unwrap_or(&null);
        let modern = version.as_str() == Some(PROTOCOL_VERSION);
        if !version.is_null() && !modern {
            let mut response = rpc_error(&id, -32022, "Unsupported protocol version");
            response["error"]["data"] = json!({"supported": VERSIONS, "requested": version});
            return Some(response);
        }
        let method = request["method"].as_str().unwrap_or_default();
        let dispatched: Result<Value, String> = match method {
            "server/discover" => Ok(Self::discover()),
            "initialize" => self.initialize(params),
            other => {
                if !modern && !self.initialized {
                    return Some(rpc_error(
                        &id,
                        -32600,
                        "Initialize first or provide modern protocol metadata",
                    ));
                }
                match other {
                    "ping" => Ok(json!({})),
                    "tools/list" => Self::tools_list(params),
                    "tools/call" => self.tools_call(params).await,
                    _ => return Some(rpc_error(&id, -32601, "Method not found")),
                }
            }
        };
        let mut result = match dispatched {
            Ok(result) => result,
            Err(message) => return Some(rpc_error(&id, -32602, &message)),
        };
        if modern || method == "server/discover" {
            result["resultType"] = json!("complete");
            result["_meta"] = json!({"io.modelcontextprotocol/serverInfo": identity()});
        }
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{Options, Stop};

    fn server() -> Server {
        // A deliberately missing binary: tool-error tests must not depend on
        // a Lightpanda install being present in PATH.
        let options = Options {
            lightpanda: "/missing/lightpanda".to_string(),
            ..Options::default()
        };
        Server::new(options, Stop::channel())
    }

    fn modern(id: u64, method: &str, params: Value) -> Value {
        let mut params = params;
        params["_meta"] = json!({VERSION_KEY: PROTOCOL_VERSION});
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    #[tokio::test]
    async fn legacy_requires_initialize_first() {
        let mut server = server();
        let response = server
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn modern_stateless_works_without_initialize() {
        let mut server = server();
        let response = server.handle(&modern(1, "ping", json!({}))).await.unwrap();
        assert_eq!(response["result"]["resultType"], "complete");
        assert_eq!(
            response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "firepanda"
        );
    }

    #[tokio::test]
    async fn notifications_have_no_reply() {
        let mut server = server();
        assert!(
            server
                .handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn unknown_version_rejected_with_data() {
        let mut server = server();
        let response = server
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping",
                "params": {"_meta": {VERSION_KEY: "2099-01-01"}}}))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32022);
        assert_eq!(response["error"]["data"]["requested"], "2099-01-01");
    }

    #[tokio::test]
    async fn legacy_initialize_negotiates_known_version() {
        let mut server = server();
        let response = server
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                           "clientInfo": {"name": "t", "version": "1"}}}))
            .await
            .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        assert!(response["result"].get("resultType").is_none());
        // Now legacy tools/list is allowed.
        let response = server
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .unwrap();
        assert_eq!(
            response["result"]["tools"].as_array().unwrap().len(),
            if cfg!(feature = "search") { 5 } else { 4 }
        );
    }

    #[tokio::test]
    async fn meta_must_be_object() {
        let mut server = server();
        let response = server
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping",
                "params": {"_meta": []}}))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn tool_errors_vs_argument_errors() {
        let mut server = server();
        // Missing Lightpanda: tool error (isError), not a JSON-RPC error.
        let response = server
            .handle(&modern(
                1,
                "tools/call",
                json!({"name": "scrape", "arguments": {"url": "https://example.com"}}),
            ))
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Cannot start Lightpanda")
        );
        // Invalid arguments: JSON-RPC -32602.
        let response = server
            .handle(&modern(
                2,
                "tools/call",
                json!({"name": "scrape", "arguments": {"url": "file:///etc/passwd"}}),
            ))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602);
        // Unknown tool name type.
        let response = server
            .handle(&modern(3, "tools/call", json!({"name": 7})))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn tool_errors_carry_no_structured_content() {
        // Matches C++: browser failures become isError results, and only
        // successful (small) payloads repeat into structuredContent.
        let mut server = server();
        let response = server
            .handle(&modern(
                1,
                "tools/call",
                json!({"name": "scrape", "arguments": {"url": "https://example.com"}}),
            ))
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"].get("structuredContent").is_none());
    }
}
