"""Offline fetch tool: raw HTTP resources, no Lightpanda, no external network."""
import base64
import http.server
import json
import os
import subprocess
import sys
import threading
import time

binary, lightpanda = sys.argv[1:3]
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Service, structured

PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)


class QuietServer(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        error = sys.exc_info()[1]
        if isinstance(error, (ConnectionResetError, BrokenPipeError)):
            return
        super().handle_error(request, client_address)


class Resources(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body, content_type):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (ConnectionResetError, BrokenPipeError):
            pass

    def do_GET(self):
        self.server.ports.append(self.client_address[1])
        if self.path == "/data.json":
            self.reply(200, b'{"ok": true, "items": [1, 2, 3]}', "application/json")
        elif self.path == "/doc.md":
            self.reply(200, b"# Doc title\n\nMarkdown body.\n", "text/markdown; charset=utf-8")
        elif self.path == "/feed.xml":
            self.reply(200, b"<feed><title>Fixture</title></feed>", "application/xml")
        elif self.path == "/icon.svg":
            self.reply(200, b'<svg xmlns="http://www.w3.org/2000/svg"></svg>', "image/svg+xml")
        elif self.path == "/unknown":
            self.reply(200, b'{"sniffed": true}', "application/octet-stream")
        elif self.path == "/image.png":
            self.reply(200, PNG, "image/png")
        elif self.path == "/blob.bin":
            self.reply(200, PNG, "application/octet-stream")
        elif self.path == "/empty":
            self.reply(200, b"", "text/plain")
        elif self.path == "/page.html":
            self.reply(200, b"<html><body><h1>Rendered</h1></body></html>", "text/html")
        elif self.path == "/redirect-json":
            self.send_response(302)
            self.send_header("Location", "/data.json")
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif self.path == "/missing":
            self.reply(404, b'{"error": "not found"}', "application/json")
        elif self.path == "/slow":
            time.sleep(2)
            self.reply(200, b'{"slow": true}', "application/json")
        elif self.path == "/huge":
            self.reply(200, b"x" * (4 * 1024 * 1024 + 1), "application/octet-stream")
        else:
            self.reply(404, b"unknown route", "text/plain")


server = QuietServer(("127.0.0.1", 0), Resources)
server.ports = []
threading.Thread(target=server.serve_forever, daemon=True).start()
base = "http://127.0.0.1:" + str(server.server_port)
service = Service([binary, "--lightpanda", lightpanda])


def raw_call(method, **params):
    identifier = service.post(method, **params)
    return json.loads(service.process.stdout.readline())


def fetch(path, **arguments):
    return service.call("tools/call", name="fetch",
                        arguments={"url": base + path, **arguments})


try:
    schema = service.call("tools/list")["tools"]
    names = [tool["name"] for tool in schema]
    assert names[:4] == ["scrape", "fetch", "crawl", "map"], names
    tool = schema[1]
    assert tool["title"] == "Fetch resource", tool
    assert set(tool["inputSchema"]["properties"]) == {"url", "timeout"}, tool

    data = structured(fetch("/data.json"))
    assert data["encoding"] == "text", data
    assert data["text"] == '{"ok": true, "items": [1, 2, 3]}', data
    assert data["statusCode"] == 200 and data["sourceURL"] == base + "/data.json", data
    assert data["url"] == base + "/data.json" and data["size"] == len(
        b'{"ok": true, "items": [1, 2, 3]}'), data

    markdown = structured(fetch("/doc.md"))
    assert markdown["text"] == "# Doc title\n\nMarkdown body.\n", markdown
    # HTTP/1.1 keepalive: the second request reuses the pooled connection.
    assert server.ports[0] == server.ports[1], server.ports
    assert structured(fetch("/feed.xml"))["text"] == "<feed><title>Fixture</title></feed>"
    assert structured(fetch("/icon.svg"))["encoding"] == "text"
    # Unknown/binary media types fall back to a UTF-8 sniff.
    assert structured(fetch("/unknown"))["text"] == '{"sniffed": true}'
    assert structured(fetch("/empty"))["text"] == ""

    image = structured(fetch("/image.png"))
    assert image["encoding"] == "base64", image
    assert image["contentType"] == "image/png" and image["size"] == len(PNG), image
    assert base64.b64decode(image["base64"]) == PNG, image
    blob = structured(fetch("/blob.bin"))
    assert blob["encoding"] == "base64", blob
    assert base64.b64decode(blob["base64"]) == PNG, blob

    redirected = structured(fetch("/redirect-json"))
    assert redirected["url"] == base + "/data.json", redirected
    assert redirected["sourceURL"] == base + "/redirect-json", redirected
    assert redirected["text"] == '{"ok": true, "items": [1, 2, 3]}', redirected

    missing = structured(fetch("/missing"))
    assert missing["statusCode"] == 404 and "not found" in missing["text"], missing

    html = fetch("/page.html")
    assert html.get("isError") and "use the scrape tool" in html["content"][0]["text"], html
    assert "structuredContent" not in html, html

    slow = fetch("/slow", timeout=100)
    assert slow.get("isError") and "timed out" in slow["content"][0]["text"], slow

    huge = fetch("/huge")
    assert huge.get("isError") and "exceeds 4 MiB" in huge["content"][0]["text"], huge

    for arguments in ({}, {"url": "file:///etc/passwd"}, {"url": "https://example.com", "formats": ["text"]},
                      {"url": "https://example.com", "limit": 1}, {"url": "https://example.com", "timeout": 50},
                      {"url": "https://example.com", "unknown": True}):
        reply = raw_call("tools/call", name="fetch", arguments=arguments)
        assert reply["error"]["code"] == -32602, (arguments, reply)

    # fetch must never start the managed browser.
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    children = [line for line in rows.splitlines() if int(line.split()[1]) == service.process.pid]
    assert not children, children
finally:
    service.process.stdin.close()
    try:
        service.process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service.process.kill()
        service.process.wait()
        raise
    assert service.process.returncode == 0, service.process.returncode

print("PASS: fetch — JSON/Markdown/XML/images, redirects, caps, timeouts, no browser")
server.shutdown()
