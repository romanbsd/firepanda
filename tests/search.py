"""Offline search tool: engine HTML fixtures, no network, no Lightpanda."""
import http.server
import json
import os
import subprocess
import sys
import threading
import urllib.parse

binary, lightpanda = sys.argv[1:3]
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Service, structured

DDG_PAGE = """<!doctype html><html><body>
<div class="result results_links result__body">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Falpha&rut=1">Alpha result</a></h2>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Falpha">Alpha snippet</a>
</div>
<div class="result results_links result__body">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Falpha&rut=2">Alpha again</a></h2>
  <a class="result__snippet">Duplicate destination</a>
</div>
<div class="result result--ad result__body">
  <h2><a class="result__a" href="//duckduckgo.com/y.js?ad_provider=x">Sponsored</a></h2>
  <a class="result__snippet">Buy things</a>
</div>
<div class="result results_links result__body">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fbeta%3Fq%3D1">Beta result</a></h2>
  <a class="result__snippet">Beta snippet</a>
</div>
</body></html>"""

BRAVE_PAGE = """<!doctype html><html><body>
<div data-type="web">
  <a href="https://brave.example/one"><div class="title">Brave One</div></a>
  <div class="snippet"><div class="content">Brave snippet one</div></div>
</div>
<div data-type="news">
  <a href="https://brave.example/news"><div class="title">News item</div></a>
  <div class="snippet"><div class="content">Not a web result</div></div>
</div>
<div data-type="web">
  <a href="https://brave.example/two"><div class="sitename-container"><span>site</span></div>
    <div class="title">Brave Two</div></a>
  <div class="snippet"><div class="content">Brave snippet two</div></div>
</div>
</body></html>"""

EMPTY_PAGE = "<!doctype html><html><body><p>No results found</p></body></html>"


class Engines(http.server.BaseHTTPRequestHandler):
    """Canned engine pages; records the request each engine received."""

    def log_message(self, *args):
        pass

    def reply(self, status, body):
        payload = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def route(self, path, body):
        if path == "/ddg":
            self.reply(200, DDG_PAGE)
        elif path == "/brave":
            self.reply(200, BRAVE_PAGE)
        elif path == "/empty":
            self.reply(200, EMPTY_PAGE)
        elif path == "/huge":
            self.reply(200, "x" * (8 * 1024 * 1024 + 1))
        elif path == "/broken":
            self.reply(503, "engine unavailable")
        else:
            self.reply(404, "unknown engine route")

    def do_GET(self):
        self.server.requests.append(("GET", self.path, ""))
        self.route(urllib.parse.urlparse(self.path).path, "")


class QuietServer(http.server.ThreadingHTTPServer):
    """The oversized-body test aborts mid-response; that reset is expected."""

    def handle_error(self, request, client_address):
        error = sys.exc_info()[1]
        if isinstance(error, (ConnectionResetError, BrokenPipeError)):
            return
        super().handle_error(request, client_address)


server = QuietServer(("127.0.0.1", 0), Engines)
server.requests = []
threading.Thread(target=server.serve_forever, daemon=True).start()
base = "http://127.0.0.1:" + str(server.server_port)


def service_for(ddg="/ddg", brave="/brave"):
    env = dict(os.environ,
               FIREPANDA_SEARCH_DDG_URL=base + ddg,
               FIREPANDA_SEARCH_BRAVE_URL=base + brave)
    return Service([binary, "--lightpanda", lightpanda], env=env)


def stop(service):
    service.process.stdin.close()
    try:
        service.process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service.process.kill()
        service.process.wait()
        raise
    assert service.process.returncode == 0, service.process.returncode


def raw_call(service, method, **params):
    """One request, returning the full reply including JSON-RPC errors."""
    identifier = service.post(method, **params)
    reply = json.loads(service.process.stdout.readline())
    assert reply["id"] == identifier, reply
    return reply


def results(payload):
    return payload["data"]["web"]


# tools/list advertises the fourth tool with its own schema.
service = service_for()
try:
    schema = service.call("tools/list")["tools"]
    names = [tool["name"] for tool in schema]
    assert names == ["scrape", "fetch", "crawl", "map", "search"], names
    search = schema[4]
    assert search["inputSchema"]["required"] == ["query"], search
    assert search["title"] == "Search the web", search
    assert search["annotations"]["idempotentHint"] is True, search
    assert set(search["inputSchema"]["properties"]) == {"query", "limit", "engine", "timeout"}
    assert search["inputSchema"]["additionalProperties"] is False
