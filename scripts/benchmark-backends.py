#!/usr/bin/env python3
"""Benchmark every firepanda tool against Lightpanda and Obscura.

Measures wall latency, CPU time, peak RSS, response size, and cold start on
deterministic local fixtures. `search` uses the real network and is labelled as
such. Stdlib only.
"""
import argparse
import datetime
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from harness import Service, structured

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--lightpanda", type=Path, required=True)
parser.add_argument("--obscura", type=Path, required=True)
parser.add_argument("--iterations", type=int, default=10,
                    help="samples for single-page scenarios (default 10)")
parser.add_argument("--heavy-iterations", type=int, default=3,
                    help="samples for crawl/map/search (default 3)")
parser.add_argument("--output", type=Path, default=Path("dist/backend-benchmark.json"))
args = parser.parse_args()
for name in ("iterations", "heavy_iterations"):
    if not 1 <= getattr(args, name) <= 1000:
        parser.error(f"--{name.replace('_', '-')} must be 1..1000")


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        origin = f"http://127.0.0.1:{self.server.server_port}"
        content_type = "text/html; charset=utf-8"
        if self.path.startswith("/search-ddg"):
            body = """<!doctype html><html><body>
<div class="result results_links result__body">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Falpha">Alpha result</a></h2>
  <a class="result__snippet">Alpha snippet</a>
</div>
<div class="result results_links result__body">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fbeta">Beta result</a></h2>
  <a class="result__snippet">Beta snippet</a>
</div>
</body></html>""".encode()
        elif self.path == "/robots.txt":
            body = f"Sitemap: {origin}/sitemap.xml\n".encode()
            content_type = "text/plain; charset=utf-8"
        elif self.path == "/sitemap.xml":
            body = ('<?xml version="1.0" encoding="UTF-8"?>'
                    '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
                    + "".join(f"<url><loc>{origin}/leaf/{i}</loc></url>" for i in range(100))
                    + "</urlset>").encode()
            content_type = "application/xml"
        elif self.path == "/data.json":
            body = json.dumps({"ok": True, "items": list(range(100)), "nested": {"a": "b"}}).encode()
            content_type = "application/json"
        elif self.path == "/links":
            body = ("<html><title>Links</title><main>"
                    + "".join(f'<a href="/leaf/{i}">Link {i}</a>' for i in range(10000))
                    + "</main></html>").encode()
        elif self.path == "/article":
            body = ("<html><title>Article</title><main>"
                    + '<p>Paragraph <b>bold</b> and <em>italic</em> text.</p>' * 3000
                    + "</main></html>").encode()
        elif self.path == "/ready":
            body = ('<html><title>Ready</title><main><p id="content">Loading</p>'
                    '<script>setTimeout(()=>{document.getElementById("content").textContent='
                    '"Content ready";},300);</script></main></html>').encode()
        elif self.path.startswith("/leaf/"):
            body = f"<html><title>Leaf</title><main>{self.path}</main></html>".encode()
        else:
            body = ("<html><title>Benchmark</title><main><h1>Firepanda</h1>"
                    "<p>Local benchmark page with a short paragraph.</p></main></html>").encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def parse_cpu(text):
    """`ps -o time=` -> seconds (MM:SS.ss or HH:MM:SS.ss)."""
    total = 0.0
    for part in text.strip().split(":"):
        total = total * 60 + float(part)
    return total


def descendants(root):
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    children = {}
    for line in rows.splitlines():
        pid, ppid = (int(value) for value in line.split())
        children.setdefault(ppid, []).append(pid)
    found, frontier = [], [root]
    while frontier:
        pid = frontier.pop()
        found.append(pid)
        frontier.extend(children.get(pid, []))
    return found


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def browser_version(path):
    for argument in ("--version", "version"):
        try:
            output = subprocess.check_output([str(path), argument], text=True,
                                             stderr=subprocess.DEVNULL).strip()
            if output:
                return output.splitlines()[0]
        except (OSError, subprocess.CalledProcessError):
            continue
    return "unknown"


