//! Tool table, option schema, argument validation, and URL normalization.
//! Schema bounds, defaults, and per-tool applicability all derive from the
//! same static tables.

mod crawl;
mod fetch;

use crate::browser::{Browser, Format, PageRequest};
use crate::json::as_integer;
use serde_json::{Value, json};
use std::time::Duration;

/// Frontier cap for crawl/map link discovery (M4).
pub const MAX_LINKS: usize = 10000;
/// Longest accepted URL, before and after normalization.
const MAX_URL_BYTES: usize = 8192;
/// Serialization budget for crawl results.
const MAX_CRAWL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Scrape,
    Fetch,
    Crawl,
    Map,
    /// Keyless web search.
    Search,
}

impl Tool {
    /// Applicability bit used by the option table.
    const fn bit(self) -> u8 {
        match self {
            Self::Scrape => SCRAPE,
            Self::Fetch => FETCH,
            Self::Crawl => CRAWL,
            Self::Map => MAP,
            Self::Search => SEARCH,
        }
    }
}

const SCRAPE: u8 = 1 << 0;
const FETCH: u8 = 1 << 1;
const CRAWL: u8 = 1 << 2;
const MAP: u8 = 1 << 3;
const SEARCH: u8 = 1 << 4;

struct ToolDef {
    id: Tool,
    name: &'static str,
    title: &'static str,
    description: &'static str,
}

/// MCP behavior hints shared by every tool: all are read-only, idempotent
/// network readers with no destructive side effects.
fn annotations() -> Value {
    json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": true,
    })
}

