//! Keyless web search, feature-gated behind `--features search`.
//!
//! Engine recipes are ported from `ddgs` (`ddgs/engines/duckduckgo.py` and
//! `brave.py`): primp performs browser-impersonated requests, and `scraper`
//! reads the HTML engine pages with the CSS equivalent of ddgs' item `XPaths`.
//! Results are deduplicated by normalized URL and capped at `limit`.
//!
//! The response mirrors Firecrawl v2 search (`{"success": true, "data":
//! {"web": [...]}}`) so callers can swap a keyed service for this tool.
//!
//! `FIREPANDA_SEARCH_DDG_URL` and `FIREPANDA_SEARCH_BRAVE_URL` override the
//! engine endpoints; the offline fixture tests use them to point at a local
//! server instead of the network.

use crate::json::as_integer;
use crate::tools::normalize_url;
use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_LIMIT: i64 = 5;
const MAX_LIMIT: i64 = 20;
const QUERY_MAX_BYTES: usize = 512;
const DEFAULT_TIMEOUT_MS: i64 = 15000;
const MIN_TIMEOUT_MS: i64 = 1000;
const MAX_TIMEOUT_MS: i64 = 60000;

const DDG_URL: &str = "https://html.duckduckgo.com/html/";
const BRAVE_URL: &str = "https://search.brave.com/search";
const DDG_OVERRIDE: &str = "FIREPANDA_SEARCH_DDG_URL";
const BRAVE_OVERRIDE: &str = "FIREPANDA_SEARCH_BRAVE_URL";
/// Engine pages are small; cap the body so a pathological response cannot
/// consume unbounded memory before parsing.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Shared by `tools/list` and the tool table so the two cannot drift.
pub const DESCRIPTION: &str = "Search the web without an API key using browser-impersonated \
                               requests to DuckDuckGo and Brave. Returns titles, URLs, and \
                               snippets.";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Engine {
    DuckDuckGo,
    Brave,
}

impl Engine {
    /// `None` means `auto` (try engines in order).
    fn parse(value: &str) -> Result<Option<Self>, String> {
        match value {
            "auto" => Ok(None),
            "duckduckgo" | "ddg" => Ok(Some(Self::DuckDuckGo)),
            "brave" => Ok(Some(Self::Brave)),
            _ => Err("engine must be auto, duckduckgo, or brave".to_string()),
        }
    }
}

/// Validated `search` arguments.
pub struct Query {
    text: String,
    limit: usize,
    /// `None` selects the documented auto fallback order.
    engine: Option<Engine>,
    timeout: Duration,
}

impl Query {
    pub fn parse(args: &Value) -> Result<Self, String> {
        let Some(object) = args.as_object() else {
            return Err("arguments must be an object".to_string());
        };
        for key in object.keys() {
            if !matches!(key.as_str(), "query" | "limit" | "engine" | "timeout") {
                return Err(format!("Unknown argument: {key}"));
            }
        }
        let Some(text) = args.get("query").and_then(Value::as_str) else {
            return Err("query must be a string".to_string());
        };
        if text.trim().is_empty() || text.len() > QUERY_MAX_BYTES {
            return Err(format!(
                "query must be a nonempty string within {QUERY_MAX_BYTES} UTF-8 bytes"
            ));
        }
        let limit = match args.get("limit") {
            None => DEFAULT_LIMIT,
            Some(value) => bounded_integer(value, "limit", 1, MAX_LIMIT)?,
        };
        let engine = match args.get("engine") {
            None => None,
            Some(value) => {
                let Some(name) = value.as_str() else {
                    return Err("engine must be a string".to_string());
                };
                Engine::parse(name)?
            }
        };
        let timeout = match args.get("timeout") {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => bounded_integer(value, "timeout", MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)?,
        };
        Ok(Self {
            text: text.to_string(),
            limit: usize::try_from(limit).expect("limit bounds are positive"),
            engine,
            timeout: Duration::from_millis(
                u64::try_from(timeout).expect("timeout bounds are positive"),
            ),
        })
    }
}

/// Integer argument checked against its documented inclusive range.
fn bounded_integer(value: &Value, name: &str, minimum: i64, maximum: i64) -> Result<i64, String> {
    let Some(number) = as_integer(value) else {
        return Err(format!("{name} must be an integer"));
    };
    let Ok(number) = i64::try_from(number) else {
        return Err(format!("{name} is outside the supported range"));
    };
    if number < minimum || number > maximum {
        return Err(format!("{name} is outside the supported range"));
    }
    Ok(number)
}

