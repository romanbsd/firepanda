# Native service decisions

The native program is a C++20 MCP stdio service for macOS and Linux. Its three
tools are `scrape`, `crawl`, and `map`. The executable does not invoke Node, Docker, or an intermediate HTTP API.

## Communication

`MCP client -> stdin/stdout -> firepanda -> persistent WebSocket over UDS -> Lightpanda`

Lightpanda's native `serve --unix-socket` interface supports CDP WebSockets at
`/`. Firepanda connects directly, without HTTP endpoint discovery or a TCP port.
The socket lives in a `mkdtemp` directory with mode 0700; Lightpanda creates the
socket with mode 0600. Each MCP process owns one lazy browser child and one CDP
connection. `--socket` attaches to an independently managed browser instead.
CDP replies and events share this connection. Only bounded page-load and main-document status observations are retained for the
active session; arbitrary event bodies are discarded immediately.
cpp-httplib's blocking read timeout can restart while processing control frames
or fragments. A deadline worker exists only while a page operation is active;
it shuts down a duplicate socket descriptor on timeout or shutdown. Owning the
duplicate prevents descriptor reuse during retries from targeting another file.
The worker is joined and its descriptor closed before the operation returns.
The shutdown flag uses a lock-free atomic shared with the signal handler.
While idle, the main thread uses `pselect` with an atomic signal-mask transition
instead of checking a flag and then entering a separate blocking read. This
closes the idle shutdown race without periodically waking to poll for signals.

One connection and serialized operations avoid contention. Each scrape uses a
fresh browser context, navigates once, waits for `Page.loadEventFired`, evaluates
one projection of the DOM, then disposes the context. Requested formats alone
cross the socket. Extra `waitFor` is available for scripts that continue after
load. There is no claim of network-idle detection. Connection/process reuse is
tested, but Unix-socket versus TCP speed has not been benchmarked; rendering and
network requests can dominate either transport.

Timeouts invalidate the CDP connection. Contexts use `disposeOnDetach`, so failed
streams are disconnected without sending further cleanup commands. Managed browser failures also stop the
child before the next operation can restart it. EOF and SIGTERM/SIGINT initiate
cleanup; only the child PID created by this process is signalled and reaped.
An external browser is never terminated. SIGKILL cannot run cleanup.

## Libraries researched

