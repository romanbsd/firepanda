# Firepanda

A small Rust [MCP](https://modelcontextprotocol.io/) service for scraping,
crawling, discovering links, and searching the web with native
[Lightpanda](https://lightpanda.io/). Runs locally on macOS and Linux, without
Docker, Node, Chromium, or libcurl.

Firepanda exposes three browser tools over stdio and talks directly to Lightpanda
over a persistent CDP WebSocket on a private Unix-domain socket. Lightpanda starts
lazily, is reused across requests, and stops after 60 seconds of inactivity.

- **scrape**: rendered Markdown, text, cleaned HTML, raw HTML, and links.
- **crawl**: bounded, breadth-first traversal of same-origin pages.
- **map**: anchor-link discovery, including external links without following them.
- **search** (feature-gated): keyless web search through DuckDuckGo and Brave,
  using browser-impersonated HTTP requests instead of a browser or an API key.

This is a focused scraping service. It does not provide screenshots, LLM
extraction, or full Firecrawl API compatibility. Websites requiring browser APIs
Lightpanda does not implement may need a full browser instead.

## Build and run

Requirements: Rust 1.85+ (edition 2024), Git, and native Lightpanda. Python 3.11+
is needed for the installer, tests, and benchmarks, but not to run the compiled
service. macOS and Linux are supported; Windows is not.

From a checkout of this repository:

```sh
python3 scripts/install-lightpanda.py --release 1.0.2
cargo build --manifest-path rust/Cargo.toml --release --features search
./rust/target/release/firepanda --lightpanda "$PWD/.local/bin/lightpanda"
```

Omit `--features search` for the parity build, which advertises exactly the three
browser tools. `make release` is the same command; `make build` is the parity
build. `make gates` runs the browser-dependent suites against the parity binary,
and `make search-gates` runs the offline search suite against a separate
`rust/target-search` build so the parity binary is never overwritten.

The last command waits for MCP JSON-RPC messages on stdin. Stdout is reserved for
protocol replies; diagnostics go to stderr. It does not open an HTTP port.
Close stdin or send SIGTERM/SIGINT to shut down and clean up the managed browser.

The Lightpanda installer supports macOS/Linux on arm64 and x86_64, verifies the
upstream asset's SHA-256 digest, and writes a manifest beside the binary as
`lightpanda.json`. Use `--release TAG` to select a version. Its default is the
mutable `nightly` tag; the command above explicitly uses the tested stable release.
You can also supply an existing Lightpanda binary with `--lightpanda`.

To install Firepanda under a user-owned prefix:

```sh
make install PREFIX="$HOME/.local"
"$HOME/.local/bin/firepanda" --version
```

Release archives contain the binary, documentation, and license. Lightpanda is
installed separately. After extracting an archive and entering its directory, use
the included installer and prebuilt executable:

```sh
python3 scripts/install-lightpanda.py --release 1.0.2
./bin/firepanda --lightpanda "$PWD/.local/bin/lightpanda"
```

The installer chooses `.local/bin/lightpanda` beside the extracted `scripts/`
directory. Verify the archive's `.sha256` before extraction using `shasum -a 256 -c`
on macOS or `sha256sum -c` on Linux. Test and benchmark commands require a source
checkout. See [release maintenance](docs/releasing.md).

## Connect an MCP client

Replace the two example paths with absolute paths on your machine:

```json
{
  "mcpServers": {
    "firepanda": {
      "command": "/absolute/path/to/firepanda/rust/target/release/firepanda",
      "args": ["--lightpanda", "/absolute/path/to/firepanda/.local/bin/lightpanda"]
    }
  }
}
```

Use your client's stdio-server configuration format, then restart/reconnect it.
The working directory and PATH of GUI clients can differ from your terminal;
absolute executable paths avoid that ambiguity. Tool discovery does not start
Lightpanda; the first browser request does.

For a direct protocol smoke test:

```sh
./rust/target/release/firepanda --lightpanda "$PWD/.local/bin/lightpanda" <<'JSONRPC'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"scrape","arguments":{"url":"https://example.com","formats":["markdown","links"]}}}
JSONRPC
```

## Use the tools

Pass these argument objects through your MCP client's tool-call interface.

Scrape one page:

```json
{"url":"https://example.com","formats":["markdown","text","links"]}
```

Fetch a single non-HTML resource. `text` returns the document body as-is;
`markdown` renders preformatted bodies (Markdown files, JSON) inside a code fence:

```json
{"url":"https://example.com/README.md","formats":["text"]}
{"url":"https://api.example.com/data.json","formats":["text"]}
```

Wait for JavaScript-generated content:

```json
{"url":"https://quotes.toscrape.com/js-delayed/","waitForSelector":".quote","waitForText":"Albert Einstein","timeout":30000}
```

Crawl a small section using `crawl`:

```json
{"url":"https://example.com","limit":10,"maxDepth":2,"formats":["markdown"]}
```

Discover links using `map`:

```json
{"url":"https://example.com","limit":5,"maxDepth":1}
```

Search the web without an API key using `search` (search-enabled build):

```json
{"query":"lightpanda headless browser","limit":5}
```

### Arguments

| Argument | Default | Behavior |
| --- | --- | --- |
| `url` | Required | Absolute HTTP(S) URL; credentials are rejected |
| `formats` | `["markdown"]` | Any of `markdown`, `text`, `html`, `rawHtml`, `links`; map always extracts links |
| `onlyMainContent` | `true` | Remove common page chrome and prefer main/article content; applies to projected content, not raw HTML or discovery links |
| `timeout` | `30000` | Per-page budget, 100–120000 ms, including navigation, waits, and extraction |
| `waitFor` | `0` | Extra post-load wait, 0–30000 ms, inside the page budget |
| `waitForSelector` | Unset | Nonempty CSS selector, at most 2048 UTF-8 bytes; tests presence, not visibility |
| `waitForText` | Unset | Nonempty, literal, case-sensitive body text, at most 4096 UTF-8 bytes; excludes scripts/styles/templates |
| `limit` | `10` | Crawl/map only: 1–100 navigation attempts |
| `maxDepth` | `1` | Crawl/map only: 0–5; the starting page has depth 0 |
| `maxConcurrency` | `4` | Crawl/map only: 1–16 pages fetched at once |

Search takes its own arguments:

| Argument | Default | Behavior |
| --- | --- | --- |
| `query` | Required | Nonempty query, at most 512 UTF-8 bytes |
| `limit` | `5` | 1–20 results |
| `engine` | `auto` | `auto`, `duckduckgo`, or `brave`; `auto` falls back to the next engine on failure or no results |
| `timeout` | `15000` | Total search budget, 1000–60000 ms; shared by every engine attempt |

Both readiness conditions must hold when supplied together. They run after
`waitFor`, share the page deadline, and apply to every page of a crawl/map.
Choose conditions that also make sense on descendant pages. They do not inspect
iframe documents, shadow trees, or canvas pixels. There is no network-idle heuristic.

### Results and errors

`scrape` returns requested fields plus `metadata` (`url`, `sourceURL`, `title`,
`description`, and `httpStatus`/`statusCode` when observed). `statusCode` is the
Firecrawl-compatible alias of `httpStatus`. Links have `{url, title}` records.
`crawl` returns `pages`, `attempted`, `errors`, and `truncated`.
`map` returns `links`, `attempted`, `errors`, and `truncated`.
`search` returns Firecrawl v2-shaped data: `{"success": true, "data": {"web":
[{"title", "description", "url"}]}}`, deduplicated by normalized URL in engine
order.

Crawls stay on the initial origin (scheme, host, and port). External redirects
may be returned, but their destinations are not expanded. Final destination URLs
are deduplicated. Unknown redirect aliases still count as navigation attempts.
A truncated result is partial; an individual page can fail while other pages succeed.

Warnings preserve the content and do not set MCP `isError`:

| Code | Meaning |
| --- | --- |
| `http_error` | Observed main-document HTTP status is 4xx/5xx |
| `suspected_block` | Content resembles a security challenge |
| `suspected_error_page` | Content resembles an error page |
| `empty_content` | Requested projection or discovered link set is empty |

The latter three are heuristics, not completeness guarantees. Unknown HTTP status
is omitted; iframe responses do not establish main-document status. Crawl pages
retain warnings; map exposes them as `warnings: [{url, warnings: [...]}]`.

Invalid arguments are JSON-RPC errors. Browser/extraction failures are MCP tool
errors; crawl/map also report per-page errors, and search reports engine/network
failures as tool errors. Use readiness conditions for content that loads after the
initial page-load event. Firepanda does not bypass access controls.

See [Firecrawl parity](docs/firecrawl-parity.md) for the implemented subset and
intentional response-shape differences.

Search is keyless and local: requests use primp browser impersonation to the
DuckDuckGo HTML endpoint (Brave as fallback). Engine availability depends on the
network and can change; the offline fixture suite pins parsing and fallback
behavior without contacting either engine.

## Browser lifecycle and design

```text
MCP client ── stdin/stdout ── Firepanda ── CDP WebSocket / Unix socket ── Lightpanda
```

- One serialized request stream, one reusable browser connection pool, and a fresh
  browser context per page isolate cookies and page state between operations.
- A managed browser uses a private temporary directory (0700) and socket (0600).
  No TCP discovery endpoint, local proxy, or heartbeat thread is needed.
- Extraction runs in the page and returns only requested formats. Link-only and
  raw-HTML-only calls skip DOM cloning. Text-readiness scans are cached until DOM
  mutations invalidate them; selector state is still polled.
- Every CDP read and write is deadline-bounded with `select!` on the cancellation-safe
  stream, so a peer that keeps sending control frames or fragments cannot extend a
  page past its deadline. Ping frames are answered inline.
- Crawl/map use a bounded dispatch window: pages are dispatched in frontier order
  and consumed oldest-first, preserving sequential breadth-first output. A failed
  page drains its in-flight siblings (keeping their results), then reaps and
  relaunches the managed browser before continuing.
- Failure, idle expiry, EOF, and orderly shutdown share owned-resource cleanup.
  Failed CDP connections are discarded so late responses cannot contaminate later calls.

The default browser idle timeout is 60 seconds after completed browser work.
Pings and tool discovery do not renew it; active requests are protected. The next
browser request restarts the child. Change it with `--idle-timeout SECONDS`
(integer 0–86400); `0` keeps the browser warm indefinitely.

Attach to a browser you manage yourself:

```sh
mkdir -m 700 /tmp/my-lightpanda
lightpanda serve --unix-socket /tmp/my-lightpanda/cdp.sock
# In another terminal:
./rust/target/release/firepanda --socket /tmp/my-lightpanda/cdp.sock
```

The socket path must be absolute and at most 100 bytes. Firepanda closes its own
connection on failure/shutdown but never kills an external browser or removes its
socket. SIGKILL cannot run cleanup. Calls are serialized, and cancellation
notifications do not interrupt an in-flight tool call.

See [design details](docs/native-design.md) for code structure and protocol behavior.

### Resource bounds

| Resource | Bound |
| --- | --- |
| MCP request | 1 MiB before parsing |
| JSON nesting | 64 levels |
| WebSocket message | 16 MiB |
| Serialized tool data | 8 MiB; results above 1 MiB omit the duplicate structured field |
| Crawl page data | 4 MiB total |
| Crawl/map total work | 120 seconds |
| Crawl/map traversal | 100 attempts, depth 5 maximum |
| Unique discovery/frontier links | 10000 |
| Search results | 20 per call; 60-second budget maximum |
| Search response body | 8 MiB |

Exactly reaching a link cap without omitting anything is complete. Dropped links
set `linksTruncated` on scrape output and propagate `truncated` to crawl/map.
The Markdown renderer handles common article elements rather than all HTML semantics.

## Benchmarks

Measurements are local synthetic workloads, not comparisons with Firecrawl,
Chromium, or TCP transport. They do not predict public-site throughput.

Five-iteration warm medians from `scripts/benchmark.py` on macOS arm64 with stable
Lightpanda 1.0.2 and local HTTP fixtures:

| Workload | Rust | Historical C++ build |
| --- | --- | --- |
| Cold launch + first scrape | 28.8 ms | 34.5 ms |
| Warm small page | 1.7 ms | 1.8 ms |
| 256 KiB page | 3.8 ms | 4.9 ms |
| 3000-paragraph article | 26.0 ms | 27.4 ms |
| 10000 links | 37.6 ms | 138.5 ms |
| Delayed JavaScript text | 309.2 ms | 308.7 ms |
| Crawl of 51 pages, `maxConcurrency=1` | 142.5 ms | 252.9 ms |
| Crawl of 51 pages, `maxConcurrency=4` | 90.2 ms | 199.3 ms |

The crawl numbers show the dispatch window doing useful work: 1.58× speedup at
concurrency 4 versus 1.27× for the earlier threading implementation. The C++
column is retained only as a reference point; that implementation no longer
exists in this repository.

Reproduce current service wall-time measurements from a source checkout:

```sh
python3 scripts/benchmark.py --binary rust/target/release/firepanda \
  --lightpanda .local/bin/lightpanda --iterations 20 --output dist/benchmark.json
```

The Python benchmark uses local HTTP fixtures, reports cold launch separately,
and measures warm small pages, 256 KiB content, 3000-paragraph articles, 10000 links,
and delayed JavaScript content. See [methodology and retained data](docs/benchmarks.md).

## Development and validation

The black-box suites spawn the binary over stdio, so they run unchanged against
any build:

```sh
make check          # cargo fmt --check + clippy for both feature sets
make test           # cargo test for both feature sets
make gates          # protocol, transport, integration, memory (needs Lightpanda)
make search-gates   # offline search fixtures, separate target directory
```

`clippy` runs with the pedantic and nursery groups enabled in `Cargo.toml`
(doc-only lints that would force boilerplate are allowed), so the CI `-D warnings`
step ratchets the stricter set.

`tests/protocol.py` covers modern/legacy wire behavior, malformed requests,
argument validation, notifications, and missing-browser errors.
`tests/transport.py` uses adversarial Unix-socket CDP peers for pings, fragments,
malformed replies, reconnection, and SIGTERM during reads.
`tests/integration.py` drives a real Lightpanda child and a local HTTP fixture:
JavaScript rendering, all formats, crawl/map ordering and concurrency, failure
recovery, browser reuse, socket permissions, external-browser ownership, EOF
cleanup, and SIGTERM teardown.
`tests/memory.py` repeats 100 browsing cycles with large outputs and failure
recovery, and checks descriptor hygiene.
`tests/search.py` serves canned DuckDuckGo/Brave HTML locally and pins redirect
decoding, ad filtering, deduplication, limits, fallback, and validation without
network access.

CI runs the parity suite on macOS and Linux, the search feature suite separately,
and verifies release packaging. See [contributing and release maintenance](docs/releasing.md).

## Security and licensing

The stdio service uses the MCP client's local permissions. Target pages execute
JavaScript in Lightpanda and can access whatever network that process can reach;
a private control socket is not network isolation. Use appropriate egress controls
when browsing untrusted URLs or exposing tools to other users.

Firepanda is [MIT licensed](LICENSE). Rust dependencies are MIT/Apache-2.0
licensed, and Lightpanda is downloaded separately under its own upstream license.
