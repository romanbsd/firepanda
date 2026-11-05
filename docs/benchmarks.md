# Benchmark methodology

Benchmarks are local synthetic measurements, recorded on macOS arm64 on
2026-11-05. They measure specific costs, not general superiority over other tools.
Public website latency, JavaScript compatibility, blocked pages, and server load
can dominate real scraping results. No UDS-versus-TCP comparison was performed.

## Service wall time

From a Release source build with Python 3.11+ and a native Lightpanda binary:

```sh
python3 scripts/benchmark.py --binary build/firepanda \
  --lightpanda .local/bin/lightpanda --iterations 20 --output build/benchmark.json
```

The benchmark runs a local threaded HTTP fixture and one Firepanda stdio process,
with browser idle expiry disabled. It measures request write through complete
MCP response parse using `perf_counter`, so timings include browser/CDP work,
local HTTP, extraction, and serialization of both text and structured results.
There is one untimed warm-up per warm scenario. It reports min/median/max and a
separate first-call time including lazy browser launch. No external website is used.
The report includes executable size, version, architecture, and Lightpanda SHA-256.

| Scenario | Fixture |
| --- | --- |
| Small page | One heading and paragraph |
| Large content | 256 KiB paragraph, Markdown projection |
| Article | 3000 paragraphs containing bold and italic elements |
| Link discovery | 10000 unique local links |
| JavaScript readiness | Text replaced after 300 ms; wait for `Content ready` |

Run on an otherwise idle machine, retain the compiler/build configuration, and
compare the same fixtures and Lightpanda version across binaries. Earlier service
A/B runs varied between runs, so the README makes no whole-service speedup claim.

## Current tagged build

Firepanda 0.2.0, AppleClang 21 Release, Lightpanda 1.0.2, cpp-httplib v0.64.0,
and nlohmann/json v3.12.0; 20 samples per warm scenario. The cold value is one
first request. The executable was 539672 bytes (about 527 KiB),
excluding the separately installed Lightpanda binary and its runtime memory.
Raw metadata and min/max timings are in [benchmark-data.json](benchmark-data.json).

| Scenario | Time (ms) |
| --- | ---: |
| Cold first scrape | 67.23 |
| Warm small page | 2.57 |
| 256 KiB Markdown | 6.11 |
| 3000-paragraph article | 29.92 |
| 10000 links | 142.78 |
| JavaScript readiness (300 ms timer) | 309.71 |

These are local fixture wall times, not public-site latency or throughput guarantees.

## Bounded JSON counting

```sh
./build/json_test --benchmark
```

The executable constructs JSON with 256 KiB and 9 MiB text fields and compares
100 calls to `dump().size()` against `serialized_size_within` at a 4 MiB limit.
This uses the real JSON serializer for escaping/UTF-8/number behavior. Counting
avoids a full payload-sized output allocation and stops once the limit is exceeded.
One development run measured about 1501 ms versus 662 ms for the rejected 9 MiB
case. The accepted 256 KiB case was about 44 ms versus 41 ms. Timing varies by
compiler and host; this is a microbenchmark rather than a complete crawl.

## Readiness scan CPU

[Retained measurements](benchmark-data.json) contain an instrumented page probe:
10000 sidebar div/paragraph pairs, 300 article paragraphs, a one-second
`waitForText` wait for absent literal `absent_needle`, and a 50 ms polling interval.
It counts calls to the text scanner and sums `performance.now()` durations around
those calls, including the same DOM traversal in both versions.

The original scan ran on every polling iteration. The current version observes
child-list and character-data mutations on the document root and scans only after
invalidation. The stable Lightpanda 1.0.2 run measured 20 scans / 65.62 ms before
and 1 scan / 3.09 ms after. The wall-clock readiness timeout is still one second.
A nightly run also reduced 20 scans to one. Mutation notifications were checked
for inserted nodes and direct character-data edits. Functional tests cover these,
body replacement, selector-property changes, and fallback without an observer.

The same probe measured a prototype cloning only the first main element, with
20 timed repetitions and one warm-up. It saved about 1.5 ms on that synthetic
fixture, but did not implement general hidden-ancestor/article selection rules.
That prototype was not adopted; these numbers are explicitly an upper bound.
