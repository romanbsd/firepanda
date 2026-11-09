//! Sitemap collection and the merge of those URLs into map links.

use super::paths::{path_allowed, url_path};
use super::{normalize_parsed, origin};
use crate::http;
use crate::json::serialized_size_within;
use flate2::read::GzDecoder;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::io::Read;

/// Fetched sitemap documents, not counting `robots.txt`.
const DOCUMENT_CAP: usize = 32;
/// One document, compressed or decompressed.
const MAX_SITEMAP_BYTES: usize = 8 * 1024 * 1024;

/// Page URLs and failures from one sitemap read.
pub struct Collected {
    pub pages: Vec<String>,
    pub errors: Vec<serde_json::Value>,
    pub truncated: bool,
}

/// Robots and sitemap state. The caller performs HTTP; this type only decides
/// the next URL and how a response changes the collection.
pub struct Collector {
    start_origin: String,
    robots_url: String,
    robots_pending: bool,
    /// Sitemap file URLs still to fetch. Children of an index sit in front of
    /// siblings so expansion is depth-first.
    queue: VecDeque<String>,
    seen_files: HashSet<String>,
    seen_pages: HashSet<String>,
    documents: usize,
    pages: Vec<String>,
    errors: Vec<Value>,
    truncated: bool,
    /// Deadline before a request is sent: no further URLs.
    stopped: bool,
}

impl Collector {
    /// `start_origin` is `http://host[:port]` with no path.
    pub fn new(start_origin: &str) -> Self {
        Self {
            robots_url: format!("{start_origin}/robots.txt"),
            start_origin: start_origin.to_string(),
            robots_pending: true,
            queue: VecDeque::new(),
            seen_files: HashSet::new(),
            seen_pages: HashSet::new(),
            documents: 0,
            pages: Vec::new(),
            errors: Vec::new(),
            truncated: false,
            stopped: false,
        }
    }

    /// `robots.txt` first, then each sitemap file once. `None` after the
    /// document cap, or after a deadline that fired before the next send.
    pub fn next_request(&mut self) -> Option<String> {
        if self.stopped {
            return None;
        }
        if self.robots_pending {
            self.robots_pending = false;
            return Some(self.robots_url.clone());
        }
        if self.documents >= DOCUMENT_CAP {
            return None;
        }
        self.queue.pop_front()
    }

    /// A non-200 records one error and queues nothing. A 200 contributes
    /// `Sitemap:` values only; other robots fields are ignored. A value that
    /// does not normalize is dropped and not fetched.
    pub fn note_robots(&mut self, requested_url: &str, status: u16, body: &[u8]) {
        if status != 200 {
            self.push_error(requested_url, format!("robots.txt returned HTTP {status}"));
            return;
        }
        for value in sitemap_values(body) {
            if let Ok(url) = resolve_against(requested_url, &value) {
                self.enqueue_back(url);
            }
        }
    }

    /// Count one sitemap document. Non-200 and unreadable bodies are not parsed.
    pub fn note_document(
        &mut self,
        requested_url: &str,
        final_url: &str,
        status: u16,
        body: &[u8],
    ) {
        self.documents += 1;
        if status != 200 {
            self.push_error(requested_url, format!("Sitemap returned HTTP {status}"));
            return;
        }
        let decoded = match decode_body(body, final_url) {
            Ok(decoded) => decoded,
            Err(message) => {
                if message.contains("exceeds") {
                    self.truncated = true;
                }
                self.push_error(requested_url, message);
                return;
            }
        };
        match extract_locs(&decoded, final_url, &self.start_origin) {
            Ok(locs) => self.commit(locs),
            Err(message) => self.push_error(requested_url, message),
        }
    }

    /// Record `{url, error}`. A fetched body over 8 MiB or a transport error on
    /// a sitemap URL consumes one of the 32 document slots.
    pub fn note_failure(&mut self, url: &str, message: String, counts_as_document: bool) {
        self.push_error(url, message);
        if counts_as_document {
            self.documents += 1;
        }
    }

