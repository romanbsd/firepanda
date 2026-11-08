# Rust Migration Plan — firepanda

**Status (2026-11-08): M0–M8 complete on `rust-rewrite`.** The core binary keeps the
three original tools plus a browser-free `fetch`; `--features search` adds the fifth.
The historical plan below is retained for provenance; `docs/native-design.md` and
the README describe the shipped code.

Module-by-module plan for rewriting firepanda in Rust, with `primp` for browser impersonation
(search), mapped to the existing black-box test suite. The Python tests are the parity contract:
they spawn `$BINARY --lightpanda $LP` over stdio and never see Rust internals, so they run
unchanged against a Rust binary.

## 1. Scope and ground rules

- Source today: `src/main.cpp` (326) + `src/browser.cpp` (805) + `src/tools.cpp` (595) +
  `src/browser.hpp` (75) + `src/json.hpp` (60) + `src/extract.js` (197) = **2,058 lines**.
- Target: **~1.5–2.5k lines Rust** (serde types are verbose; deadline/pool code shrinks).
- Parity contract (historical): the seven ctest suites green against the Rust binary:
  `json_size`, `protocol`, `dependency_updates`, `installer`, `cdp_transport`,
  `native_browser`, `memory_stress`. After M8, `json_size` lives in `cargo test`,
  `dependency_updates` is gone with FetchContent, and the Python suites remain.
- Behavior that must survive verbatim (see §5): BFS ordering under concurrency,
  failure → drain → `restart()`, SIGTERM teardown, lazy idle shutdown, one managed Lightpanda
  child, validation-before-browser-start, closed-stdout fast exit.
- Working method: same `master`, no branches required; each milestone lands only when its
  mapped tests pass. Keep the C++ binary buildable until M7 so the two can be diffed.

## 2. Crate choices

| Concern | Crate | Notes |
|---|---|---|
| Async runtime | `tokio` (rt-multi-thread, io-util, time, process, signal, sync, net) | Replaces TaskPool, SocketDeadline, wakeup pipes, self-pipe SIGTERM |
| MCP JSON-RPC | **hand-rolled** over `tokio::io::stdin/stdout` + `serde_json` | `main.cpp` is only 326 lines; tests pin exact error codes/ordering (legacy negotiation, validation-before-start). `rmcp` 3.9 is an option later — not for parity |
| CDP WebSocket | `tokio-tungstenite` (`client_async`) over `tokio::net::UnixStream` | Handshake frames per RFC 6455; Lightpanda quirks stay in one module |
| JSON | `serde` + `serde_json` | Keep the 1 MiB `max_structured_content_bytes` cap and bounded output helpers as a thin wrapper |
| Browser impersonation (search) | `primp` 2 (`Impersonate::ChromeV146`) | Feature-gated `--features search` so parity milestones don't depend on it |
| Search HTML parsing | `scraper` (CSS selectors) | Translate ddgs XPath → CSS; port `extract.js` stays in-page |
| URL handling | **port `normalize_url`/`parse_authority` 1:1** | Tests pin exact behavior (`/null`, `javascript:`, IPv6 rejection). Do not swap in `url` crate semantics unless it passes the same fixtures |
| Process spawn/temp | `tokio::process` + `tempfile` | Replaces `posix_spawn` + temp-dir logic in `SpawnActions` |
| CLI args | hand-rolled | Preserves exact validation messages (`test_idle_timeout_cli_validation`) |
| Embedded JS | `include_str!("extract.js")` | Replaces CMake `extraction.hpp.in` |
| Lint/test | `clippy -D warnings`, `cargo fmt --check`, `cargo test` | Replaces clang-format + ASan/UBSan/TSan jobs; `loom` only if any custom sync primitive survives (goal: none) |

Dropped from C++ build: FetchContent httplib/nlohmann, sanitizer options, `clang-format` gate,
`scripts/bump_fetchcontent_tags.py` + `tests/dependency_updates.py` (replaced by
`cargo update` policy / Dependabot; `tests/installer.py` and `scripts/package-release.py`
stay — they only touch the binary path/version).

## 3. Module mapping

```
src/main.cpp (326)          →  src/main.rs            — argv, SIGTERM hook, stdio loop
                               src/mcp/mod.rs          — JSON-RPC framing, versions[], error()
                               src/mcp/negotiate.rs    — modern stateless vs legacy handshake
                               src/mcp/dispatch.rs     — tools/list, tools/call, validation order
src/tools.cpp (595)         →  src/tools/mod.rs       — tool_table, OptionDef, schema_of
                               src/tools/args.rs       — parse_integer/string/boolean + bounds
                               src/tools/url.rs        — normalize_url, parse_authority, origin
                               src/tools/crawl.rs      — BFS crawl/map pipeline, formats filter
src/browser.hpp (75)        →  src/browser/mod.rs     — Browser handle: scrape, scrape_async,
                                                          set_parallelism, page_completed,
                                                          restart, idle_deadline, stop_if_idle
src/browser.cpp (805)       →  src/browser/cdp.rs     — Connection, open_page, navigate,
                                                          wait_after_load, extract, run_scrape
                               src/browser/pool.rs     — checkout/connect, N sessions, semaphore
                               src/browser/child.rs    — Lightpanda spawn, ready-wait, restart,
                                                          SIGTERM kill, single-child invariant
                               src/browser/deadline.rs — tokio::time::timeout wrappers (deletes
                                                          SocketDeadline, wakeup pipes, poll())
                               src/browser/tasks.rs    — JoinSet + semaphore (deletes TaskPool,
                                                          packaged_task, condition variables)
src/json.hpp (60)           →  src/json.rs            — bounded serializer helpers, 1 MiB cap
src/extract.js (197)        →  src/extract.js         — unchanged, include_str! (CMake step gone)
(search, new)               →  src/search/mod.rs      — primp client, engine recipes (DDG/Brave),
                                                          dedupe/rank, Firecrawl-shaped response
```