/// Schema row for `tools/list`; lives here so `tools.rs` stays table-driven.
pub fn tool_definition() -> Value {
    json!({
        "name": "search",
        "description": DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": QUERY_MAX_BYTES,
                    "description": "Search query.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "default": DEFAULT_LIMIT,
                    "description": "Maximum results to return.",
                },
                "engine": {
                    "type": "string",
                    "enum": ["auto", "duckduckgo", "brave"],
                    "default": "auto",
                    "description": "Engine to use; auto falls back to the next engine.",
                },
                "timeout": {
                    "type": "integer",
                    "minimum": MIN_TIMEOUT_MS,
                    "maximum": MAX_TIMEOUT_MS,
                    "default": DEFAULT_TIMEOUT_MS,
                    "description": "Total search budget in milliseconds.",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        },
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "openWorldHint": true,
        },
    })
}

/// Run one search, trying engines in order until one yields results.
pub async fn execute(query: &Query) -> Result<Value, String> {
    let both = [Engine::DuckDuckGo, Engine::Brave];
    let single = [query.engine.unwrap_or(Engine::DuckDuckGo)];
    let engines: &[Engine] = if query.engine.is_some() {
        &single
    } else {
        &both
    };
    let deadline = tokio::time::Instant::now() + query.timeout;
    let mut last_error = String::new();
    for engine in engines {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            last_error = "Search request timed out".to_string();
            break;
        }
        let base = match engine {
            Engine::DuckDuckGo => endpoint(DDG_OVERRIDE, DDG_URL),
            Engine::Brave => endpoint(BRAVE_OVERRIDE, BRAVE_URL),
        };
        match search_engine(*engine, &base, &query.text, remaining).await {
            Ok(items) if !items.is_empty() => {
                return Ok(json!({"success": true, "data": {"web": dedupe(items, query.limit)}}));
            }
            Ok(_) => last_error = "No search results".to_string(),
            Err(message) => last_error = message,
        }
    }
    Err(if last_error.is_empty() {
        "No search results".to_string()
    } else {
        last_error
    })
}

fn endpoint(override_name: &str, default_url: &str) -> String {
    std::env::var(override_name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_url.to_string())
}

fn client() -> Result<&'static primp::Client, String> {
    static CLIENT: OnceLock<Result<primp::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            primp::Client::builder()
                .impersonate(primp::Impersonate::ChromeV146)
                // Backstop only: each attempt is bounded by the remaining
                // tool budget, which may be the full 60-second maximum.
                .timeout(Duration::from_millis(
                    u64::try_from(MAX_TIMEOUT_MS).expect("positive timeout bound"),
                ))
                .build()
                .map_err(|error| format!("Search client failed: {}", error.without_url()))
        })
        .as_ref()
        .map_err(Clone::clone)
}

async fn search_engine(
    engine: Engine,
    base: &str,
    query: &str,
    timeout: Duration,
) -> Result<Vec<Item>, String> {
    let client = client()?;
    let request = match engine {
        // GET keeps the HTML endpoint from serving a cookie challenge.
        Engine::DuckDuckGo => client
            .get(base)
            .query(&[("q", query), ("b", ""), ("l", "us-en")]),
        Engine::Brave => client.get(base).query(&[("q", query), ("source", "web")]),
    };
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| "Search request timed out".to_string())?
        .map_err(|error| format!("Search request failed: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Search engine returned HTTP {status}"));
    }
    let body = read_body(response).await?;
    Ok(match engine {
        Engine::DuckDuckGo => parse_ddg(&body, base),
        Engine::Brave => parse_brave(&body, base),
    })
}

