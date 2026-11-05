# Native C++ review and memory validation

This is a historical validation record. See the current README and CI for the
latest test groups; historical binary sizes and counts below describe earlier builds.

Validated locally on 2026-11-04, macOS arm64. The regular Release build uses
AppleClang 21; the sanitizer Debug build uses Homebrew Clang 23.1.4.

## Simplifications and allocation reductions

- Tool arguments are validated once into a typed `PageRequest`, reused by crawl.
  Integer defaults/bounds and format names feed both schemas and validation.
- Tool schemas are constructed once. Crawl no longer builds an unused map of
  every discovered link. Frontier membership uses hash sets.
- CDP retains only the active session's page-load flag, replacing a queue of
  up to 256 arbitrary JSON event bodies and repeated queue scans.
- Large JSON results move through the browser/tool/MCP layers instead of being
  unnecessarily copied between layers. The pinned JSON library also moves
  rvalues through initializer lists; explicit assignments are not required to avoid copies. Structured and text MCP outputs still
  duplicate the payload on the wire for client compatibility.
- URL normalization uses bounded scans and `from_chars`, removing recursive
  regex matching. JSON nesting is limited to 64 on both MCP and CDP input.
- Link-only and raw-HTML-only requests skip cloning the DOM. Link extraction
  stops after 10,000 distinct HTTP(S) links instead of allocating a record for
  every anchor and truncating afterward.
- Spawn actions use RAII; `waitpid` retries `EINTR` so a signal cannot abandon
  the owned child. Post-load waits check shutdown promptly.
- clang-format 23.1.4 formatted all C++ implementation/header files. The checked-in
  `.clang-format`, `format`, and `format-check` targets keep formatting repeatable.

The Release executable decreased from approximately 543 KiB to 477 KiB on the
first pass and is approximately 483 KiB after the second-pass deadline enforcement.
These are code/allocation reductions; no end-to-end throughput comparison was run.

## Results

| Check | Result |
| --- | --- |
| Normal CTest suite | 4/4 passed: protocol, CDP transport, native browser, memory stress |
| AddressSanitizer + UndefinedBehaviorSanitizer + LeakSanitizer | Same 4/4 passed; no reported memory errors, undefined behavior, or leaks |
| ThreadSanitizer | All four groups passed without race reports; native browser was rerun with a larger page budget for the 10,000-link boundary workload |
| macOS `leaks --atExit` on the regular binary | 0 leaks for 0 total leaked bytes |
| Descriptor accounting | 4 open descriptors after warm-up and 4 after stress/timeout recovery in normal, address-sanitized, and thread-sanitized runs |
| Clang static analyzer | No diagnostics for main.cpp, browser.cpp, tools.cpp |
| clang-format dry run and git diff whitespace check | Passed |

The stress workload runs 100 browsing cycles, repeatedly lists schemas, mixes
scrape/crawl/map, handles 256 KiB pages, rejects a 9 MiB result, times out a slow
navigation, and verifies recovery. Other regressions cover 5,000-level JSON input,
long URLs, signed/unsigned numeric boundaries, malformed/oversized requests,
private socket permissions, child reuse, external-browser ownership, EOF,
and SIGTERM during both navigation and a 30-second post-load wait.

The second pass reproduced and fixed invalid IPv6 acceptance, exact-cap false
truncation, silent overflowing-link truncation, and waiting indefinitely for
stdin after the stdout reader disconnects. IPv6 literals now use the system
parser and canonical spelling for same-origin comparisons. Leaf crawl pages
skip automatic link extraction when links are not requested.

Adversarial CDP peers cover malformed extraction payloads, missing/conflicting
reply envelopes, excessive JSON nesting, continuous WebSocket pings, and fragment
streams. A scoped deadline worker enforces total operation budgets even while a
blocking library read stays active. It owns a duplicate descriptor, joins on all
exit paths, and uses a lock-free shutdown flag shared with the signal handler.
The resource stress test checks descriptor counts as well as heap diagnostics.

The 10,000-link boundary fixture exceeded its original 30-second page budget
under ThreadSanitizer. That test now explicitly allows 120 seconds per page;
the rerun passed in approximately 63 seconds. The separate 150 ms adversarial
deadline regressions retain their short budgets.

Retained local evidence (under the ignored build directories):

- `build/evidence/macos-leaks.txt`: independent exit-time leak report.
- `build/evidence/sanitizers.txt`: final sanitizer CTest transcript.
- `build/evidence/normal-pass2.txt`: normal four-group CTest transcript.
- `build/evidence/threadsanitizer-initial-pass2.txt` and
  `build/evidence/threadsanitizer-rerun-pass2.txt`: race-check results including
  the original fixture-budget failure and successful focused rerun.
- `build/evidence/analyzer-{main,browser,tools}.txt`: static analyzer diagnostics
  (empty on success).