finally:
    stop(service)

# DuckDuckGo: redirect decoding, ad filtering, dedupe, and snippet extraction.
server.requests.clear()
service = service_for()
try:
    payload = service.tool("search", query="rust async")
    assert payload["success"] is True, payload
    web = results(payload)
    assert [item["url"] for item in web] == [
        "https://example.com/alpha", "https://example.org/beta?q=1"], web
    assert [item["title"] for item in web] == ["Alpha result", "Beta result"], web
    assert web[0]["description"] == "Alpha snippet", web
    assert web[1]["description"] == "Beta snippet", web
    limited = results(service.tool("search", query="rust async", limit=1))
    assert [item["url"] for item in limited] == ["https://example.com/alpha"], limited
    method, path, _ = server.requests[0]
    assert method == "GET" and path.startswith("/ddg?"), server.requests[0]
    form = urllib.parse.parse_qs(urllib.parse.urlparse(path).query, keep_blank_values=True)
    assert form["q"] == ["rust async"] and "b" in form and form["l"] == ["us-en"], form
    # Search must never launch the managed browser.
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    assert not [line for line in rows.splitlines()
                if int(line.split()[1]) == service.process.pid], rows
finally:
    stop(service)

# Brave: web items only, wrapped-title anchor, snippet content.
service = service_for()
try:
    web = results(service.tool("search", query="rust async", engine="brave"))
    assert [(item["title"], item["url"]) for item in web] == [
        ("Brave One", "https://brave.example/one"),
        ("Brave Two", "https://brave.example/two")], web
    assert web[1]["description"] == "Brave snippet two", web
    method, path, _ = server.requests[-1]
    assert method == "GET" and path.startswith("/brave?"), server.requests[-1]
    query = urllib.parse.parse_qs(urllib.parse.urlparse(path).query)
    assert query["q"] == ["rust async"] and query["source"] == ["web"], query
finally:
    stop(service)

# Auto falls back to Brave when DuckDuckGo fails.
service = service_for(ddg="/broken")
try:
    web = results(service.tool("search", query="fallback"))
    assert [item["url"] for item in web] == [
        "https://brave.example/one", "https://brave.example/two"], web
finally:
    stop(service)

# Engine-specific failure is a tool error with no structured content.
service = service_for(ddg="/broken")
try:
    failed = service.call("tools/call", name="search",
                          arguments={"query": "x", "engine": "duckduckgo"})
    assert failed.get("isError") and "HTTP 503" in failed["content"][0]["text"], failed
    assert "structuredContent" not in failed, failed
finally:
    stop(service)

# All engines empty is a tool error too.
service = service_for(ddg="/empty", brave="/empty")
try:
    empty = service.call("tools/call", name="search", arguments={"query": "nothing"})
    assert empty.get("isError") and "No search results" in empty["content"][0]["text"], empty
finally:
    stop(service)

# A response larger than the 8 MiB body cap is rejected before parsing.
service = service_for(ddg="/huge")
try:
    huge = service.call("tools/call", name="search",
                        arguments={"query": "x", "engine": "duckduckgo"})
    assert huge.get("isError") and "exceeds 8 MiB" in huge["content"][0]["text"], huge
finally:
    stop(service)

# Argument validation is a JSON-RPC error, matching the other tools.
service = service_for()
try:
    for arguments in ({"query": ""}, {"query": "  "}, {"query": "ok", "limit": 0},
                      {"query": "ok", "limit": 21}, {"query": "ok", "engine": "bing"},
                      {"query": "ok", "timeout": 10}, {"query": "ok", "url": base}):
        reply = raw_call(service, "tools/call", name="search", arguments=arguments)
        assert reply["error"]["code"] == -32602, (arguments, reply)
    reply = raw_call(service, "tools/call", name="nope", arguments={"query": "ok"})
    assert reply["error"]["code"] == -32602, reply
finally:
    stop(service)

print("PASS: search — DuckDuckGo/Brave parsing, dedupe, limits, fallback, validation")
server.shutdown()
