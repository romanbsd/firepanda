//! BFS crawl/map with bounded concurrency. The dispatch loop fills the window
//! in frontier order and consumes results oldest-first, so pages, links, and
//! errors keep sequential breadth-first order even though fetches overlap. A
//! failed page drains its
//! siblings (keeping their results) before the browser is reaped and
//! relaunched.

use super::{MAX_CRAWL_BYTES, MAX_LINKS, Tool, normalize_url, origin};
use crate::browser::{Browser, PageRequest, PageTask};
use crate::json::serialized_size_within;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Fixed crawl/map budget (C++ `Clock::now() + 120s`).
const CRAWL_BUDGET: Duration = Duration::from_secs(120);
/// Remaining budget below this stops dispatching (C++ `< 100ms`).
const DISPATCH_FLOOR: Duration = Duration::from_millis(100);

/// One dispatched page awaiting consumption, in dispatch order.
struct Inflight {
    depth: i64,
    url: String,
    task: PageTask,
}

/// Frontier, dedupe sets, and the ordered result arrays. `record` folds each
/// successful page exactly where the sequential crawl would have.
struct Crawl {
    tool: Tool,
    start_origin: String,
    max_depth: i64,
    return_links: bool,
    queue: VecDeque<(String, i64)>,
    queued: HashSet<String>,
    discovered: HashSet<String>,
    visited: HashSet<String>,
    pages: Vec<Value>,
    links: Vec<Value>,
    errors: Vec<Value>,
    warnings: Vec<Value>,
    attempted: i64,
    bytes: usize,
    truncated: bool,
    /// A bound was reached: stop dispatching; in-flight pages still drain.
    exhausted: bool,
    /// Serialization budget overflowed: later pages are dropped even though
    /// they were fetched, matching the sequential crawl that stops there.
    budget_reached: bool,
}

impl Crawl {
    fn new(tool: Tool, start: &str, max_depth: i64, return_links: bool) -> Self {
        Self {
            tool,
            start_origin: origin(start).to_string(),
            max_depth,
            return_links,
            queue: VecDeque::new(),
            queued: HashSet::new(),
            discovered: HashSet::new(),
            visited: HashSet::new(),
            pages: Vec::new(),
            links: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            attempted: 0,
            bytes: 0,
            truncated: false,
            exhausted: false,
            budget_reached: false,
        }
    }

    /// Fold one successful page into the result. `Err` means the page itself
    /// was malformed; the caller records it as a per-page error.
    fn record(&mut self, depth: i64, mut page: Value) -> Result<(), String> {
        if self.budget_reached {
            return Ok(()); // Sequential stops at the first overflow.
        }
        let listed = page
            .pointer("/metadata/url")
            .and_then(Value::as_str)
            .ok_or_else(|| "Page metadata must include a URL".to_string())?;
        let canonical = normalize_url(listed)?;
        // A redirect may reach a URL already queued through a direct link.
        // Track final destinations so it is neither rendered nor returned twice.
        if !self.visited.insert(canonical.clone()) {
            return Ok(());
        }
        // Redirected pages are returned, but external destinations never expand.
        let expand = origin(&canonical) == self.start_origin;
        if self.tool == Tool::Map
            && let Some(warnings) = page.get("warnings").cloned()
        {
            self.warnings
                .push(json!({"url": canonical, "warnings": warnings}));
        }
        if page.get("linksTruncated").and_then(Value::as_bool) == Some(true) {
            self.truncated = true;
        }
        if let Some(links) = page.get("links").and_then(Value::as_array) {
            for link in links {
                let Some(candidate) = link.get("url").and_then(Value::as_str) else {
                    continue;
                };
                let Ok(candidate) = normalize_url(candidate) else {
                    continue; // Invalid link URLs never join the frontier.
                };
                // Crawl only needs its frontier; retaining a second map of
                // every link would waste memory.
                if self.tool == Tool::Map && !self.discovered.contains(&candidate) {
                    if self.discovered.len() == MAX_LINKS {
                        self.truncated = true;
                    } else {
                        self.discovered.insert(candidate.clone());
                        let title = link
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.links.push(json!({"url": candidate, "title": title}));
                    }
                }
                if expand
                    && depth < self.max_depth
                    && origin(&candidate) == self.start_origin
                    && !self.queued.contains(&candidate)
                    && !self.visited.contains(&candidate)
                {
                    if self.queued.len() == MAX_LINKS {
                        self.truncated = true;
                    } else {
                        self.queued.insert(candidate.clone());
                        self.queue.push_back((candidate, depth + 1));
                    }
                }
            }
        }
        if self.tool == Tool::Crawl {
            if !self.return_links {
                // Leaf pages need no frontier links unless requested.
                if let Some(object) = page.as_object_mut() {
                    object.remove("links");
                    object.remove("linksTruncated");
                }
            }
            let Some(size) = serialized_size_within(&page, MAX_CRAWL_BYTES - self.bytes) else {
                self.truncated = true;
                self.exhausted = true;
                self.budget_reached = true;
                return Ok(());
            };
            self.bytes += size;
            self.pages.push(page);
        }
        Ok(())
    }
}

