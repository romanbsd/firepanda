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
import threading
import time

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
sequence = 0
with args.lightpanda.open('rb') as source:
    browser_digest = hashlib.file_digest(source, 'sha256').hexdigest()
report = {'platform': platform.system(), 'architecture': platform.machine(),
          'version': subprocess.check_output([str(args.binary.resolve()), '--version'], text=True).strip(),
          'binary_bytes': args.binary.stat().st_size, 'iterations': args.iterations,
          'lightpanda_sha256': browser_digest,
          'clock': 'perf_counter; wall time including MCP serialization and local HTTP', 'scenarios': {}}
process = subprocess.Popen([str(args.binary.resolve()), '--lightpanda', str(args.lightpanda.resolve()), '--idle-timeout', '0'],
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)


def call(path, formats, **extra):
    global sequence
    sequence += 1
    message = {'jsonrpc': '2.0', 'id': sequence, 'method': 'tools/call', 'params': {
        '_meta': {'io.modelcontextprotocol/protocolVersion': '2026-07-28'},
        'name': 'scrape', 'arguments': {'url': base + path, 'formats': formats, 'timeout': 120000, **extra}}}
    start = time.perf_counter()
    process.stdin.write(json.dumps(message) + '\n')
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    elapsed = (time.perf_counter() - start) * 1000
    if 'error' in response or response.get('result', {}).get('isError'):
        raise RuntimeError(response)
    return elapsed


try:
    report['cold_scrape_ms'] = call('/', ['markdown'])
    for name, formats, extra in [('/', ['markdown'], {}), ('/large', ['markdown'], {}),
                                ('/article', ['markdown'], {}), ('/links', ['links'], {}),
                                ('/ready', ['text'], {'waitForText': 'Content ready'})]:
        call(name, formats, **extra)  # untimed warm-up for each scenario
        samples = [call(name, formats, **extra) for _ in range(args.iterations)]
        report['scenarios'][name] = {'median_ms': statistics.median(samples), 'min_ms': min(samples), 'max_ms': max(samples)}
finally:
    process.stdin.close()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
    server.shutdown()
text = json.dumps(report, indent=2) + '\n'
if args.output:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text)
print(text, end='')
