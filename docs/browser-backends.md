# Pluggable browser backends: Lightpanda and Obscura

Status: implemented 2026-11-10 (see implementation notes at the end)
Date: 2026-11-10

## 1. Verdict

Obscura is technically viable as a second CDP backend for firepanda, and the
work is contained: firepanda already speaks the exact CDP subset Obscura
implements. The real cost is not the adapter, it is proving extraction parity
(the two engines have different DOM/JS implementations) and accepting a
transport downgrade from a private Unix socket to loopback TCP plus a bearer
token.

Recommendation: add an explicit `--backend lightpanda|obscura` switch, keep
Lightpanda as the default until a parity suite passes against a frozen fixture
corpus, then allow `auto`. Do not embed the `obscura` crate and do not replace
Lightpanda.

## 2. What Obscura is

Verified from the repository at `main` (2026-11-10) and the v0.2.6 release
(2026-10-31):

- Headless browser engine in Rust with V8 for JavaScript, its own DOM
  (html5ever), network stack, and optional render layer (screenshots,
  screencast, PDF). CSS layout covers block/inline/flex/grid/table/float, etc.,
  but is explicitly not Chromium-equivalent.
- License: Apache-2.0. Distribution: standalone binaries for macOS/Linux
  (aarch64/x86_64) in four variants (render/stealth), a Distroless Docker image,
  and crates published as a workspace. Binary size ~70 MiB.
- Maturity: 31.9k stars, 2.4k forks, 211 open issues, first commit 2026-04-13,
  commits landing the same day this was evaluated. Young but extremely active.
- Ships its own `scrape`/`fetch` CLI and MCP server. Those are alternatives to
  firepanda, not integration points (see §10).

## 3. What Obscura exposes to a CDP client

Launch:

```sh
obscura serve --host 127.0.0.1 --port 0 --ready-file /private/dir/cdp.json \
              --workers 1 --max-connections 128 [--stealth] [--obey-robots]
```

- `--host` defaults to `127.0.0.1`; `--port 0` is allowed.
- `--ready-file PATH` is written atomically (0600) after V8 initialization and
  the listener bind. Content is JSON:
  `{"pid":…,"host":"127.0.0.1","port":…,"websocket_path":"/devtools/browser"}`.
  `--ready-file` requires `--workers 1`.
- Endpoint: `ws://host:port/devtools/browser`; discovery at `GET /json/version`
  (returns `webSocketDebuggerUrl`).
- `OBSCURA_CDP_TOKEN` (>= 32 bytes) makes `Authorization: Bearer <token>`
  mandatory on every control/WS request.
- `--max-connections` (default 128 per worker) refuses excess connections with
  HTTP 503 rather than queueing.
- Other flags relevant to firepanda: `--stealth`, `--obey-robots`,
  `--user-agent`, `--proxy`, `--allow-private-network`, `--allow-file-access`,
  `--storage-dir`, `--workers N`, worker recycle options.
- Environment: `OBSCURA_SCRIPT_DEADLINE_MS` (default 30000) caps page script
  execution during navigation; `OBSCURA_MODULE_BUDGET_MS` (default 3000) caps
  enhancement modules; `OBSCURA_FETCH_TIMEOUT_MS`, `OBSCURA_NETWORK_BODY_BUFFER_BYTES`
  (default 2 MiB), `OBSCURA_ALLOW_PRIVATE_NETWORK`.

CDP surface implemented (from `crates/obscura-cdp/src/domains/*`):

| Domain | Relevant methods |
| --- | --- |
| Target | createTarget, closeTarget, attachToTarget (flatten), createBrowserContext, disposeBrowserContext, getBrowserContexts |
| Page | enable, navigate, getFrameTree, lifecycleEvent, loadEventFired, captureScreenshot, printToPDF |
| Runtime | evaluate (expression, returnByValue, awaitPromise, timeout), callFunctionOn |
| Network | enable, responseReceived (including `type: "Document"` and integer `status`) |
| DOM / Fetch / IO / Storage / Input | present (not needed by firepanda today) |

`Target.createBrowserContext` creates a real `BrowserContext` with its own
storage/network state; `Target.createTarget` honours `browserContextId`. This
matches firepanda's per-page isolation model.