What **disappears** rather than ports: `SocketDeadline` + watcher thread, wakeup/self pipes,
`pselect` idle wait, `TaskPool`/`packaged_task`, `SpawnActions`, the TSan/ASan CMake options.
This is the main simplification win — ~400 lines of the trickiest C++ becomes ~80 lines of
tokio timeouts and `JoinSet`.

## 4. Milestones and test gates

| # | Deliverable | Gate (must pass) |
|---|---|---|
| M0 | Cargo scaffold, CI matrix (fmt/clippy/test), harness points at `target/release/firepanda` | `cargo test` empty suite; ctest wiring smoke |
| M1 | `mcp/*`: stdio loop, versions/negotiation, dispatch, validation-before-start, closed-stdout exit | **protocol.py** — all 11: version↔identity, idle-timeout CLI, modern/legacy, invalid-request recovery, validation-before-browser, depth/large-URL, uint boundaries, readiness, IPv6, closed stdout, idle SIGTERM + pipelined input |
| M2 | `browser/cdp.rs` + `child.rs`: UDS WS, frames, status/fragment deadlines, spawn/ready | **cdp_transport.py** — 4 tests: malformed reply recovery, status main-document-only, control/fragment deadlines, SIGTERM during fragment stream |
| M3 | `browser/pool.rs`, `run_scrape`, `extract.js`, formats/warnings pipeline | **integration.py** core: formats, JS render, challenge/error/empty warnings, anchors, ready state |
| M4 | `tools/crawl.rs`: BFS crawl/map, concurrency, ordering | **integration.py** crawl/map section + **scripts/benchmark.py** concurrency sweep (ratios at conc 1/2/4/8) |
| M5 | lifecycle: failure→drain→restart, lazy idle shutdown, SIGTERM teardown, descriptor hygiene | **memory.py** (100 iterations, `--leaks` path becomes descriptor/RSS check) + protocol idle/sigterm tests re-run |
| M6 | `json.rs` bounded helpers + cap | **json_size** ported as `cargo test` |
| M7 | `search/*` behind `--features search`: primp DDG/Brave, `search` tool (4th tool) | new fixture tests (offline HTML) + `tests/memory.py` tool-count 3→4 update |
| M8 | packaging/release swap: delete `src/*.cpp`, CMake → thin wrapper or `just`/Makefile, docs (README, native-design, releasing), CI drops sanitizer jobs | full suite green on release artifacts for macOS arm64 + Linux |
M0–M5 are parity; M6–M7 are additive; M8 is deletion. Estimate: **8–15 working days**
(M1+M2 are the long poles; M3–M5 mostly re-derive encoded quirks).

## 5. Behavior-sensitive areas (parity traps)

1. **BFS ordering under concurrency** — crawl pages must appear in the same order the C++
   pipeline emits them under `set_parallelism(N)`. Benchmark sweep (558/288/157/105 ms at
   1/2/4/8) is the performance contract; integration crawl asserts content/order.
2. **Failure → drain → `restart()`** — a CDP failure drains in-flight tasks before killing and
   respawning Lightpanda; tasks must not observe a half-dead connection. In Rust: `JoinSet`
   drain-then-abort ordering, explicit state machine in `child.rs`.
3. **SIGTERM teardown** — kill child, unblock readers, exit promptly even mid-fragment
   (`test_sigterm_during_fragment_stream`) and with pipelined stdin
   (`test_idle_sigterm_and_pipelined_input`).
4. **Lazy idle shutdown** — `idle_deadline()`/`stop_if_idle()` semantics: browser starts on
   first real work, shuts down after `--idle-timeout` of quiet. tokio `time::sleep` reset per
   request; must not start Lightpanda during validation errors.
5. **Single managed child** — `children()` == 1 under stress; restart replaces, never leaks a
   second process. Descriptor counts flat over 100 iterations.
6. **Validation before browser start** — bad args/URLs return JSON-RPC errors with zero
   process spawn (asserted by not-yet-started browser in protocol tests).
7. **Closed stdout** — write error on stdout must exit without waiting on stdin EOF.
8. **Error taxonomy** — warnings codes (`suspected_block`, `suspected_error_page`,
   `http_error`, `empty_content`), `isError` vs warnings, 1 MiB structured-content cap.

## 6. Risks

| Risk | Mitigation |
|---|---|
| primp young (v2, first release Apr 2026), BoringSSL blob → +8–20 MB binary, longer CI | feature-gate `search`; pin exact version; keep Brave-in-Lightpanda as documented fallback |
| Lightpanda/CDP quirks live in C++ comments + tests | port comments along; M2 gate is the transport suite, M3 the render suite — don't batch |
| Ordering regressions appear only under load | run benchmark sweep at each of M4/M5 on an idle machine; keep ratio assertions |
| Legacy MCP negotiation subtlety | M1 first, protocol.py is the earliest gate; hand-rolled loop, no SDK |
| Two binaries drift during migration | single test matrix runs both binaries until M8; diff outputs on fixtures |
| macOS signal/pipe semantics differ from tokio assumptions | transport.py SIGTERM tests run in CI on macOS from M2 onward |

## 7. Decision point before M0

Recommendation stands: **only rewrite if the rewrite is the goal** (async simplicity, smaller
concurrency surface, primp-native search). If search alone is the driver, the
Brave-in-Lightpanda C++ path (~300 lines) or a Rust helper binary gets there cheaper and puts
nothing at risk. This plan assumes the full rewrite is chosen; M0 starts only on explicit go.