    /// A timeout before the request is sent: one error, `truncated`, and no
    /// further `next_request` URLs. Does not consume a document slot.
    fn stop_for_deadline(&mut self, url: &str) {
        self.note_failure(url, "Sitemap request timed out".to_string(), false);
        self.truncated = true;
        self.stopped = true;
    }

    pub fn finish(self) -> Collected {
        let mut errors = self.errors;
        let mut truncated = self.truncated;
        // URLs past the 32-document cap were never parsed, so they are not pages.
        if self.documents >= DOCUMENT_CAP
            && let Some(url) = self.queue.front()
        {
            truncated = true;
            errors.push(error_record(
                url,
                "Sitemap document limit reached".to_string(),
            ));
        }
        Collected {
            pages: self.pages,
            errors,
            truncated,
        }
    }

    fn commit(&mut self, locs: Locs) {
        for url in locs.pages {
            if self.seen_pages.insert(url.clone()) {
                self.pages.push(url);
            }
        }
        self.enqueue_children(locs.children);
    }

    fn enqueue_back(&mut self, url: String) {
        if self.seen_files.insert(url.clone()) {
            self.queue.push_back(url);
        }
    }

    /// First-seen order, inserted in front of whatever was already queued.
    fn enqueue_children(&mut self, children: Vec<String>) {
        let mut unique = Vec::new();
        for url in children {
            if self.seen_files.insert(url.clone()) {
                unique.push(url);
            }
        }
        for url in unique.into_iter().rev() {
            self.queue.push_front(url);
        }
    }

    fn push_error(&mut self, url: &str, message: impl Into<String>) {
        self.errors.push(error_record(url, message.into()));
    }
}

/// Fetch `robots.txt` and sitemap documents until the deadline or the caps.
/// Does not request `/sitemap.xml` unless robots names it.
pub async fn collect(start_origin: &str, deadline: tokio::time::Instant) -> Collected {
    let mut collector = Collector::new(start_origin);
    let client = match http::client() {
        Ok(client) => client,
        Err(message) => {
            let robots = collector.robots_url.clone();
            collector.note_failure(&robots, message, false);
            return collector.finish();
        }
    };
    while let Some(url) = collector.next_request() {
        if tokio::time::Instant::now() >= deadline {
            collector.stop_for_deadline(&url);
            break;
        }
        let response = match tokio::time::timeout_at(deadline, client.get(&url).send()).await {
            Err(_) => {
                collector.stop_for_deadline(&url);
                break;
            }
            Ok(Err(error)) => {
                let counts = url != collector.robots_url;
                collector.note_failure(
                    &url,
                    format!("Sitemap request failed: {}", error.without_url()),
                    counts,
                );
                continue;
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();
        let final_url = normalize_parsed(response.url().clone()).unwrap_or_else(|_| url.clone());
        let robots = url == collector.robots_url;
        if status != 200 {
            if robots {
                collector.note_robots(&url, status, &[]);
            } else {
                collector.note_document(&url, &final_url, status, &[]);
            }
            continue;
        }
        match http::read_limited(response, MAX_SITEMAP_BYTES, deadline, "Sitemap").await {
            Ok(body) if robots => collector.note_robots(&url, status, &body),
            Ok(body) => collector.note_document(&url, &final_url, status, &body),
            Err(message) => {
                let timed_out = message.contains("timed out");
                let capped = message.contains("exceeds");
                collector.note_failure(&url, message, !robots);
                if capped {
                    collector.truncated = true;
                }
                if timed_out {
                    collector.truncated = true;
                    collector.stopped = true;
                    break;
                }
            }
        }
    }
    collector.finish()
}

fn error_record(url: &str, message: String) -> Value {
    crate::json::object([
        ("url", Value::String(url.to_string())),
        ("error", Value::String(message)),
    ])
}

/// `Sitemap:` field values, case-insensitive, in file order.
fn sitemap_values(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    let mut values = Vec::new();
    for line in text.trim_start_matches('\u{feff}').split('\n') {
        let Some((field, value)) = line.trim().split_once(':') else {
            continue;
        };
        if field.trim().eq_ignore_ascii_case("sitemap") {
            let value = value.trim();
            if !value.is_empty() {
                values.push(value.to_string());
            }
        }
    }
    values
}

fn resolve_against(base: &str, value: &str) -> Result<String, String> {
    let base = url::Url::parse(base).map_err(|error| format!("Invalid sitemap URL: {error}"))?;
    let joined = base
        .join(value)
        .map_err(|error| format!("Invalid sitemap URL: {error}"))?;
    normalize_parsed(joined)
}

/// Gzip magic or a final path ending in `.gz`. Other bodies are already plain.
fn decode_body<'a>(body: &'a [u8], final_url: &str) -> Result<Cow<'a, [u8]>, String> {
    let named_gzip = url::Url::parse(final_url).is_ok_and(|url| {
        std::path::Path::new(url.path())
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gz"))
    });
    let magic = body.starts_with(&[0x1f, 0x8b]);
    if !named_gzip && !magic {
        return Ok(Cow::Borrowed(body));
    }
    let mut decoder = GzDecoder::new(body);
    let mut plain = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = decoder
            .read(&mut chunk)
            .map_err(|_| "Sitemap gzip decode failed".to_string())?;
        if read == 0 {
            break;
        }
        if plain.len() + read > MAX_SITEMAP_BYTES {
            return Err(format!(
                "Sitemap response exceeds {} MiB",
                MAX_SITEMAP_BYTES / (1024 * 1024)
            ));
        }
        plain.extend_from_slice(&chunk[..read]);
    }
    Ok(Cow::Owned(plain))
}