## 4. Fit against firepanda today

Firepanda (`src/browser/mod.rs`, `src/browser/cdp.rs`) launches
`lightpanda serve --unix-socket <dir>/cdp.sock --log-level error`, connects a
CDP WebSocket to `/` over `UnixStream`, and per page calls:

`Target.createBrowserContext` → `Target.createTarget` → `Target.attachToTarget`
→ `Page.enable` → `Network.enable` → `Page.navigate` → wait for load →
`Runtime.evaluate(extract.js)` → `Target.disposeBrowserContext`.

Every one of those is implemented by Obscura with compatible result shapes
(`Page.navigate` returns `{frameId, loaderId}`, plus `errorText` only when the
navigation failed; `Runtime.evaluate` returns `{result:{value}}`).

Three verified differences need shims:

1. **Events precede the command reply.** Obscura deliberately flushes queued
   events before the command response, following Chromium semantics
   (`crates/obscura-cdp/src/server.rs`, "events emitted as a side-effect of a
   command … MUST arrive BEFORE the command's response"). `Page.navigate`
   therefore delivers `Page.lifecycleEvent … "load"` and
   `Page.loadEventFired` before its reply. Firepanda currently sets
   `page_loaded = false`, sends `Page.navigate`, and only then knows
   `main_frame`; `observe()` ignores load events while `main_frame` is empty
   (`src/browser/cdp.rs:161-176`). Against Obscura this would consume the whole
   page deadline and fail. Fix: buffer early load events exactly like the
   existing `early_document_statuses` mechanism, matching on `frameId` after
   the navigate reply. Buffer only `Page.lifecycleEvent{name:"load"}` with a
   frame id; an early bare `Page.loadEventFired` is ambiguous (the about:blank
   load) and must stay ignored.
2. **Evaluation budget.** Firepanda allows page timeouts up to 120 s
   (`src/tools.rs`, `timeout` max `120_000`) and does not send a CDP `timeout`.
   Obscura defaults `Runtime.evaluate` to 30 s and the in-page script phase to
   `OBSCURA_SCRIPT_DEADLINE_MS=30000`. Fix: send
   `"timeout": remaining_ms` in the evaluate params, and set
   `OBSCURA_SCRIPT_DEADLINE_MS` to firepanda's maximum page timeout (120 s) on
   the child.
3. **Transport.** Obscura has no Unix-socket listener (`grep` across the
   workspace finds none). Firepanda's 0700 socket directory / 0600 socket
   becomes loopback TCP + ephemeral port + bearer token. Same-user processes
   are still blocked by the token; the token lives in the child environment.

Also note:

- Obscura blocks loopback/RFC1918/link-local by default (SSRF guard). Scraping
  local fixtures or a localhost dev server needs `--allow-private-network`.
  Lightpanda has no such guard, so parity means enabling it by default (with a
  firepanda flag to turn it off) — or firepanda's local-scrape behaviour
  silently regresses.
- `disposeOnDetach` is not implemented (the param is ignored). Firepanda
  disposes contexts explicitly and drops per-connection state on disconnect, so
  this is acceptable, but a raw abrupt disconnect can leave the context alive
  until the WebSocket closes.
- `--ready-file` only works with `--workers 1`. Firepanda's own connection pool
  already gives concurrency (one connection per in-flight page), and each
  Obscura connection gets its own OS thread + V8 isolates, so a single worker is
  the right default. `--workers N` would be a separate optimisation, requiring
  readiness probing via `/json/version` instead of the ready file.
- `--workers 1` runs in-process, so firepanda's existing SIGTERM → SIGKILL →
  reap lifecycle and single-PID ownership still hold.
- DOM/JS fidelity is the main unknown: Obscura's DOM bindings are not
  Chromium's, so `extract.js` (visibility checks, computed styles,
  MutationObserver, TreeWalker, etc.) must be asserted on real fixtures, not
  assumed.

## 5. Architecture

Two seams, both small.

### 5.1 Endpoint instead of a hardcoded Unix socket

