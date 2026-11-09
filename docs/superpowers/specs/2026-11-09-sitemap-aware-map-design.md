# Sitemap-aware map

## Intent

`map` discovers links by opening pages in Lightpanda and reading rendered anchors. A public URL that is listed in a sitemap and not linked from a page the walk opens never appears. Callers need those URLs in the result without sending every sitemap entry through the browser, and without a heavier browser dependency.

This design adds an optional sitemap read to the Rust `map` tool. Sitemap URLs are merged into the link list. The walk keeps opening the same pages it opens today.

## Goals

- When `sitemap` is true, read `Sitemap:` entries from the start origin's `robots.txt`, then sitemap indexes and URL-set documents, and merge the page URLs into `links`.
- Leave the rendered-page walk on its current rules: same pages opened, same 1–100 attempt limit, same depth, same "list any origin, open only the start origin" rule.
- Deduplicate. A URL found by the walk keeps its rendered-link place and its title when a sitemap lists it too.
- Trim the returned list with path prefixes and a separate return cap. Neither one chooses which pages open.
- When the return cap drops URLs, set `truncated` and report how many rendered links and how many sitemap-only URLs were left out, counted after filtering and deduplication.
- Keep sitemap fetching on the existing direct HTTP client.

## Non-goals

- A traversal-pruning mode. If conserving the 100 page attempts matters later, that is a separate explicit option. It is not implied by a path filter or by `linkLimit`.
- Ranking or filtering by sitemap `priority` or `lastmod`.
- Honoring `Disallow`, `Allow`, or `Crawl-delay`.
- Guessing `/sitemap.xml` when `robots.txt` names no sitemap.
- Sitemap support on `crawl`, `scrape`, `fetch`, or `search`.
- The legacy Node HTTP service.
- Opening sitemap-only URLs in Lightpanda, collecting titles for them, or collecting link descriptions.
- A new browser engine or a new browser dependency.

## Anti-goals

- A full return list, or an `includePaths` value, that quietly changes which pages Lightpanda opens. An include of `/careers` must still allow the walk to open an intermediate `/company` page that links into `/careers`.
- A large sitemap that turns into one browser navigation per URL.

## Decisions

