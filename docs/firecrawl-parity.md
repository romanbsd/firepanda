# Firecrawl parity

Firepanda follows Firecrawl's vocabulary where the behavior is implemented, but it
is not a drop-in Firecrawl API. This page records the implemented subset, the
intentional differences, and the MCP surface clients see.

## MCP tool surface

Every tool in `tools/list` carries:

- `name`, human-readable `title`, and `description`;
- `inputSchema` with `additionalProperties: false` and the documented bounds;
- annotations: `readOnlyHint: true`, `destructiveHint: false`,
  `idempotentHint: true`, `openWorldHint: true` — all four tools are idempotent
  network readers with no destructive side effects.

No `outputSchema` is declared. Crawl and map results can exceed the 1 MiB
`structuredContent` threshold and then omit that field, so a fixed output schema
would not always validate; `content[0].text` is always the same JSON payload.
Invalid arguments return JSON-RPC `-32602`; browser/network failures return an
`isError` tool result with text content only.

`map` rejects `formats` and `onlyMainContent`: it always extracts links, so those
projection options would be no-ops. `scrape` rejects discovery options
(`limit`, `maxDepth`, `maxConcurrency`). `fetch` accepts only `url` and `timeout`.

## fetch

Firepanda extension: no Firecrawl counterpart. Fetches a raw non-HTML resource
over HTTP without starting the browser.

Arguments: `url` and `timeout` (100–120000 ms). Response:
`{url, sourceURL, statusCode, contentType, size, encoding, text|base64}`.
`encoding` is `text` for JSON, Markdown, XML, CSV, SVG, and other text types;
unknown/`application/octet-stream` bodies are sniffed as UTF-8, otherwise returned
as base64. `text/html` and XHTML are rejected with a pointer to `scrape`. Redirects
are followed and `url` is the final URL. Bodies are capped at 4 MiB.

## scrape

Implemented arguments:

| Argument | Firecrawl counterpart | Notes |
| --- | --- | --- |
| `url` | `url` | required, absolute HTTP(S), no credentials |
| `formats` | `formats` | strings only; `markdown`, `text`, `html`, `rawHtml`, `links` |
| `onlyMainContent` | `onlyMainContent` | deterministic HTML-level main-content filter |
| `timeout` | `timeout` | 100–120000 ms (Firepanda bound) |
| `waitFor` | `waitFor` | 0–30000 ms, inside the page budget |
| `waitForSelector` | — | Firepanda addition: wait for CSS-selector presence |
| `waitForText` | — | Firepanda addition: wait for literal body text |

Use `fetch` for a single non-HTML resource of any content type: JSON, Markdown,
XML, CSV, text, SVG, and images. It bypasses the browser entirely. `scrape` also
renders a preformatted Markdown/JSON document through Lightpanda (as a code
fence), but `fetch` is the fast path and returns text verbatim or binary as
base64.

Not implemented: object-form formats (`json`, `summary`, `screenshot`, …),
`includeTags`/`excludeTags`, `headers`, `actions`, `location`, `proxy`, caching
options, and LLM-based cleaning.

Response shape differs: Firepanda returns the payload directly
(`{markdown, …, metadata, warnings?}`), not Firecrawl's `{success, data}` envelope.
`metadata` includes `url`, `sourceURL`, `title`, `description`, and
`httpStatus`/`statusCode` (the Firecrawl name) when the main-document status is
observed. Firecrawl's single `warning` string is replaced by a `warnings` array of
`{code, message}` records. Links are `{url, title}` records.

## crawl

Implemented arguments: `url`, `formats`, `onlyMainContent`, `timeout`, `waitFor`,
`waitForSelector`, `waitForText`, `limit` (1–100), `maxDepth` (0–5), and
`maxConcurrency` (1–16).

Firecrawl's crawl is asynchronous: `POST /v2/crawl` returns `{success, id, url}`
and clients poll a job. Firepanda runs the crawl synchronously inside the tool
call and returns all pages at once. It is breadth-first, same-origin, and bounded
by 120 seconds and a 4 MiB page budget. `maxDepth` is Firepanda's name for
Firecrawl's `maxDiscoveryDepth`. Not implemented: `includePaths`/`excludePaths`,
`sitemap` modes, `delay`, `allowSubdomains`, `allowExternalLinks`, webhooks, and
the async status endpoint.

Response: `pages`, `attempted`, `errors`, and `truncated`. Each page is a scrape
payload with warnings retained.

## map

Implemented arguments: `url`, `limit` (1–100 attempts), `maxDepth` (0–5),
`maxConcurrency` (1–16), `timeout`, and the `waitFor*` readiness options.

Firecrawl's map uses its URL index/sitemap and accepts `search`, `sitemap`,
`includeSubdomains`, `ignoreQueryParameters`, `ignoreCache`, `location`, and
`timeout` (default 60000). Firepanda instead runs a bounded same-origin BFS:
external links are listed but never followed, and discovery depends on rendered
anchors rather than a sitemap. `search`/`sitemap`/subdomain options are not
implemented, and the limit maximum is 100 attempts rather than 5000 links.

Response: `links` (`{url, title}`), `attempted`, `errors`, and `truncated`.
Firecrawl also returns `success` and a per-link `description`; Firepanda's
extractor does not collect link descriptions.

## search

Implemented arguments: `query`, `limit` (1–20), `engine`
(`auto`/`duckduckgo`/`brave`), and `timeout` (1000–60000 ms).

The response is Firecrawl v2 shaped — `{"success": true, "data": {"web":
[{"title", "description", "url"}]}}` — with deduplicated results in engine order.
It is keyless and local: primp impersonates a browser against DuckDuckGo's HTML
endpoint with Brave as fallback. Not implemented: `sources`, `categories`, `tbs`,
`location`, `country`, `safe`, `scrapeOptions`, result scraping, and the
`id`/`creditsUsed` job metadata.

## Related

- [Native design](native-design.md) for the MCP/transport architecture.
- [README](../README.md#use-the-tools) for argument examples and bounds.
