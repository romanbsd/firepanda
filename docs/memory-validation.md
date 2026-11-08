# Validation notes

This page records what the current Rust implementation is exercised by. It is not
a proof for every input; the prebuilt Lightpanda child is not instrumented, and the
tests verify its context/process cleanup rather than its internal memory safety.

## Local and CI gates

| Check | Command |
| --- | --- |
| Formatting | `cargo fmt --manifest-path rust/Cargo.toml --check` |
| Lints (both feature sets) | `make check` |
| Unit tests (both feature sets) | `make test` |
| Protocol/CLI contracts | `make gates` → `tests/protocol.py` |
| Adversarial CDP transport | `make gates` → `tests/transport.py` |
| Native browser integration | `make gates` → `tests/integration.py` |
| 100-cycle memory/descriptor stress | `make gates` → `tests/memory.py` |
| Offline search fixtures | `make search-gates` → `tests/search.py` |
| Release packaging | `scripts/package-release.py` on the built binary |

The Python suites spawn the binary over stdio and never see Rust internals, so the
same assertions that gated the C++ build run unchanged.

## Memory and lifecycle evidence

`tests/memory.py` runs 100 browsing cycles that repeatedly list tool schemas, mix
`scrape`/`crawl`/`map`, handle 256 KiB pages and large link sets, reject oversized
results, time out a slow navigation, and verify recovery. On macOS arm64 with the
release build and stable Lightpanda 1.0.2, the suite reports a flat descriptor
count after warm-up and final recovery (10/10) and a clean exit code. The suite
also asserts no sanitizer diagnostics when the binary is run under them.

Other regressions cover malformed and oversized MCP requests, 64-level JSON nesting
on both input paths, long URLs, signed/unsigned numeric boundaries, private socket
permissions, child reuse, external-browser ownership, EOF cleanup, and SIGTERM
during navigation and a post-load wait.

The failure path keeps results from siblings: when one page of a crawl fails, the
remaining in-flight pages are drained, their successful results recorded, and the
browser is reaped and relaunched before dispatch continues. `tests/integration.py`
exercises this with a flaky fixture.

## Historical C++ validation

The earlier C++ implementation was validated locally with ASan/UBSan/LeakSanitizer
and ThreadSanitizer on macOS arm64, with zero reported leaks and flat descriptor
counts. Those configurations were removed with the C++ sources. Their findings
drove behavior that the Rust port preserves (deadline-bounded reads, bounded JSON,
failure→drain→restart, external-browser ownership), and the corresponding black-box
tests remain the contract. Historical transcripts lived in ignored build
directories and are not part of this repository.