Sanitizers used `ASAN_OPTIONS=detect_leaks=1:halt_on_error=1` and
`UBSAN_OPTIONS=halt_on_error=1:print_stacktrace=1`, without suppressions.
The separate thread build uses Clang 23.1.4, Debug configuration,
`-fsanitize=thread` in both `CMAKE_CXX_FLAGS` and `CMAKE_EXE_LINKER_FLAGS`, and
`TSAN_OPTIONS=halt_on_error=1`. Address and thread sanitizers are separate builds.
See the [Clang sanitizer documentation](https://clang.llvm.org/docs/AddressSanitizer.html)
and [LeakSanitizer documentation](https://clang.llvm.org/docs/LeakSanitizer.html).
The macOS leak report notes that allocation contents are restricted for the
non-debuggable target; it still reports its allocation count and leak result.

## Scope

This is evidence that the exercised workloads are clean, not a proof for every
possible input or allocation failure. Sanitizers instrument Firepanda and its
header-only C++ dependencies. The prebuilt Lightpanda child is not instrumented;
the tests verify its context/process cleanup but do not establish the memory
safety of Lightpanda's implementation. Linux sanitizer CI is configured but has
not been executed from this local session.

Reproduction commands are in [README.md](../README.md#formatting-and-memory-validation).

## Third pass

The third pass reproduced incorrect Markdown base-URL resolution, changes to
literal backticks/blank lines in code, and duplicate crawl results for a redirect
plus an already queued direct destination. Markdown now resolves relative links
against the document base URI and chooses delimiters longer than the code's own
backtick runs. Prose whitespace is compacted outside fenced code only.

Crawl/map track final destinations, skip queued destinations already visited,
and avoid falsely reporting truncation when only known destinations remain at
the page limit. This is exercised at both an ordinary limit and the exact cap.

Source inspection also identified a signal delivery window between checking the
shutdown flag and a blocking stdin read. `pselect` now atomically transitions the
signal mask while waiting; it adds no periodic idle wakeups or descriptors.
Regressions cover repeated idle SIGTERM and 100 pipelined requests.

Final third-pass results:

- Normal suite: 4/4 groups passed.
- Address/undefined/leak sanitizers: 4/4 groups passed without reports.
- Focused ThreadSanitizer run: protocol, adversarial CDP, and updated resource
  stress groups all passed without race reports.
- Updated 100-cycle stress includes repeated redirect deduplication and code
  extraction. Descriptor counts remained 4/4; macOS `leaks` reported zero leaks.
- Clang static analysis of the changed C++ units and clang-format checks passed.

One closed-reader check timed out once in the initial normal run. It passed
unchanged on rerun, in 50 additional concurrent checks, and in the final normal
and sanitizer suites. The cause was not established; the timeout was not
relaxed. The test now captures an owned-child stack on macOS if it recurs.

Third-pass evidence is retained as `build/evidence/normal-pass3.txt`,
`sanitizers-pass3.txt`, `threadsanitizer-pass3.txt`, and `macos-leaks-pass3.txt`,
plus `analyzer-main-pass3.txt` and `analyzer-tools-pass3.txt` (empty diagnostics).
The separate prebuilt Lightpanda binary remains outside sanitizer coverage.

## OCR finding validation

The initial committed-range OCR pass identified installer, CI, and test-harness
issues. The resulting fix batch adds explicit read-only CI token permissions,
job/concurrency bounds, a stable `1.0.2` browser pin, root-anchored build ignores,
non-colliding installer sidecars, release-tag validation, complete-response
assertions, and client wait budgets that cover the full crawl deadline.

Stable Lightpanda `1.0.2` was downloaded through the SHA-256-checking installer;
its help includes Unix sockets and the macOS integration suite passed. All five
normal test groups (including the new offline installer tests) passed against
that release. This does not establish Linux CI success or complete OCR coverage.
No C++ runtime code changed in this fix batch, so the earlier sanitizer evidence
is retained separately from these installer/CI/harness checks.

## November 5 validation

Subsequent normal and ASan/UBSan/LeakSanitizer runs passed six groups: protocol,
installer, CDP transport, native browser, JSON size boundaries, and memory stress.
Stable Lightpanda 1.0.2 and the locally installed nightly both passed browser
integration, including idle expiry, failure cleanup, external ownership, readiness
cache invalidation, body replacement, and fallback when MutationObserver is absent.
The release-preparation change adds an offline dependency-updater test group.
No remote Linux or GitHub Actions result is implied by these local macOS results.

The release-preparation build using cpp-httplib v0.64.0 and nlohmann/json v3.12.0
passed all seven current groups normally and under ASan/UBSan/LeakSanitizer on
macOS arm64 with stable Lightpanda 1.0.2. Workflow YAML passed actionlint locally;
no remote workflow result is claimed.
