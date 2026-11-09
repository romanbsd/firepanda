# Sitemap-aware map Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Rust `map` optionally merge start-origin sitemap URLs into its link list, and trim that list with path prefixes and `linkLimit`, without changing which pages the walk opens.

**Architecture:** Pure path matching and the link merge live beside the crawl. A sitemap collector turns robots.txt and XML documents into an ordered page list. `crawl::run` keeps today's frontier and, for `map` only, joins a concurrent HTTP collect that cannot halt the walk, then builds the response through one `map_result` function.

**Tech Stack:** Rust edition 2024, existing `primp` client in `src/http.rs`, `serde_json`, new `quick-xml` 0.38 and `flate2` 1. Python black-box tests over the release binary.

**Spec:** `docs/superpowers/specs/2026-11-09-sitemap-aware-map-design.md`

## Global Constraints

- Rust `map` only. `crawl`, `scrape`, `fetch`, `search`, and the Node service do not gain these arguments or response fields.
- `sitemap` is boolean, default false. `includePaths` and `excludePaths` default to `[]`. `linkLimit` is an integer from 1 to 10000.
- When `sitemap` is true and `linkLimit` is omitted, the cap is 100. When `sitemap` is false and `linkLimit` is omitted, there is no new cap: today's 10000-link ceiling and 1 MiB crawl byte budget stay as they are.
- At most 32 prefixes, each at most 2048 bytes, each starting with `/`, none containing `?` or `#`.
- A prefix matches the URL path on a segment boundary. `/careers` matches `/careers` and `/careers/role` and does not match `/careers-old`. The query is ignored. Exclude wins. Empty `includePaths` does not restrict.
- Path filters and `linkLimit` only change the returned list. The start page and the same later pages still open. `attempted` counts browser navigations only.
- Rendered links stay first in discovery order and win when a sitemap lists the same URL. Leftover slots fill with sitemap-only URLs in document order. Sitemap page URLs must be on the start origin. A sitemap-only link is `{"url"}` with no `title`.
- `omittedRendered` and `omittedSitemap` are always present on `map`, counted after filtering and deduplication. They stay 0 when `sitemap` is false and `linkLimit` is omitted, even if today's ceilings already set `truncated`.
- `robots.txt` contributes `Sitemap:` entries only. `Disallow`, `Allow`, and `Crawl-delay` are ignored. No fallback fetch of `/sitemap.xml`.
- At most 32 sitemap documents after robots.txt, 8 MiB each. The collect shares the crawl's 120s wall clock and must not shorten page dispatch. `priority` and `lastmod` do not reorder.
- Sitemap HTTP uses `src/http.rs`. No new browser dependency. Relative `Sitemap:` values resolve against the requested robots.txt URL.
- `truncated` is true when the walk would already set it, or either omitted count is greater than 0, or a sitemap document cap, body cap, or deadline stops the read.
- Invalid arguments stay JSON-RPC errors (`CallError::Invalid`). Sitemap transport and parse failures are `{url, error}` entries. The walk's links are still returned.

## Review Focus

- A `Sitemap:` value that is a path resolves against the requested robots.txt URL, so `Sitemap: /maps/a.xml` on `http://example.com/robots.txt` fetches `http://example.com/maps/a.xml`.
- A gzip body (magic `1f 8b`, or a final path ending in `.gz`) is decompressed before XML parsing, and its `loc` values are kept.
- The 33rd sitemap document is not fetched. `truncated` is true, there is one error, and page URLs that existed only in an unfetched document do not increase `omittedSitemap`.
- An exclude prefix wins over an include prefix, and `/careers` does not match `/careers-old`, so those URLs leave the returned list and do not increment either omitted count.
- A candidate that does not fit the 1 MiB `links` byte budget is omitted, and every later candidate on that side is omitted too. When that happens on the rendered side, every remaining sitemap-only URL is omitted as well.

---

### Task 1: Path filters, `linkLimit`, and `map` omitted counts

**Files:**
- Create: `src/tools/paths.rs`
- Create: `src/tools/sitemap.rs`
- Modify: `src/tools.rs`
- Modify: `src/tools/crawl.rs`
- Test: `src/tools/paths.rs`, `src/tools/sitemap.rs`, `src/tools.rs`

