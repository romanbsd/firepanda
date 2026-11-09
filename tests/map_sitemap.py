"""Map sitemap merge against a local fixture. No external website needed."""
import http.server
import subprocess
import sys
import threading

binary, lightpanda = sys.argv[1:3]

from fixtures import Fixture
from harness import Service


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
origin = "http://127.0.0.1:" + str(server.server_port)
service = Service([binary, "--lightpanda", lightpanda])


def tool(**arguments):
    return service.tool("map", url=origin + "/company-start", limit=5, maxDepth=2, **arguments)


def urls(result):
    return [link["url"] for link in result["links"]]


try:
    plain = tool()
    listed = tool(sitemap=True)
    assert plain["attempted"] == listed["attempted"], (plain["attempted"], listed["attempted"])
    assert origin + "/sitemap-only" not in urls(plain), plain
    assert plain["omittedRendered"] == 0 and plain["omittedSitemap"] == 0, plain

    link_urls = urls(listed)
    careers = next(link for link in listed["links"] if link["url"] == origin + "/careers")
    assert link_urls.count(origin + "/company") == 1, listed
    assert careers.get("title") == "Careers", listed
    assert link_urls.index(origin + "/careers") < link_urls.index(origin + "/sitemap-only") < link_urls.index(origin + "/second-doc"), listed
    assert link_urls.index(origin + "/careers/role") < link_urls.index(origin + "/sitemap-only"), listed
    assert {"url": origin + "/sitemap-only"} in listed["links"], listed
    assert "https://example.com/outside" in link_urls, listed
    assert "https://other.test/off" not in link_urls, listed
    assert listed["errors"], listed
    assert listed["attempted"] == plain["attempted"], listed

    filtered = tool(sitemap=True, includePaths=["/careers"])
    filtered_urls = urls(filtered)
    assert origin + "/company" not in filtered_urls, filtered
    assert origin + "/careers" in filtered_urls, filtered
    assert origin + "/careers/role" in filtered_urls, filtered
    assert filtered["attempted"] == listed["attempted"], (filtered["attempted"], listed["attempted"])

    capped = tool(sitemap=True, linkLimit=1)
    assert urls(capped) == [origin + "/company"], capped
    assert capped["omittedRendered"] == 2, capped
    assert capped["omittedSitemap"] == 3, capped
    assert capped["truncated"] is True, capped
    print("PASS")
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