struct Locs {
    pages: Vec<String>,
    children: Vec<String>,
}

/// Local names only. A `loc` counts when its parent is `url` or `sitemap`.
/// Predefined XML entities are decoded; external entities are not fetched.
fn extract_locs(body: &[u8], final_url: &str, start_origin: &str) -> Result<Locs, String> {
    let mut reader = Reader::from_reader(body);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut locs = Locs {
        pages: Vec::new(),
        children: Vec::new(),
    };
    let mut loc = String::new();
    let mut capturing = false;
    let mut saw_root = false;
    loop {
        let event = reader
            .read_event()
            .map_err(|error| format!("Sitemap XML is invalid: {error}"))?;
        match event {
            Event::Start(event) => {
                let name = event.local_name().as_ref().to_vec();
                note_element(&stack, &mut saw_root, &mut capturing, &mut loc, &name)?;
                stack.push(name);
            }
            Event::Empty(event) => {
                let name = event.local_name().as_ref().to_vec();
                note_element(&stack, &mut saw_root, &mut capturing, &mut loc, &name)?;
                // An empty `loc` has no text. Do not leave `capturing` set, and
                // do not clear it for an empty element nested inside a real loc.
                if name.as_slice() == b"loc" {
                    capturing = false;
                }
            }
            Event::Text(event) => {
                if capturing {
                    let text = event
                        .decode()
                        .map_err(|error| format!("Sitemap XML is invalid: {error}"))?;
                    loc.push_str(&text);
                }
            }
            Event::CData(event) => {
                if capturing {
                    let text = event
                        .decode()
                        .map_err(|error| format!("Sitemap XML is invalid: {error}"))?;
                    loc.push_str(&text);
                }
            }
            Event::GeneralRef(event) => {
                if capturing {
                    loc.push_str(&reference_text(&event)?);
                }
            }
            Event::End(event) => {
                let name = event.local_name().as_ref().to_vec();
                if capturing && name.as_slice() == b"loc" {
                    let parent = stack
                        .iter()
                        .rev()
                        .nth(1)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    accept_loc(&mut locs, parent, &loc, final_url, start_origin);
                    capturing = false;
                }
                if stack.pop().as_deref() != Some(name.as_slice()) {
                    return Err("Sitemap XML is invalid".to_string());
                }
            }
            // quick-xml emits Eof while start tags are still open.
            Event::Eof => {
                if !stack.is_empty() {
                    return Err("Sitemap XML is invalid".to_string());
                }
                break;
            }
            _ => {}
        }
    }
    if !saw_root {
        return Err("Sitemap root must be urlset or sitemapindex".to_string());
    }
    Ok(locs)
}