**Interfaces:**
- Consumes: `crate::json::serialized_size_within`, `crate::tools::origin`, `MAX_CRAWL_BYTES` (1 MiB, private in `src/tools.rs`; pass it in as `byte_budget`).
- Produces:
  - `pub fn url_path(normalized: &str) -> &str` in `src/tools/paths.rs`. Path only, query stripped. `https://example.com/a?b` yields `/a`. `https://example.com/` yields `/`.
  - `pub fn path_allowed(path: &str, include: &[String], exclude: &[String]) -> bool` in `src/tools/paths.rs`.
  - `pub struct MapQuery { pub sitemap: bool, pub include_paths: Vec<String>, pub exclude_paths: Vec<String>, pub link_limit: Option<usize> }` in `src/tools.rs`.
  - `pub fn parse_map_query(args: &serde_json::Value) -> Result<MapQuery, String>` in `src/tools.rs`. This task always sets `sitemap` to false. `link_limit` is `None` when the key is absent.
  - `pub struct Merged { pub links: Vec<serde_json::Value>, pub omitted_rendered: usize, pub omitted_sitemap: usize, pub truncated: bool }` and `pub fn merge_links(rendered: &[serde_json::Value], sitemap_pages: &[String], include: &[String], exclude: &[String], link_limit: Option<usize>, byte_budget: usize) -> Merged` in `src/tools/sitemap.rs`.
  - `pub(crate) fn map_result(rendered: Vec<serde_json::Value>, attempted: i64, errors: Vec<serde_json::Value>, warnings: Vec<serde_json::Value>, walk_truncated: bool, query: &MapQuery, sitemap_pages: &[String], sitemap_errors: Vec<serde_json::Value>, sitemap_truncated: bool) -> serde_json::Value` in `src/tools/crawl.rs`.
  - `crawl::run` gains a final `map: MapQuery` argument. `execute` passes `parse_map_query` for `Tool::Map` and `MapQuery { sitemap: false, include_paths: vec![], exclude_paths: vec![], link_limit: None }` for crawl.

- [ ] **Step 1: Write the failing path tests**

Add `mod paths;` in `src/tools.rs` and this test module in `src/tools/paths.rs` (the functions do not exist yet):

```rust
#[cfg(test)]
mod tests {
    use super::{path_allowed, url_path};

    #[test]
    fn segment_boundary_and_exclude_wins() {
        let careers = vec!["/careers".to_string()];
        let draft = vec!["/careers/draft".to_string()];
        assert!(path_allowed("/careers", &careers, &[]));
        assert!(path_allowed("/careers/role", &careers, &[]));
        assert!(!path_allowed("/careers-old", &careers, &[]));
        assert!(path_allowed("/careers/", &["/careers/".into()], &[]));
        assert!(!path_allowed("/careers", &["/careers/".into()], &[]));
        assert!(!path_allowed("/careers/draft", &careers, &draft));
        assert!(path_allowed("/careers/role", &careers, &draft));
        assert!(path_allowed("/about", &[], &[]));
        assert!(!path_allowed("/about", &[], &["/about".into()]));
        assert!(!path_allowed("/about", &careers, &[]));
    }

    #[test]
    fn url_path_drops_the_query() {
        assert_eq!(url_path("https://example.com/careers?x=1"), "/careers");
        assert_eq!(url_path("https://example.com/"), "/");
    }
}
```

- [ ] **Step 2: Run the path tests to verify they fail**

Run: `cargo test --release --bin firepanda -- segment_boundary_and_exclude_wins url_path_drops_the_query`

Expected: FAIL to compile because `path_allowed` and `url_path` are missing.

- [ ] **Step 3: Implement `url_path` and `path_allowed` in `src/tools/paths.rs`**

`url_path` slices after `origin`. If a `?` remains, cut there. `path_allowed` keeps a path when `include` is empty or a segment-boundary prefix matches, and no exclude prefix matches. A prefix `p` matches path `path` when `path == p` or `path` starts with `p` plus `/`.

- [ ] **Step 4: Run the path tests to verify they pass**

Run: `cargo test --release --bin firepanda -- segment_boundary_and_exclude_wins url_path_drops_the_query`

Expected: PASS

- [ ] **Step 5: Write the failing merge tests**

