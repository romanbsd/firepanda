#!/usr/bin/env python3
"""Measure native MCP wall time against deterministic local HTTP pages (stdlib only)."""
import argparse
import hashlib
import http.server
import json
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from harness import Service, structured

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--lightpanda', type=Path, required=True)
parser.add_argument('--iterations', type=int, default=20)
parser.add_argument('--output', type=Path)
args = parser.parse_args()
if not 1 <= args.iterations <= 1000:
    parser.error('--iterations must be 1..1000')


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.path == '/links':
            content = ''.join(f'<a href="/leaf/{i}">Link {i}</a>' for i in range(10000))
        elif self.path == '/large':
            content = '<p>' + 'x' * (256 * 1024) + '</p>'
        elif self.path == '/article':
            content = '<p>Paragraph <b>bold</b> and <em>italic</em> text.</p>' * 3000
        elif self.path == '/ready':
            content = '<p id="content">Loading</p><script>setTimeout(()=>{document.getElementById("content").textContent="Content ready";},300);</script>'
        else:
            content = '<h1>Firepanda</h1><p>Local benchmark</p>'
        body = ('<html><title>Benchmark</title><main>' + content + '</main></html>').encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
with args.lightpanda.open('rb') as source:
    browser_digest = hashlib.file_digest(source, 'sha256').hexdigest()
report = {'platform': platform.system(), 'architecture': platform.machine(),
          'version': subprocess.check_output([str(args.binary.resolve()), '--version'], text=True).strip(),
          'binary_bytes': args.binary.stat().st_size, 'iterations': args.iterations,
          'lightpanda_sha256': browser_digest,
          'clock': 'perf_counter; wall time including MCP serialization and local HTTP', 'scenarios': {}}
service = Service([str(args.binary.resolve()), '--lightpanda', str(args.lightpanda.resolve()),
                   '--idle-timeout', '0'], stderr=subprocess.DEVNULL)


def call(path, formats, **extra):
    start = time.perf_counter()
    result = service.call('tools/call', name='scrape',
                          arguments={'url': base + path, 'formats': formats, 'timeout': 120000, **extra})
    elapsed = (time.perf_counter() - start) * 1000
    assert not result.get('isError'), result
    return elapsed


def crawl(concurrency):
    """One bounded crawl of the 10000-link root: root plus 50 leaves."""
    start = time.perf_counter()
    result = service.call('tools/call', name='crawl',
                          arguments={'url': base + '/links', 'limit': 51, 'maxDepth': 1,
                                     'timeout': 120000, 'maxConcurrency': concurrency})
    elapsed = (time.perf_counter() - start) * 1000
    assert not result.get('isError'), result
    payload = structured(result)
    assert payload['attempted'] == 51 and not payload['errors'], payload
    return elapsed


try:
    report['cold_scrape_ms'] = call('/', ['markdown'])
    for name, formats, extra in [('/', ['markdown'], {}), ('/large', ['markdown'], {}),
                                ('/article', ['markdown'], {}), ('/links', ['links'], {}),
                                ('/ready', ['text'], {'waitForText': 'Content ready'})]:
        call(name, formats, **extra)  # untimed warm-up for each scenario
        samples = [call(name, formats, **extra) for _ in range(args.iterations)]
        report['scenarios'][name] = {'median_ms': statistics.median(samples), 'min_ms': min(samples), 'max_ms': max(samples)}
    for name, concurrency in [('/links crawl maxConcurrency=1', 1),
                              ('/links crawl maxConcurrency=4', 4)]:
        crawl(concurrency)  # untimed warm-up
        samples = [crawl(concurrency) for _ in range(args.iterations)]
        report['scenarios'][name] = {'median_ms': statistics.median(samples), 'min_ms': min(samples), 'max_ms': max(samples)}
finally:
    service.process.stdin.close()
    try:
        service.process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service.process.kill()
        service.process.wait()
    server.shutdown()
text = json.dumps(report, indent=2) + '\n'
if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text)
print(text, end='')