/// Run one crawl/map tool call to completion (C++ `execute` discovery path).
pub async fn run(
    browser: &Browser,
    tool: Tool,
    request: PageRequest,
    limit: i64,
    max_depth: i64,
    concurrency: usize,
) -> Value {
    let start_origin = request.url.clone();
    let deadline = Instant::now() + CRAWL_BUDGET;
    let page_timeout = request.timeout;
    let page_formats = if tool == Tool::Map {
        vec!["links".to_string()]
    } else {
        request.formats.clone()
    };
    let return_links = page_formats.iter().any(|format| format == "links");
    let mut crawl = Crawl::new(tool, &start_origin, max_depth, return_links);
    crawl.queued.insert(start_origin.clone());
    crawl.queue.push_back((start_origin, 0));
    let mut inflight: VecDeque<Inflight> = VecDeque::new();

    while !browser.stopped()
        && (!inflight.is_empty() || (!crawl.queue.is_empty() && !crawl.exhausted))
    {
        // Fill the window in frontier order; checkout runs here so the
        // managed browser is spawned exactly once per cold start.
        while !browser.stopped()
            && !crawl.exhausted
            && inflight.len() < concurrency
            && let Some((url, depth)) = crawl.queue.pop_front()
        {
            if crawl.visited.contains(&url) {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if crawl.attempted == limit || remaining < DISPATCH_FLOOR {
                crawl.truncated = true;
                crawl.exhausted = true;
                break;
            }
            crawl.attempted += 1;
            let mut page_request = request.clone();
            page_request.url = url.clone();
            page_request.timeout = std::cmp::min(page_timeout, remaining);
            page_request.formats = page_formats.clone();
            if (return_links || depth < crawl.max_depth)
                && !page_request.formats.iter().any(|format| format == "links")
            {
                page_request.formats.push("links".to_string());
            }
            let task = browser.start_page(page_request).await;
            inflight.push_back(Inflight { depth, url, task });
        }
        let Some(item) = inflight.pop_front() else {
            break; // Nothing in flight and nothing dispatchable.
        };
        let outcome = match item.task {
            PageTask::Failed(message) => Err(message),
            PageTask::Running(handle) => match handle.await {
                Ok(result) => result,
                Err(join_error) => Err(format!("Page worker failed: {join_error}")),
            },
        };
        match outcome {
            Ok(page) => {
                browser.page_completed().await; // Idle counts from the fetch.
                if let Err(message) = crawl.record(item.depth, page) {
                    // Recording failures come from a healthy page; the browser
                    // stays up.
                    crawl
                        .errors
                        .push(json!({"url": item.url, "error": message}));
                }
            }
            Err(message) => {
                crawl
                    .errors
                    .push(json!({"url": item.url, "error": message}));
                // A failed fetch invalidates its connection: pause dispatch,
                // drain the siblings while keeping their results, then reap
                // and relaunch the browser before anything else is fetched.
                while let Some(sibling) = inflight.pop_front() {
                    let sibling_outcome = match sibling.task {
                        PageTask::Failed(message) => Err(message),
                        PageTask::Running(handle) => match handle.await {
                            Ok(result) => result,
                            Err(join_error) => Err(format!("Page worker failed: {join_error}")),
                        },
                    };
                    match sibling_outcome {
                        Ok(page) => {
                            if let Err(message) = crawl.record(sibling.depth, page) {
                                crawl
                                    .errors
                                    .push(json!({"url": sibling.url, "error": message}));
                            }
                        }
                        Err(message) => {
                            crawl
                                .errors
                                .push(json!({"url": sibling.url, "error": message}));
                        }
                    }
                }
                // restart(): reap the browser and every pooled connection; the
                // next dispatch cold-starts a new child.
                browser.shutdown().await;
            }
        }
    }
    // Like the C++ Drain scope: wait for pages still in flight on a stop so
    // teardown can never race a page worker; their results are discarded.
    while let Some(item) = inflight.pop_front() {
        if let PageTask::Running(handle) = item.task {
            let _ = handle.await;
        }
    }

    let mut result = json!({
        "attempted": crawl.attempted,
        "truncated": crawl.truncated || !crawl.queue.is_empty(),
        "errors": crawl.errors,
    });
    if !crawl.warnings.is_empty() {
        result["warnings"] = Value::Array(crawl.warnings);
    }
    result[if tool == Tool::Map { "links" } else { "pages" }] = if tool == Tool::Map {
        Value::Array(crawl.links)
    } else {
        Value::Array(crawl.pages)
    };
    result
}