Add `mod sitemap;` in `src/tools.rs` and these tests in `src/tools/sitemap.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::merge_links;
    use serde_json::json;

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
        let sitemap = ["https://example.com/a".into(), "https://example.com/c".into()];
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
        let budget = serde_json::to_vec(&serde_json::Value::Array(first.links)).unwrap().len();
        let sitemap = ["https://example.com/c".into(), "https://example.com/d".into()];
        let merged = merge_links(&rendered, &sitemap, &[], &[], Some(10), budget);
        assert_eq!(merged.links, vec![url("https://example.com/a")]);
        assert_eq!(merged.omitted_rendered, 1);
        assert_eq!(merged.omitted_sitemap, 2);
        assert!(merged.truncated);
    }
}
```

- [ ] **Step 6: Run the merge tests to verify they fail**

Run: `cargo test --release --bin firepanda -- rendered_links_win_and_filters_are_not_omissions cap_omits_rendered_tail_then_all_sitemap_urls no_cap_keeps_omissions_at_zero byte_budget_omits_the_rest_of_that_side_and_later_sitemap_urls`

Expected: FAIL to compile because `merge_links` is missing.

- [ ] **Step 7: Implement `merge_links` in `src/tools/sitemap.rs`**

Classify sitemap URLs against the rendered URL set before path filtering. A URL on that set stays a rendered link even when the filter later removes it. Filter each side with `path_allowed(url_path(url), ...)`. Keep discovery order, then sitemap document order. When `link_limit` is `None`, return the filtered rendered links, leave both omitted counts at 0, and leave `truncated` false. When it is `Some`, fill rendered slots first. Account bytes the way `Crawl::store_map_link` does: `serialized_size_within` per entry, framing `2` for the first entry and `1` for each later one, against `byte_budget`. An entry that does not fit, and every later candidate on that side, is omitted. A byte stop on the rendered side also omits every remaining sitemap-only URL. Sitemap entries are `{"url": normalized}` with no `title`. Rendered entries are copied through unchanged. `truncated` is true when either omitted count is greater than 0.

- [ ] **Step 8: Run the merge tests to verify they pass**

Run: `cargo test --release --bin firepanda -- rendered_links_win_and_filters_are_not_omissions cap_omits_rendered_tail_then_all_sitemap_urls no_cap_keeps_omissions_at_zero byte_budget_omits_the_rest_of_that_side_and_later_sitemap_urls`

Expected: PASS

- [ ] **Step 9: Write the failing argument and result tests**

Put the parse assertions in a test named `parse_map_query`. Assert:

- `parse_map_query` on `{"url":"https://example.com"}` returns `sitemap == false`, empty prefixes, `link_limit == None`.
- `{"includePaths":["/careers"],"excludePaths":["/careers/draft"],"linkLimit":5}` parses to those values.
- `includePaths` of `["/careers-old"]` is accepted by the parser (the segment rule is applied later, not at parse time).
- These are errors: `includePaths` of `["careers"]`, `[""]`, `["/a?b"]`, `["/a#b"]`, 33 copies of `"/a"`, a prefix longer than 2048 bytes, a non-array, `linkLimit` of `0` and `10001`, `excludePaths` on `Tool::Crawl` via `parse_request` (`Unknown argument: excludePaths`).
- `tools_list()` map properties include `includePaths`, `excludePaths`, and `linkLimit`. Crawl properties include none of them. `linkLimit` has minimum 1, maximum 10000, and no `default` field. Its description contains `100` and `sitemap`.

Name the crawl test `map_result_applies_the_cap`. Call `map_result` with rendered URLs `https://example.com/a` and `https://example.com/b`, `include_paths` of `["/a"]`, `link_limit: Some(1)`, empty sitemap input, and `walk_truncated: false`. Assert the only returned URL is `https://example.com/a`, `omittedRendered` is 0, `omittedSitemap` is 0, and `truncated` is false. Call it again with `include_paths` empty and the same two URLs and assert `omittedRendered` is 1 and `truncated` is true.

- [ ] **Step 10: Run the new argument tests to verify they fail**

Run: `cargo test --release --bin firepanda -- schema_shape parse_map_query map_result_applies_the_cap`

Expected: FAIL to compile, or FAIL the schema assertion, because `parse_map_query`, `map_result`, and the new properties are missing.

- [ ] **Step 11: Register the arguments and build map results through `map_result`**

Add `OptionKind::StringList`. Grow `OPTIONS` with map-only entries:

- `includePaths`, description `Path prefixes a returned link must match. Empty allows every path. A prefix matches on a segment boundary and does not change which pages are opened.`
- `excludePaths`, description `Path prefixes removed from the returned links. Matching does not change which pages are opened.`
- `linkLimit`, integer, minimum 1, maximum 10000, fallback 0 (meaning absent). Description `Maximum links to return, from 1 to 10000. Defaults to 100 when sitemap is true. When omitted with sitemap false, the existing link and byte ceilings apply.`

