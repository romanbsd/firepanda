//! BFS crawl/map with bounded concurrency. The dispatch loop fills the window
//! in frontier order and consumes results oldest-first, so pages, links, and
//! errors keep sequential breadth-first order even though fetches overlap. A
//! page whose CDP connection is unusable drains its siblings (keeping their
//! results) before the browser is reaped and relaunched. Readiness, validation,
//! and wait errors stay on that page.

use super::sitemap::{Collected, merge_links};
use super::{MAX_CRAWL_BYTES, MAX_LINKS, MapQuery, Tool, normalize_url, origin};
use crate::browser::{Browser, Format, PageError, PageRequest, PageTask};
use crate::json::serialized_size_within;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fixed crawl/map budget.
const CRAWL_BUDGET: Duration = Duration::from_secs(120);
/// Remaining budget below this stops dispatching.
const DISPATCH_FLOOR: Duration = Duration::from_millis(100);

/// One dispatched page awaiting consumption, in dispatch order.
struct Inflight {
    depth: i64,
    url: Arc<str>,
    task: PageTask,
}

/// Why dispatch stopped. `Budget` also drops pages that were fetched after the
/// serialization budget filled, matching the sequential crawl that stops there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Halt {
    None,
    Dispatch,
    Budget,
}

/// Frontier, dedupe sets, and the ordered result arrays. `record` folds each
/// successful page exactly where the sequential crawl would have.
struct Crawl {
    tool: Tool,
    start_origin: String,
    max_depth: i64,
    return_links: bool,
    queue: VecDeque<(Arc<str>, i64)>,
    queued: HashSet<Arc<str>>,
    discovered: HashSet<Arc<str>>,
    visited: HashSet<Arc<str>>,
    pages: Vec<Value>,
    links: Vec<Value>,
    errors: Vec<Value>,
    warnings: Vec<Value>,
    attempted: i64,
    bytes: usize,
    truncated: bool,
    /// Dispatch stop reason; `Halt::Budget` also drops later pages.
    halt: Halt,
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
            halt: Halt::None,
        }
    }

    /// Fold one successful page into the result. `Err` means the page itself
    /// was malformed; the caller records it as a per-page error.
    fn record(&mut self, depth: i64, mut page: Value) -> Result<(), String> {
        if self.halt == Halt::Budget {
            return Ok(()); // Sequential stops at the first overflow.
        }
        let listed = page
            .get("metadata")
            .and_then(|metadata| metadata.get("url"))
            .and_then(Value::as_str)
            .ok_or_else(|| "Page metadata must include a URL".to_string())?;
        // One shared URL allocation for the visited/queued/frontier sets.
        let canonical: Arc<str> = Arc::from(normalize_url(listed)?);
        // A redirect may reach a URL already queued through a direct link.
        // Track final destinations so it is neither rendered nor returned twice.
        if !self.visited.insert(canonical.clone()) {
            // The redirect target was already fetched. Count it as an error so
            // attempted stays equal to pages plus errors, and do not fetch it
            // again.
            return Err("Redirected to an already visited URL".to_string());
        }
        // Redirected pages are returned, but external destinations never expand.
        let expand = origin(&canonical) == self.start_origin;
        if self.tool == Tool::Map
            && let Some(warnings) = page
                .as_object_mut()
                .and_then(|object| object.remove("warnings"))
        {
            // Map does not return the page itself, so move its warnings.
            self.warnings.push(crate::json::object([
                ("url", Value::String(canonical.to_string())),
                ("warnings", warnings),
            ]));
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
                let candidate: Arc<str> = Arc::from(candidate);
                // Crawl only needs its frontier; retaining a second map of
                // every link would waste memory.
                if self.tool == Tool::Map && !self.discovered.contains(candidate.as_ref()) {
                    let title = link
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty());
                    self.store_map_link(&candidate, title);
                }
                if expand
                    && depth < self.max_depth
                    && origin(candidate.as_ref()) == self.start_origin
                    && !self.queued.contains(candidate.as_ref())
                    && !self.visited.contains(candidate.as_ref())
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
                self.halt = Halt::Budget;
                return Ok(());
            };
            self.bytes += size;
            self.pages.push(page);
        }
        Ok(())
    }

    /// Keep a map link when it fits the shared 1 MiB result budget and the
    /// count cap. A link that does not fit is omitted and not marked
    /// discovered, so a later smaller link can still be stored.
    fn store_map_link(&mut self, candidate: &Arc<str>, title: Option<&str>) {
        if self.discovered.len() == MAX_LINKS {
            self.truncated = true;
            return;
        }
        let mut entry = crate::json::object([("url", Value::String(candidate.to_string()))]);
        if let Some(title) = title {
            entry["title"] = Value::String(title.to_string());
        }
        let Some(entry_len) = serialized_size_within(&entry, MAX_CRAWL_BYTES) else {
            self.truncated = true;
            return;
        };
        // `[` + entries joined by `,` + `]`.
        let framing = if self.links.is_empty() { 2 } else { 1 };
        if self.bytes + entry_len + framing > MAX_CRAWL_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += entry_len + framing;
        self.discovered.insert(candidate.clone());
        self.links.push(entry);
    }

    /// Fold a resolved page (or its error) into the result arrays.
    fn finish(&mut self, depth: i64, url: &str, outcome: Result<Value, String>) {
        match outcome {
            Ok(page) => {
                if let Err(message) = self.record(depth, page) {
                    // Recording failures come from a healthy page; the browser
                    // stays up.
                    self.errors.push(error_record(url, message));
                }
            }
            Err(message) => self.errors.push(error_record(url, message)),
        }
    }
}

