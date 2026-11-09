//! Tool table, option schema, argument validation, and URL normalization.
//! Schema bounds, defaults, and per-tool applicability all derive from the
//! same static tables.

mod crawl;
mod fetch;
mod paths;
mod sitemap;

use crate::browser::{Browser, Format, PageRequest};
use crate::json::as_integer;
use serde_json::{Value, json};
use std::time::Duration;

/// Frontier cap for crawl/map link discovery (M4).
pub const MAX_LINKS: usize = 10000;
/// Longest accepted URL, before and after normalization.
const MAX_URL_BYTES: usize = 8192;
/// Serialization budget for crawl results.
const MAX_CRAWL_BYTES: usize = 1024 * 1024;
/// Longest accepted `includePaths` or `excludePaths` prefix.
const MAX_PREFIX_BYTES: usize = 2048;

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
        description: "Render one page in a browser and extract the requested formats. Use crawl or map for many pages.",
    },
    ToolDef {
        id: Tool::Fetch,
        name: "fetch",
        title: "Fetch resource",
        description: "Fetch one raw non-HTML resource (JSON, text, XML, images, files) without a browser. Use scrape for HTML.",
    },
    ToolDef {
        id: Tool::Crawl,
        name: "crawl",
        title: "Crawl site",
        description: "Breadth-first crawl of same-origin pages; returns pages and per-page errors.",
    },
    ToolDef {
        id: Tool::Map,
        name: "map",
        title: "Map links",
        description: "List links found by a bounded same-origin crawl; external links are listed, not followed. Optionally merge same-origin URLs from robots.txt sitemaps without opening them.",
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
    /// One of a fixed set of strings; the first is the default.
    Choice(&'static [&'static str]),
    /// Array of strings. `maximum` is `maxItems`.
    StringList,
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