def host_info():
    info = {"platform": platform.platform(), "architecture": platform.machine(),
            "python": platform.python_version()}
    if platform.system() == "Darwin":
        for key, name in (("machdep.cpu.brand_string", "cpu"),
                          ("hw.memsize", "memory_bytes"), ("hw.ncpu", "cpus")):
            try:
                info[name] = subprocess.check_output(["sysctl", "-n", key], text=True).strip()
            except (OSError, subprocess.CalledProcessError):
                pass
        info["memory_bytes"] = int(info.get("memory_bytes", 0))
    elif platform.system() == "Linux":
        try:
            cpuinfo = Path("/proc/cpuinfo").read_text()
            match = re.search(r"model name\s*:\s*(.+)", cpuinfo)
            if match:
                info["cpu"] = match.group(1)
            info["cpus"] = str(os.cpu_count() or "")
            meminfo = Path("/proc/meminfo").read_text()
            match = re.search(r"MemTotal:\s*(\d+) kB", meminfo)
            if match:
                info["memory_bytes"] = int(match.group(1)) * 1024
        except OSError:
            pass
    try:
        info["load_average"] = [round(value, 2) for value in os.getloadavg()]
    except OSError:
        pass
    return info


class Monitor:
    """Sample CPU time and summed RSS for a process tree."""

    def __init__(self, root_pid):
        self.root = root_pid
        self.pids = []
        self.samples = []
        self.thread = None
        self.stopping = threading.Event()

    def start(self):
        self.pids = descendants(self.root)
        self.samples = []
        self.stopping.clear()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _sample(self):
        if not self.pids:
            self.pids = descendants(self.root)
        try:
            rows = subprocess.check_output(
                ["ps", "-p", ",".join(str(pid) for pid in self.pids),
                 "-o", "pid=,rss=,time="], text=True)
        except subprocess.CalledProcessError:
            return None
        rss = 0
        cpu = 0.0
        count = 0
        for line in rows.splitlines():
            parts = line.split(None, 2)
            if len(parts) != 3:
                continue
            rss += int(parts[1])
            cpu += parse_cpu(parts[2])
            count += 1
        return {"rss_kb": rss, "cpu_s": cpu, "processes": count}

    def current(self):
        return self._sample() or {"rss_kb": 0, "cpu_s": 0.0, "processes": 0}

    def _run(self):
        while not self.stopping.is_set():
            sample = self._sample()
            if sample:
                self.samples.append(sample)
            self.stopping.wait(0.05)

    def stop(self):
        self.stopping.set()
        if self.thread:
            self.thread.join()
        if not self.samples:
            sample = self._sample()
            if sample:
                self.samples.append(sample)
        first = self.samples[0]
        return {
            "rss_baseline_mb": round(first["rss_kb"] / 1024, 1),
            "rss_peak_mb": round(max(s["rss_kb"] for s in self.samples) / 1024, 1),
            "processes_peak": max(s["processes"] for s in self.samples),
            "samples": len(self.samples),
        }


def percentile(values, fraction):
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(round(fraction * len(ordered))) - 1))
    return ordered[index]


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{server.server_port}"


def service_env():
    """Route search at the local engine fixture instead of the public network."""
    return dict(os.environ,
                FIREPANDA_SEARCH_DDG_URL=base + "/search-ddg",
                FIREPANDA_SEARCH_BRAVE_URL=base + "/search-ddg")


def start_service(backend, browser, env=None):
    return Service([str(args.binary.resolve()), "--backend", backend,
                    "--browser", str(browser.resolve()), "--idle-timeout", "0"],
                   env=service_env() if env is None else env,
                   stderr=subprocess.DEVNULL)


def close_service(service):
    service.process.stdin.close()
    try:
        service.process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service.process.kill()
        service.process.wait()