fn note_element(
    stack: &[Vec<u8>],
    saw_root: &mut bool,
    capturing: &mut bool,
    loc: &mut String,
    name: &[u8],
) -> Result<(), String> {
    if stack.is_empty() {
        if *saw_root || !is_root(name) {
            return Err("Sitemap root must be urlset or sitemapindex".to_string());
        }
        *saw_root = true;
    }
    if name == b"loc" && is_loc_parent(stack.last().map(Vec::as_slice)) {
        *capturing = true;
        loc.clear();
    }
    Ok(())
}

const fn is_root(name: &[u8]) -> bool {
    matches!(name, b"urlset" | b"sitemapindex")
}

const fn is_loc_parent(parent: Option<&[u8]>) -> bool {
    matches!(parent, Some(b"url" | b"sitemap"))
}

fn accept_loc(locs: &mut Locs, parent: &[u8], text: &str, final_url: &str, start_origin: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if parent == b"url" {
        // Invalid and off-origin page locs are dropped with no error.
        if let Ok(url) = resolve_against(final_url, text)
            && origin(&url) == start_origin
        {
            locs.pages.push(url);
        }
        return;
    }
    if parent == b"sitemap"
        && let Ok(url) = resolve_against(final_url, text)
    {
        locs.children.push(url);
    }
}

/// Character references and the five predefined XML entities. Anything else,
/// including a `SYSTEM` entity, is an error and is not loaded.
fn reference_text(event: &quick_xml::events::BytesRef<'_>) -> Result<String, String> {
    if let Some(ch) = event
        .resolve_char_ref()
        .map_err(|error| format!("Sitemap XML is invalid: {error}"))?
    {
        return Ok(ch.to_string());
    }
    let name = event
        .decode()
        .map_err(|error| format!("Sitemap XML is invalid: {error}"))?;
    quick_xml::escape::resolve_xml_entity(&name)
        .map(ToString::to_string)
        .ok_or_else(|| "Sitemap XML entity is not defined".to_string())
}

/// Links returned by `map` after filtering, dedupe, and the return cap.
pub struct Merged {
    pub links: Vec<Value>,
    pub omitted_rendered: usize,
    pub omitted_sitemap: usize,
    pub truncated: bool,
}

/// Rendered links keep discovery order and win duplicates. Sitemap-only URLs
/// follow in document order. Path filters remove candidates without counting
/// them as omissions. `link_limit: None` returns the filtered rendered links
/// and leaves both counts at 0.
pub fn merge_links(
    rendered: &[Value],
    sitemap_pages: &[String],
    include: &[String],
    exclude: &[String],
    link_limit: Option<usize>,
    byte_budget: usize,
) -> Merged {
    let rendered_urls: HashSet<&str> = rendered.iter().filter_map(link_url).collect();
    let kept_rendered: Vec<&Value> = rendered
        .iter()
        .filter(|entry| link_url(entry).is_some_and(|url| passes(url, include, exclude)))
        .collect();
    let kept_sitemap: Vec<&str> = sitemap_pages
        .iter()
        .map(String::as_str)
        .filter(|url| !rendered_urls.contains(url))
        .filter(|url| passes(url, include, exclude))
        .collect();
    let Some(limit) = link_limit else {
        return Merged {
            links: kept_rendered.into_iter().cloned().collect(),
            omitted_rendered: 0,
            omitted_sitemap: 0,
            truncated: false,
        };
    };

    let mut links = Vec::new();
    let mut bytes = 0usize;
    let (omitted_rendered, byte_stopped) = store_some(
        kept_rendered.len(),
        limit,
        &mut links,
        &mut bytes,
        byte_budget,
        |index| kept_rendered[index].clone(),
    );
    let omitted_sitemap = if byte_stopped || links.len() == limit {
        kept_sitemap.len()
    } else {
        store_some(
            kept_sitemap.len(),
            limit,
            &mut links,
            &mut bytes,
            byte_budget,
            |index| json!({"url": kept_sitemap[index]}),
        )
        .0
    };

    Merged {
        links,
        omitted_rendered,
        omitted_sitemap,
        truncated: omitted_rendered > 0 || omitted_sitemap > 0,
    }
}

