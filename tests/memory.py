"""Repeat native browsing and failure recovery under sanitizers or macOS leaks."""
import argparse
import http.server
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading

from fixtures import Fixture
from harness import Service, structured

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary")
parser.add_argument("lightpanda")
parser.add_argument("--iterations", type=int, default=100)
parser.add_argument("--leaks", action="store_true")
parser.add_argument("--report", type=Path)
args = parser.parse_args()
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = "http://127.0.0.1:" + str(server.server_port)
command = [args.binary, "--lightpanda", args.lightpanda]
env = os.environ.copy()
if args.leaks:
    command = ["/usr/bin/leaks", "--atExit", "--"] + command
    env["MallocStackLogging"] = "1"
# Store diagnostics in a file: a sanitizer report must never fill a pipe while
# the parent waits for a JSON-RPC reply. The actual browser is not instrumented.
with tempfile.TemporaryFile(mode="w+") as diagnostics:
    service = Service(command, env=env, stderr=diagnostics)

    def tool(name, path="/", **arguments):
        return service.call("tools/call", name=name, arguments={"url": base + path, **arguments})

    def descriptor_count():
        if args.leaks:  # the process is the leaks wrapper in this mode
            return None
        directory = Path(f"/proc/{service.process.pid}/fd")
        if directory.exists():
            return len(list(directory.iterdir()))
        output = subprocess.check_output(["/usr/sbin/lsof", "-a", "-p", str(service.process.pid), "-Ff"], text=True)
        return sum(bool(re.fullmatch(r"f[0-9]+", line)) for line in output.splitlines())

    try:
        baseline_descriptors = None
        for i in range(args.iterations):
            assert len(service.call("tools/list")["tools"]) == 3
            result = tool("scrape", "/large" if i % 10 == 0 else "/", formats=["markdown", "html", "links"])
            assert not result["isError"], result
            if i % 10 == 0:
                assert len(structured(result)["markdown"]) >= 256 * 1024
                code = structured(tool("scrape", "/code-base"))["markdown"]
                assert "first\n\n\n```literal\nlast\n" in code
            if i % 5 == 0:
                assert not structured(tool("crawl", limit=3, maxDepth=1))["errors"]
                assert len(structured(tool("map", limit=2))["links"]) == 2
                redirects = structured(tool("crawl", "/redirect-root", limit=2, maxDepth=1,
                                            maxConcurrency=1))
                assert len(redirects["pages"]) == 2 and not redirects["truncated"]
            if i == 0:
                baseline_descriptors = descriptor_count()
        assert tool("scrape", "/too-large", formats=["text"])["isError"]
        assert tool("scrape", "/slow", timeout=100)["isError"]
        assert not tool("scrape")["isError"], "Recovery after size/timeout failure failed"
        # Concurrent fetches must not leak connections across a crawl failure.
        parallel = structured(tool("crawl", "/parallel-root", limit=10, maxDepth=1, timeout=5000))
        assert not parallel["errors"] and len(parallel["pages"]) == 4, parallel
        flaky = structured(tool("crawl", "/flaky-root", limit=10, maxDepth=1, timeout=8000,
                                maxConcurrency=2))
        assert [error["url"] for error in flaky["errors"]] == [base + "/p/broken"], flaky["errors"]
        assert [page["metadata"]["url"] for page in flaky["pages"]] == [
            base + "/flaky-root", base + "/p/slow-a", base + "/p/slow-b"], flaky
        final_descriptors = descriptor_count()
        # The pool may shrink when a failure reaps a warm multi-connection
        # browser; it must never grow past the post-warm-up baseline.
        assert baseline_descriptors is None or final_descriptors <= baseline_descriptors, (
            baseline_descriptors, final_descriptors)
        service.process.stdin.close()
        service.process.stdin = None
        tail, _ = service.process.communicate(timeout=30)
        diagnostics.seek(0)
        report = diagnostics.read() + tail
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(report)
        assert service.process.returncode == 0, report
        assert not re.search(r"ERROR: (AddressSanitizer|LeakSanitizer)|runtime error:", report), report
        if args.leaks:
            assert re.search(r"0 leaks for 0 total leaked bytes", tail), report
        print(f"PASS: {args.iterations} browsing cycles, large outputs, size/timeout recovery, clean exit")
        if baseline_descriptors is not None:
            print(f"Open descriptors after warm-up/final recovery: {baseline_descriptors}/{final_descriptors}")
        if args.leaks:
            print("macOS leaks: 0 leaks for 0 total leaked bytes")
    except Exception:
        diagnostics.seek(0)
        print(diagnostics.read())
        raise
    finally:
        if service.process.poll() is None:
            service.process.terminate()
            try:
                service.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                service.process.kill()
                service.process.wait()
        server.shutdown()