/// One row per tool: dispatch and tools/list both read this table.
static TOOL_TABLE: &[ToolDef] = &[
    ToolDef {
        id: Tool::Scrape,
        name: "scrape",
        title: "Scrape page",
        description: "Fetch one page or document (HTML, Markdown, plain text, or a JSON \
                      endpoint) and extract the requested formats. Use crawl or map for \
                      multiple pages.",
    },
    ToolDef {
        id: Tool::Fetch,
        name: "fetch",
        title: "Fetch resource",
        description: "Fetch one raw non-HTML resource over HTTP without starting the browser: \
                      JSON, Markdown, XML, plain text, images, and other files. Use scrape for \
                      rendered HTML.",
    },
    ToolDef {
        id: Tool::Crawl,
        name: "crawl",
        title: "Crawl site",
        description: "Breadth-first crawl of same-origin pages. Returns pages and per-page \
                      errors; bounded by depth, page count, 120 seconds, and result size.",
    },
    ToolDef {
        id: Tool::Map,
        name: "map",
        title: "Map links",
        description: "Discover links with a bounded same-origin crawl. External links are \
                      listed but never followed. Not a sitemap or search engine.",
    },
    ToolDef {
        id: Tool::Search,
        name: "search",
        title: "Search the web",
        description: crate::search::DESCRIPTION,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionKind {
    Integer,
    String,
    Boolean,
    Url,
    Formats,
}

/// One row per accepted argument. The advertised schema, validation bounds,
/// defaults, and per-tool applicability all derive from these entries.
struct OptionDef {
    name: &'static str,
    kind: OptionKind,
    description: &'static str,
    /// Integer/boolean default.
    fallback: i64,
    minimum: i64,
    /// Integer maximum or string byte limit.
    maximum: i64,
    /// Tools accepting this argument (bitmask from `Tool::bit`).
    applies: u8,
}

macro_rules! opt {
    ($name:literal, $kind:expr, $description:literal, tools = $tools:expr) => {
        OptionDef {
            name: $name,
            kind: $kind,
            description: $description,
            fallback: 0,
            minimum: 0,
            maximum: 0,
            applies: $tools,
        }
    };
    ($name:literal, $kind:expr, $description:literal, tools = $tools:expr, fallback = $fallback:expr, minimum = $minimum:expr, maximum = $maximum:expr) => {
        OptionDef {
            name: $name,
            kind: $kind,
            description: $description,
            fallback: $fallback,
            minimum: $minimum,
            maximum: $maximum,
            applies: $tools,
        }
    };
}

const OPTIONS: [OptionDef; 10] = [
    opt!(
        "url",
        OptionKind::Url,
        "Absolute HTTP(S) URL without credentials",
        tools = SCRAPE | CRAWL | MAP | FETCH
    ),
    opt!(
        "formats",
        OptionKind::Formats,
        "Output formats to extract",
        tools = SCRAPE | CRAWL
    ),
    opt!(
        "onlyMainContent",
        OptionKind::Boolean,
        "Remove common page chrome and prefer main/article content",
        tools = SCRAPE | CRAWL,
        fallback = 1,
        minimum = 0,
        maximum = 0
    ),
    opt!(
        "timeout",
        OptionKind::Integer,
        "Total request budget in milliseconds",
        tools = SCRAPE | CRAWL | MAP | FETCH,
        fallback = 30000,
        minimum = 100,
        maximum = 120_000
    ),
    opt!(
        "waitFor",
        OptionKind::Integer,
        "Extra wait after page load, within timeout",
        tools = SCRAPE | CRAWL | MAP,
        fallback = 0,
        minimum = 0,
        maximum = 30000
    ),
    opt!(
        "waitForSelector",
        OptionKind::String,
        "Wait for a CSS selector to match an element (presence, not visibility), within timeout.",
        tools = SCRAPE | CRAWL | MAP,
        fallback = 0,
        minimum = 0,
        maximum = 2048
    ),
    opt!(
        "waitForText",
        OptionKind::String,
        "Wait for literal case-sensitive body text outside scripts/styles/templates, within timeout.",
        tools = SCRAPE | CRAWL | MAP,
        fallback = 0,
        minimum = 0,
        maximum = 4096
    ),
    opt!(
        "limit",
        OptionKind::Integer,
        "Maximum pages to attempt",
        tools = CRAWL | MAP,
        fallback = 10,
        minimum = 1,
        maximum = 100
    ),
    opt!(
        "maxDepth",
        OptionKind::Integer,
        "Maximum link depth from the start URL",
        tools = CRAWL | MAP,
        fallback = 1,
        minimum = 0,
        maximum = 5
    ),
    opt!(
        "maxConcurrency",
        OptionKind::Integer,
        "Pages fetched in flight at once during this crawl",
        tools = CRAWL | MAP,
        fallback = 4,
        minimum = 1,
        maximum = 16
    ),
];

fn find_option(name: &str) -> Option<&'static OptionDef> {
    OPTIONS.iter().find(|option| option.name == name)
}

/// Options are registered statically; a miss is a programming error.
fn known_option(name: &str) -> &'static OptionDef {
    find_option(name).unwrap_or_else(|| panic!("Unregistered option: {name}"))
}

fn parse_integer(args: &Value, name: &str) -> Result<i64, String> {
    let option = known_option(name);
    args.get(name).map_or(Ok(option.fallback), |value| {
        bounded_integer(value, name, option.minimum, option.maximum)
    })
}

/// Exact JSON integer within an inclusive range; shared with `search`.
pub fn bounded_integer(
    value: &Value,
    name: &str,
    minimum: i64,
    maximum: i64,
) -> Result<i64, String> {
    let Some(actual) = as_integer(value) else {
        return Err(format!("{name} must be an integer"));
    };
    if actual < i128::from(minimum) || actual > i128::from(maximum) {
        return Err(format!("{name} is outside the supported range"));
    }
    i64::try_from(actual).map_err(|_| format!("{name} is outside the supported range"))
}

fn parse_string(args: &Value, name: &str) -> Result<String, String> {
    let option = known_option(name);
    let Some(value) = args.get(name) else {
        return Ok(String::new());
    };
    let invalid = || format!("{name} must be a nonempty string within its byte limit");
    let Some(text) = value.as_str() else {
        return Err(invalid());
    };
    if text.is_empty() || text.len() > usize::try_from(option.maximum).unwrap_or(usize::MAX) {
        return Err(invalid());
    }
    Ok(text.to_string())
}

fn parse_boolean(args: &Value, name: &str) -> Result<bool, String> {
    let option = known_option(name);
    let Some(value) = args.get(name) else {
        return Ok(option.fallback != 0);
    };
    value
        .as_bool()
        .ok_or_else(|| format!("{name} must be boolean"))
}