fn error_record(url: &str, message: String) -> Value {
    crate::json::object([
        ("url", Value::String(url.to_string())),
        ("error", Value::String(message)),
    ])
}

/// Await one in-flight page and normalize worker failures.
async fn resolve(task: PageTask) -> Result<Value, PageError> {
    match task {
        PageTask::Failed(message) => Err(PageError {
            message,
            reap: true,
        }),
        PageTask::Running(handle) => match handle.await {
            Ok(result) => result,
            Err(join_error) => Err(PageError {
                message: format!("Page worker failed: {join_error}"),
                reap: true,
            }),
        },
    }
}

/// Per-page timeout and wait, both capped by the time still in the crawl.
fn clamped_wait(
    wait_for: Duration,
    page_timeout: Duration,
    remaining: Duration,
) -> (Duration, Duration) {
    let timeout = page_timeout.min(remaining);
    (timeout, wait_for.min(timeout))
}

/// Run one crawl/map tool call to completion (the discovery path).
pub async fn run(
    browser: &Browser,
    tool: Tool,
    request: PageRequest,
    limit: i64,
    max_depth: i64,
    concurrency: usize,
    map: MapQuery,
) -> Value {
    let start_origin = request.url.clone();
    let deadline = Instant::now() + CRAWL_BUDGET;
    // Same 120s budget as the walk. The sitemap task must not shorten dispatch.
    let tokio_deadline = tokio::time::Instant::now() + CRAWL_BUDGET;
    let site_origin = origin(&start_origin).to_string();
    let sitemap_task = (tool == Tool::Map && map.sitemap).then(|| {
        let origin = site_origin.clone();
        tokio::spawn(async move { super::sitemap::collect(&origin, tokio_deadline).await })
    });
    let page_timeout = request.timeout;
    let page_formats = if tool == Tool::Map {
        vec![Format::Links]
    } else {
        request.formats.clone()
    };
    let return_links = page_formats.contains(&Format::Links);
    let mut crawl = Crawl::new(tool, &start_origin, max_depth, return_links);
    let start: Arc<str> = Arc::from(start_origin.as_str());
    crawl.queued.insert(start.clone());
    crawl.queue.push_back((start, 0));
    let mut inflight: VecDeque<Inflight> = VecDeque::new();

    while !browser.stopped()
        && (!inflight.is_empty() || (!crawl.queue.is_empty() && crawl.halt == Halt::None))
    {
        // Fill the window in frontier order; checkout runs here so the
        // managed browser is spawned exactly once per cold start.
        while !browser.stopped()
            && crawl.halt == Halt::None
            && inflight.len() < concurrency
            && let Some((url, depth)) = crawl.queue.pop_front()
        {
            if crawl.visited.contains(url.as_ref()) {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if crawl.attempted == limit || remaining < DISPATCH_FLOOR {
                crawl.truncated = true;
                crawl.halt = Halt::Dispatch;
                break;
            }
            crawl.attempted += 1;
            let mut formats = page_formats.clone();
            if (return_links || depth < crawl.max_depth) && !formats.contains(&Format::Links) {
                formats.push(Format::Links);
            }
            // Build the per-page request directly: cloning the template and
            // then overwriting url/formats/timeout would copy them twice.
            let (timeout, wait_for) = clamped_wait(request.wait_for, page_timeout, remaining);
            let page_request = PageRequest {
                url: url.to_string(),
                formats,
                only_main_content: request.only_main_content,
                timeout,
                wait_for,
                wait_for_selector: request.wait_for_selector.clone(),
                wait_for_text: request.wait_for_text.clone(),
                max_chars: request.max_chars,
                markdown_links: request.markdown_links,
            };
            let task = browser.start_page(page_request).await;
            inflight.push_back(Inflight { depth, url, task });
        }
        let Some(item) = inflight.pop_front() else {
            break; // Nothing in flight and nothing dispatchable.
        };
        let outcome = resolve(item.task).await;
        let reap = matches!(&outcome, Err(error) if error.reap);
        let outcome = outcome.map_err(|error| error.message);
        if outcome.is_ok() {
            browser.page_completed().await; // Idle counts from the fetch.
        }
        crawl.finish(item.depth, &item.url, outcome);
        if reap {
            // The connection is unusable: pause dispatch, drain the siblings
            // while keeping their results, then reap and relaunch before
            // anything else is fetched.
            while let Some(sibling) = inflight.pop_front() {
                let outcome = resolve(sibling.task).await;
                crawl.finish(
                    sibling.depth,
                    &sibling.url,
                    outcome.map_err(|error| error.message),
                );
            }
            // restart(): reap the browser and every pooled connection; the
            // next dispatch cold-starts a new child.
            browser.shutdown().await;
        }
    }
    // Wait for pages still in flight on a stop so
    // teardown can never race a page worker; their results are discarded.
    while let Some(item) = inflight.pop_front() {
        if let PageTask::Running(handle) = item.task {
            let _ = handle.await;
        }
    }

    // Queued URLs already reached through a redirect were not left behind.
    // Sitemap URLs stay off the frontier; join only after the walk.
    let collected = join_sitemap(sitemap_task, &site_origin).await;
    tool_result(tool, crawl, &map, collected)
}

/// The sitemap read never feeds the crawl queue. A panicked task is one error.
async fn join_sitemap(
    task: Option<tokio::task::JoinHandle<Collected>>,
    site_origin: &str,
) -> Collected {
    let Some(handle) = task else {
        return Collected {
            pages: Vec::new(),
            errors: Vec::new(),
            truncated: false,
        };
    };
    handle.await.unwrap_or_else(|_| Collected {
        pages: Vec::new(),
        errors: vec![error_record(
            &format!("{site_origin}/robots.txt"),
            "Sitemap read failed".to_string(),
        )],
        truncated: true,
    })
}

fn tool_result(tool: Tool, crawl: Crawl, map: &MapQuery, collected: Collected) -> Value {
    let unvisited = crawl
        .queue
        .iter()
        .any(|(url, _)| !crawl.visited.contains(url));
    let walk_truncated = crawl.truncated || unvisited;
    if tool == Tool::Map {
        return map_result(
            crawl.links,
            crawl.attempted,
            crawl.errors,
            crawl.warnings,
            walk_truncated,
            map,
            &collected.pages,
            collected.errors,
            collected.truncated,
        );
    }
    let mut result = crate::json::object([
        ("attempted", Value::from(crawl.attempted)),
        ("truncated", Value::Bool(walk_truncated)),
    ]);
    result["errors"] = Value::Array(crawl.errors);
    if !crawl.warnings.is_empty() {
        result["warnings"] = Value::Array(crawl.warnings);
    }
    result["pages"] = Value::Array(crawl.pages);
    result
}

/// Map payload. Omitted counts come from the merge. `truncated` is also set
/// when the walk or a sitemap read stopped early.
#[allow(
    clippy::too_many_arguments,
    clippy::redundant_pub_crate,
    clippy::needless_pass_by_value
)]
pub(crate) fn map_result(
    rendered: Vec<Value>,
    attempted: i64,
    mut errors: Vec<Value>,
    warnings: Vec<Value>,
    walk_truncated: bool,
    query: &MapQuery,
    sitemap_pages: &[String],
    sitemap_errors: Vec<Value>,
    sitemap_truncated: bool,
) -> Value {
    let merged = merge_links(
        &rendered,
        sitemap_pages,
        &query.include_paths,
        &query.exclude_paths,
        query.link_limit,
        MAX_CRAWL_BYTES,
    );
    errors.extend(sitemap_errors);
    let mut result = crate::json::object([
        ("attempted", Value::from(attempted)),
        (
            "truncated",
            Value::Bool(walk_truncated || sitemap_truncated || merged.truncated),
        ),
        ("omittedRendered", Value::from(merged.omitted_rendered)),
        ("omittedSitemap", Value::from(merged.omitted_sitemap)),
    ]);
    result["errors"] = Value::Array(errors);
    if !warnings.is_empty() {
        result["warnings"] = Value::Array(warnings);
    }
    result["links"] = Value::Array(merged.links);
    result
}