Schema for a string list: array of strings, `maxItems` 32, default `[]`. Do not put a numeric `default` on `linkLimit`. Parse prefixes in `parse_map_query` with the Global Constraints. `parse_request` / `check_keys` must accept the new keys for map and reject them for crawl. `execute` passes `MapQuery` into `crawl::run`. For `Tool::Map`, replace the inline result object with `map_result(..., sitemap_pages: &[], sitemap_errors: vec![], sitemap_truncated: false)`. Crawl's result object stays without omitted fields. `map_result` appends sitemap errors, ORs `walk_truncated`, `sitemap_truncated`, and `merge_links(...).truncated`, and always writes `omittedRendered` and `omittedSitemap` from the merge. Pass `MAX_CRAWL_BYTES` as `byte_budget`. Do not stop link storage or page dispatch because of `linkLimit` or a path filter.

- [ ] **Step 12: Run the unit tests**

Run: `cargo test --release --bin firepanda`

Expected: PASS, including the existing crawl byte-budget tests and `schema_shape`.

- [ ] **Step 13: Commit**

```bash
git add src/tools.rs src/tools/paths.rs src/tools/sitemap.rs src/tools/crawl.rs
git commit -m "feat: cap and filter map links"
```

### Task 2: Sitemap read

**Files:**
- Modify: `src/tools/sitemap.rs`
- Modify: `src/tools.rs`
- Modify: `src/tools/crawl.rs`
- Modify: `Cargo.toml`
- Modify: `README.md`
- Modify: `docs/firecrawl-parity.md`
- Modify: `tests/fixtures.py`
- Modify: `tests/protocol.py`
- Create: `tests/map_sitemap.py`
- Modify: `Makefile`

**Interfaces:**
- Consumes: Task 1's `MapQuery`, `merge_links`, and `map_result`. `http::client`, `http::read_limited`, `normalize_url`, `origin`.
- Produces:
  - `pub struct Collected { pub pages: Vec<String>, pub errors: Vec<serde_json::Value>, pub truncated: bool }` in `src/tools/sitemap.rs`.
  - `pub struct Collector` with `pub fn new(start_origin: &str) -> Self`, `pub fn next_request(&mut self) -> Option<String>`, `pub fn note_robots(&mut self, requested_url: &str, status: u16, body: &[u8])`, `pub fn note_document(&mut self, requested_url: &str, final_url: &str, status: u16, body: &[u8])`, `pub fn note_failure(&mut self, url: &str, message: String, counts_as_document: bool)`, `pub fn finish(self) -> Collected`.
  - `pub async fn collect(start_origin: &str, deadline: tokio::time::Instant) -> Collected`.
  - `parse_map_query` now honors `sitemap`. When it is true and `linkLimit` is absent, `link_limit` is `Some(100)`.

- [ ] **Step 1: Write the failing collector tests**

Add these tests next to the merge tests in `src/tools/sitemap.rs`. Drive `Collector` with no HTTP.