/// Integer option converted to a duration. Every registered integer duration
/// has a nonnegative bound, so the conversion cannot fail.
fn parse_millis(args: &Value, name: &str) -> Result<Duration, String> {
    let value = parse_integer(args, name)?;
    Ok(Duration::from_millis(
        u64::try_from(value).expect("duration option bounds are nonnegative"),
    ))
}

fn schema_of(option: &OptionDef) -> Value {
    match option.kind {
        OptionKind::Integer => json!({
            "type": "integer",
            "minimum": option.minimum,
            "maximum": option.maximum,
            "default": option.fallback,
            "description": option.description,
        }),
        // JSON Schema lengths count characters; validation also enforces UTF-8 bytes.
        OptionKind::String => json!({
            "type": "string",
            "minLength": 1,
            "maxLength": option.maximum,
            "description": format!("{} Maximum {} UTF-8 bytes.", option.description, option.maximum),
        }),
        OptionKind::Boolean => json!({
            "type": "boolean",
            "default": option.fallback != 0,
            "description": option.description,
        }),
        OptionKind::Url => json!({
            "type": "string",
            "description": option.description,
        }),
        OptionKind::Formats => json!({
            "type": "array",
            "minItems": 1,
            "items": {"type": "string", "enum": Format::ALL},
            "default": ["markdown"],
            "description": option.description,
        }),
    }
}

fn parse_tool(name: &str) -> Result<Tool, String> {
    TOOL_TABLE
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.id)
        .ok_or_else(|| format!("Unknown tool: {name}"))
}

/// WHATWG parsing (what Lightpanda and the extractor's `new URL()` use) plus
/// Firepanda's rules. The pre-check matters: the parser would otherwise
/// rewrite `\\` to `/` and accept `http://x\\@y`.
pub fn normalize_url(input: &str) -> Result<String, String> {
    if input.len() > MAX_URL_BYTES || input.bytes().any(|c| c <= 32 || c == 127 || c == b'\\') {
        return Err("Invalid URL characters or length".to_string());
    }
    let url = url::Url::parse(input).map_err(|error| format!("Invalid URL: {error}"))?;
    normalize_parsed(url)
}

/// Apply the scheme, credential, port, and length rules to an already parsed
/// URL and drop its fragment. Serialized URLs never carry raw controls or
/// backslashes, so only the length needs rechecking.
pub fn normalize_parsed(mut url: url::Url) -> Result<String, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only HTTP(S) URLs are supported".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with credentials are not supported".to_string());
    }
    if url.port() == Some(0) {
        return Err("Invalid URL port".to_string());
    }
    url.set_fragment(None);
    let normalized: String = url.into();
    if normalized.len() > MAX_URL_BYTES {
        return Err("Invalid URL characters or length".to_string());
    }
    Ok(normalized)
}

/// Call only after normalization; avoid reparsing for every crawl link.
pub fn origin(normalized: &str) -> &str {
    let after_scheme = normalized.find("://").map_or(normalized.len(), |i| i + 3);
    normalized[after_scheme..]
        .find('/')
        .map_or(normalized, |offset| &normalized[..after_scheme + offset])
}

/// Reject non-object arguments and keys the option table does not grant `tool`.
fn check_keys(args: &Value, tool: Tool) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err("arguments must be an object".to_string());
    };
    object
        .keys()
        .find(|key| find_option(key).is_none_or(|option| option.applies & tool.bit() == 0))
        .map_or(Ok(()), |key| Err(format!("Unknown argument: {key}")))
}