These were settled in discussion. The calls in [Calls](#calls) were not.

- Sitemap reading augments discovery. The rendered walk still runs.
- Sitemap URLs join the result even when Lightpanda never opens them. If the walk reaches one, that page is opened once, under the existing attempt limit, and its rendered links are discovered as usual.
- Rendered links stay first, in today's discovery order. After deduplication and path filtering, remaining return slots fill with sitemap-only URLs in sitemap document order.
- The same URL in both sources is a rendered link, not a sitemap-only URL.
- The return cap applies to the merged list. Extra rendered links and extra sitemap-only URLs are both omitted. Each side has its own omitted count, after filtering and deduplication.
- Filling the cap does not stop the walk. The walk still runs to its attempt limit, depth limit, and existing time and byte budgets.
- Rendered links from other origins can still be listed and are not opened.
- A sitemap page URL is kept only when it is on the start origin.
- A sitemap-only result is `{"url": "..."}` with no `title`.
- `robots.txt` is only a list of sitemap addresses.
- Path filters only remove URLs from the returned list. The start page is still opened when its path would be filtered out of the results.

## Arguments

All four are `map` only. Unknown arguments stay invalid, as they are today. `crawl` does not accept them.

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `sitemap` | boolean | `false` | Read robots.txt and sitemaps. |
| `includePaths` | array of strings | `[]` | Path prefixes a returned URL must match. Empty means no include restriction. |
| `excludePaths` | array of strings | `[]` | Path prefixes removed from the returned list. |
| `linkLimit` | integer | see below | Maximum length of the returned `links` array. Range 1–10000. |

`linkLimit` when omitted:

- `sitemap: true` defaults it to 100.
- `sitemap` omitted or false leaves the returned list under today's ceilings only (`MAX_LINKS` of 10000 and the crawl byte budget).

Passing `linkLimit` without `sitemap` caps the filtered rendered list and leaves `omittedSitemap` at 0.

Invalid arguments are JSON-RPC errors, same as today's bad `limit`. That includes a `linkLimit` outside 1–10000, a non-boolean `sitemap`, a path array that is not an array of strings, more than 32 prefixes, a prefix longer than 2048 bytes, an empty prefix, a prefix that does not start with `/`, and a prefix that contains `?` or `#`.

## Path filters

Match the URL path only, after `normalize_url`. The query is ignored. Matching is case-sensitive and stops on a path-segment boundary:

- prefix `/careers` matches `/careers` and `/careers/role`
- prefix `/careers` does not match `/careers-old`
- prefix `/careers/` matches `/careers/` and `/careers/role`, and does not match `/careers`

A URL is kept when both of these hold:

- `includePaths` is empty, or the path matches at least one include prefix
- the path matches no exclude prefix

An exclude wins when a path matches both. Filtering removes the URL from the candidate list. It does not increment either omitted count.

The filter runs on the rendered link list map would have returned, and on sitemap-only candidates. It does not run on the frontier. The start page is opened even when its own path would be filtered out of `links`.

## Sitemap read

Runs only when `sitemap` is true. Uses `src/http.rs` (primp). Does not start Lightpanda by itself. Must not consume page attempts, must not set the walk's halt, and must not change the frontier. Running it concurrently with the walk is the intended shape. Running it after the walk is acceptable. Running it in a way that shortens the walk is not.

1. `GET {start-origin}/robots.txt`.
2. Collect `Sitemap:` values. The field name is case-insensitive. Resolve a relative value against the robots.txt URL. Fetch each sitemap URL once, in first-seen order.
3. A missing robots.txt, a non-200 robots.txt, or a robots.txt with no `Sitemap:` line is not a guessed `/sitemap.xml`. No sitemap URLs are added. A transport failure or a non-200 is one `errors` entry on the robots URL. The walk's result is still returned. A 200 with no `Sitemap:` line adds no error.
4. A sitemap document is either a sitemap index (`sitemapindex` / `sitemap` / `loc`) or a URL set (`urlset` / `url` / `loc`). Read `loc` text in document order. Ignore `priority`, `lastmod`, `changefreq`, and extension tags.
5. An index is expanded depth-first in document order: all page URLs from the first child, then the next child. Nested indexes follow the same rule.
6. Accept the sitemap namespace and no namespace. Other XML shapes are a parse error for that document.
7. Sitemap file URLs may be on another host. They must pass `normalize_url` (http or https, no credentials). Page URLs must pass `normalize_url` and `origin` must equal the start origin. Anything else is dropped, not counted as omitted, and not an error.
8. Invalid page locs are skipped the same way invalid rendered hrefs are skipped.
9. Duplicate sitemap file URLs are fetched once. Duplicate page URLs keep the first document-order occurrence.
10. Gzip: the HTTP client already decodes `Content-Encoding`. If the body still starts with gzip magic `1f 8b`, or the final URL path ends in `.gz`, gunzip it before parsing. A gzip body that does not decode is an error for that document.

Bounds, so one call cannot pull an unbounded sitemap tree:

- At most 32 sitemap documents, not counting robots.txt. Past that, stop fetching, set `truncated`, and add one `errors` entry. URLs inside documents that were not fetched are not included in `omittedSitemap`, because they were never parsed.
- Each document body is capped at 8 MiB, compressed or not, using the existing limited reader. Over the cap: error for that URL, skip the document, set `truncated`, continue with the other documents.
- Index nesting past the document cap stops the same way. There is no separate depth knob.
- Sitemap requests share the crawl's 120s wall-clock budget as their own deadline. When that deadline hits, stop fetching, record an error, set `truncated`, and keep the URLs already parsed. This deadline must not be implemented by stopping page dispatch early.

`Disallow` and the other robots group records are ignored, even when they would cover a sitemap URL or a rendered link.

## Merge

Start from the rendered `links` list the walk already built, including today's truncation at 10000 links and the crawl byte budget. Do not stop that collection early because of `linkLimit` or a path filter. Precedence uses this list: a URL on it is a rendered link even if storage later drops it for the return cap.

1. Split sitemap page URLs into sitemap-only URLs by removing any URL already on the rendered list. Order is sitemap document order.
2. Path-filter each side.
3. Give return slots to filtered rendered links, in discovery order, until `linkLimit` is exhausted.
4. Give leftover slots to filtered sitemap-only URLs, in document order.
5. While adding a slot, the existing crawl byte budget still applies to the serialized `links` array. A link that does not fit is not returned. It and every later candidate on that same side count as omitted. Later sitemap-only URLs are omitted too when the byte budget trips on the rendered side.

Counts:

- `omittedRendered` is the number of path-filtered rendered links that did not receive a slot.
- `omittedSitemap` is the number of path-filtered sitemap-only URLs that did not receive a slot.
- URLs removed by the path filter, by the origin rule, or by normalization are in neither count.
- Both counts are 0 when `sitemap` is false and `linkLimit` is omitted, including when today's 10000-link or byte ceiling already set `truncated`.

`truncated` is true when today's walk would set it, or when either omitted count is greater than 0, or when a sitemap document cap, body cap, or sitemap deadline stops the read.

A rendered link that today's ceiling never stored is not on the rendered list. Its sitemap copy may still fill a sitemap-only slot.

## Response

`map` always includes the two new counts, including on calls that do not enable sitemap:

```json
{
  "links": [{"url": "https://example.com/a", "title": "A"}],
  "attempted": 3,
  "errors": [{"url": "https://cdn.example/sitemap.xml", "error": "..."}],
  "truncated": true,
  "omittedRendered": 0,
  "omittedSitemap": 12
}
```

Existing fields keep their meaning. `attempted` counts browser page attempts only. Sitemap HTTP does not increment it. `errors` keeps `{url, error}`. Warnings stay per visited page. A sitemap-only link has no `title` and no warning.

## Calls

These are author calls. A builder should follow them unless an implementation constraint makes one of them false.

- Feature lives on the Rust `map` tool only. Schema, validation, README argument table, `docs/firecrawl-parity.md`, and the `map` tool description are updated together. The Node service is left alone.
- Argument names are `sitemap`, `includePaths`, `excludePaths`, and `linkLimit`.
- Count names are `omittedRendered` and `omittedSitemap`. They are always present on `map`.
- No `/sitemap.xml` fallback.
- Segment-boundary prefix match, case-sensitive path, at most 32 prefixes, 2048 bytes each.
- 32 sitemap documents, 8 MiB each, shared 120s wall clock that must not shorten the walk.
- Relative `Sitemap:` values resolve against the robots.txt URL.
- XML parsing may be a small crate or a bounded reader. It has to honor the namespace rule, the body cap, and the document cap. It must not pull in a browser stack. Prefer a parser that does not expand entities from the network.

## Tests

Unit tests can cover prefix matching, robots `Sitemap:` extraction, index expansion order, gzip magic, origin drops, and the merge counts without a browser.

Black-box tests against the `map` tool should show:

- `sitemap` false leaves the opened pages and the link order unchanged, with both omitted counts at 0.
- A sitemap-only URL is returned without a navigation to that URL, as `{url}` only.
- A URL present in both sources keeps the rendered position and title.
- An off-origin sitemap `loc` is dropped. An off-origin rendered link is still listed and not opened.
- `includePaths: ["/careers"]` removes other paths from `links` while `attempted` still includes the intermediate page the walk would have opened.
- `linkLimit` smaller than the filtered rendered list omits the rendered tail, omits every remaining sitemap-only URL, and sets `truncated`.
- With `sitemap: true` and no `linkLimit`, the list stops at 100.
- `priority` and `lastmod` do not change order. Document order does, across an index.
- A robots.txt with no `Sitemap:` line adds no sitemap URLs and no error.
- A malformed sitemap adds an `errors` entry and still returns rendered links.

## What the builder still decides

- Module split, as long as HTTP stays in the existing client and the walk's frontier stays untouched.
- Error string wording.
- Whether the sitemap task is joined at the end of `crawl::run` or folded in just before the result object is built.
- How the README and parity doc sentences are phrased, as long as they say sitemap discovery is off by default, path filters do not steer the walk, and `linkLimit` is not `limit`.