def scenario(tool, arguments, iterations, validate=None):
    service.call("tools/call", name=tool, arguments=arguments)  # untimed warm-up
    latencies, sizes, errors = [], [], []
    monitor = Monitor(service.process.pid)
    monitor.start()
    cpu_before = monitor.current()["cpu_s"]
    started = time.perf_counter()
    for _ in range(iterations):
        begin = time.perf_counter()
        result = service.call("tools/call", name=tool, arguments=arguments)
        elapsed = (time.perf_counter() - begin) * 1000
        if result.get("isError"):
            text = result.get("content", [{}])[0].get("text", "")
            errors.append(text[:160])
            continue
        payload = structured(result)
        if validate:
            validate(payload)
        sizes.append(len(json.dumps(payload)))
        latencies.append(elapsed)
    wall = time.perf_counter() - started
    cpu_after = monitor.current()["cpu_s"]
    resources = monitor.stop()
    resources.update({
        "iterations": iterations,
        "errors": len(errors),
        "error_messages": errors,
        "wall_ms": round(wall * 1000, 1),
        "cpu_ms": round((cpu_after - cpu_before) * 1000, 1),
        "latency_ms": {
            "mean": round(statistics.fmean(latencies), 1) if latencies else None,
            "median": round(statistics.median(latencies), 1) if latencies else None,
            "p95": round(percentile(latencies, 0.95), 1) if latencies else None,
            "min": round(min(latencies), 1) if latencies else None,
            "max": round(max(latencies), 1) if latencies else None,
        },
        "cpu_utilization_pct": round((cpu_after - cpu_before) / wall * 100, 1) if wall else None,
        "cpu_quantised": wall < 0.05,
        "rss_delta_mb": round(resources["rss_peak_mb"] - resources["rss_baseline_mb"], 1),
        "throughput_per_sec": round(1000 / statistics.median(latencies), 1) if latencies else None,
        "response_kb_median": round(statistics.median(sizes) / 1024, 1) if sizes else None,
    })
    return resources


def network_search(backend, browser):
    """One best-effort public search; rate limits make it unsuitable for timings."""
    probe = start_service(backend, browser, env=dict(os.environ))
    started = time.perf_counter()
    try:
        result = probe.call("tools/call", name="search", arguments={
            "query": "rust async programming", "limit": 5, "timeout": 15000})
        error = None
        if result.get("isError"):
            error = result.get("content", [{}])[0].get("text", "")[:160]
        return {"ok": error is None,
                "ms": round((time.perf_counter() - started) * 1000, 1),
                "error": error}
    except Exception as exception:  # noqa: BLE001 - report, do not abort the run
        return {"ok": False,
                "ms": round((time.perf_counter() - started) * 1000, 1),
                "error": str(exception)[:160]}
    finally:
        close_service(probe)


def memory_growth(backend, browser):
    """Fresh process: RSS after a fixed number of article scrapes."""
    service = start_service(backend, browser)
    points = {}
    monitor = Monitor(service.process.pid)
    try:
        service.call("tools/call", name="scrape", arguments={
            "url": base + "/", "formats": ["markdown"], "timeout": 120000})
        monitor.start()
        baseline = monitor.current()["rss_kb"] / 1024
        for page in range(1, 51):
            service.call("tools/call", name="scrape", arguments={
                "url": base + "/article", "formats": ["markdown"], "timeout": 120000})
            if page in (1, 5, 10, 20, 30, 50):
                points[str(page)] = round(monitor.current()["rss_kb"] / 1024, 1)
        monitor.stop()
    finally:
        close_service(service)
    return {"baseline_mb": round(baseline, 1), "after_pages_mb": points,
            "growth_mb": round(points["50"] - baseline, 1),
            "growth_kb_per_page": round((points["50"] - baseline) * 1024 / 50, 1)}