#[cfg(test)]
mod tests {
    use super::{Crawl, MAX_CRAWL_BYTES, Tool};
    use serde_json::{Value, json};
    use std::time::Duration;

    fn page(url: &str, links: &Value) -> Value {
        json!({
            "metadata": {"url": url},
            "markdown": "body",
            "links": links,
        })
    }

    #[test]
    fn redirect_duplicate_is_an_error_and_is_not_stored_again() {
        let mut crawl = Crawl::new(Tool::Crawl, "https://example.com/start", 1, false);
        let first = page("https://example.com/a", &json!([]));
        crawl.record(0, first.clone()).expect("first visit");
        let error = crawl
            .record(0, first)
            .expect_err("a redirect to a visited URL");
        assert!(error.contains("already visited"), "{error}");
        assert_eq!(crawl.pages.len(), 1);
        crawl.attempted = 2;
        crawl.finish(
            1,
            "https://example.com/b",
            Ok(page("https://example.com/a", &json!([]))),
        );
        assert_eq!(crawl.pages.len(), 1);
        assert_eq!(crawl.errors.len(), 1);
        assert!(
            crawl.errors[0]["error"]
                .as_str()
                .unwrap()
                .contains("already visited")
        );
        assert_eq!(
            crawl.attempted,
            i64::try_from(crawl.pages.len() + crawl.errors.len()).unwrap()
        );
        assert!(crawl.queue.is_empty());
    }