const OPTIONS: [OptionDef; 16] = [
    opt!(
        "url",
        OptionKind::Url,
        "HTTP(S) URL without credentials",
        tools = SCRAPE | CRAWL | MAP | FETCH
    ),
    opt!(
        "formats",
        OptionKind::Formats,
        "Output formats",
        tools = SCRAPE | CRAWL
    ),
    opt!(
        "onlyMainContent",
        OptionKind::Boolean,
        "Drop page chrome; prefer main/article content",
        tools = SCRAPE | CRAWL,
        fallback = 1,
        minimum = 0,
        maximum = 0
    ),
    opt!(
        "timeout",
        OptionKind::Integer,
        "Time budget in ms; per page for crawl/map",
        tools = SCRAPE | CRAWL | MAP | FETCH,
        fallback = 30000,
        minimum = 100,
        maximum = 120_000
    ),
    opt!(
        "waitFor",
        OptionKind::Integer,
        "Extra ms to wait after load",
        tools = SCRAPE | CRAWL | MAP,
        fallback = 0,
        minimum = 0,
        maximum = 30000
    ),
    opt!(
        "waitForSelector",
        OptionKind::String,
        "Wait until this CSS selector matches",
        tools = SCRAPE | CRAWL | MAP,
        fallback = 0,
        minimum = 0,
        maximum = 2048
    ),
    opt!(
        "waitForText",
        OptionKind::String,
        "Wait until body text contains this literal, case-sensitive string",
        tools = SCRAPE | CRAWL | MAP,
        fallback = 0,
        minimum = 0,
        maximum = 4096
    ),
    opt!(
        "maxChars",
        OptionKind::Integer,
        "Cut each content format to this many characters (0 = no limit)",
        tools = SCRAPE | CRAWL,
        fallback = 0,
        minimum = 0,
        maximum = 10_000_000
    ),
    opt!(
        "markdownLinks",
        OptionKind::Choice(&["full", "relative", "none"]),
        "Markdown link/image targets: full URL, same-origin path, or text only",
        tools = SCRAPE | CRAWL
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
        "Pages fetched at once",
        tools = CRAWL | MAP,
        fallback = 4,
        minimum = 1,
        maximum = 16
    ),
    opt!(
        "sitemap",
        OptionKind::Boolean,
        "Read Sitemap directives from robots.txt and merge sitemap URLs. Off by default. Does not open those URLs in the browser.",
        tools = MAP,
        fallback = 0,
        minimum = 0,
        maximum = 0
    ),
    opt!(
        "includePaths",
        OptionKind::StringList,
        "Path prefixes a returned link must match. Empty allows every path. A prefix matches on a segment boundary and does not change which pages are opened.",
        tools = MAP,
        fallback = 0,
        minimum = 0,
        maximum = 32
    ),
    opt!(
        "excludePaths",
        OptionKind::StringList,
        "Path prefixes removed from the returned links. Matching does not change which pages are opened.",
        tools = MAP,
        fallback = 0,
        minimum = 0,
        maximum = 32
    ),
    opt!(
        "linkLimit",
        OptionKind::Integer,
        "Maximum links to return, from 1 to 10000. Defaults to 100 when sitemap is true. When omitted with sitemap false, the existing link and byte ceilings apply.",
        tools = MAP,
        fallback = 0,
        minimum = 1,
        maximum = 10_000
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

fn parse_choice(args: &Value, name: &str) -> Result<&'static str, String> {
    let OptionKind::Choice(choices) = known_option(name).kind else {
        panic!("{name} is not a choice option");
    };
    let Some(value) = args.get(name) else {
        return Ok(choices[0]);
    };
    value
        .as_str()
        .and_then(|text| choices.iter().find(|choice| **choice == text).copied())
        .ok_or_else(|| format!("{name} must be one of: {}", choices.join(", ")))
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
        OptionKind::Integer => {
            let mut schema = json!({
                "type": "integer",
                "minimum": option.minimum,
                "maximum": option.maximum,
                "description": option.description,
            });
            // An omitted `linkLimit` is not a numeric default. Zero means absent.
            if option.name != "linkLimit" {
                schema["default"] = Value::from(option.fallback);
            }
            schema
        }
        // `maxLength` counts characters. Validation uses UTF-8 bytes, so the
        // byte cap lives in the description instead of a character limit.
        OptionKind::String => {
            let description = format!(
                "{}, at most {} UTF-8 bytes",
                option.description, option.maximum
            );
            json!({
                "type": "string",
                "minLength": 1,
                "description": description,
            })
        }
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
        OptionKind::Choice(choices) => json!({
            "type": "string",
            "enum": choices,
            "default": choices[0],
            "description": option.description,
        }),
        OptionKind::StringList => json!({
            "type": "array",
            "items": {"type": "string"},
            "maxItems": option.maximum,
            "default": [],
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

/// Map return filters and the optional sitemap read.
pub struct MapQuery {
    pub sitemap: bool,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub link_limit: Option<usize>,
}

/// Read map return filters. `link_limit` is `None` when the key is absent and
/// `sitemap` is false. `sitemap: true` without `linkLimit` uses 100.
pub fn parse_map_query(args: &Value) -> Result<MapQuery, String> {
    let sitemap = parse_boolean(args, "sitemap")?;
    let mut link_limit = match args.get("linkLimit") {
        None => None,
        Some(value) => {
            let option = known_option("linkLimit");
            let limit = bounded_integer(value, "linkLimit", option.minimum, option.maximum)?;
            Some(usize::try_from(limit).expect("linkLimit bounds are nonnegative"))
        }
    };
    let include_paths = parse_prefixes(args, "includePaths")?;
    let exclude_paths = parse_prefixes(args, "excludePaths")?;
    if sitemap && link_limit.is_none() {
        link_limit = Some(100);
    }
    Ok(MapQuery {
        sitemap,
        include_paths,
        exclude_paths,
        link_limit,
    })
}

fn parse_prefixes(args: &Value, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(name) else {
        return Ok(Vec::new());
    };
    let Some(list) = value.as_array() else {
        return Err(format!("{name} must be an array of strings"));
    };
    let max_items = usize::try_from(known_option(name).maximum).expect("prefix cap is nonnegative");
    if list.len() > max_items {
        return Err(format!("{name} accepts at most {max_items} prefixes"));
    }
    let mut prefixes = Vec::with_capacity(list.len());
    for value in list {
        let Some(text) = value.as_str() else {
            return Err(format!("{name} must be an array of strings"));
        };
        if text.is_empty()
            || text.len() > MAX_PREFIX_BYTES
            || !text.starts_with('/')
            || text.contains('?')
            || text.contains('#')
        {
            return Err(format!(
                "{name} entries must be path prefixes of at most {MAX_PREFIX_BYTES} bytes starting with '/' and containing no '?' or '#'"
            ));
        }
        prefixes.push(text.to_string());
    }
    Ok(prefixes)
}

pub fn parse_request(args: &Value, tool: Tool) -> Result<PageRequest, String> {
    check_keys(args, tool)?;
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "url must be a string".to_string())?;
    let timeout = parse_millis(args, "timeout")?;
    // A wait longer than the page budget cannot finish; shorten it here so
    // crawl pages inherit a wait that fits this request's own timeout.
    let wait_for = parse_millis(args, "waitFor")?.min(timeout);
    let mut request = PageRequest {
        url: normalize_url(url)?,
        formats: Vec::new(),
        only_main_content: parse_boolean(args, "onlyMainContent")?,
        timeout,
        wait_for,
        wait_for_selector: parse_string(args, "waitForSelector")?,
        wait_for_text: parse_string(args, "waitForText")?,
        max_chars: usize::try_from(parse_integer(args, "maxChars")?)
            .expect("maxChars bounds are nonnegative"),
        markdown_links: parse_choice(args, "markdownLinks")?,
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
    let map_query = if tool == Tool::Map {
        parse_map_query(args).map_err(CallError::Invalid)?
    } else {
        MapQuery {
            sitemap: false,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            link_limit: None,
        }
    };
    Ok(crawl::run(
        browser,
        tool,
        request,
        limit,
        max_depth,
        usize::try_from(concurrency).expect("maxConcurrency bounds are nonnegative"),
        map_query,
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
                &json!({"url": "https://example.com", "markdownLinks": "short"}),
                tool
            )
            .is_err()
        );
        assert_eq!(
            parse_request(&json!({"url": "https://example.com"}), tool)
                .unwrap()
                .markdown_links,
            "full"
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
    fn wait_for_is_clamped_to_the_page_timeout() {
        let request = parse_request(
            &json!({"url": "https://example.com", "timeout": 1000, "waitFor": 30000}),
            Tool::Scrape,
        )
        .expect("valid request");
        assert_eq!(request.timeout, Duration::from_millis(1000));
        assert_eq!(request.wait_for, request.timeout);
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
                assert!(properties["query"].get("maxLength").is_none());
                assert!(
                    properties["query"]["description"]
                        .as_str()
                        .unwrap()
                        .contains("UTF-8 bytes")
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
                assert!(properties[name].get("maxLength").is_none());
                assert!(
                    properties[name]["description"]
                        .as_str()
                        .unwrap()
                        .contains("UTF-8 bytes")
                );
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
            let map_only = ["sitemap", "includePaths", "excludePaths", "linkLimit"];
            let is_map = tool["name"] == "map";
            for name in map_only {
                assert_eq!(properties.get(name).is_some(), is_map, "{name}");
            }
            if is_map {
                assert_eq!(properties["sitemap"]["type"], "boolean");
                assert_eq!(properties["sitemap"]["default"], json!(false));
                assert_eq!(properties["includePaths"]["type"], "array");
                assert_eq!(properties["includePaths"]["items"]["type"], "string");
                assert_eq!(properties["includePaths"]["maxItems"], 32);
                assert_eq!(properties["includePaths"]["default"], json!([]));
                assert_eq!(properties["excludePaths"]["type"], "array");
                assert_eq!(properties["excludePaths"]["maxItems"], 32);
                assert_eq!(properties["excludePaths"]["default"], json!([]));
                assert_eq!(properties["linkLimit"]["type"], "integer");
                assert_eq!(properties["linkLimit"]["minimum"], 1);
                assert_eq!(properties["linkLimit"]["maximum"], 10000);
                assert!(properties["linkLimit"].get("default").is_none());
                let description = properties["linkLimit"]["description"].as_str().unwrap();
                assert!(description.contains("100"), "{description}");
                assert!(description.contains("sitemap"), "{description}");
            }
        }
    }

    #[test]
    fn parse_map_query() {
        let bare = super::parse_map_query(&json!({"url": "https://example.com"})).unwrap();
        assert!(!bare.sitemap);
        assert_eq!(bare.include_paths, Vec::<String>::new());
        assert_eq!(bare.exclude_paths, Vec::<String>::new());
        assert_eq!(bare.link_limit, None);

        let query = super::parse_map_query(&json!({
            "includePaths": ["/careers"],
            "excludePaths": ["/careers/draft"],
            "linkLimit": 5,
        }))
        .unwrap();
        assert!(!query.sitemap);
        assert_eq!(query.include_paths, vec!["/careers".to_string()]);
        assert_eq!(query.exclude_paths, vec!["/careers/draft".to_string()]);
        assert_eq!(query.link_limit, Some(5));

        let accepted = super::parse_map_query(&json!({"includePaths": ["/careers-old"]})).unwrap();
        assert_eq!(accepted.include_paths, vec!["/careers-old".to_string()]);

        let sitemap_default =
            super::parse_map_query(&json!({"url": "https://example.com", "sitemap": true}))
                .unwrap();
        assert!(sitemap_default.sitemap);
        assert_eq!(sitemap_default.link_limit, Some(100));
        let sitemap_capped =
            super::parse_map_query(&json!({"sitemap": true, "linkLimit": 5})).unwrap();
        assert!(sitemap_capped.sitemap);
        assert_eq!(sitemap_capped.link_limit, Some(5));

        let at_item_cap = vec!["/a"; 32];
        assert!(super::parse_map_query(&json!({"includePaths": at_item_cap})).is_ok());
        let at_byte_cap = format!("/{}", "a".repeat(2047));
        assert!(super::parse_map_query(&json!({"includePaths": [at_byte_cap]})).is_ok());

        let too_many = vec!["/a"; 33];
        let too_long = format!("/{}", "a".repeat(2048));
        for args in [
            json!({"includePaths": ["careers"]}),
            json!({"includePaths": [""]}),
            json!({"includePaths": ["/a?b"]}),
            json!({"includePaths": ["/a#b"]}),
            json!({"includePaths": too_many}),
            json!({"includePaths": [too_long]}),
            json!({"includePaths": "nope"}),
            json!({"linkLimit": 0}),
            json!({"linkLimit": 10001}),
        ] {
            assert!(super::parse_map_query(&args).is_err(), "{args}");
        }

        let crawl_error = parse_request(
            &json!({"url": "https://example.com", "excludePaths": ["/a"]}),
            Tool::Crawl,
        )
        .unwrap_err();
        assert_eq!(crawl_error, "Unknown argument: excludePaths");
    }
}
