//! Keyless web search, feature-gated behind `--features search`.
//!
//! Engine recipes are ported from `ddgs` (`ddgs/engines/duckduckgo.py` and
//! `brave.py`): primp performs browser-impersonated requests, and `scraper`
//! reads the HTML engine pages with the CSS equivalent of ddgs' item XPaths.
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Auto,
    DuckDuckGo,
    Brave,
}

impl Engine {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "duckduckgo" | "ddg" => Ok(Self::DuckDuckGo),
            "brave" => Ok(Self::Brave),
            _ => Err("engine must be auto, duckduckgo, or brave".to_string()),
        }
    }
}

/// Validated `search` arguments.
pub struct Query {
    text: String,
    limit: usize,
    engine: Engine,
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
            Some(value) => {
                let Some(number) = as_integer(value) else {
                    return Err("limit must be an integer".to_string());
                };
                if number < 1 || number > MAX_LIMIT as i128 {
                    return Err("limit is outside the supported range".to_string());
                }
                number as i64
            }
        };
        let engine = match args.get("engine") {
            None => Engine::Auto,
            Some(value) => {
                let Some(name) = value.as_str() else {
                    return Err("engine must be a string".to_string());
                };
                Engine::parse(name)?
            }
        };
        let timeout = match args.get("timeout") {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => {
                let Some(number) = as_integer(value) else {
                    return Err("timeout must be an integer".to_string());
                };
                if number < MIN_TIMEOUT_MS as i128 || number > MAX_TIMEOUT_MS as i128 {
                    return Err("timeout is outside the supported range".to_string());
                }
                number as i64
            }
        };
        Ok(Self {
            text: text.to_string(),
            limit: limit as usize,
            engine,
            timeout: Duration::from_millis(timeout as u64),
        })
    }
}

/// Schema row for `tools/list`; lives here so `tools.rs` stays table-driven.
pub fn tool_definition() -> Value {
    json!({
        "name": "search",
        "description": "Search the web without an API key using browser-impersonated requests \
                        to DuckDuckGo and Brave. Returns titles, URLs, and snippets.",
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
    let single = [query.engine];
    let engines: &[Engine] = if query.engine == Engine::Auto {
        &[Engine::DuckDuckGo, Engine::Brave]
    } else {
        &single
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
            Engine::Auto => unreachable!("auto is expanded above"),
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
                .timeout(Duration::from_secs(30))
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
        Engine::Auto => unreachable!("auto never reaches an engine request"),
    };
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| "Search request timed out".to_string())?
        .map_err(|error| format!("Search request failed: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Search engine returned HTTP {status}"));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("Search response failed: {}", error.without_url()))?;
    Ok(match engine {
        Engine::DuckDuckGo => parse_ddg(&body, base),
        Engine::Brave => parse_brave(&body, base),
        Engine::Auto => unreachable!(),
    })
}

struct Item {
    title: String,
    url: String,
    description: String,
}

/// Keep first occurrence of each normalized URL, in engine order.
fn dedupe(items: Vec<Item>, limit: usize) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
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
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// DuckDuckGo HTML endpoint: `//div[contains(@class,'body')]` items, `h2`
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
            .map(|element| collapse(&element))
            .unwrap_or_else(|| collapse(&anchor));
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
    normalize_url(&resolve(base, &target)).ok()
}

/// DuckDuckGo wraps results in `/l/?uddg=<encoded target>`; recover the target.
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
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Minimal RFC 3986 resolution: absolute, protocol-relative, root-relative,
/// and relative paths (the shapes engine pages emit).
fn resolve(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let origin_end = rest.find('/').unwrap_or(rest.len());
    let origin = &rest[..origin_end];
    if let Some(protocol_relative) = href.strip_prefix("//") {
        return format!("{scheme}://{protocol_relative}");
    }
    if href.starts_with('/') {
        return format!("{scheme}://{origin}{href}");
    }
    let path = &rest[origin_end..];
    let directory = &path[..path.rfind('/').map_or(0, |index| index + 1)];
    format!("{scheme}://{origin}{directory}{href}")
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
        assert!(query.engine == Engine::Brave);
        let defaults = Query::parse(&json!({"query": "rust"})).expect("valid query");
        assert!(defaults.engine == Engine::Auto);
        assert_eq!(defaults.limit, DEFAULT_LIMIT as usize);
        assert_eq!(
            defaults.timeout,
            Duration::from_millis(DEFAULT_TIMEOUT_MS as u64)
        );
    }

    #[test]
    fn resolution_covers_engine_href_shapes() {
        assert_eq!(
            resolve("https://example.com/base/page", "https://other.test/x"),
            "https://other.test/x"
        );
        assert_eq!(
            resolve("https://example.com/base/page", "//cdn.test/x"),
            "https://cdn.test/x"
        );
        assert_eq!(
            resolve("https://example.com/base/page", "/root"),
            "https://example.com/root"
        );
        assert_eq!(
            resolve("https://example.com/base/page", "child"),
            "https://example.com/base/child"
        );
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fa%20b+c"),
            "https://example.com/a%20b+c"
        );
    }
}
