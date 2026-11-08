"""Temporary M3 gate: core render assertions from tests/integration.py.

integration.py is monolithic and hits `map`/`crawl` (M4) before the
formats/warnings/anchors sections. This runner executes exactly those
core sections (integration.py lines 67-80 and 85-129) against the Rust
binary so M3 can be verified before M4 lands. Not part of the suite.
"""
import http.server
import subprocess
import sys
import threading
from pathlib import Path

binary, lightpanda = sys.argv[1:3]
sys.path.insert(0, str(Path(__file__).resolve().parent))

from fixtures import Fixture
from harness import Service

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = "http://127.0.0.1:" + str(server.server_port)
service = Service([binary, "--lightpanda", lightpanda])


def tool(name, **arguments):
    return service.tool(name, **arguments)


try:
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
    print("PASS: M3 core — warnings pipeline, formats, JS render, anchors, ready state")
finally:
    service.process.stdin.close()
    try:
        service.process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        service.process.kill()
        service.process.wait()
        raise
assert service.process.returncode == 0, service.process.returncode
server.shutdown()
