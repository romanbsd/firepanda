# Firepanda

A small C++20 MCP service that renders pages with native [Lightpanda](https://lightpanda.io/).
Runs on macOS and Linux without Docker or Node. Uses **cpp-httplib** and
**nlohmann/json**, with a persistent CDP WebSocket over a private Unix-domain socket.

## Build and run natively

Requirements: CMake 3.24+, a C++20 compiler (AppleClang, Clang, or GCC), and native
Lightpanda. Python 3 is used only for the optional installer and tests. The first
CMake configure downloads pinned, hash-checked library sources.

```sh
python3 scripts/install-lightpanda.py
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release \
  -DFIREPANDA_TEST_LIGHTPANDA="$PWD/.local/bin/lightpanda"
cmake --build build --parallel 2
ctest --test-dir build --output-on-failure
./build/firepanda --lightpanda "$PWD/.local/bin/lightpanda"
```

The last command runs an MCP **stdio** server: it waits for JSON-RPC requests on
stdin, emits protocol replies on stdout, and sends diagnostics to stderr.
Lightpanda starts when the first browser tool is called, stays alive for reuse,
and shuts down when stdin closes. No HTTP listening port is needed.

The installer checks the upstream asset's SHA-256 and writes a manifest beside
the binary. Its default `nightly` release is mutable: keep the downloaded binary
and manifest for reproducible runtime deployments. A release tag can be selected
with `--release TAG`. It supports arm64 and x86_64 assets on macOS/Linux.

Add this to your MCP client's configuration, replacing both absolute paths:

```json
{
  "mcpServers": {
    "firepanda": {
      "command": "/absolute/path/to/firepanda/build/firepanda",
      "args": ["--lightpanda", "/absolute/path/to/firepanda/.local/bin/lightpanda"]
    }
  }
}
```

| Tool | Behavior |
| --- | --- |
| `scrape` | Render one URL; return Markdown, text, HTML, raw HTML, or links |
| `crawl` | Bounded breadth-first same-origin crawl with page results and errors |
| `map` | Discover anchor links using a bounded crawl; report external links without queuing them |

All tools require `url`. Optional `formats` defaults to `["markdown"]`;
`onlyMainContent` defaults to true. `timeout` is a per-page budget in milliseconds
(100–120000, default 30000); `waitFor` adds a wait after page load within that
budget. Crawl/map accept `limit` (1–100, default 10) and `maxDepth` (0–5, default 1).
They return `attempted`, `errors`, and `truncated` alongside `pages` or `links`.
Total crawl work is capped at 120 seconds and result size is bounded.
Scrape returns `linksTruncated: true` when it omits links beyond the 10,000-link
cap; crawl/map propagate omitted discovery work through `truncated`. Reaching
exactly the cap without dropping anything is reported as complete.
Crawl/map deduplicate rendered destination URLs, so a redirect and a queued
direct link do not return the same page twice. `attempted` counts actual browser
navigations, including aliases whose destination was not known beforehand.

Example tool arguments:

```json
{"url":"https://example.com","formats":["markdown","links"]}
```

For an independently managed browser:

```sh
lightpanda serve --unix-socket /absolute/path/to/private-directory/lightpanda.sock
./build/firepanda --socket /absolute/path/to/private-directory/lightpanda.sock
```

In this mode Firepanda never terminates the external Lightpanda process.
An older Lightpanda without `--unix-socket` must be upgraded. Browsing calls are
serialized; in-flight cancellation notifications are not implemented. The native
tools implement the documented subset, including a basic article Markdown
renderer, rather than the legacy Firecrawl HTTP request contract.

See [library research and design](docs/native-design.md) for dependency choices,
MCP versions, resource limits, lifecycle, and validation details.

### Formatting and memory validation

With `clang-format` installed, `cmake --build build --target format` formats the
C++ sources using `.clang-format`; `format-check` checks without changing files.
The source separates MCP dispatch (`native/main.cpp`), CDP/process ownership
(`native/browser.cpp`), and tool validation/crawl policy (`native/tools.cpp`).

For AddressSanitizer, UndefinedBehaviorSanitizer, and leak checks:

```sh
cmake -S . -B build-sanitize -DCMAKE_BUILD_TYPE=Debug \
  -DFIREPANDA_SANITIZERS=ON \
  -DFIREPANDA_TEST_LIGHTPANDA="$PWD/.local/bin/lightpanda"
cmake --build build-sanitize --parallel 2
ASAN_OPTIONS=detect_leaks=1:halt_on_error=1 \
UBSAN_OPTIONS=halt_on_error=1:print_stacktrace=1 \
  ctest --test-dir build-sanitize --output-on-failure
```

On macOS, use Homebrew LLVM's compiler if the default Apple sanitizer runtime
does not support leak detection: pass
`-DCMAKE_CXX_COMPILER=/opt/homebrew/opt/llvm/bin/clang++` when first configuring
the sanitizer build (adjust the Homebrew prefix on Intel Macs).
On the regular macOS build, `cmake --build build --target memory-check` runs
the browser workload under the independent system `leaks` tool and saves
`build/evidence/macos-leaks.txt`.

The tests include 100 browsing cycles, large output and failure recovery, deeply
nested input, numeric boundaries, process reuse, and shutdown during navigation
and post-load waits. Adversarial CDP peers exercise malformed replies, continuous
control frames, fragmentation, and deadline enforcement. Instrumentation covers Firepanda and its header-only C++
dependencies. The separately downloaded Lightpanda binary is not instrumented.
See [retained validation results](docs/memory-validation.md) for the tested scope.

## Legacy Node HTTP API

The existing Firecrawl-compatible HTTP service remains available separately in
`src/server.js`. Its Node/Docker setup and routes below apply to that service.

### Run the legacy API

With Docker Compose:

```sh
docker compose up --build
```

Set `API_KEY` in the environment to require `Authorization: Bearer <key>`. For local development, run Lightpanda separately and set its CDP endpoint:

```sh
lightpanda serve --host 127.0.0.1 --port 9222
corepack pnpm install
LIGHTPANDA_CDP_URL=http://127.0.0.1:9222 pnpm start
```

The API listens on port `4000` by default. `PORT`, `HOST`, `LIGHTPANDA_CDP_URL`, `API_KEY`, `MAX_BODY_BYTES`, `MAX_CRAWL_PAGES`, `MAX_CONCURRENCY`, and `JOB_TTL_MS` can be configured with environment variables. The default CDP endpoint is `http://127.0.0.1:9222`; completed in-memory jobs expire after 24 hours by default.

Requests are logged as JSON to stdout (timestamp, method, path, status, and duration). Follow the API logs with `docker compose logs -f firepanda`, or both API and Lightpanda logs with `docker compose logs -f`.

## Routes

Both `/v1` and `/v2` prefixes are accepted.

| Method | Path | Behavior |
| --- | --- | --- |
| POST | `/v1/scrape` | Scrape one URL and return Firecrawl-style `success` and `data` |
| POST | `/v1/crawl` | Start an in-memory, asynchronous same-origin crawl |
| GET, DELETE | `/v1/crawl/:id` | Read status/results or cancel the crawl |
| POST | `/v1/batch/scrape` | Start an in-memory batch scrape |
| GET, DELETE | `/v1/batch/scrape/:id` | Read batch status/results or cancel the job |
| POST | `/v1/map` | Return discovered same-host links |
| GET | `/health`, `/healthz` | Liveness response |

Example scrape:

```sh
curl http://localhost:4000/v1/scrape \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","formats":["markdown","html","links"]}'
```

Example crawl:

```sh
curl http://localhost:4000/v1/crawl \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","limit":10,"scrapeOptions":{"formats":["markdown"]}}'
```

The implemented response envelopes follow Firecrawl v2: scrape returns `{ "success": true, "data": { ...formats, "metadata": { ... } } }`; map returns `{ "success": true, "links": [{ "url", "title", "description" }] }`; crawl and batch creation return `{ "success": true, "id", "url" }`; job status returns `status`, `total`, `completed`, `creditsUsed`, timestamps, `duration`, and a `data` array. Supported scrape request options are `url`, `formats`, `onlyMainContent`, `includeTags`, `excludeTags`, `headers`, `mobile`, `waitFor`, and `timeout`. Crawl also accepts `includePaths`, `excludePaths`, `limit`, `maxDiscoveryDepth` (and the v1 `maxDepth` alias), `maxConcurrency`, and `scrapeOptions`. Batch accepts `urls`, `formats`/scrape options, `maxConcurrency`, and `ignoreInvalidURLs`.

Jobs and results live in process memory and are lost on restart. Crawl depth, page limit, and concurrency are capped by configuration. Scrape supports `markdown`, `html`, `rawHtml`, and `links`; headers, `includeTags`, `excludeTags`, `onlyMainContent`, `mobile`, `waitFor`, and `timeout` are supported. Crawls stay on the starting hostname and respect `includePaths`/`excludePaths`.

## Compatibility limits

This is a lightweight replacement for the common scraping workflow, not a full Firecrawl clone. Search, LLM-backed JSON extraction, screenshots, PDF parsing, actions, webhooks, persistent job storage, and Firecrawl-specific billing/usage endpoints are not implemented. Unsupported output formats return HTTP 400. Lightpanda does not render pixels, so screenshot output is intentionally unavailable. Some sites depend on browser APIs Lightpanda does not yet implement; use a full Chromium browser for those pages.

The service has no authentication unless `API_KEY` is set. Keep it on a trusted network or configure a key before exposing it. Treat target URLs as untrusted input: deploy with network egress controls appropriate to your environment.
