# Native service decisions

Firepanda is a Rust (edition 2024, Tokio) MCP stdio service for macOS and Linux.
Its tools are `fetch`, `scrape`, `crawl`, `map`, and `search`. The executable does not invoke Node, Docker, or an intermediate
HTTP API.

## Communication

`MCP client -> stdin/stdout -> firepanda -> persistent WebSocket over UDS -> Lightpanda`

Lightpanda's native `serve --unix-socket` interface supports CDP WebSockets at
`/`. Firepanda connects directly, without HTTP endpoint discovery or a TCP port.
The socket lives in a `mkdtemp` directory with mode 0700; Lightpanda creates the
socket with mode 0600. Each MCP process owns at most one lazy browser child.
SIGTERM, idle expiry, EOF, and failure paths share one cleanup routine: close the
pooled connections, send SIGTERM, wait up to 500 ms, then SIGKILL and reap. It only
signals the child PID it created; an external browser is never terminated.

## Module map

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | stdio loop: chunked reads, line assembly, idle select arm, EOF/SIGTERM exit |
| `src/cli.rs` | argv parsing and exit codes |
| `src/json.rs` | 64-level nesting check, bounded line parse, `serialized_size_within` |
| `src/mcp.rs` | JSON-RPC framing, modern/legacy negotiation, result wrapping and size caps |
| `src/tools.rs` | tool table, argument schema/validation, URL normalization, dispatch |
| `src/tools/crawl.rs` | bounded BFS crawl/map, ordering, failure drain |
| `src/browser/mod.rs` | process lifecycle, connection pool, idle bookkeeping, shutdown |
| `src/browser/cdp.rs` | deadline-bounded CDP connection, scrape pipeline, validation |
| `src/browser/extract.js` | in-page extractor (embedded with `include_str!`) |
| `src/http.rs` | shared browser-impersonated HTTP client, deadline-bounded body reader |
| `src/tools/fetch.rs` | raw JSON/Markdown/XML/image fetch without the browser |
| `src/search/mod.rs` | keyless search (primp + scraper) |

## Concurrency and lifecycle

The C++ build used a self-pipe and a lock-free atomic shared with the signal
handler. The Rust build uses a Tokio `watch` channel (`Stop`): the signal task
publishes `true`, and every waiter — the stdio select loop, connect retries,
deadline-bounded CDP reads/writes, and readiness sleeps — observes it through a
receiver clone. There is no periodic wakeup and no extra descriptor.

Process state (options, socket directory, child handle, idle timestamp) sits
behind one async mutex. Free CDP connections sit in a second mutex; a page worker
owns its connection for the duration of the scrape and returns it only on success.
A failed page discards its connection and the crawl loop reaps the browser after
draining siblings.

Crawl/map dispatch fills a `maxConcurrency` window in frontier order and consumes
results oldest-first. Fetches overlap, but pages, links, and errors keep sequential
breadth-first order. Bounds match the previous implementation: 120-second crawl
budget, 100 ms dispatch floor, `limit`/`maxDepth` caps, a 10000-link frontier, and
a 1 MiB serialization budget that drops later pages without discarding siblings.

Idle expiry counts from completed browser work (`page_completed`), not from
dispatch. Tool discovery, pings, and notifications do not renew it. The idle
select arm and `/dev/stdin` read share the same loop, so shutdown never races an
in-flight request; the browser is restarted by the next browser request.

## MCP and bounds

Modern (`_meta`) and legacy initialize negotiation are handled in one dispatch
path; unknown protocol versions return `-32022` with data, malformed lines return
parse errors, and notifications never receive replies. Argument validation is a
JSON-RPC error (`-32602`); browser or extraction failures are MCP tool results with
`isError` and no `structuredContent`.

Input is capped at 1 MiB per line before parsing and 64 levels of JSON nesting on
both MCP and CDP paths. WebSocket messages are capped at 16 MiB. Fetch bodies are
capped at 4 MiB raw and search bodies at 8 MiB. Tool results are capped at 8 MiB;
results above 1 MiB omit the duplicate structured field. Text and structured
results are duplicated on the wire below that threshold for client compatibility.
The link cap is defined once in `tools.rs` and passed into the extractor. Option
definitions feed both validation and JSON schemas, including per-tool applicability.

## Memory

- Tool payloads are moved into replies, not re-serialized. `json::object` builds
  response objects from owned values; `json!` would call `to_value(&value)` and
  copy large strings. This covers the serialized tool text, the JSON-RPC envelope,
  fetch bodies, and crawl link/error/warning records.
- Crawl frontier URLs use one `Arc<str>` shared by the queue, the queued/visited
  sets, and map discovery. Map output owns one `String` per returned link.
- `fetch` frees the raw body after base64 encoding; UTF-8 text moves into the
  response string without a copy.
