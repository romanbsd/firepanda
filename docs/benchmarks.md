# Benchmark methodology

Benchmarks are local synthetic measurements recorded on macOS arm64. They measure
specific costs, not general superiority over other tools. Public website latency,
JavaScript compatibility, blocked pages, and server load can dominate real
scraping results. No UDS-versus-TCP comparison was performed.

## Service wall time

From a release source build with Python 3.11+ and a native Lightpanda binary:

```sh
python3 scripts/benchmark.py --binary target/release/firepanda \
  --lightpanda .local/bin/lightpanda --iterations 20 --output dist/benchmark.json
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
| Bounded crawl, serial | 10000-link root plus 50 leaves at `maxConcurrency=1` |
| Bounded crawl, concurrent | The same crawl at `maxConcurrency=4` |

Run on an otherwise idle machine, retain the build configuration, and compare the
same fixtures and Lightpanda version across binaries. Whole-service A/B runs vary
between executions, so the README makes no general throughput claim.

## Rust 0.2.0 build

Firepanda 0.2.0, Rust 1.99.1 release, Lightpanda 1.0.2, local HTTP fixtures,
5 samples per warm scenario. The cold value is one first request.

| Scenario | Time (ms) |
| --- | ---: |
| Cold launch + first scrape | 28.8 |
| Warm small page | 1.7 |
| 256 KiB Markdown | 3.8 |
| 3000-paragraph article | 26.0 |
| 10000 links | 37.6 |
| JavaScript readiness (300 ms timer) | 309.2 |
| Crawl of 51 pages, `maxConcurrency=1` | 142.5 |
| Crawl of 51 pages, `maxConcurrency=4` | 90.2 |

The crawl window improves 1.58× from concurrency 1 to 4. The previous C++
implementation measured 252.9 ms and 199.3 ms (1.27×) on the same fixtures; those
numbers are retained only as a reference point. These are local fixture wall
times, not public-site latency or throughput guarantees.

## Bounded JSON size accounting

The JSON budget helper is `serialized_size_within` in `src/json.rs`, ported
from the former C++ test suite. It walks the value with a counting writer that
fails as soon as the limit is exceeded, so measuring a page allocates no
page-sized string. It is covered by `cargo test` across
null/booleans/integers/unsigned maximum/floats/non-finite values/escaped
UTF-8/arrays/objects/nested values and 256 KiB and 4 MiB payloads, at exact-size,
size+1, size−1, zero, and `usize::MAX` limits.

The historical C++ build instead serialized the page to count it (or used a
bounded counting stream in a separate microbenchmark). Crawl keeps the same
4 MiB budget semantics.

## Lightpanda vs Obscura (2026-11-10)

`scripts/benchmark-backends.py` runs every tool against a deterministic local
fixture on both backends and samples RSS and cumulative CPU (`ps`, 50 ms) for the
service and its descendants. Search timings use local engine fixtures; a separate
best-effort public search probe is reported but not used for comparison (Brave
returned HTTP 429 during this run).

Environment: macOS 26.6 arm64, Apple M5 Pro (15 CPUs), 48 GiB RAM, load average
4.75 at start, Firepanda 0.2.0 release, Lightpanda
1.1.0-nightly.11647 (`e988f663…`), Obscura 0.2.6. 30 samples per single-page
scenario, 5 per crawl/map (crawl = 21 pages, `maxConcurrency=4`). CPU totals span
the firepanda process plus its browser, so they can exceed 100%; very short
scenarios are shown as `<tick` because `ps` reports CPU time in 10 ms ticks.

| Scenario | Lightpanda median | Obscura median | Obscura / Lightpanda |
| --- | ---: | ---: | ---: |
| Cold launch + first scrape | 64.6 ms | 64.6 ms | 1.00× |
| Static scrape | 2.3 ms | 4.0 ms | 1.74× |
| 3000-paragraph article | 28.3 ms | 42.5 ms | 1.50× |
| JavaScript readiness (300 ms timer) | 311.1 ms | 319.1 ms | 1.03× |
| `fetch` JSON (no browser) | 0.2 ms | 0.3 ms | ≈1× |
| Crawl 21 pages | 78.1 ms | 146.0 ms | 1.87× |
| Map links (10000-link page) | 99.4 ms | 172.3 ms | 1.73× |
| Map sitemap (50 URLs) | 2.0 ms | 4.1 ms | 2.05× |
| Search (local engine fixture) | 0.3 ms | 0.3 ms | ≈1× |

Idle RSS after the first scrape was 37.7 MB (Lightpanda) and 36.0 MB (Obscura).
Under repeated article scrapes the gap widens: a fresh process grew from 37.7 MB
to 94.1 MB after 50 pages (1.15 MB/page) with Lightpanda, and from 35.9 MB to
334.1 MB (6.1 MB/page) with Obscura. In a separate 100-page probe Lightpanda
reached 115.5 MB and Obscura 410.7 MB, plateauing around page 75 (V8 heap
growth, not unbounded in this sample). Response payloads were byte-identical
across engines in every scenario.

Interpretation: Obscura costs roughly 1.5–2× the CPU and wall time of Lightpanda
on rendered pages and 2–3.6× the resident memory during sustained crawls, while
matching it on cold start and non-browser tools. Its advantage is V8 fidelity,
rendering, and an active upstream; Lightpanda remains the lower-footprint default.
These are local fixture numbers, not public-site latency or throughput claims. A
loaded machine, heavy SPAs, or different CSS/DOM coverage can change the ranking.

## Readiness scan CPU (historical)

The earlier C++ extractor instrumented its readiness probe: 10000 sidebar
div/paragraph pairs, 300 article paragraphs, a one-second `waitForText` wait for
absent literal `absent_needle`, and a 50 ms polling interval. Mutation-observer
invalidation reduced 20 scans / 65.62 ms to 1 scan / 3.09 ms on Lightpanda 1.0.2.

The shipped Rust extractor keeps the mutation-observer behavior but not the
instrumentation, so those numbers describe the earlier build. Functional tests
cover invalidation, body replacement, selector-property changes, and fallback
without an observer. [Retained data](benchmark-data.json) is from that C++ run.
