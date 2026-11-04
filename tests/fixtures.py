"""Shared local website for native integration and memory workloads."""
import http.server
import time


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/two")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path == "/slow":
            time.sleep(2)
        if self.path == "/redirect-root":
            body = b'<html><title>Redirect root</title><main><a href="/redirect">Redirect</a><a href="/two">Direct</a></main></html>'
        elif self.path == "/code-base":
            body = (b'<html><head><title>Code</title><base href="/assets/"></head><body><main>'
                    b'<a href="child">Child</a><pre><code>first\n\n\n```literal\nlast\n</code></pre>'
                    b'<code>value`inside</code></main></body></html>')
        elif self.path in ("/links-exact", "/links-overflow"):
            count = 10000 if self.path == "/links-exact" else 10001
            body = ("<html><title>Links</title><main>" +
                    "".join(f'<a href="/link/{i}">Link {i}</a>' for i in range(count)) +
                    "</main></html>").encode()
        elif self.path in ("/large", "/too-large"):
            size = 256 * 1024 if self.path == "/large" else 9 * 1024 * 1024
            body = ("<html><title>Large fixture</title><main><p>" + "x" * size + "</p></main></html>").encode()
        else:
            body = ("<html><head><title>Fixture</title></head><body><nav>Noise</nav><main>"
                    "<h1>Native Firepanda</h1><p id='dynamic'>Before JS</p>"
                    "<a href='/two#anchor'>Two</a><a href='/two'>Duplicate</a>"
                    "<a href='https://outside.invalid/'>External</a>"
                    "<script>document.getElementById('dynamic').textContent='Rendered by JavaScript';</script>"
                    "</main></body></html>").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