```rust
#[test]
fn relative_sitemap_directive_resolves_against_the_robots_url() {
    let mut collector = Collector::new("http://example.com");
    assert_eq!(collector.next_request().as_deref(), Some("http://example.com/robots.txt"));
    collector.note_robots(
        "http://example.com/robots.txt",
        200,
        b"User-agent: *\nDisallow: /\nSitemap: /maps/a.xml\n",
    );
    assert_eq!(collector.next_request().as_deref(), Some("http://example.com/maps/a.xml"));
}

#[test]
fn missing_sitemap_directive_does_not_guess_sitemap_xml() {
    let mut collector = Collector::new("http://example.com");
    collector.next_request();
    collector.note_robots("http://example.com/robots.txt", 404, b"");
    assert_eq!(collector.next_request(), None);
    let collected = collector.finish();
    assert!(collected.pages.is_empty());
    assert_eq!(collected.errors.len(), 1);
    assert!(!collected.truncated);
    let mut quiet = Collector::new("http://example.com");
    quiet.next_request();
    quiet.note_robots("http://example.com/robots.txt", 200, b"User-agent: *\nDisallow: /\n");
    let collected = quiet.finish();
    assert!(collected.pages.is_empty());
    assert!(collected.errors.is_empty());
}

#[test]
fn gzip_namespace_and_extension_locs() {
    let xml = b"<?xml version=\"1.0\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\"><url><loc>http://example.com/a</loc><priority>0.1</priority><image:image><image:loc>http://example.com/a.png</image:loc></image:image></url><url><loc>http://example.com/b</loc><lastmod>2000-01-01</lastmod></url></urlset>";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut encoder, xml).unwrap();
    let gzip = encoder.finish().unwrap();
    let mut collector = Collector::new("http://example.com");
    collector.next_request();
    collector.note_robots("http://example.com/robots.txt", 200, b"Sitemap: https://cdn.example/a.xml.gz\n");
    assert_eq!(collector.next_request().as_deref(), Some("https://cdn.example/a.xml.gz"));
    collector.note_document("https://cdn.example/a.xml.gz", "https://cdn.example/a.xml.gz", 200, &gzip);
    assert_eq!(
        collector.finish().pages,
        vec!["http://example.com/a".to_string(), "http://example.com/b".to_string()]
    );
}

#[test]
fn relative_loc_resolves_against_the_final_document_url() {
    let mut collector = Collector::new("http://example.com");
    collector.next_request();
    collector.note_robots("http://example.com/robots.txt", 200, b"Sitemap: http://example.com/maps/a.xml\n");
    let url = collector.next_request().unwrap();
    collector.note_document(
        &url,
        "http://example.com/maps/a.xml",
        200,
        br#"<urlset><url><loc>jobs/1</loc></url></urlset>"#,
    );
    assert_eq!(collector.finish().pages, vec!["http://example.com/maps/jobs/1".to_string()]);
}

#[test]
fn document_cap_does_not_parse_the_unfetched_child() {
    let mut locs = String::from("<sitemapindex>");
    for i in 0..40 {
        locs.push_str(&format!("<sitemap><loc>http://cdn.example/{i}.xml</loc></sitemap>"));
    }
    locs.push_str("</sitemapindex>");
    let mut collector = Collector::new("http://example.com");
    collector.next_request();
    collector.note_robots("http://example.com/robots.txt", 200, b"Sitemap: http://cdn.example/index.xml\n");
    let index = collector.next_request().unwrap();
    collector.note_document(&index, &index, 200, locs.as_bytes());
    let mut fetched = 1usize;
    while let Some(url) = collector.next_request() {
        fetched += 1;
        collector.note_document(&url, &url, 200, br#"<urlset><url><loc>http://example.com/seen</loc></url></urlset>"#);
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
    collector.note_document(&first, &first, 200, br#"<urlset><url><loc>http://example.com/a</loc></url><url><loc>http://other.test/a</loc></url><url><loc>http://example.com/a</loc></url></urlset>"#);
    let second = collector.next_request().unwrap();
    assert_eq!(second, "http://example.com/bad.xml");
    collector.note_document(&second, &second, 200, b"<not-xml");
    assert_eq!(collector.next_request(), None);
    let collected = collector.finish();
    assert_eq!(collected.pages, vec!["http://example.com/a".to_string()]);
    assert_eq!(collected.errors.len(), 1);
}

#[test]
fn external_entity_is_not_file_contents() {
    let xml = br#"<!DOCTYPE foo [<!ENTITY x SYSTEM "file:///etc/passwd">]><urlset><url><loc>&x;</loc></url></urlset>"#;
    let mut collector = Collector::new("http://example.com");
    collector.next_request();
    collector.note_robots("http://example.com/robots.txt", 200, b"Sitemap: http://example.com/x.xml\n");
    let url = collector.next_request().unwrap();
    collector.note_document(&url, &url, 200, xml);
    let collected = collector.finish();
    assert!(collected.pages.iter().all(|page| !page.contains("root:")));
}
```

- [ ] **Step 2: Run the collector tests to verify they fail**

Run: `cargo test --release --bin firepanda -- relative_sitemap_directive_resolves_against_the_robots_url missing_sitemap_directive_does_not_guess_sitemap_xml gzip_namespace_and_extension_locs relative_loc_resolves_against_the_final_document_url document_cap_does_not_parse_the_unfetched_child off_origin_and_duplicate_locs_and_bad_xml external_entity_is_not_file_contents`

Expected: FAIL to compile because `Collector` is missing.

- [ ] **Step 3: Implement `Collector` and `collect`**