fn link_url(entry: &Value) -> Option<&str> {
    entry.get("url").and_then(Value::as_str)
}

fn passes(url: &str, include: &[String], exclude: &[String]) -> bool {
    path_allowed(url_path(url), include, exclude)
}

/// Store candidates until `limit` or the byte budget. The `bool` is true when a
/// byte stop, not the count cap, ended the side.
fn store_some(
    total: usize,
    limit: usize,
    links: &mut Vec<Value>,
    bytes: &mut usize,
    byte_budget: usize,
    mut entry_at: impl FnMut(usize) -> Value,
) -> (usize, bool) {
    let mut index = 0;
    while index < total && links.len() < limit {
        if !try_store(entry_at(index), links, bytes, byte_budget) {
            return (total - index, true);
        }
        index += 1;
    }
    (total - index, false)
}

/// Account one entry the way `Crawl::store_map_link` does: serialized size plus
/// framing of 2 for the first entry and 1 for each later one.
fn try_store(entry: Value, links: &mut Vec<Value>, bytes: &mut usize, byte_budget: usize) -> bool {
    let Some(entry_len) = serialized_size_within(&entry, byte_budget) else {
        return false;
    };
    // `[` + entries joined by `,` + `]`.
    let framing = if links.is_empty() { 2 } else { 1 };
    if *bytes + entry_len + framing > byte_budget {
        return false;
    }
    *bytes += entry_len + framing;
    links.push(entry);
    true
}

#[cfg(test)]
mod tests {
    use super::{Collector, merge_links};
    use serde_json::json;
    use std::fmt::Write;

    fn url(url: &str) -> serde_json::Value {
        json!({"url": url})
    }

    #[test]
    fn rendered_links_win_and_filters_are_not_omissions() {
        let rendered = vec![
            json!({"url": "https://example.com/about", "title": "About"}),
            json!({"url": "https://example.com/careers?x=1", "title": "Jobs"}),
        ];
        let sitemap = [
            "https://example.com/about".to_string(),
            "https://example.com/careers/role".to_string(),
        ];
        let merged = merge_links(
            &rendered,
            &sitemap,
            &["/careers".into()],
            &[],
            Some(10),
            1024 * 1024,
        );
        assert_eq!(
            merged.links,
            vec![
                json!({"url": "https://example.com/careers?x=1", "title": "Jobs"}),
                url("https://example.com/careers/role"),
            ]
        );
        assert_eq!(merged.omitted_rendered, 0);
        assert_eq!(merged.omitted_sitemap, 0);
        assert!(!merged.truncated);
    }

    #[test]
    fn cap_omits_rendered_tail_then_all_sitemap_urls() {
        let rendered = vec![url("https://example.com/a"), url("https://example.com/b")];
        let sitemap = [
            "https://example.com/a".into(),
            "https://example.com/c".into(),
        ];
        let merged = merge_links(&rendered, &sitemap, &[], &[], Some(1), 1024 * 1024);
        assert_eq!(merged.links, vec![url("https://example.com/a")]);
        assert_eq!(merged.omitted_rendered, 1);
        assert_eq!(merged.omitted_sitemap, 1);
        assert!(merged.truncated);
    }

    #[test]
    fn no_cap_keeps_omissions_at_zero() {
        let rendered = vec![url("https://example.com/a"), url("https://example.com/b")];
        let merged = merge_links(&rendered, &[], &["/a".into()], &[], None, 1024 * 1024);
        assert_eq!(merged.links, vec![url("https://example.com/a")]);
        assert_eq!(merged.omitted_rendered, 0);
        assert_eq!(merged.omitted_sitemap, 0);
        assert!(!merged.truncated);
    }

