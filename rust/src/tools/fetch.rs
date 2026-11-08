//! Raw HTTP fetch for simple resources. JSON, Markdown, XML, text, images, and
//! other files bypass Lightpanda entirely; HTML is rejected with a pointer to
//! `scrape`. Bodies are capped and every request runs under the tool deadline.

use super::{Tool, find_option, normalize_url, parse_millis};
use crate::http;
use base64::Engine;
use serde_json::{Value, json};
use std::time::Duration;

/// Raw body cap. Base64 inflates by 4/3, so this keeps the tool result under
/// the 8 MiB MCP result budget.
const MAX_FETCH_BYTES: usize = 4 * 1024 * 1024;

pub struct Request {
    url: String,
    timeout: Duration,
}

impl Request {
    pub fn parse(args: &Value) -> Result<Self, String> {
        let Some(object) = args.as_object() else {
            return Err("arguments must be an object".to_string());
        };
        for key in object.keys() {
            let Some(option) = find_option(key) else {
                return Err(format!("Unknown argument: {key}"));
            };
            if option.applies & Tool::Fetch.bit() == 0 {
                return Err(format!("Unknown argument: {key}"));
            }
        }
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "url must be a string".to_string())?;
        Ok(Self {
            url: normalize_url(url)?,
            timeout: parse_millis(args, "timeout")?,
        })
    }
}

pub async fn run(request: &Request) -> Result<Value, String> {
    let client = http::client()?;
    let deadline = tokio::time::Instant::now() + request.timeout;
    let response = tokio::time::timeout_at(deadline, client.get(&request.url).send())
        .await
        .map_err(|_| "Fetch request timed out".to_string())?
        .map_err(|error| format!("Fetch request failed: {}", error.without_url()))?;
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mime = mime_type(&content_type);
    if is_html(&mime) {
        // The point of this tool is to avoid starting the browser; HTML needs
        // the renderer, so send the caller to scrape instead.
        return Err("HTML content: use the scrape tool for rendered HTML".to_string());
    }
    let bytes = http::read_limited(response, MAX_FETCH_BYTES, deadline).await?;
    let size = bytes.len();
    let mut payload = json!({
        "url": final_url,
        "sourceURL": request.url,
        "statusCode": status,
        "contentType": content_type,
        "size": size,
    });
    let textual = if is_text(&mime) {
        true
    } else if mime.is_empty() || mime == "application/octet-stream" {
        sniff_text(&bytes)
    } else {
        false
    };
    if textual {
        payload["encoding"] = json!("text");
        payload["text"] = json!(http::text_or_lossy(bytes));
    } else {
        payload["encoding"] = json!("base64");
        payload["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
    }
    Ok(payload)
}

fn mime_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_html(mime: &str) -> bool {
    matches!(mime, "text/html" | "application/xhtml+xml")
}

/// Text-like media types parsed without a browser. SVG is XML text.
fn is_text(mime: &str) -> bool {
    if mime.starts_with("text/") {
        return !is_html(mime);
    }
    matches!(
        mime,
        "application/json"
            | "application/xml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/toml"
            | "application/javascript"
            | "application/ecmascript"
            | "application/csv"
            | "application/x-ndjson"
            | "application/ld+json"
    ) || mime.ends_with("+json")
        || (mime.ends_with("+xml") && !is_html(mime))
}

/// Unknown/binary media types: treat a NUL-free valid UTF-8 body as text, which
/// covers JSON/Markdown served as `application/octet-stream`.
fn sniff_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_classify_consistently() {
        assert_eq!(
            mime_type("Application/JSON; charset=utf-8"),
            "application/json"
        );
        assert!(is_text("text/markdown"));
        assert!(is_text("application/ld+json"));
        assert!(is_text("application/xml"));
        assert!(is_text("image/svg+xml"));
        assert!(!is_text("text/html"));
        assert!(!is_text("image/png"));
        assert!(!is_text("application/pdf"));
        assert!(is_html("application/xhtml+xml"));
        assert!(sniff_text(b"{\"ok\":true}"));
        assert!(!sniff_text(b"\x89PNG\r\n\x1a\n\0"));
    }

    #[test]
    fn fetch_arguments_follow_the_option_table() {
        let request = Request::parse(&json!({"url": "https://example.com/data.json"}))
            .expect("valid fetch request");
        assert_eq!(request.timeout, Duration::from_secs(30));
        assert!(
            Request::parse(&json!({"url": "https://example.com", "formats": ["text"]})).is_err()
        );
        assert!(Request::parse(&json!({"url": "https://example.com", "limit": 1})).is_err());
        assert!(Request::parse(&json!({"url": "file:///etc/passwd"})).is_err());
    }
}
