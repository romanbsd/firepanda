# Firepanda

A small C++20 [MCP](https://modelcontextprotocol.io/) service for scraping,
crawling, and discovering links with native [Lightpanda](https://lightpanda.io/).
Runs locally on macOS and Linux, without Docker, Node, Chromium, or libcurl.

Firepanda exposes three tools over stdio and talks directly to Lightpanda over a
persistent CDP WebSocket on a private Unix-domain socket. Lightpanda starts lazily,
is reused across requests, and stops after 60 seconds of inactivity.

- **scrape**: rendered Markdown, text, cleaned HTML, raw HTML, and links.
- **crawl**: bounded, breadth-first traversal of same-origin pages.
- **map**: anchor-link discovery, including external links without following them.

This is a focused scraping service. It does not provide screenshots, search,
LLM extraction, or full Firecrawl API compatibility. Websites requiring browser
APIs Lightpanda does not implement may need a full browser instead.

## Build and run

Requirements: a C++20 compiler, CMake 3.24+, Git, and native Lightpanda.
Python 3.11+ is needed for the installer, tests, and benchmarks, but not to run the
compiled service. macOS and Linux are supported; Windows is not supported.

From a checkout of this repository:

```sh
python3 scripts/install-lightpanda.py --release 1.0.2
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release \
  -DFIREPANDA_TEST_LIGHTPANDA="$PWD/.local/bin/lightpanda"
cmake --build build --parallel 2
ctest --test-dir build --output-on-failure
./build/firepanda --lightpanda "$PWD/.local/bin/lightpanda"
```

The last command waits for MCP JSON-RPC messages on stdin. Stdout is reserved for
protocol replies; diagnostics go to stderr. It does not open an HTTP port.
Close stdin or send SIGTERM/SIGINT to shut down and clean up the managed browser.

CMake fetches the release tags selected in [CMakeLists.txt](CMakeLists.txt):
cpp-httplib v0.64.0 and nlohmann/json v3.12.0. Git tags are readable version pins,
but upstream can move them. Offline builds can supply local source directories
with `FETCHCONTENT_SOURCE_DIR_HTTPLIB` and `FETCHCONTENT_SOURCE_DIR_JSON`.

The Lightpanda installer supports macOS/Linux on arm64 and x86_64, verifies the
upstream asset's SHA-256 digest, and writes a manifest beside the binary as
`lightpanda.json`. Use `--release TAG` to select a version. Its default is the
mutable `nightly` tag; the command above explicitly uses the tested stable release.
You can also supply an existing Lightpanda binary with `--lightpanda`.

To install Firepanda under a user-owned prefix:

```sh
cmake --install build --prefix "$HOME/.local"
"$HOME/.local/bin/firepanda" --version
```

Release archives, when published, contain the binary, documentation, and license.
Lightpanda is installed separately. After extracting an archive and
entering its directory, use the included installer and prebuilt executable:

```sh
python3 scripts/install-lightpanda.py --release 1.0.2
./bin/firepanda --lightpanda "$PWD/.local/bin/lightpanda"
```

The installer chooses `.local/bin/lightpanda` beside the extracted `scripts/`
directory. Verify the archive's `.sha256` before extraction using `shasum -a 256 -c`
on macOS or `sha256sum -c` on Linux. CMake/test/benchmark commands require a source
checkout. See [release maintenance](docs/releasing.md).

## Connect an MCP client

Replace the two example paths with absolute paths on your machine:

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

Use your client's stdio-server configuration format, then restart/reconnect it.
The working directory and PATH of GUI clients can differ from your terminal;
absolute executable paths avoid that ambiguity. Tool discovery does not start
Lightpanda; the first browser request does.

For a direct protocol smoke test:

```sh
./build/firepanda --lightpanda "$PWD/.local/bin/lightpanda" <<'JSONRPC'
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

Both readiness conditions must hold when supplied together. They run after
`waitFor`, share the page deadline, and apply to every page of a crawl/map.
Choose conditions that also make sense on descendant pages. They do not inspect
iframe documents, shadow trees, or canvas pixels. There is no network-idle heuristic.

### Results and errors

`scrape` returns requested fields plus `metadata` (`url`, `sourceURL`, `title`,
`description`, and `httpStatus` when observed). Links have `{url, title}` records.
`crawl` returns `pages`, `attempted`, `errors`, and `truncated`.
`map` returns `links`, `attempted`, `errors`, and `truncated`.

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
errors; crawl/map also report per-page errors. Use readiness conditions for content
that loads after the initial page-load event. Firepanda does not bypass access controls.

## Browser lifecycle and design

```text
MCP client ── stdin/stdout ── Firepanda ── CDP WebSocket / Unix socket ── Lightpanda
```

- One serialized request stream, one reusable browser connection, and a fresh
  browser context per page isolate cookies and page state between operations.
- A managed browser uses a private temporary directory (0700) and socket (0600).
  No TCP discovery endpoint, local proxy, or heartbeat thread is needed.
- Extraction runs in the page and returns only requested formats. Link-only and
  raw-HTML-only calls skip DOM cloning. Text-readiness scans are cached until DOM
  mutations invalidate them; selector state is still polled.
- A deadline worker exists during active page work to enforce a total deadline
  even if a peer continuously sends WebSocket control frames or fragments.
- Failure, idle expiry, EOF, and orderly shutdown share owned-resource cleanup.
  Failed CDP streams are discarded so late responses cannot contaminate later calls.

The default browser idle timeout is 60 seconds after completed browser work.
Pings and tool discovery do not renew it; active requests are protected. The next
browser request restarts the child. Change it with `--idle-timeout SECONDS`
(integer 0–86400); `0` keeps the browser warm indefinitely.

Attach to a browser you manage yourself:

```sh
mkdir -m 700 /tmp/my-lightpanda
lightpanda serve --unix-socket /tmp/my-lightpanda/cdp.sock
# In another terminal:
./build/firepanda --socket /tmp/my-lightpanda/cdp.sock
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

Exactly reaching a link cap without omitting anything is complete. Dropped links
set `linksTruncated` on scrape output and propagate `truncated` to crawl/map.
The Markdown renderer handles common article elements rather than all HTML semantics.

## Benchmarks

Measurements are local synthetic workloads, not comparisons with Firecrawl,
Chromium, or TCP transport. They do not predict public-site throughput.

| Workload | Before | After | Scope |
| --- | --- | --- | --- |
| Unchanged text readiness, 1-second wait | 20 scans / 65.62 ms CPU | 1 scan / 3.09 ms CPU | Stable Lightpanda 1.0.2, macOS arm64; instrumented extraction |
| Rejecting a 9 MiB JSON page at a 4 MiB cap, 100 iterations | ~1501 ms | ~662 ms | Release C++ microbenchmark; bounded counting versus `dump().size()` |

These A/B numbers were recorded during development on 2026-11-05. Readiness
numbers measure scanning CPU, not the one-second wait. The JSON optimization
avoids allocating a full serialized page solely to count it. Whole-service A/B
measurements varied enough that no general throughput improvement is claimed.

A fresh tagged Release build measured approximately
2.6 ms for a warm small page,
6.1 ms for 256 KiB Markdown,
and 142.8 ms for 10000 links (20 local-fixture samples per case).
The executable was about 527 KiB, excluding Lightpanda.
[Full results and environment](docs/benchmarks.md#current-tagged-build) are retained.

Reproduce current service wall-time measurements from a source checkout:

```sh
python3 scripts/benchmark.py --binary build/firepanda \
  --lightpanda .local/bin/lightpanda --iterations 20 --output build/benchmark.json
./build/json_test --benchmark
```

The Python benchmark uses local HTTP fixtures, reports cold launch separately,
and measures warm small pages, 256 KiB content, 3000-paragraph articles, 10000 links,
and delayed JavaScript content. See [methodology and retained data](docs/benchmarks.md).

## Development and validation

CTest includes protocol/CLI contracts, offline dependency-update and installer
checks, adversarial CDP peers, native browser integration, JSON serialization
boundaries, and 100-cycle memory stress. Local macOS runs passed normally and
under ASan/UBSan/leak detection; the Linux workflows still need a remote run.
Instrumentation covers Firepanda and its C++ dependencies, not the prebuilt
Lightpanda binary. See [validation notes](docs/memory-validation.md).

```sh
cmake --build build --target format        # requires clang-format
cmake --build build --target format-check
cmake -S . -B build-sanitize -DCMAKE_BUILD_TYPE=Debug \
  -DFIREPANDA_SANITIZERS=ON \
  -DFIREPANDA_TEST_LIGHTPANDA="$PWD/.local/bin/lightpanda"
cmake --build build-sanitize --parallel 2
ASAN_OPTIONS=detect_leaks=1:halt_on_error=1 \
UBSAN_OPTIONS=halt_on_error=1:print_stacktrace=1 \
  ctest --test-dir build-sanitize --output-on-failure

cmake -S . -B build-tsan -DCMAKE_BUILD_TYPE=Debug \
  -DFIREPANDA_TSAN=ON \
  -DFIREPANDA_TEST_LIGHTPANDA="$PWD/.local/bin/lightpanda"
cmake --build build-tsan --parallel 2
TSAN_OPTIONS=halt_on_error=1:second_deadlock_stack=1 \
  ctest --test-dir build-tsan --output-on-failure
```

On macOS, use Homebrew LLVM if Apple's sanitizer runtime lacks leak detection;
pass `-DCMAKE_CXX_COMPILER="$(brew --prefix llvm)/bin/clang++"` when creating the
sanitizer build. ThreadSanitizer works with the system Clang on Apple silicon. The
sanitizer and ThreadSanitizer builds are mutually exclusive. The regular macOS
build also supports the `memory-check` target using the system `leaks` tool.

CI runs macOS/Linux builds, Linux sanitizers and ThreadSanitizer, and C++ formatting.
A weekly/manual
[dependency workflow](.github/workflows/bump-fetchcontent.yml) updates FetchContent
release tags, tests the candidate, and opens a pull request. No automatic merge is
performed. See [contributing and release maintenance](docs/releasing.md).

## Security and licensing

The stdio service uses the MCP client's local permissions. Target pages execute
JavaScript in Lightpanda and can access whatever network that process can reach;
a private control socket is not network isolation. Use appropriate egress controls
when browsing untrusted URLs or exposing tools to other users.

Firepanda is [MIT licensed](LICENSE). Its C++ dependencies are MIT licensed.
Lightpanda is downloaded separately under its own upstream license.