    #[test]
    fn byte_budget_omits_the_rest_of_that_side_and_later_sitemap_urls() {
        let rendered = vec![url("https://example.com/a"), url("https://example.com/b")];
        let first = merge_links(&rendered[..1], &[], &[], &[], Some(10), 1024 * 1024);
        let budget = serde_json::to_vec(&serde_json::Value::Array(first.links))
            .unwrap()
            .len();
        let sitemap = [
            "https://example.com/c".into(),
            "https://example.com/d".into(),
        ];
        let merged = merge_links(&rendered, &sitemap, &[], &[], Some(10), budget);
        assert_eq!(merged.links, vec![url("https://example.com/a")]);
        assert_eq!(merged.omitted_rendered, 1);
        assert_eq!(merged.omitted_sitemap, 2);
        assert!(merged.truncated);
    }

    #[test]
    fn relative_sitemap_directive_resolves_against_the_robots_url() {
        let mut collector = Collector::new("http://example.com");
        assert_eq!(
            collector.next_request().as_deref(),
            Some("http://example.com/robots.txt")
        );
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"User-agent: *\nDisallow: /\nSitemap: /maps/a.xml\n",
        );
        assert_eq!(
            collector.next_request().as_deref(),
            Some("http://example.com/maps/a.xml")
        );
    }

    #[test]
    fn missing_sitemap_directive_does_not_guess_sitemap_xml() {
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots("http://example.com/robots.txt", 404, b"");
        assert_eq!(collector.next_request(), None);
        let collected = collector.finish();
        assert_eq!(collected.pages, Vec::<String>::new());
        assert_eq!(collected.errors.len(), 1);
        assert!(!collected.truncated);
        let mut quiet = Collector::new("http://example.com");
        quiet.next_request();
        quiet.note_robots(
            "http://example.com/robots.txt",
            200,
            b"User-agent: *\nDisallow: /\n",
        );
        let collected = quiet.finish();
        assert_eq!(collected.pages, Vec::<String>::new());
        assert_eq!(collected.errors, Vec::<serde_json::Value>::new());
    }

    #[test]
    fn gzip_namespace_and_extension_locs() {
        let xml = b"<?xml version=\"1.0\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\"><url><loc>http://example.com/a</loc><priority>0.1</priority><image:image><image:loc>http://example.com/a.png</image:loc></image:image></url><url><loc>http://example.com/b</loc><lastmod>2000-01-01</lastmod></url></urlset>";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut encoder, xml).unwrap();
        let gzip = encoder.finish().unwrap();
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: https://cdn.example/a.xml.gz\n",
        );
        assert_eq!(
            collector.next_request().as_deref(),
            Some("https://cdn.example/a.xml.gz")
        );
        collector.note_document(
            "https://cdn.example/a.xml.gz",
            "https://cdn.example/a.xml.gz",
            200,
            &gzip,
        );
        assert_eq!(
            collector.finish().pages,
            vec![
                "http://example.com/a".to_string(),
                "http://example.com/b".to_string()
            ]
        );
    }

    #[test]
    fn relative_loc_resolves_against_the_final_document_url() {
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: http://example.com/maps/a.xml\n",
        );
        let url = collector.next_request().unwrap();
        collector.note_document(
            &url,
            "http://example.com/maps/a.xml",
            200,
            br"<urlset><url><loc>jobs/1</loc></url></urlset>",
        );
        assert_eq!(
            collector.finish().pages,
            vec!["http://example.com/maps/jobs/1".to_string()]
        );
    }

    #[test]
    fn document_cap_does_not_parse_the_unfetched_child() {
        let mut locs = String::from("<sitemapindex>");
        for i in 0..40 {
            let _ = write!(
                locs,
                "<sitemap><loc>http://cdn.example/{i}.xml</loc></sitemap>"
            );
        }
        locs.push_str("</sitemapindex>");
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: http://cdn.example/index.xml\n",
        );
        let index = collector.next_request().unwrap();
        collector.note_document(&index, &index, 200, locs.as_bytes());
        let mut fetched = 1usize;
        while let Some(url) = collector.next_request() {
            fetched += 1;
            collector.note_document(
                &url,
                &url,
                200,
                br"<urlset><url><loc>http://example.com/seen</loc></url></urlset>",
            );
        }
        assert_eq!(fetched, 32);
        let collected = collector.finish();
        assert!(collected.truncated);
        assert_eq!(collected.errors.len(), 1);
        assert_eq!(collected.pages, vec!["http://example.com/seen".to_string()]);
    }

    #[test]
    fn off_origin_and_duplicate_locs_and_bad_xml() {
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: http://example.com/one.xml\nSitemap: http://example.com/one.xml\nSitemap: http://example.com/bad.xml\n",
        );
        let first = collector.next_request().unwrap();
        collector.note_document(&first, &first, 200, br"<urlset><url><loc>http://example.com/a</loc></url><url><loc>http://other.test/a</loc></url><url><loc>http://example.com/a</loc></url></urlset>");
        let second = collector.next_request().unwrap();
        assert_eq!(second, "http://example.com/bad.xml");
        collector.note_document(&second, &second, 200, b"<not-xml");
        assert_eq!(collector.next_request(), None);
        let collected = collector.finish();
        assert_eq!(collected.pages, vec!["http://example.com/a".to_string()]);
        assert_eq!(collected.errors.len(), 1);
    }

    #[test]
    fn unclosed_sitemap_commits_nothing() {
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: http://example.com/open.xml\n",
        );
        let url = collector.next_request().unwrap();
        collector.note_document(
            &url,
            &url,
            200,
            br"<urlset><url><loc>http://example.com/a</loc>",
        );
        assert_eq!(collector.next_request(), None);
        let collected = collector.finish();
        assert_eq!(collected.pages, Vec::<String>::new());
        assert_eq!(collected.errors.len(), 1);
        assert!(!collected.truncated);
    }

    #[test]
    fn sitemap_file_urls_that_fail_normalization_are_not_fetched() {
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: http://user:pass@cdn.example/secret.xml\nSitemap: http://example.com/index.xml\n",
        );
        let index = collector.next_request().unwrap();
        assert_eq!(index, "http://example.com/index.xml");
        collector.note_document(
            &index,
            &index,
            200,
            br"<sitemapindex>
                <sitemap><loc>http://user:pass@cdn.example/child.xml</loc></sitemap>
                <sitemap><loc>javascript:alert(1)</loc></sitemap>
                <sitemap><loc>http://example.com/ok.xml</loc></sitemap>
            </sitemapindex>",
        );
        let child = collector.next_request().unwrap();
        assert_eq!(child, "http://example.com/ok.xml");
        collector.note_document(
            &child,
            &child,
            200,
            br"<urlset><url><loc>http://example.com/a</loc></url></urlset>",
        );
        assert_eq!(collector.next_request(), None);
        let collected = collector.finish();
        assert_eq!(collected.pages, vec!["http://example.com/a".to_string()]);
        assert_eq!(collected.errors, Vec::<serde_json::Value>::new());
        assert!(!collected.truncated);
    }

    #[test]
    fn external_entity_is_not_file_contents() {
        let xml = br#"<!DOCTYPE foo [<!ENTITY x SYSTEM "file:///etc/passwd">]><urlset><url><loc>&x;</loc></url></urlset>"#;
        let mut collector = Collector::new("http://example.com");
        collector.next_request();
        collector.note_robots(
            "http://example.com/robots.txt",
            200,
            b"Sitemap: http://example.com/x.xml\n",
        );
        let url = collector.next_request().unwrap();
        collector.note_document(&url, &url, 200, xml);
        let collected = collector.finish();
        assert!(collected.pages.iter().all(|page| !page.contains("root:")));
    }
}