pub fn parse_request(args: &Value, tool: Tool) -> Result<PageRequest, String> {
    check_keys(args, tool)?;
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "url must be a string".to_string())?;
    let mut request = PageRequest {
        url: normalize_url(url)?,
        formats: Vec::new(),
        only_main_content: parse_boolean(args, "onlyMainContent")?,
        timeout: parse_millis(args, "timeout")?,
        wait_for: parse_millis(args, "waitFor")?,
        wait_for_selector: parse_string(args, "waitForSelector")?,
        wait_for_text: parse_string(args, "waitForText")?,
    };
    if let Some(value) = args.get("formats") {
        let Some(list) = value.as_array() else {
            return Err("formats must be a nonempty array".to_string());
        };
        if list.is_empty() {
            return Err("formats must be a nonempty array".to_string());
        }
        for value in list {
            let Some(name) = value.as_str() else {
                return Err("Each format must be a string".to_string());
            };
            let Some(format) = Format::parse(name) else {
                return Err(format!("Unsupported format: {name}"));
            };
            if !request.formats.contains(&format) {
                request.formats.push(format);
            }
        }
    } else {
        request.formats.push(Format::Markdown);
    }
    Ok(request)
}

/// Search takes its own arguments; every other tool derives from `OPTIONS`.
fn input_schema(tool: Tool) -> Value {
    if tool == Tool::Search {
        return crate::search::input_schema();
    }
    let properties: serde_json::Map<String, Value> = OPTIONS
        .iter()
        .filter(|option| option.applies & tool.bit() != 0)
        .map(|option| (option.name.to_string(), schema_of(option)))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": ["url"],
        "additionalProperties": false,
    })
}

/// Immutable tool schemas; do not rebuild for every tools/list request.
pub fn tools_list() -> &'static Value {
    use std::sync::OnceLock;
    static LIST: OnceLock<Value> = OnceLock::new();
    LIST.get_or_init(|| {
        let tools: Vec<Value> = TOOL_TABLE
            .iter()
            .map(|entry| {
                json!({
                    "name": entry.name,
                    "title": entry.title,
                    "description": entry.description,
                    "inputSchema": input_schema(entry.id),
                    "annotations": annotations(),
                })
            })
            .collect();
        Value::Array(tools)
    })
}

pub enum CallError {
    /// Invalid tool arguments are JSON-RPC errors, not tool results.
    Invalid(String),
    /// Execution failure returned as an isError tool result.
    Tool(String),
}

/// Browser-free tools never see the stop flag on their own; abandon them on
/// SIGTERM/SIGINT so shutdown does not wait out their timeouts.
async fn until_stopped(
    browser: &Browser,
    work: impl Future<Output = Result<Value, String>>,
) -> Result<Value, CallError> {
    tokio::select! {
        result = work => result.map_err(CallError::Tool),
        () = browser.wait_stopped() => Err(CallError::Tool("Service is stopping".to_string())),
    }
}