    #[test]
    fn map_links_share_the_crawl_byte_budget() {
        let mut crawl = Crawl::new(Tool::Map, "https://example.com/start", 1, true);
        crawl
            .record(
                0,
                page(
                    "https://example.com/start",
                    &json!([
                        {"url": "https://example.com/huge", "title": "t".repeat(MAX_CRAWL_BYTES + 1)},
                        {"url": "https://example.com/small", "title": "ok"},
                    ]),
                ),
            )
            .expect("map page");
        assert!(crawl.truncated);
        assert_eq!(crawl.links.len(), 1);
        assert_eq!(crawl.links[0]["url"], "https://example.com/small");
        assert!(!crawl.discovered.iter().any(|url| url.contains("/huge")));

        let links: Vec<Value> = (0..5000)
            .map(|i| {
                json!({
                    "url": format!("https://example.com/p/{i:0>180}"),
                    "title": format!("title-{i}"),
                })
            })
            .collect();
        let mut crawl = Crawl::new(Tool::Map, "https://example.com/start", 0, true);
        crawl
            .record(0, page("https://example.com/start", &Value::Array(links)))
            .expect("bulk map page");
        assert!(crawl.truncated);
        assert!(crawl.links.len() < 5000, "{}", crawl.links.len());
        let encoded = serde_json::to_string(&crawl.links).expect("links json");
        assert!(encoded.len() <= MAX_CRAWL_BYTES, "{}", encoded.len());
    }