def bench(backend, browser):
    global service
    service = start_service(backend, browser)
    report = {
        "browser": {"path": str(browser.resolve()), "version": browser_version(browser),
                    "bytes": browser.stat().st_size, "sha256": sha256(browser)},
        "scenarios": {},
    }
    try:
        # Cold start includes lazy spawn, transport setup, V8/browser init, and
        # the first page. Measure it before any warm-up.
        cold_monitor = Monitor(service.process.pid)
        cold_start = time.perf_counter()
        cold_monitor.start()
        cold_cpu = cold_monitor.current()["cpu_s"]
        result = service.call("tools/call", name="scrape", arguments={
            "url": base + "/", "formats": ["markdown"], "timeout": 120000})
        report["cold_start_ms"] = round((time.perf_counter() - cold_start) * 1000, 1)
        cold_resources = cold_monitor.stop()
        cold_resources["cpu_ms"] = round(
            (cold_monitor.current()["cpu_s"] - cold_cpu) * 1000, 1)
        report["cold_start_resources"] = cold_resources
        assert not result.get("isError"), result
        idle = Monitor(service.process.pid)
        idle.start()
        time.sleep(0.1)
        report["idle_rss_mb"] = idle.stop()["rss_peak_mb"]

        report["scenarios"]["scrape_static"] = scenario(
            "scrape", {"url": base + "/", "formats": ["markdown"], "timeout": 120000},
            args.iterations)
        report["scenarios"]["scrape_article"] = scenario(
            "scrape", {"url": base + "/article", "formats": ["markdown"], "timeout": 120000},
            args.iterations)
        report["scenarios"]["scrape_js_ready"] = scenario(
            "scrape", {"url": base + "/ready", "formats": ["text"], "timeout": 120000,
                       "waitForText": "Content ready"}, args.iterations)
        report["scenarios"]["fetch_json"] = scenario(
            "fetch", {"url": base + "/data.json", "timeout": 120000}, args.iterations,
            validate=lambda payload: payload.get("text") is not None)
        report["scenarios"]["crawl_21_pages"] = scenario(
            "crawl", {"url": base + "/links", "limit": 21, "maxDepth": 1,
                      "maxConcurrency": 4, "timeout": 120000}, args.heavy_iterations,
            validate=lambda payload: payload["attempted"] == 21 and not payload["errors"])
        report["scenarios"]["map_links"] = scenario(
            "map", {"url": base + "/links", "limit": 100, "maxDepth": 1,
                    "timeout": 120000}, args.heavy_iterations)
        report["scenarios"]["map_sitemap"] = scenario(
            "map", {"url": base + "/", "sitemap": True, "limit": 50, "maxDepth": 0,
                    "timeout": 120000}, args.heavy_iterations)
        report["scenarios"]["search_fixture"] = scenario(
            "search", {"query": "rust async programming", "limit": 5,
                       "engine": "duckduckgo", "timeout": 60000},
            args.iterations,
            validate=lambda payload: len(payload.get("data", {}).get("web", [])) > 0)
        report["search_network_probe"] = network_search(backend, browser)
    finally:
        close_service(service)
    return report


report = {
    "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "host": host_info(),
    "binary": {"path": str(args.binary.resolve()), "bytes": args.binary.stat().st_size,
               "version": subprocess.check_output(
                   [str(args.binary.resolve()), "--version"], text=True).strip()},
    "method": {
        "clock": "perf_counter around one MCP tools/call including serialization",
        "resources": "ps samples every 50 ms of the service and its descendants; "
                     "CPU totals come from cumulative ps time (10 ms tick, so very "
                     "short scenarios are quantised); aggregate CPU can exceed 100% "
                     "across processes",
        "fixtures": "local threaded HTTP server; search timings use local engine "
                    "fixtures, with one best-effort public search reported separately",
        "iterations": {"single_page": args.iterations, "crawl_map": args.heavy_iterations},
    },
    "backends": {},
}
try:
    report["backends"]["lightpanda"] = bench("lightpanda", args.lightpanda)
    report["backends"]["obscura"] = bench("obscura", args.obscura)
    report["memory_growth"] = {
        "lightpanda": memory_growth("lightpanda", args.lightpanda),
        "obscura": memory_growth("obscura", args.obscura),
    }
