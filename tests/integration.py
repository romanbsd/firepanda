"""Actual native Lightpanda + local fixture website. No external website needed."""
import http.server
import os
import shlex
import subprocess
import sys
import threading
import time
import tempfile

binary, lightpanda = sys.argv[1:3]


from fixtures import Fixture
from harness import Service, structured


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = "http://127.0.0.1:" + str(server.server_port)
service = Service([binary, "--lightpanda", lightpanda])


def children():
    """Inspect only the service's direct child; do not infer global orphans."""
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    return [int(pid) for pid, ppid in (line.split() for line in rows.splitlines())
            if int(ppid) == service.process.pid]


def child():
    pids = children()
    assert len(pids) == 1, pids
    command = subprocess.check_output(["ps", "-p", str(pids[0]), "-o", "command="], text=True)
    args = shlex.split(command)
    path = args[args.index("--unix-socket") + 1]
    return pids[0], path


def rpc(method, **params):
    return service.call(method, **params)


def tool(name, **arguments):
    return service.tool(name, **arguments)


try:
    tool_list = rpc("tools/list")
    assert len(tool_list["tools"]) == 3
    ready = tool("scrape", url=url + "/ready", waitForSelector="#ready", waitForText="Content ready", timeout=3000)
    assert "**ready**" in ready["markdown"], ready
    assert ready["metadata"]["httpStatus"] == 200, ready
    for path, phrase in (('/ready-character-data', 'Character ready'),
                         ('/ready-body-replacement', 'Replacement ready'),
                         ('/ready-no-observer', 'Content ready')):
        changed = tool('scrape', url=url + path, waitForText=phrase, formats=['text'], timeout=3000)
        assert phrase in changed['text'], changed
    state = tool('scrape', url=url + '/ready-selector-state', waitForText='Stable text',
                 waitForSelector='#target:checked', timeout=3000)
    assert 'Stable text' in state['markdown'], state
    for arguments in ({"waitForText": "Content ready"}, {"waitForSelector": "#absent"}, {"waitForSelector": "["}):
        failed = rpc("tools/call", name="scrape", arguments={"url": url + "/never", "timeout": 350, **arguments})
        assert failed["isError"], (arguments, failed)
    ready_map = tool("map", url=url + "/ready", waitForText="Content ready", limit=1, maxDepth=0)
    assert len(ready_map["links"]) == 1 and not ready_map["errors"], ready_map
    challenge = tool("scrape", url=url + "/challenge")
    assert "suspected_block" in [w["code"] for w in challenge["warnings"]], challenge
    assert "verification" in challenge["markdown"], challenge
    error_page = tool("scrape", url=url + "/error-page")
    assert error_page["metadata"]["httpStatus"] == 200, error_page
    assert [w["code"] for w in error_page["warnings"]] == ["suspected_error_page"], error_page
    actual_error = tool("scrape", url=url + "/http-redirect")
    assert actual_error["metadata"]["httpStatus"] == 503, actual_error
    assert [w["code"] for w in actual_error["warnings"]] == ["http_error"], actual_error
    good = tool("scrape", url=url + "/http-ok-subframe")
    assert good["metadata"]["httpStatus"] == 200 and "warnings" not in good, good
    assert "warnings" not in tool("scrape", url=url + "/ordinary-article")
    empty = tool("scrape", url=url + "/empty")
    assert [w["code"] for w in empty["warnings"]] == ["empty_content"], empty
    warning_map = tool("map", url=url + "/http-error", limit=1, maxDepth=0)
    assert warning_map["warnings"][0]["warnings"][-1]["code"] == "http_error", warning_map
    warning_crawl = tool("crawl", url=url + "/http-error", limit=1, maxDepth=0)
    assert warning_crawl["pages"][0]["metadata"]["httpStatus"] == 503, warning_crawl
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
    assert f'[Fragment](<{url}/assets/#part>)' in extracted["markdown"], extracted
    assert {link['url'] for link in extracted['links']} == {url + '/assets/', url + '/assets/child'}, extracted
    assert 'Invalid' in extracted['markdown'] and 'mailto:' not in extracted['markdown'], extracted
    wide = tool("crawl", url=url + "/wide-frontier", limit=100, maxDepth=1)
    assert not wide['errors'] and wide['attempted'] == 100 and wide['truncated'], wide
    assert [page['metadata']['url'] for page in wide['pages'][1:]] == [url + f'/leaf/{i}' for i in range(99)], wide
    budget_result = rpc("tools/call", name="crawl", arguments={
        "url": url + "/budget-root", "formats": ['text'], "limit": 2, "maxDepth": 1})
    assert not budget_result.get("isError"), budget_result
    assert "structuredContent" not in budget_result, "large results must not duplicate the payload"
    budget = structured(budget_result)
    assert not budget['errors'] and budget['truncated'] and budget['attempted'] == 2, budget.keys()
    assert len(budget['pages']) == 1 and len(budget['pages'][0]['text']) > 2 * 1024 * 1024, budget.keys()
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
    assert not children(), "Failed scrape must reap its owned browser"
    assert not os.path.exists(os.path.dirname(socket_path)), "Failed scrape left its socket directory"
    recovered = tool("scrape", url=url)
    assert "Rendered by JavaScript" in recovered["markdown"]
    final_child, socket_path = child()
    assert final_child != first_child, "Timeout must replace the interrupted child"
    print("PASS: native UDS WebSocket, JS, formats, crawl/map, timeout recovery, reuse, private permissions")