    #[test]
    fn map_result_applies_the_cap() {
        use super::super::MapQuery;

        let rendered = vec![
            json!({"url": "https://example.com/a"}),
            json!({"url": "https://example.com/b"}),
        ];
        let filtered = MapQuery {
            sitemap: false,
            include_paths: vec!["/a".to_string()],
            exclude_paths: vec![],
            link_limit: Some(1),
        };
        let result = super::map_result(
            rendered.clone(),
            2,
            vec![],
            vec![],
            false,
            &filtered,
            &[],
            vec![],
            false,
        );
        let links = result["links"].as_array().unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["url"], "https://example.com/a");
        assert_eq!(result["omittedRendered"], json!(0));
        assert_eq!(result["omittedSitemap"], json!(0));
        assert_eq!(result["truncated"], json!(false));

        let capped = MapQuery {
            include_paths: vec![],
            ..filtered
        };
        let result = super::map_result(
            rendered,
            2,
            vec![],
            vec![],
            false,
            &capped,
            &[],
            vec![],
            false,
        );
        assert_eq!(result["links"].as_array().unwrap().len(), 1);
        assert_eq!(result["links"][0]["url"], "https://example.com/a");
        assert_eq!(result["omittedRendered"], json!(1));
        assert_eq!(result["omittedSitemap"], json!(0));
        assert_eq!(result["truncated"], json!(true));

        let with_errors = super::map_result(
            vec![json!({"url": "https://example.com/a"})],
            1,
            vec![json!({"url": "https://example.com/a", "error": "walk"})],
            vec![],
            false,
            &MapQuery {
                sitemap: false,
                include_paths: vec![],
                exclude_paths: vec![],
                link_limit: None,
            },
            &[],
            vec![json!({"url": "https://example.com/sitemap.xml", "error": "sitemap"})],
            true,
        );
        assert_eq!(with_errors["truncated"], json!(true));
        assert_eq!(with_errors["omittedRendered"], json!(0));
        assert_eq!(with_errors["omittedSitemap"], json!(0));
        assert_eq!(
            with_errors["errors"],
            json!([
                {"url": "https://example.com/a", "error": "walk"},
                {"url": "https://example.com/sitemap.xml", "error": "sitemap"},
            ])
        );
    }

    #[test]
    fn sitemap_default_stops_at_100() {
        let query = super::super::parse_map_query(&json!({
            "url": "https://example.com",
            "sitemap": true,
        }))
        .unwrap();
        let pages: Vec<String> = (0..101)
            .map(|n| format!("https://example.com/p/{n}"))
            .collect();
        let result = super::map_result(
            vec![],
            0,
            vec![],
            vec![],
            false,
            &query,
            &pages,
            vec![],
            false,
        );
        assert_eq!(result["links"].as_array().unwrap().len(), 100);
        assert_eq!(result["omittedRendered"], json!(0));
        assert_eq!(result["omittedSitemap"], json!(1));
        assert_eq!(result["truncated"], json!(true));
    }

    #[test]
    fn crawl_wait_uses_the_tighter_page_budget() {
        let (timeout, wait) = super::clamped_wait(
            Duration::from_secs(30),
            Duration::from_secs(10),
            Duration::from_secs(4),
        );
        assert_eq!(timeout, Duration::from_secs(4));
        assert_eq!(wait, Duration::from_secs(4));
    }
}
