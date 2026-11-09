"""Shared local website for native integration and memory workloads."""
import http.server
import time


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        content_type = "text/html"
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/two")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path == "/http-redirect":
            self.send_response(302)
            self.send_header("Location", "/http-error")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path == "/p/broken":
            # An invalid status line aborts the navigation immediately while the
            # crawl window still holds sibling pages in flight.
            self.wfile.write(b"not-an-http-response\r\n\r\n")
            self.close_connection = True
            return
        if self.path == "/slow":
            time.sleep(2)
        if self.path in ("/ready", "/ready-no-observer"):
            body = (b'<html><title>Ready</title><main><p id="target">Loading</p></main><script>'
                    b'const expected="Content ready";'
                    b'setTimeout(()=>{document.getElementById("target").innerHTML="<span id=ready>Content </span>";},120);'
                    b'setTimeout(()=>{document.getElementById("ready").innerHTML+="<b>ready</b><a href=/two>Two</a>";},300);'
                    b'</script></html>')
            if self.path == '/ready-no-observer':
                body = body.replace(b'<script>', b'<script>globalThis.MutationObserver=undefined;', 1)
        elif self.path == '/ready-character-data':
            body = (b'<html><body><main><p id="target">Loading</p></main><script>'
                    b'setTimeout(()=>{document.getElementById("target").firstChild.data="Character ready";},120);'
                    b'</script></body></html>')
        elif self.path == '/ready-body-replacement':
            body = (b'<html><body><main>Loading</main><script>'
                    b'setTimeout(()=>{document.body.innerHTML="<main>Replacement ready</main>";},120);'
                    b'</script></body></html>')
        elif self.path == '/ready-selector-state':
            body = (b'<html><body><main>Stable text<input id="target" type="checkbox"></main><script>'
                    b'setTimeout(()=>{document.getElementById("target").checked=true;},120);'
                    b'</script></body></html>')
        elif self.path == "/never":
            body = b'<html><main>Loading</main><script>const expected="Content ready";</script></html>'
        elif self.path == "/challenge":
            body = b'<html><title>Just a moment...</title><main>Performing security verification</main></html>'
        elif self.path == "/error-page":
            body = b'<html><title>Editor</title><main>Error 500 Internal Error Return to Home</main></html>'
        elif self.path == "/http-error":
            body = b'<html><title>Unavailable</title><main>Please try later.</main></html>'
        elif self.path == "/http-ok-subframe":
            body = b'<html><main>Good document<iframe src="/http-error"></iframe></main></html>'
        elif self.path == "/ordinary-article":
            body = ('<html><title>HTTP troubleshooting</title><main><h1>Working with errors</h1>'
                    '<p>Learn about CAPTCHA systems and Error 500 responses.</p>' + '<p>Helpful content.</p>' * 100 + '</main></html>').encode()
        elif self.path == "/empty":
            body = b'<html><main></main></html>'
        elif self.path == "/redirect-root":
            body = b'<html><title>Redirect root</title><main><a href="/redirect">Redirect</a><a href="/two">Direct</a></main></html>'
        elif self.path == "/parallel-root":
            body = (b'<html><title>Parallel root</title><main>'
                    b'<a href="/p/a">A</a><a href="/p/b">B</a><a href="/p/c">C</a></main></html>')
        elif self.path == "/flaky-root":
            body = (b'<html><title>Flaky root</title><main>'
                    b'<a href="/p/broken">Broken</a><a href="/p/slow-a">Slow A</a>'
                    b'<a href="/p/slow-b">Slow B</a></main></html>')
        elif self.path.startswith("/p/"):
            time.sleep(1)
            body = f'<html><title>{self.path}</title><main>Parallel {self.path}</main></html>'.encode()
        elif self.path == "/listing":
            body = (b'<html><head><title>Listing</title></head><body><header>Site chrome</header>'
                    b'<nav>Navigation noise</nav><section><h1>All products</h1>'
                    b'<article><header><h2>First product</h2></header><p>First description</p></article>'
                    b'<article><header><h2>Second product</h2></header><p>Second description</p></article>'
                    b'</section><footer>Footer noise</footer></body></html>')
        elif self.path == "/article-header":
            body = (b'<html><head><title>Article</title></head><body><header>Site chrome</header>'
                    b'<aside><article>Sidebar teaser</article></aside><article>'
                    b'<header><h1>Article headline</h1><p>Article author</p></header>'
                    b'<p>Article body</p></article><footer>Footer noise</footer></body></html>')
        elif self.path == "/section-header":
            body = (b'<html><body><header>Site chrome</header><section>'
                    b'<header><h2>Section headline</h2></header><p>Section body</p>'
                    b'</section></body></html>')
        elif self.path == "/hidden-main":
            body = (b'<html><body><section><header><h1>Visible app</h1></header>'
                    b'<input placeholder="New item"><main id="empty"><p>Hidden empty list</p></main>'
                    b'<main hidden>Hidden attribute</main><div role="main" aria-hidden="true">Hidden ARIA</div>'
                    b'<main style="visibility: hidden !important">Hidden visibility</main>'
                    b'<script>document.getElementById("empty").style.display="none";</script>'
                    b'</section></body></html>')
        elif self.path in ("/main-priority", "/role-main-priority"):
            tag = 'main' if self.path == '/main-priority' else 'div role="main"'
            end_tag = 'main' if self.path == '/main-priority' else 'div'
            body = (f'<html><body><article>Early teaser</article><{tag}>'
                    '<header><h1>Main headline</h1></header><p>Main body</p>'
                    f'</{end_tag}><article>Late teaser</article></body></html>').encode()
        elif self.path == "/anchor-without-href":
            body = (b'<html><body><main><a id="named">Named anchor</a>'
                    b'<a href="">Current page</a><a href="child">Relative link</a>'
                    b'<a href="javascript:void(0)">Action</a></main></body></html>')
        elif self.path == "/code-base":
            body = (b'<html><head><title>Code</title><base href="/assets/"></head><body><main>'
                    b'<a href="child">Child</a><a href="">Base</a><a href="#part">Fragment</a>'
                    b'<a href="http://[invalid">Invalid</a><a href="mailto:test@example.com">Mail</a>'
                    b'<pre><code>first\n\n\n```literal\nlast\n</code></pre>'
                    b'<code>value`inside</code></main></body></html>')
        elif self.path == "/markdown-structure":
            body = (b'<html><head><title>Structure</title></head><body><main>'
                    b'<p># not a heading</p><p>1. not a list</p>'
                    b'<ol start="3"><li>three</li><li>four<ul><li>nested</li></ul></li></ol>'
                    b'<table><tr><th>Name</th><th>Note</th></tr><tr><td>a|b</td><td>x</td></tr></table>'
                    b'<p><img src="/pic.png" alt="Picture"><img src="data:image/png;base64,AA==" alt="Inline"></p>'
                    b'<blockquote><p>Quoted</p><ul><li>point</li></ul></blockquote>'
                    b'<ul><li>item<pre>a\n\n\nb\n</pre></li></ul>'
                    b'<p><b></b>Plain - dash <a href="#top">Jump</a></p>'
                    b'<div>\n      Indented\n\n\n\n      text</div><pre>  keep\n    indent</pre>'
                    b'</main></body></html>')
        elif self.path == "/wide-frontier":
            # Force unordered_set rehash while queue entries still reference its keys.
            body = ('<html><main>' + ''.join(
                f'<a href="/leaf/{i}">Leaf {i}</a>' for i in range(300)) + '</main></html>').encode()
        elif self.path.startswith('/leaf/'):
            body = f'<html><main>{self.path}</main></html>'.encode()
        elif self.path in ('/budget-root', '/budget-leaf'):
            body = ('<html><main>' + 'x' * (2 * 1024 * 1024) +
                    '<a href="/budget-leaf">Next</a></main></html>').encode()
        elif self.path == "/data.json":
            body = b'{"ok": true, "items": [1, 2, 3], "nested": {"a": "b"}}'
            content_type = "application/json"
        elif self.path == "/doc.md":
            body = b"# Doc title\n\nMarkdown body.\n"
            content_type = "text/markdown; charset=utf-8"
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
        self.send_response(503 if self.path == "/http-error" else 200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
