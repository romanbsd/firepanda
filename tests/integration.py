"""Actual native Lightpanda + local fixture website. No external website needed."""
import http.server
import json
import os
import selectors
import shlex
import subprocess
import sys
import threading
import time
import tempfile

binary, lightpanda = sys.argv[1:3]


from fixtures import Fixture


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = "http://127.0.0.1:" + str(server.server_port)
process = subprocess.Popen([binary, "--lightpanda", lightpanda], stdin=subprocess.PIPE,
                           stdout=subprocess.PIPE, text=True, bufsize=1)
sequence = 0


def child():
    """Inspect only the service's direct child; do not infer global orphans."""
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    pids = [int(pid) for pid, ppid in (line.split() for line in rows.splitlines())
            if int(ppid) == process.pid]
    assert len(pids) == 1, pids
    command = subprocess.check_output(["ps", "-p", str(pids[0]), "-o", "command="], text=True)
    args = shlex.split(command)
    path = args[args.index("--unix-socket") + 1]
    return pids[0], path


def rpc(method, **params):
    global sequence
    sequence += 1
    params["_meta"] = {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": sequence, "method": method, "params": params}) + "\n")
    process.stdin.flush()
    with selectors.DefaultSelector() as sel:
        sel.register(process.stdout, selectors.EVENT_READ)
        seconds = params.get("arguments", {}).get("timeout", 30000) / 1000 + 10
        if method == "tools/call" and params.get("name") in ("crawl", "map"):
            seconds = max(seconds, 130)  # fixed 120-second crawl budget plus transport slack
        assert sel.select(seconds), "Service did not answer within its test budget"
    response = json.loads(process.stdout.readline())
    assert response["id"] == sequence, response
    assert "error" not in response, response
    return response["result"]


def tool(name, **arguments):
    result = rpc("tools/call", name=name, arguments=arguments)
    assert not result.get("isError"), result
    return result["structuredContent"]


try:
    tool_list = rpc("tools/list")
    assert len(tool_list["tools"]) == 3
    listing = tool("scrape", url=url + "/listing", formats=["markdown", "text", "html", "rawHtml"])
    for text in ("All products", "First product", "First description", "Second product", "Second description"):
        for format in ("markdown", "text", "html"):
            assert text in listing[format], (format, text, listing[format])
    for text in ("Site chrome", "Navigation noise", "Footer noise"):
        assert text not in listing["text"], listing["text"]
        assert text in listing["rawHtml"], listing["rawHtml"]
    article = tool("scrape", url=url + "/article-header")
    assert "# Article headline" in article["markdown"], article
    assert "Article author" in article["markdown"] and "Article body" in article["markdown"], article
    assert "Site chrome" not in article["markdown"] and "Sidebar teaser" not in article["markdown"], article
    section = tool("scrape", url=url + "/section-header")
    assert "## Section headline" in section["markdown"] and "Section body" in section["markdown"], section
    assert "Site chrome" not in section["markdown"], section
    visible = tool("scrape", url=url + "/hidden-main", formats=["markdown", "text", "html", "rawHtml"])
    assert "# Visible app" in visible["markdown"], visible
    assert "New item" in visible["html"], visible
    assert "Hidden" not in visible["text"], visible
    assert "Hidden empty list" in visible["rawHtml"], visible
    for path in ("/main-priority", "/role-main-priority"):
        main = tool("scrape", url=url + path)
        assert "# Main headline" in main["markdown"] and "Main body" in main["markdown"], main
        assert "teaser" not in main["markdown"], main
    full = tool("scrape", url=url + "/listing", onlyMainContent=False)
    assert "Site chrome" in full["markdown"] and "Second product" in full["markdown"], full
    anchors = tool("scrape", url=url + "/anchor-without-href", formats=["markdown", "links"])
    assert "Named anchor" in anchors["markdown"] and "/null" not in anchors["markdown"], anchors
    assert f'[Current page](<{url}/anchor-without-href>)' in anchors["markdown"], anchors
    assert f'[Relative link](<{url}/child>)' in anchors["markdown"], anchors
    assert "javascript:" not in anchors["markdown"] and "Action" in anchors["markdown"], anchors
    assert len(anchors["links"]) == 2, anchors
    page = tool("scrape", url=url, formats=["markdown", "text", "html", "rawHtml", "links"])
    assert "Rendered by JavaScript" in page["markdown"], page
    assert "# Native Firepanda" in page["markdown"], page
    assert "Noise" not in page["markdown"], page
    assert "Noise" in page["rawHtml"]
    assert len(page["links"]) == 2, page
    assert page["metadata"]["title"] == "Fixture"
    extracted = tool("scrape", url=url + "/code-base", formats=["markdown", "links"])
    assert url + "/assets/child" in extracted["markdown"], extracted["markdown"]
    assert "first\n\n\n```literal\nlast\n" in extracted["markdown"], extracted["markdown"]
    assert "``value`inside``" in extracted["markdown"], extracted["markdown"]
    redirects = tool("crawl", url=url + "/redirect-root", limit=5, maxDepth=1)
    assert not redirects["errors"], redirects["errors"]
    assert redirects["attempted"] == 2 and len(redirects["pages"]) == 2, redirects
    redirect_cap = tool("crawl", url=url + "/redirect-root", limit=2, maxDepth=1)
    assert not redirect_cap["truncated"] and len(redirect_cap["pages"]) == 2, redirect_cap
    first_child, socket_path = child()
    assert os.stat(os.path.dirname(socket_path)).st_mode & 0o777 == 0o700
    assert os.stat(socket_path).st_mode & 0o777 == 0o600
    crawl = tool("crawl", url=url, limit=3, maxDepth=1)
    assert len(crawl["pages"]) == 2 and not crawl["errors"], crawl
    assert not crawl["truncated"], crawl
    mapping = tool("map", url=url, limit=1, maxDepth=1)
    assert len(mapping["links"]) == 2 and mapping["truncated"], mapping
    assert mapping["attempted"] == 1, mapping
    # These cases test count limits, not deadlines; allow sanitizer overhead.
    exact = tool("map", url=url + "/links-exact", limit=1, maxDepth=0, timeout=120000)
    assert not exact["errors"], exact["errors"]
    assert len(exact["links"]) == 10000 and not exact["truncated"], (len(exact["links"]), exact["truncated"])
    overflow = tool("crawl", url=url + "/links-overflow", formats=["links"], limit=1, maxDepth=0, timeout=120000)
    assert not overflow["errors"], overflow["errors"]
    assert len(overflow["pages"][0]["links"]) == 10000 and overflow["truncated"]
    assert child()[0] == first_child, "Browser must be reused between calls"
    failure = rpc("tools/call", name="scrape", arguments={"url": url + "/slow", "timeout": 100})
    assert failure["isError"], failure
    recovered = tool("scrape", url=url)
    assert "Rendered by JavaScript" in recovered["markdown"]
    final_child, socket_path = child()
    assert final_child != first_child, "Timeout must replace the interrupted child"
    print("PASS: native UDS WebSocket, JS, formats, crawl/map, timeout recovery, reuse, private permissions")
finally:
    process.stdin.close()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
        raise
assert process.returncode == 0, process.returncode
assert not os.path.exists(os.path.dirname(socket_path)), "Private socket directory leaked"
try:
    os.kill(final_child, 0)
except ProcessLookupError:
    pass
else:
    raise AssertionError("Native Lightpanda child survived EOF")

# An independently managed browser must survive Firepanda shutdown.
with tempfile.TemporaryDirectory(prefix="firepanda-external-", dir="/tmp") as directory:
    sock = directory + "/cdp.sock"
    external = subprocess.Popen([lightpanda, "serve", "--unix-socket", sock, "--log-level", "error"])
    try:
        for _ in range(200):
            if os.path.exists(sock):
                break
            assert external.poll() is None, "External browser exited"
            time.sleep(0.01)
        process = subprocess.Popen([binary, "--socket", sock], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, text=True, bufsize=1)
        try:
            assert len(rpc("tools/list")["tools"]) == 3
            assert "Rendered by JavaScript" in tool("scrape", url=url)["markdown"]
        finally:
            process.stdin.close()
            process.wait(timeout=10)
        assert process.returncode == 0, process.returncode
        assert external.poll() is None, "Firepanda terminated the external browser"
    finally:
        external.terminate()
        external.wait(timeout=10)
print("PASS: EOF cleanup and external-browser ownership")

# Exercise both a blocked CDP read and the post-load wait, which must be interruptible.
def check_sigterm(arguments):
    global process, sequence
    process = subprocess.Popen([binary, "--lightpanda", lightpanda], stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, text=True, bufsize=1)
    try:
        tool("scrape", url=url)
        owned_pid, owned_socket = child()
        sequence += 1
        process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": sequence, "method": "tools/call", "params": {
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"},
            "name": "scrape", "arguments": arguments}}) + "\n")
        process.stdin.flush()
        time.sleep(0.1)
        process.terminate()
        process.wait(timeout=5)
        assert process.returncode == 0, process.returncode
        assert not os.path.exists(os.path.dirname(owned_socket)), "Socket directory survived SIGTERM"
        try:
            os.kill(owned_pid, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError("Owned browser survived SIGTERM")
    finally:
        process.stdin.close()
        if process.poll() is None:
            process.kill()
            process.wait()


try:
    check_sigterm({"url": url + "/slow"})
    check_sigterm({"url": url, "waitFor": 30000, "timeout": 60000})
finally:
    server.shutdown()
print("PASS: SIGTERM during navigation and post-load wait cleans up the owned browser")