- Crawl's 1 MiB page budget is measured with a counting writer, so a page is never
  serialized into a page-sized temporary string just to size it.
- Remaining ceiling: a reply above 1 MiB still holds the tool payload while the
  text field is serialized, then the serialized text while the envelope is built.
  Streaming the text field directly to stdout would remove one copy; results up to
  1 MiB intentionally duplicate `structuredContent` for MCP compatibility.

## Fetch shortcut

`fetch` bypasses Lightpanda for resources that do not need rendering. A shared
primp client (Chrome impersonation, connection pooling) performs the request under
the tool deadline and reads the body in chunks up to 4 MiB. JSON, Markdown, XML,
text, CSV, SVG, and unknown UTF-8 bodies return as `text`; other media types return
base64. `text/html` and XHTML are rejected with a pointer to `scrape`, so a fetch
never starts the managed browser. Redirects are followed. Responses keep their
status code and body (including 4xx/5xx), matching an HTTP client rather than a
scraper.

The client negotiates HTTP/2 over TLS through ALPN (with Chrome's HTTP/2 stream and
connection window settings) and falls back to HTTP/1.1; cleartext requests use
HTTP/1.1. It keeps pooled connections (90 s idle, TCP_NODELAY, compression), so
repeated fetches and searches reuse connections. The CDP WebSocket is separate:
it is an HTTP/1.1 Upgrade over the Unix socket (no HTTP/2 extended CONNECT) with
16 MiB message, frame, and write-buffer caps.

## Search

scraper (CSS parsing) and url are core dependencies, and primp is shared with
`fetch`. The recipes follow `ddgs`: DuckDuckGo's HTML
endpoint via GET (its form POST is answered with a cookie challenge from
non-browser clients), and Brave's `data-type="web"` result blocks. DuckDuckGo
redirect wrappers (`/l/?uddg=...`) are decoded, ad redirects (`y.js`) are dropped,
and results are deduplicated by normalized URL. `auto` tries DuckDuckGo, then
Brave; network/HTTP failures and empty result sets fall through, all under one
total timeout.

A 429 is retried only while the remaining budget covers the wait. `Retry-After`
(delta seconds or HTTP-date) and `X-RateLimit-Reset` are honored and clamped to
one hour. Brave's HTML endpoint was observed sending no rate-limit headers and
returning 429 after 15 s and 60 s, so headerless 429s get one- and two-second
in-call retries, then a five-minute engine cooldown; a subsequent call inside
that window fails fast with a retry hint. A successful response clears the
cooldown.

The response is Firecrawl v2 shaped (`{"success": true, "data": {"web": [...]}}`).
Engine bodies are capped at 8 MiB and each attempt runs under the remaining tool
budget. `FIREPANDA_SEARCH_DDG_URL` and `FIREPANDA_SEARCH_BRAVE_URL` override the
engine endpoints; `tests/search.py` uses them to serve canned HTML locally, so
parsing, fallback, and validation are tested offline. Live availability is an
engine property, not a test gate.

## Validation

`tests/harness.py` provides the shared JSON-RPC client, protocol-version
constant, and pipelined batch runner used by the suites below.
`tests/protocol.py` checks modern and legacy wire behavior, malformed/oversized
requests, URL/argument validation, notifications, and missing-browser errors.
`tests/integration.py` starts an actual native Lightpanda child and a local HTTP
fixture, testing JavaScript rendering, all formats, deduplication, same-origin
crawl, limits, timeout recovery, browser reuse, private socket permissions,
concurrent crawl ordering and wall time, sibling results kept across a page
failure with child relaunch, external-browser ownership, EOF cleanup, and
SIGTERM during navigation/post-load waits.
`tests/transport.py` uses adversarial Unix-socket CDP peers to check continuous
pings, fragmented messages, malformed replies, reconnection, and SIGTERM while
reading a fragment stream.
`tests/memory.py` repeats 100 browsing cycles, large outputs, and failure recovery
and checks that the descriptor count is flat.
`tests/search.py` covers the offline search fixtures.
`tests/fetch.py` covers raw JSON/Markdown/XML/image fetch, redirects, HTTP errors,
timeouts, the body cap, and asserts that no browser child is spawned.
`cargo test` ports the JSON size-boundary suite and covers URL normalization,
schemas, MCP negotiation, browser pool/ownership, fetch classification, and search
parsing.

CI runs the full suite on macOS and Linux. Release packaging is verified by
running `scripts/package-release.py` on the built binary.

## Packaging

The crate version (`Cargo.toml`) feeds CLI and MCP identity. `make build` and
`make release` both build the five-tool binary.
`make install` copies the binary and MIT license under a prefix.
`scripts/package-release.py` builds `firepanda-VERSION-OS-ARCH.tar.gz` plus a
SHA-256 file containing the binary, README, docs, and the Lightpanda installer.
Lightpanda itself is installed separately.