Add `quick-xml = "0.38"` and `flate2 = "1"` to `Cargo.toml`. Parse with `quick-xml` local names only. A document root must be `urlset` or `sitemapindex`. Collect a `loc` only when its parent is `url` or `sitemap`. Ignore `priority`, `lastmod`, `changefreq`, and extension elements. Trim loc text. Decode XML entities that `quick-xml` decodes by default, and do not enable a feature that fetches external entities.

`next_request` returns `{start_origin}/robots.txt` first. `note_robots` on a non-200 records one error and queues nothing. On a 200, scan lines for a `Sitemap:` field, case-insensitive, and ignore other fields. Resolve a relative value with `url::Url::join` against `requested_url`, then `normalize_url`. Fetch each sitemap URL once, in first-seen order. A file URL may be off-origin.

`note_document` counts toward 32. A non-200 records an error and does not parse. Gunzip when `body` starts with `1f 8b` or `final_url`'s path ends in `.gz`; a gzip failure is an error for that URL. Resolve a relative page or child loc against `final_url`. Keep a page loc when `normalize_url` succeeds and `origin` equals `start_origin`; drop the rest with no error. A child loc that fails `normalize_url` records an error on the parent document and is not fetched. After 32 documents, `next_request` returns `None`. If queued URLs remain, `finish` sets `truncated` and adds one `{url, error}` on the next unfetched URL. Parsed page URLs stay in document order, depth-first through indexes, first occurrence only.

`note_failure` records `{url, error}`. When `counts_as_document` is true it consumes one of the 32 slots (a fetched body over 8 MiB, or a transport error on a sitemap URL). A timeout before the request is sent uses `counts_as_document: false`, sets `truncated`, and stops further `next_request`s.

`collect` creates `Collector::new`, takes `http::client()`, and loops on `next_request` until the deadline. `GET` with `tokio::time::timeout_at`. Read at most `8 * 1024 * 1024` bytes via `http::read_limited(..., "Sitemap")`. A body-cap or transport error calls `note_failure` and continues until the cap or the deadline. Do not request `/sitemap.xml` yourself.

- [ ] **Step 4: Run the collector tests to verify they pass**

Run: `cargo test --release --bin firepanda -- relative_sitemap_directive_resolves_against_the_robots_url missing_sitemap_directive_does_not_guess_sitemap_xml gzip_namespace_and_extension_locs relative_loc_resolves_against_the_final_document_url document_cap_does_not_parse_the_unfetched_child off_origin_and_duplicate_locs_and_bad_xml external_entity_is_not_file_contents`

Expected: PASS

- [ ] **Step 5: Write the failing schema and black-box tests**

In `src/tools.rs` tests, assert `parse_map_query` on `{"url":"https://example.com","sitemap":true}` yields `sitemap == true` and `link_limit == Some(100)`, and that an explicit `linkLimit` of 5 wins. Assert map's schema has boolean `sitemap` default false, and crawl's schema does not.

Name the crawl test `sitemap_default_stops_at_100`. Call `map_result` with `parse_map_query` of that sitemap-true object, no rendered links, and 101 sitemap page URLs `https://example.com/p/{n}` for `n` in `0..101`. Assert `links` has length 100, `omittedSitemap` is 1, `omittedRendered` is 0, and `truncated` is true.

In `tests/protocol.py`, inside the map branch of `test_initialize_and_tools`, assert the property set includes `sitemap`, `includePaths`, `excludePaths`, and `linkLimit`, and that a crawl tool's properties include none of those names.

Add these routes on the shared `Fixture` in `tests/fixtures.py`. Build absolute locs from the request `Host`.

- `GET /company-start` returns `<a href="/company">Company</a>` and nothing else.
- `GET /company` returns `<a href="/careers">Careers</a>` and `<a href="https://example.com/outside">Outside</a>`.
- `GET /careers` returns an empty HTML page.
- `GET /robots.txt` returns `Sitemap: http://{host}/sitemap.xml`.
- `GET /sitemap.xml` is a sitemap index whose children, in order, are `/child-a.xml`, `/child-b.xml`, and `/bad.xml`.
- `GET /child-a.xml` lists, in order: `https://other.test/off`, `{origin}/company`, `{origin}/careers/role` wrapped with `<priority>0.1</priority>` and `<lastmod>1999-01-01</lastmod>`, and `{origin}/sitemap-only`.
- `GET /child-b.xml` lists `{origin}/second-doc`.
- `GET /bad.xml` returns body `not xml` with status 200.