finally:
    service.process.stdin.close()
    try:
        service.process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service.process.kill()
        service.process.wait()
        raise
assert service.process.returncode == 0, service.process.returncode
assert not os.path.exists(os.path.dirname(socket_path)), "Private socket directory leaked"
try:
    os.kill(final_child, 0)
except ProcessLookupError:
    pass
else:
    raise AssertionError("Native Lightpanda child survived EOF")

# The MCP service remains available after its lazily started browser goes idle.
service = Service([binary, "--lightpanda", lightpanda, "--idle-timeout", "1"])
try:
    assert len(rpc("tools/list")["tools"]) == 3
    assert not children(), "Tool discovery must not launch the browser"
    tool("scrape", url=url)
    idle_pid, idle_socket = child()
    # No further MCP input: pselect must wake at the idle deadline itself.
    until = time.monotonic() + 4
    while children() and time.monotonic() < until:
        time.sleep(0.05)
    assert not children(), "Idle browser was not stopped and reaped"
    assert not os.path.exists(os.path.dirname(idle_socket)), "Idle socket directory leaked"
    assert service.process.poll() is None, "Idle shutdown must keep the MCP service running"
    rpc("ping")
    assert not children(), "Ping must not relaunch the browser"
    tool("scrape", url=url)
    active_pid, _ = child()
    assert active_pid != idle_pid, "Next browser operation must cold-start a new child"
    tool("scrape", url=url, waitFor=1300, timeout=10000)
    assert child()[0] == active_pid, "Idle timeout interrupted an active request"
    time.sleep(0.6)
    tool("scrape", url=url)
    time.sleep(0.6)
    assert child()[0] == active_pid, "Browser activity did not renew the idle deadline"
    # Continuous non-browser MCP activity must not indefinitely defer expiration.
    until = time.monotonic() + 4
    while children() and time.monotonic() < until:
        rpc("ping")
        rpc("tools/list")
        time.sleep(0.05)
    assert not children(), "MCP housekeeping kept the browser alive"
finally:
    service.process.stdin.close()
    service.process.wait(timeout=10)
assert service.process.returncode == 0

service = Service([binary, "--lightpanda", lightpanda, "--idle-timeout", "0"])
try:
    tool("scrape", url=url)
    warm_pid, _ = child()
    time.sleep(1.3)
    assert child()[0] == warm_pid, "Disabled idle timeout stopped the browser"
finally:
    service.process.stdin.close()
    service.process.wait(timeout=10)
assert service.process.returncode == 0
print("PASS: lazy launch, quiet/active MCP idle expiry, restart, active-request protection, deadline renewal, disable")

# An independently managed browser must survive idle timeouts and Firepanda shutdown.
with tempfile.TemporaryDirectory(prefix="firepanda-external-", dir="/tmp") as directory:
    sock = directory + "/cdp.sock"
    external = subprocess.Popen([lightpanda, "serve", "--unix-socket", sock, "--log-level", "error"])
    try:
        for _ in range(200):
            if os.path.exists(sock):
                break
            assert external.poll() is None, "External browser exited"
            time.sleep(0.01)
        service = Service([binary, "--socket", sock, "--idle-timeout", "1"])
        try:
            assert len(rpc("tools/list")["tools"]) == 3
            assert "Rendered by JavaScript" in tool("scrape", url=url)["markdown"]
            time.sleep(1.3)
            assert external.poll() is None, "Idle timeout terminated an external browser"
            failed = rpc("tools/call", name="scrape", arguments={"url": url + "/slow", "timeout": 100})
            assert failed["isError"] and external.poll() is None, failed
            assert os.path.exists(sock), "Failure cleanup removed an external socket"
            assert "Rendered by JavaScript" in tool("scrape", url=url)["markdown"]
        finally:
            service.process.stdin.close()
            service.process.wait(timeout=10)
        assert service.process.returncode == 0, service.process.returncode
        assert external.poll() is None, "Firepanda terminated the external browser"
    finally:
        external.terminate()
        external.wait(timeout=10)
print("PASS: EOF cleanup and external-browser ownership")

# Exercise both a blocked CDP read and the post-load wait, which must be interruptible.
def check_sigterm(arguments):
    global service
    service = Service([binary, "--lightpanda", lightpanda])
    try:
        tool("scrape", url=url)
        owned_pid, owned_socket = child()
        # Post the request without waiting: SIGTERM must interrupt the running call.
        service.post("tools/call", name="scrape", arguments=arguments)
        time.sleep(0.1)
        service.process.terminate()
        service.process.wait(timeout=5)
        assert service.process.returncode == 0, service.process.returncode
        assert not os.path.exists(os.path.dirname(owned_socket)), "Socket directory survived SIGTERM"
        try:
            os.kill(owned_pid, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError("Owned browser survived SIGTERM")
    finally:
        service.process.stdin.close()
        if service.process.poll() is None:
            service.process.kill()
            service.process.wait()


try:
    check_sigterm({"url": url + "/slow"})
    check_sigterm({"url": url, "waitFor": 30000, "timeout": 60000})
    check_sigterm({"url": url + "/never", "waitForSelector": "#absent", "timeout": 60000})
finally:
    server.shutdown()
print("PASS: SIGTERM during navigation and post-load wait cleans up the owned browser")