pub async fn execute(browser: &Browser, name: &str, args: &Value) -> Result<Value, CallError> {
    let tool = parse_tool(name).map_err(CallError::Invalid)?;
    if tool == Tool::Fetch {
        let request = fetch::Request::parse(args).map_err(CallError::Invalid)?;
        return until_stopped(browser, fetch::run(&request)).await;
    }
    // Search takes its own arguments (no URL), so it bypasses parse_request.
    if tool == Tool::Search {
        let query = crate::search::Query::parse(args).map_err(CallError::Invalid)?;
        return until_stopped(browser, crate::search::execute(&query)).await;
    }
    let request = parse_request(args, tool).map_err(CallError::Invalid)?;
    if tool == Tool::Scrape {
        return browser.scrape(&request).await.map_err(CallError::Tool);
    }
    let limit = parse_integer(args, "limit").map_err(CallError::Invalid)?;
    let max_depth = parse_integer(args, "maxDepth").map_err(CallError::Invalid)?;
    let concurrency = parse_integer(args, "maxConcurrency").map_err(CallError::Invalid)?;
    Ok(crawl::run(
        browser,
        tool,
        request,
        limit,
        max_depth,
        usize::try_from(concurrency).expect("maxConcurrency bounds are nonnegative"),
    )
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizeessentials() {
        assert_eq!(
            normalize_url("https://Example.COM").unwrap(),
            "https://example.com/"
        );
        assert_eq!(normalize_url("http://x:80").unwrap(), "http://x/");
        assert_eq!(normalize_url("http://x:8080").unwrap(), "http://x:8080/");
        assert_eq!(
            normalize_url("https://x:443/a?b#c").unwrap(),
            "https://x/a?b"
        );
        assert_eq!(normalize_url("http://x").unwrap(), "http://x/");
        assert_eq!(normalize_url("http://x?z").unwrap(), "http://x/?z");
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("+https://x").is_err());
        assert!(normalize_url("http://x\\@y").is_err());
        assert!(normalize_url("https://user:pass@example.com").is_err());
        assert!(normalize_url("https://example.com:99999").is_err());
        assert!(normalize_url("http://[dead]/").is_err());
        assert!(normalize_url("http://[:::]/").is_err());
        assert!(normalize_url("http://[1:2:3]/").is_err());
        assert_eq!(
            normalize_url("http://[::1]:80/a").unwrap(),
            "http://[::1]/a"
        );
        assert_eq!(normalize_url("http://x:/a").unwrap(), "http://x/a");
        assert_eq!(
            normalize_url("https://münchen.de/").unwrap(),
            "https://xn--mnchen-3ya.de/"
        );
        assert_eq!(
            normalize_url("https://x/a/../b/./c").unwrap(),
            "https://x/b/c"
        );
        assert!(normalize_url("https://x:0/").is_err());
        assert!(normalize_url("https://user@x/").is_err());
    }

    #[test]
    fn origins() {
        assert_eq!(origin("http://x/a/b"), "http://x");
        assert_eq!(origin("http://x"), "http://x");
    }

    #[test]
    fn validation_errors() {
        let tool = Tool::Scrape;
        assert!(parse_request(&json!({}), tool).is_err());
        assert!(
            parse_request(
                &json!({"url": "https://example.com", "timeout": "100"}),
                tool
            )
            .is_err()
        );
        assert!(
            parse_request(&json!({"url": "https://example.com", "formats": []}), tool).is_err()
        );
        assert!(
            parse_request(
                &json!({"url": "https://example.com", "formats": ["pdf"]}),
                tool
            )
            .is_err()
        );
        assert!(
            parse_request(
                &json!({"url": "https://example.com", "onlyMainContent": "true"}),
                tool
            )
            .is_err()
        );
        assert!(
            parse_request(
                &json!({"url": "https://example.com", "unknown": true}),
                tool
            )
            .is_err()
        );
        // Discovery-only options are rejected by scrape but accepted by crawl.
        assert!(parse_request(&json!({"url": "https://example.com", "limit": 5}), tool).is_err());
        let ok = parse_request(
            &json!({"url": "https://example.com", "limit": 5}),
            Tool::Crawl,
        )
        .unwrap();
        assert_eq!(ok.formats, vec![Format::Markdown]);
    }

    #[test]
    fn schema_shape() {
        let list = tools_list();
        let tools = list.as_array().unwrap();
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["scrape", "fetch", "crawl", "map", "search"]);
        for tool in tools {
            if tool["name"] == "search" {
                let properties = &tool["inputSchema"]["properties"];
                assert_eq!(tool["inputSchema"]["required"], json!(["query"]));
                assert_eq!(properties["limit"]["maximum"], 20);
                assert_eq!(
                    properties["engine"]["enum"],
                    json!(["auto", "duckduckgo", "brave"])
                );
                continue;
            }
            let properties = &tool["inputSchema"]["properties"];
            if tool["name"] == "fetch" {
                assert_eq!(tool["inputSchema"]["required"], json!(["url"]));
                let map = properties.as_object().expect("object schema");
                assert_eq!(map.len(), 2);
                assert!(map.contains_key("url") && map.contains_key("timeout"));
                assert_eq!(tool["title"], "Fetch resource");
                continue;
            }
            for name in ["waitForSelector", "waitForText"] {
                assert_eq!(properties[name]["type"], "string");
                assert_eq!(properties[name]["minLength"], 1);
            }
            let has_concurrency = properties.get("maxConcurrency").is_some();
            assert_eq!(
                has_concurrency,
                tool["name"] == "crawl" || tool["name"] == "map"
            );
            if has_concurrency {
                assert_eq!(properties["maxConcurrency"]["default"], 4);
                assert_eq!(properties["maxConcurrency"]["minimum"], 1);
                assert_eq!(properties["maxConcurrency"]["maximum"], 16);
            }
        }
    }
}