`src/browser/mod.rs:17` and `cdp.rs:76,106` hardcode
`WebSocketStream<UnixStream>`.

```rust
pub enum Endpoint {
    Unix { path: String },
    Tcp { host: String, port: u16, token: Option<String> },
}
```

- Unix → `ws://localhost/` (unchanged).
- TCP → `ws://{host}:{port}/devtools/browser`.
- Authorization: build the handshake request instead of passing a `&str`:
  `IntoClientRequest::into_client_request(url)`, then insert
  `Authorization: Bearer <token>`; pass that to
  `client_async_with_config(request, stream, Some(config))` (tungstenite's
  re-exported `http` types, no new dependency).
- Stream type: replace `WebSocketStream<UnixStream>` with
  `WebSocketStream<Box<dyn AsyncReadWrite>>` (blanket impl for
  `AsyncRead + AsyncWrite + Unpin + Send`). Tokio implements both traits for
  `Box<T>`, so `connect` boxes either `UnixStream` or `TcpStream`. One
  allocation per connection is irrelevant next to a V8 process.
- Keep the 16 MiB message/frame/write caps and deadline-bounded reads unchanged.

### 5.2 Backend adapter for launch, readiness, and labels

```rust
pub enum Backend { Lightpanda, Obscura }

pub struct Options {
    pub backend: Backend,
    pub program: String,   // renamed from `lightpanda`
    pub socket: String,    // external Lightpanda / explicit override
    pub cdp_url: String,   // external endpoint (both engines, future)
    pub idle_timeout: Duration,
    // obscura-only knobs
    pub stealth: bool,
    pub obey_robots: bool,
    pub allow_private_network: bool,
}
```

`Process` gains `endpoint: Endpoint` and a per-launch scratch directory (the
existing `make_socket_directory()` already gives a 0700 dir under `/tmp`;
reuse it for the ready file and owner record).

Lightpanda launch stays byte-for-byte identical:

```
lightpanda serve --unix-socket <dir>/cdp.sock --log-level error
```

Obscura launch:

```
obscura serve --host 127.0.0.1 --port 0 --ready-file <dir>/cdp.json \
              --workers 1 --max-connections <pool bound> \
              [--stealth] [--obey-robots] [--allow-private-network]
env: OBSCURA_CDP_TOKEN=<hex 32 bytes>
     OBSCURA_SCRIPT_DEADLINE_MS=120000
```

Readiness differs; replace the current `attempt()` retry loop with a small
per-backend `wait_ready`:

- Lightpanda: `UnixStream::connect` with the existing 10 s window.
- Obscura: poll the ready file until the connect deadline, parse JSON, then
  connect to `host:port`. (Do not guess a port: `--port 0` plus `--ready-file`
  is race-free.)

Token generation: read 32 bytes from `/dev/urandom` (Unix-only project; no new
dependency) and hex-encode. The ready file is 0600 inside the 0700 directory.

### 5.3 Lifecycle, idle shutdown, orphan sweep

- Idle shutdown, SIGTERM/SIGINT handling, and `kill_on_drop(false)` are
  backend-agnostic and stay as they are.
- `sweep_orphans()` currently proves a stale PID by connecting to
  `cdp.sock`. Generalize the owner record from `"<pid> <pid>"` text to JSON:
  `{backend, program, pid, endpoint, token}` (0600 in the 0700 dir).
  For TCP, probe `GET /json/version` with the bearer token and only signal the
  PID when the response identifies Obscura and the recorded PID is alive.
  Keep the same rule: never signal a process this firepanda did not spawn.

### 5.4 Backend-neutral error strings

`cdp.rs` classifies fatal errors by matching literals such as
`"Lightpanda exited"` and `"Cannot write Lightpanda CDP command"`. Inject the
backend label (`BROWSER_NAME`) into those messages, or better, replace the
prefix heuristic with an explicit `Transport` vs `Peer` classification while
touching the file. User-visible text becomes "browser" plus the backend name
where useful. `tests/transport.py` pins some of these strings; update it in the
same change.

## 6. CLI contract