Add `tests/map_sitemap.py` by copying the service launch from `tests/integration.py` (`BINARY` argv, `LIGHTPANDA` argv, `Fixture` on `127.0.0.1:0`). All calls use `{origin}/company-start`, `limit` 5, and `maxDepth` 2. The rendered links are `{origin}/company`, `{origin}/careers` with title `Careers`, and `https://example.com/outside`. Assert:

- Without `sitemap`: `attempted` equals the same call with `sitemap` true, `{origin}/sitemap-only` is absent, and both omitted counts are 0.
- With `sitemap` true and no `linkLimit`: `{origin}/company` appears once, `{origin}/careers` keeps title `Careers` and comes before `{origin}/sitemap-only` and `{origin}/second-doc` in that order, `{origin}/sitemap-only` is exactly `{"url": "{origin}/sitemap-only"}`, `https://example.com/outside` is listed, the off-origin loc is absent, `errors` is non-empty because `/bad.xml` does not parse, and `attempted` matches the call without `sitemap`.
- With `sitemap` true and `includePaths` `["/careers"]`: `{origin}/company` is absent, `{origin}/careers` and `{origin}/careers/role` are present, and `attempted` still equals the unfiltered sitemap call.
- With `sitemap` true and `linkLimit` 1: the sole link URL is `{origin}/company`, `omittedRendered` is 2, `omittedSitemap` is 3, and `truncated` is true.

Register `python3 tests/map_sitemap.py $(BINARY) $(LIGHTPANDA)` in `Makefile` `gates` after `integration.py`.

- [ ] **Step 6: Run the schema test to verify it fails**

Run: `cargo test --release --bin firepanda -- schema_shape sitemap_default_stops_at_100`

Expected: FAIL because `sitemap` is not in the map schema and the default cap is not applied.

- [ ] **Step 7: Wire `sitemap` through `collect` without touching the frontier**

Register map-only boolean `sitemap`, default false, description `Read Sitemap directives from robots.txt and merge sitemap URLs. Off by default. Does not open those URLs in the browser.` Update the `map` tool description to `List links found by a bounded same-origin crawl; external links are listed, not followed. Optionally merge same-origin URLs from robots.txt sitemaps without opening them.`

In `parse_map_query`, read `sitemap` with `parse_boolean`. After prefixes and `linkLimit`, if `sitemap` is true and `link_limit` is still `None`, set `Some(100)`.

In `crawl::run`, before the walk loop, when `tool == Tool::Map && map.sitemap`, `tokio::spawn(sitemap::collect(origin, tokio_deadline))` using a `tokio::time::Instant` created at the start of `run` from the same 120s `CRAWL_BUDGET`. Do not await it inside the dispatch loop. Do not set `halt` from the sitemap task. Do not enqueue sitemap URLs onto `crawl.queue`. After the walk, await the join handle. On join failure, use an empty page list, one error on `{origin}/robots.txt`, and `sitemap_truncated: true`. Pass `Collected.pages`, `Collected.errors`, and `Collected.truncated` into `map_result`.

Update `README.md`: map-only rows for the four arguments with the defaults in Global Constraints; say path filters do not change which pages open; say `linkLimit` is not `limit`; extend the `map` result sentence with `omittedRendered` and `omittedSitemap`.

Update `docs/firecrawl-parity.md` map section: sitemap discovery is off by default, reads robots.txt and XML sitemaps, does not open sitemap-only URLs, path filters do not steer the walk, and `linkLimit` (1–10000, default 100 when sitemap is on) is separate from the 100-attempt `limit`.

- [ ] **Step 8: Run unit tests, clippy, protocol, and the sitemap black-box**

Run:

```bash
cargo test --release --bin firepanda
cargo clippy --release --all-targets -- -D warnings
cargo build --release
python3 tests/protocol.py target/release/firepanda
python3 tests/map_sitemap.py target/release/firepanda .local/bin/lightpanda
```

Expected: unit tests PASS, clippy prints no warnings, both Python scripts print their pass line. `map_sitemap.py` must print `PASS`.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/tools.rs src/tools/sitemap.rs src/tools/crawl.rs README.md docs/firecrawl-parity.md tests/fixtures.py tests/protocol.py tests/map_sitemap.py Makefile
git commit -m "feat: merge sitemaps into map"
```