finally:
    server.shutdown()

args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2) + "\n")
print(f"Wrote {args.output}")


def table(results):
    lines = ["| Scenario | Median ms | p95 ms | Mean ms | CPU/call ms | CPU % | Peak RSS MB | Δ RSS MB | Resp KB | Errors |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for name, data in results.items():
        latency = data["latency_ms"]
        successful = max(1, data["iterations"] - data["errors"])
        cpu_per_call = (f"<{round(10 / successful, 1)}" if data["cpu_quantised"]
                        else round(data["cpu_ms"] / successful, 1))
        cpu_percent = "n/a" if data["cpu_quantised"] else data["cpu_utilization_pct"]
        lines.append(
            f"| {name} | {latency['median']} | {latency['p95']} | {latency['mean']} | "
            f"{cpu_per_call} | {cpu_percent} | {data['rss_peak_mb']} | "
            f"{data['rss_delta_mb']} | {data['response_kb_median']} | {data['errors']} |")
    return "\n".join(lines)


print("\n## Lightpanda\n")
print(table(report["backends"]["lightpanda"]["scenarios"]))
print("\n## Obscura\n")
print(table(report["backends"]["obscura"]["scenarios"]))

lines = [
    "# Backend benchmark: Lightpanda vs Obscura",
    "",
    f"Generated: {report['generated_at']}",
    "",
    f"- Host: {report['host'].get('platform')} / {report['host'].get('cpu', 'unknown CPU')} / "
    f"{report['host'].get('cpus', '?')} CPUs / load {report['host'].get('load_average')}",
    f"- Firepanda: {report['binary']['version']} ({report['binary']['bytes']:,} bytes)",
    f"- Method: {report['method']['clock']}; {report['method']['resources']}; "
    f"{report['method']['fixtures']}",
    f"- Samples: {report['method']['iterations']['single_page']} single-page, "
    f"{report['method']['iterations']['crawl_map']} crawl/map.",
    "",
]
for backend, data in report["backends"].items():
    lines += [
        f"## {backend.capitalize()}",
        "",
        f"Browser: `{data['browser']['version']}` ({data['browser']['bytes']:,} bytes, "
        f"sha256 `{data['browser']['sha256'][:16]}…`)",
        "",
        f"Cold start: **{data['cold_start_ms']} ms**; idle RSS: **{data['idle_rss_mb']} MB**; "
        f"cold-start CPU (10 ms ticks): {data['cold_start_resources'].get('cpu_ms')} ms.",
        "",
        table(data["scenarios"]),
        "",
    ]
    probe = data["search_network_probe"]
    if probe["ok"]:
        lines.append(f"Public search probe: ok in {probe['ms']} ms.")
    else:
        lines.append(f"Public search probe: failed after {probe['ms']} ms — {probe['error']}.")
    lines.append("")

lines += ["## Memory growth (fresh process, 50 article scrapes)", ""]
lines.append("| Backend | Baseline MB | Page 1 | Page 10 | Page 30 | Page 50 | Growth MB | KB/page |")
lines.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
for backend, data in report["memory_growth"].items():
    points = data["after_pages_mb"]
    lines.append(
        f"| {backend} | {data['baseline_mb']} | {points['1']} | {points['10']} | "
        f"{points['30']} | {points['50']} | {data['growth_mb']} | "
        f"{data['growth_kb_per_page']} |")
lines.append("")
(args.output.with_suffix(".md")).write_text("\n".join(lines) + "\n")
print(f"\nWrote {args.output.with_suffix('.md')}")