```
firepanda [--backend lightpanda|obscura|auto]
          [--browser /path/to/engine]        (new, generic)
          [--lightpanda /path]               (kept: alias for backend=lightpanda)
          [--socket /absolute/cdp.sock]      (kept: external Lightpanda)
          [--cdp-url ws://host:port/...]     (new: external endpoint, any engine)
          [--idle-timeout SECONDS]           (unchanged, 0..=86400)
          [--stealth] [--obey-robots]        (obscura only; exit 2 otherwise)
          [--no-private-network]             (obscura SSRF guard opt-in)
```

Rules, matching the existing strict CLI style (exit code 2 on validation):

- Default stays `--backend lightpanda`; no behaviour change for existing users.
- `--lightpanda` and `--browser` are mutually exclusive; `--socket` and
  `--cdp-url` are the external-endpoint forms and are mutually exclusive with
  the managed forms.
- `--stealth`/`--obey-robots` with `lightpanda` is an error rather than a
  silently ignored flag.
- `auto`: use `--browser` if given (detect by basename), else the first of
  `lightpanda`, `obscura` on `PATH`; log the decision to stderr. Opt-in only,
  because a silent engine switch changes extraction behaviour.
- Help text mentions both engines and their installers.

Environment equivalents are optional; if added, use `FIREPANDA_BACKEND` and
`FIREPANDA_BROWSER` only.

## 7. Concrete change list

| File | Change |
| --- | --- |
| `src/browser/mod.rs` | `Backend`, `Endpoint`, `Options` fields; per-backend `launch`; `wait_ready`; JSON owner record; endpoint-aware sweep |
| `src/browser/cdp.rs` | Boxed stream type; handshake request with bearer token; backend label in errors; `timeout` in `Runtime.evaluate` params; buffer early load lifecycle events (mirror `early_document_statuses`) |
| `src/cli.rs` | New flags, validation, help text, exit codes |
| `src/mcp.rs` | `Options` construction (~line 214) picks up the new fields |
| `src/tools.rs` | Any user-facing "Lightpanda" text in errors |
| `tests/harness.py` / `tests/integration.py` | Parameterize the backend; pass `--allow-private-network` for local fixtures; keep `--lightpanda` cases |
| `tests/transport.py` | Keep the adversarial UDS fake peer; the TCP/bearer path is covered end-to-end by `tests/integration.py` against a real Obscura |
| `Makefile` | `gates` (default Lightpanda) + `gates-obscura` + combined `gates-all` |
| `scripts/install-obscura.py` | Mirror `install-lightpanda.py`: pin a tested tag, verify SHA-256, write `obscura.json`, choose the platform archive |
| `docs/native-design.md`, `README.md`, `docs/releasing.md`, `docs/benchmarks.md` | Document the backend, transport, and per-backend benchmarks |
| `docs/firecrawl-parity.md` | Note which engine was used for parity results |

Rough size: ~150–250 lines of Rust changed, one new transport test mode, one
installer script.

## 8. Test plan

1. **Transport/Auth:** fake TCP CDP peer; assert `/devtools/browser` path,
   Bearer header required when a token is configured, 503 handling for
   `max-connections`, and clean reconnect after a peer close.
2. **Lifecycle:** assert the early-event buffering works (navigate reply after
   `Page.lifecycleEvent{load}`), including the about:blank false-positive case.
3. **Extraction parity (the gate):** a fixture corpus covering static HTML,
   JS-rendered content, `waitForSelector`, `waitForText`, links, raw HTML,
   markdown, main-content heuristics, and HTTP status reporting. Run the same
   assertions against both engines; a format that differs is a bug to fix or a
   documented capability gap.
4. **Isolation:** two sequential pages with cookies/localStorage; assert the
   second sees no state from the first (fresh browser context).
5. **Lifecycle/ops:** lazy start, idle shutdown, SIGTERM cleanup, orphan sweep
   after a killed parent, external `--socket` / `--cdp-url` never killed.
6. **Benchmarks:** cold start, per-page latency, crawl throughput, RSS for both
   engines on identical fixtures; record the pinned Obscura version and SHA.

## 9. Rollout