/// Read an engine page with a hard size cap. Engine pages are UTF-8; a
/// lossy decode only matters for malformed responses that fail parsing anyway.
async fn read_body(mut response: primp::Response) -> Result<String, String> {
    let declared = response.content_length();
    if declared.is_some_and(|length| length > u64::try_from(MAX_BODY_BYTES).unwrap_or(u64::MAX)) {
        return Err(body_too_large());
    }
    let expected = usize::try_from(declared.unwrap_or(0))
        .unwrap_or(MAX_BODY_BYTES)
        .min(MAX_BODY_BYTES);
    let mut body = Vec::with_capacity(expected);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Search response failed: {}", error.without_url()))?
    {
        if chunk.len() > MAX_BODY_BYTES - body.len() {
            return Err(body_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(text_or_lossy(body))
}

fn body_too_large() -> String {
    format!(
        "Search response exceeds {} MiB",
        MAX_BODY_BYTES / (1024 * 1024)
    )
}

/// UTF-8 is the normal case; a lossy decode only matters for malformed pages.
fn text_or_lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

struct Item {
    title: String,
    url: String,
    description: String,
}

/// Keep first occurrence of each normalized URL, in engine order.
fn dedupe(items: Vec<Item>, limit: usize) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::with_capacity(items.len().min(limit));
    for item in items {
        if results.len() == limit {
            break;
        }
        if seen.insert(item.url.clone()) {
            results.push(json!({
                "title": item.title,
                "description": item.description,
                "url": item.url,
            }));
        }
    }
    results
}

/// CSS selector construction is static; an invalid selector is a bug.
fn selector(css: &str) -> Selector {
    Selector::parse(css).unwrap_or_else(|_| panic!("Invalid search selector: {css}"))
}

fn collapse(element: &ElementRef<'_>) -> String {
    let mut parts = element.text().flat_map(str::split_whitespace);
    let Some(first) = parts.next() else {
        return String::new();
    };
    let mut text = String::from(first);
    for part in parts {
        text.push(' ');
        text.push_str(part);
    }
    text
}

/// `DuckDuckGo` HTML endpoint: `//div[contains(@class,'body')]` items, `h2`
/// title, first anchor href, snippet text.
fn parse_ddg(document: &str, base: &str) -> Vec<Item> {
    let html = Html::parse_document(document);
    let items = selector("div[class*=\"body\"]");
    let titles = selector("h2");
    let snippets = selector("a[class*=\"snippet\"], div[class*=\"snippet\"]");
    let anchors = selector("a[href]");
    let mut results = Vec::new();
    for item in html.select(&items) {
        let Some(anchor) = item.select(&anchors).next() else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = result_url(href, base) else {
            continue;
        };
        let title = item
            .select(&titles)
            .next()
            .map(|element| collapse(&element))
            .unwrap_or_default();
        let description = item
            .select(&snippets)
            .next()
            .map_or_else(|| collapse(&anchor), |element| collapse(&element));
        if title.is_empty() || description.is_empty() {
            continue;
        }
        results.push(Item {
            title,
            url,
            description,
        });
    }
    results
}

/// Brave search: `//div[@data-type='web']` items, last title div, anchor that
/// wraps a title div, and `div.snippet div.content` text.
fn parse_brave(document: &str, base: &str) -> Vec<Item> {
    let html = Html::parse_document(document);
    let items = selector("div[data-type=\"web\"]");
    let title_divs = selector("div[class*=\"title\"]");
    let title_text = selector("div[class*=\"title\"], div[class*=\"sitename-container\"]");
    let snippets = selector("div[class*=\"snippet\"] div[class*=\"content\"]");
    let snippet_fallback = selector("div[class*=\"snippet\"]");
    let anchors = selector("a[href]");
    let mut results = Vec::new();
    for item in html.select(&items) {
        let Some(anchor) = item
            .select(&anchors)
            .find(|anchor| anchor.select(&title_divs).next().is_some())
            .or_else(|| item.select(&anchors).next())
        else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = result_url(href, base) else {
            continue;
        };
        let title = item
            .select(&title_text)
            .last()
            .map(|element| collapse(&element))
            .unwrap_or_default();
        let description = item
            .select(&snippets)
            .next()
            .or_else(|| item.select(&snippet_fallback).next())
            .map(|element| collapse(&element))
            .unwrap_or_default();
        if title.is_empty() || description.is_empty() {
            continue;
        }
        results.push(Item {
            title,
            url,
            description,
        });
    }
    results
}

/// Resolve an engine href against the engine page and normalize it. Ad
/// redirects and unsupported schemes are dropped.
fn result_url(href: &str, base: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    let target = decode_uddg(href).unwrap_or_else(|| href.to_string());
    if target.contains("duckduckgo.com/y.js") {
        return None;
    }
    // RFC 3986 resolution for absolute, protocol-relative, root-relative, and
    // relative hrefs; `normalize_url` then applies Firepanda's own URL rules.
    let base = url::Url::parse(base).ok()?;
    let joined = base.join(&target).ok()?.to_string();
    normalize_url(&joined).ok()
}

/// `DuckDuckGo` wraps results in `/l/?uddg=<encoded target>`; recover the target.
fn decode_uddg(href: &str) -> Option<String> {
    let marker = "uddg=";
    let start = href.find(marker)? + marker.len();
    let rest = &href[start..];
    let end = rest.find('&').unwrap_or(rest.len());
    Some(percent_decode(&rest[..end]))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    // Keep bytes that would make the URL invalid (space and
                    // controls) percent-encoded; the rest decode in place.
                    (Some(high), Some(low)) if high * 16 + low > 32 && high * 16 + low != 127 => {
                        output.push(high * 16 + low);
                        index += 3;
                    }
                    (Some(_), Some(_)) => {
                        output.extend_from_slice(&bytes[index..index + 3]);
                        index += 3;
                    }
                    _ => {
                        output.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    text_or_lossy(output)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddg_redirects_decode_and_ads_are_filtered() {
        let document = r#"
            <div class="result results_links result__body">
              <h2 class="result__title">
                <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage%20one&rut=abc">Example result</a>
              </h2>
              <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage%20one">First snippet</a>
            </div>
            <div class="result result--ad result__body">
              <h2><a class="result__a" href="//duckduckgo.com/y.js?ad_provider=x">Sponsored</a></h2>
              <a class="result__snippet">Buy things</a>
            </div>
        "#;
        let items = parse_ddg(document, DDG_URL);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Example result");
        assert_eq!(items[0].url, "https://example.com/page%20one");
        assert_eq!(items[0].description, "First snippet");
    }

    #[test]
    fn brave_results_use_the_wrapped_title_anchor() {
        let document = r#"
            <div data-type="web">
              <a href="https://brave.example/doc">
                <div class="title">Brave result</div>
              </a>
              <div class="snippet"><div class="content">Brave snippet text</div></div>
            </div>
            <div data-type="news">
              <a href="https://brave.example/news"><div class="title">Ignored</div></a>
              <div class="snippet"><div class="content">Not web</div></div>
            </div>
        "#;
        let items = parse_brave(document, BRAVE_URL);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Brave result");
        assert_eq!(items[0].url, "https://brave.example/doc");
        assert_eq!(items[0].description, "Brave snippet text");
    }

    #[test]
    fn dedupe_keeps_first_occurrence_and_limit() {
        let items = vec![
            Item {
                title: "one".to_string(),
                url: "https://example.com/".to_string(),
                description: "a".to_string(),
            },
            Item {
                title: "duplicate".to_string(),
                url: "https://example.com/".to_string(),
                description: "b".to_string(),
            },
            Item {
                title: "two".to_string(),
                url: "https://example.com/two".to_string(),
                description: "c".to_string(),
            },
        ];
        let results = dedupe(items, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], "one");
        assert_eq!(results[1]["title"], "two");
        let limited = dedupe(
            vec![
                Item {
                    title: "one".to_string(),
                    url: "https://example.com/".to_string(),
                    description: "a".to_string(),
                },
                Item {
                    title: "two".to_string(),
                    url: "https://example.com/two".to_string(),
                    description: "b".to_string(),
                },
            ],
            1,
        );
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn query_validation_bounds() {
        assert!(Query::parse(&json!({})).is_err());
        assert!(Query::parse(&json!({"query": "  "})).is_err());
        assert!(Query::parse(&json!({"query": "ok", "limit": 0})).is_err());
        assert!(Query::parse(&json!({"query": "ok", "limit": 21})).is_err());
        assert!(Query::parse(&json!({"query": "ok", "engine": "bing"})).is_err());
        assert!(Query::parse(&json!({"query": "ok", "timeout": 10})).is_err());
        assert!(Query::parse(&json!({"query": "ok", "url": "https://x/"})).is_err());
        let query = Query::parse(&json!({"query": "rust", "limit": 3, "engine": "brave"}))
            .expect("valid query");
        assert_eq!(query.limit, 3);
        assert_eq!(query.engine, Some(Engine::Brave));
        let defaults = Query::parse(&json!({"query": "rust"})).expect("valid query");
        assert_eq!(defaults.engine, None);
        assert_eq!(
            defaults.limit,
            usize::try_from(DEFAULT_LIMIT).expect("positive default")
        );
        assert_eq!(
            defaults.timeout,
            Duration::from_millis(DEFAULT_TIMEOUT_MS as u64)
        );
    }

    #[test]
    fn result_urls_cover_engine_href_shapes() {
        assert_eq!(
            result_url("https://other.test/x", DDG_URL).as_deref(),
            Some("https://other.test/x")
        );
        assert_eq!(
            result_url("//cdn.test/x", DDG_URL).as_deref(),
            Some("https://cdn.test/x")
        );
        assert_eq!(
            result_url("/root", DDG_URL).as_deref(),
            Some("https://html.duckduckgo.com/root")
        );
        assert_eq!(
            result_url("child", DDG_URL).as_deref(),
            Some("https://html.duckduckgo.com/html/child")
        );
        assert_eq!(
            result_url("../up", DDG_URL).as_deref(),
            Some("https://html.duckduckgo.com/up")
        );
        assert_eq!(result_url("javascript:void(0)", DDG_URL), None);
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fa%20b+c"),
            "https://example.com/a%20b+c"
        );
    }
}