| Library | Decision |
| --- | --- |
| [cpp-httplib](https://github.com/yhirose/cpp-httplib) | Selected. The selected release has WebSocket clients, UDS, host-to-socket mapping, bounded reads, and a single-header build. Heartbeat disabled to avoid an extra thread. TLS/compression disabled for the local CDP transport. Lightpanda handles HTTPS. Selected release tag: `v0.64.0`. |
| [nlohmann/json](https://json.nlohmann.me/integration/cmake/) | Selected, version 3.12.0. Small integration surface, dynamic CDP/MCP messages, portable C++20 toolchain. Not chosen on a claim of fastest JSON parsing. |
| [Boost.Beast](https://www.boost.org/doc/libs/latest/libs/beast/doc/html/beast/using_websocket.html) + [Asio](https://www.boost.org/doc/libs/latest/doc/html/boost_asio/overview/posix/local.html) | Capable alternative with async operations and generic socket streams. More build and API complexity than this single-connection blocking service needs. |
| [Glaze](https://github.com/stephenberry/glaze) | Strong option for typed serialization. Dynamic protocol envelopes and broad AppleClang compatibility made nlohmann/json the simpler initial choice. No comparative performance benchmark was run. |
| [libcurl WebSockets](https://curl.se/libcurl/c/libcurl-ws.html) | Excluded; the local transport does not need another HTTP stack. |
| [mcp-cpp-sdk](https://www.yurirocha.com/mcp-cpp-sdk/getting-started.html) | Third-party SDK with Boost dependencies. This service implements its bounded stdio protocol surface directly instead of adding its transport stack. |

The cpp-httplib WebSocket API is present in the pinned commit; older packaged
versions may not contain it. See its [WebSocket documentation](https://github.com/yhirose/cpp-httplib/blob/v0.64.0/README-websocket.md).
CMake selects Git release tags with shallow clones. Tags can be moved upstream;
these are version pins rather than immutable content hashes. Dependencies are
header-only and do not require a package manager. `FETCHCONTENT_SOURCE_DIR_HTTPLIB` and
`FETCHCONTENT_SOURCE_DIR_JSON` support pre-downloaded sources/offline builds.

## MCP and bounds

The service implements modern [2026-07-28 discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)
and per-request protocol metadata, plus legacy `initialize` for revisions
2024-11-05, 2025-03-26, 2025-06-18, and 2025-11-25. It exposes tools/list,
tools/call, ping, and structured/text results. Notifications receive no reply.
Unknown tools/invalid arguments are protocol errors; browser failures are tool
errors. No sampling, elicitation, subscriptions, or tasks are advertised.

Requests are limited to 1 MiB before parsing, JSON nesting to 64 levels,
WebSocket messages to 16 MiB, and
serialized tool data to 8 MiB (text and structured data duplicate that payload
on the MCP wire up to 1 MiB; larger results omit structured data). Crawl limits
are 100 pages, depth 5, 120 seconds total, 10,000 discovered links, and 4 MiB of
page data. Limits return `truncated` and per-page
errors. Calls execute serially; cancellation notifications do not interrupt a
running call. Each page has a configurable deadline of at most 120 seconds.

The Markdown renderer supports common article elements, not complete HTML to
CommonMark fidelity. Map discovers anchors by crawling; it does not read sitemaps
or use a search provider. Discovered external links are reported but not queued.
Redirects may leave the initial origin; their destination is not expanded.
Final destination URLs are tracked so an already queued direct destination is
skipped after a redirect reaches it. Unknown aliases still count as navigation
attempts, but their already returned destinations are not emitted again.
Browsing arbitrary URLs retains Lightpanda's normal network access.

Lightpanda also has a [native MCP server](https://github.com/lightpanda-io/browser#native-mcp-and-skill).
Users who only need its native tools can run `lightpanda mcp` directly. Firepanda
adds the scoped scrape/map/crawl interface requested here; embedding Lightpanda
as a C++ library is not an established upstream integration used by this project.

## Validation

`tests/protocol.py` checks modern and legacy wire behavior, malformed/oversized
requests, URL/argument validation, notifications, and missing-browser errors.
`tests/integration.py` starts an actual native Lightpanda child and a local HTTP
fixture, testing JavaScript rendering, all formats, deduplication, same-origin
crawl, limits, timeout recovery, browser reuse, private socket permissions,
external-browser ownership, EOF cleanup, and SIGTERM during navigation/post-load waits.
`tests/transport.py` uses adversarial Unix-socket CDP peers to check continuous
pings, fragmented messages, malformed replies, reconnection, and SIGTERM while
reading a fragment stream. Exact and overflowing link caps are tested separately.
`tests/memory.py` repeats 100 browsing cycles, large outputs, and failure recovery;
it runs as part of CTest and can also wrap the regular binary in macOS `leaks`.
CI is configured to run the suite on macOS and Linux, plus a Linux sanitizer job.
A workflow definition does not establish that any remote job has run.

## Current ownership and optimization details

The default managed-browser idle deadline is 60 seconds after completed browser
work; `--idle-timeout 0` disables expiry. Failure, destructor, and idle cleanup use
one resource-release path. External sockets/processes remain externally owned.
The main thread waits with `pselect`, including the idle deadline.

The link cap is defined once in `src/browser.hpp` and passed into extraction.
String and integer option definitions supply both validation and tool schemas.
Frontier strings are owned by a set; queue pointers remain valid across rehash
because keys are never erased. Crawl serialization budgets use the JSON library's
serializer with a bounded counting stream instead of allocating another page string.
Text readiness caches its result until child-list/character-data mutations invalidate
it; selector conditions still poll because state changes need not mutate attributes.
The observer is disconnected on success and timeout, and polling works without it.

Large results transfer ownership through the pipeline. The selected JSON library
can move rvalues through initializer lists as well as assignments; initializer lists
should not be assumed to copy every moved value. Text and structured MCP results
remain duplicated on the wire for client compatibility up to 1 MiB; larger
results omit the structured field and carry the JSON text once.

The CMake project version feeds CLI and MCP identity. Release packaging includes
Firepanda, both C++ dependency license notices, and documentation; Lightpanda is
installed separately.