| Phase | Work | Exit criterion |
| --- | --- | --- |
| 0 (spike) | Endpoint + boxed stream only; run `obscura serve` manually, drive one scrape through `--cdp-url` | one fixture scrapes end to end |
| 1 | Backend adapter, CLI, lifecycle, error labels; default Lightpanda | existing suites green, `--backend obscura` starts/cleans up correctly |
| 2 | Parity suite + fixes (early load buffer, evaluate timeout, private network); installer + docs | parity suite green on a pinned Obscura tag |
| 3 | `auto`, optional CI matrix, benchmark table | decision on making Obscura the default |

Estimated effort: 1–2 days for phase 0–1, 2–4 days for parity (the unknown),
0.5–1 day for packaging/docs. About 4–7 developer days total.

## 10. Alternatives considered

- **Embed the `obscura` crate.** Rejected: pulls V8 and a large workspace into
  firepanda's binary (which is deliberately small), requires linking, and
  couples to a pre-1.0 Rust API. Subprocess + CDP preserves the process
  boundary, crash isolation, and the existing lifecycle.
- **Shell out to `obscura scrape`/`fetch`.** Rejected: firepanda needs a
  persistent browser, per-page contexts, format selection (`links`, `rawHtml`,
  markdown options, caps), and HTTP status reporting. Obscura's CLI JSON is not
  firepanda's output contract.
- **Use Obscura's MCP server and proxy it.** Rejected: would add a second MCP
  layer and lose firepanda's tool schemas, budgets, crawl BFS, and result caps.
- **Replace Lightpanda outright.** Rejected for now: Obscura is v0.2.x with an
  independent DOM/JS stack; parity is unproven. Keeping both engines lets users
  choose fidelity vs. footprint and gives a fallback.

## 11. Open questions to settle during the spike

1. Does `extract.js` produce byte-identical output on both engines for the
   fixture corpus? Which formats diverge?
2. Cold-start time and RSS of `obscura serve` versus Lightpanda, with lazy
   spawn and 60 s idle shutdown.
3. Does `Network.responseReceived` include the main document status early
   enough for firepanda's existing early-status buffer on redirects and errors?
4. Does Obscura's `Page.navigate` reject/represent DNS/TLS failures the same
   way (`errorText` in a success result vs a CDP error)?
5. Is `--allow-private-network` acceptable as the default for parity, or should
   firepanda expose an SSRF policy flag and document the difference?

## 12. Implementation notes (2026-11-10)

Implemented: `Backend`/`Endpoint` in `src/browser/mod.rs`, boxed CDP stream and
bearer handshake, per-backend launch and readiness (Unix socket vs ready file),
bearer-token generation, JSON owner records and endpoint-checked orphan sweep,
`--backend`/`--browser`/`--cdp-url` and Obscura flags in `src/cli.rs`, backend
labels in errors, early-load buffering, and the per-evaluation timeout.

The integration suite found three engine differences that were fixed in shared
code rather than hidden in the Obscura adapter:

1. **Iframe documents share the page frame id.** Obscura emits
   `Network.responseReceived` for iframe documents with the page's `frameId`,
   which made a 503 subframe overwrite the main 200. The pipeline now accepts a
   `Document` event only when `requestId == loaderId` (the navigation loader),
   which both engines satisfy for the main document.
2. **Non-HTML rendering differs.** Lightpanda wraps JSON/plain text in `<pre>`;
   Obscura leaves a bare text node, so Markdown lost its code fence. `extract.js`
   now normalizes a non-HTML `document.contentType` into a single fenced block.
3. **Inline hidden styles differ.** Obscura did not expose
   `visibility: hidden !important` through `CSSStyleDeclaration`, so a hidden
   `<main>` displaced visible content under `onlyMainContent`. The extractor
   reads `style` attributes directly instead of relying on the style object.

Verified against Obscura v0.2.6 (aarch64 macOS release archive) and Lightpanda
1.1.0-nightly.11647: `tests/integration.py`, `tests/map_sitemap.py`,
`tests/protocol.py`, `tests/transport.py`, `tests/installer.py`, and the
repeated-browsing memory suite. Remaining unknowns are long-tail DOM/JS coverage
on arbitrary sites and cold-start/RSS numbers; run `make gates-all` when changing
either engine.
