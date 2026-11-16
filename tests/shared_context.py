"""Managed Lightpanda shared-context isolation and cleanup controls."""
import http.server
import os
import subprocess
import sys
import tempfile
import threading
import time

from harness import Service, browser_command

binary, browser = sys.argv[1:3]
SAFE_ENV = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR", "LANG", "LC_ALL")
            if key in os.environ}
CHILD_ENV_SENTINEL = "FIREPANDA_CHILD_ENV_SENTINEL"
SAFE_ENV[CHILD_ENV_SENTINEL] = "harmless-probe"


class CookieFixture(http.server.BaseHTTPRequestHandler):
    requests = []

    def do_GET(self):
        type(self).requests.append(self.path)
        if self.path == "/set":
            body = b"<title>set</title><p>cookie set</p>"
            self.send_response(200)
            self.send_header("Set-Cookie", "controlled=present; Path=/")
        else:
            body = (b"<title>read</title><p id=state></p><script>"
                    b"document.getElementById('state').textContent = "
                    b"document.cookie.includes('controlled=present') "
                    b"? 'cookie-present' : 'cookie-missing';</script>")
            self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


def children(pid):
    rows = subprocess.check_output(["ps", "-axo", "pid=,ppid="], text=True)
    return [int(child) for child, parent in
            (line.split() for line in rows.splitlines()) if int(parent) == pid]


def browser_child(service):
    pids = children(service.process.pid)
    assert len(pids) == 1, pids
    command = subprocess.check_output(["ps", "-p", str(pids[0]), "-o", "command="],
                                      text=True)
    assert "lightpanda" in command.lower(), command
    return pids[0]


def scrape(service, url):
    return service.call("tools/call", name="scrape", arguments={
        "url": url, "formats": ["text"], "timeout": 5000,
    })


fixture = http.server.ThreadingHTTPServer(("127.0.0.1", 0), CookieFixture)
threading.Thread(target=fixture.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{fixture.server_port}"
services = []
logs = []


def start(*flags, browser_path=browser):
    log = tempfile.TemporaryFile(mode="w+")
    logs.append(log)
    service = Service(browser_command(binary, "lightpanda", browser_path, *flags),
                      env=SAFE_ENV, stderr=log)
    services.append(service)
    return service


try:
    with tempfile.TemporaryDirectory(prefix="firepanda-env-probe-", dir="/tmp") as probe_dir:
        probe_file = os.path.join(probe_dir, "result")
        wrapper = os.path.join(probe_dir, "lightpanda-env-probe")
        with open(wrapper, "w", encoding="utf-8") as script:
            script.write(
                "#!/bin/sh\n"
                f"if [ -n \"${{PATH:-}}\" ] && [ -n \"${{HOME:-}}\" ] "
                f"&& [ -z \"${{{CHILD_ENV_SENTINEL}+x}}\" ]; then\n"
                f"  printf 'allowed-and-sentinel-absent\\n' > '{probe_file}'\n"
                "else\n"
                f"  printf 'failed\\n' > '{probe_file}'\n"
                "fi\n"
                f"exec '{browser}' \"$@\"\n"
            )
        os.chmod(wrapper, 0o700)
        shared = start("--shared-browser-context", "--idle-timeout", "1",
                       browser_path=wrapper)
        first = scrape(shared, base + "/set")
        assert not first.get("isError"), first
        assert open(probe_file, encoding="utf-8").read().strip() == "allowed-and-sentinel-absent"
        original_child = browser_child(shared)
        time.sleep(1.2)  # Shared mode must not use the ordinary idle reaper.
        second = scrape(shared, base + "/read")
        assert not second.get("isError"), second
        assert "cookie-present" in second["content"][0]["text"], second
        assert browser_child(shared) == original_child

        # The default continues to allocate and dispose one isolated context/page.
        fresh = start("--idle-timeout", "0")
        assert not scrape(fresh, base + "/set").get("isError")
        default_read = scrape(fresh, base + "/read")
        assert not default_read.get("isError"), default_read
        assert "cookie-missing" in default_read["content"][0]["text"], default_read

        # A managed browser restart invalidates the pinned owner permanently.
        child_pid = browser_child(shared)
        os.kill(child_pid, 9)
        lost = scrape(shared, base + "/read")
        assert lost.get("isError"), lost
        refused = scrape(shared, base + "/read")
        assert refused.get("isError"), refused
        assert "closed" in refused["content"][0]["text"].lower(), refused
        shared.process.stdin.close()
        shared.process.wait(timeout=8)
        assert not children(shared.process.pid), children(shared.process.pid)

        # Private-network guard applies in the shared mode before the local GET.
        before = len(CookieFixture.requests)
        guarded = start("--shared-browser-context", "--no-private-network")
        blocked = scrape(guarded, base + "/read")
        assert blocked.get("isError"), blocked
        assert len(CookieFixture.requests) == before, CookieFixture.requests[before:]

        # EOF cleanup must close the stdio server and reap its owned browser child.
        shutdown = start("--shared-browser-context")
        assert not scrape(shutdown, base + "/set").get("isError")
        owned_child = browser_child(shutdown)
        service_pid = shutdown.process.pid
        shutdown.process.stdin.close()
        shutdown.process.wait(timeout=8)
        assert shutdown.process.returncode == 0, shutdown.process.returncode
        deadline = time.monotonic() + 5
        while owned_child in children(service_pid) and time.monotonic() < deadline:
            time.sleep(0.05)
        assert not children(service_pid), children(service_pid)

    print("PASS: shared Lightpanda context continuity, default isolation, fail-closed owner loss, private guard, child env allowlist, and EOF cleanup")
finally:
    for service in services:
        if service.process.poll() is None:
            if service.process.stdin and not service.process.stdin.closed:
                service.process.stdin.close()
            try:
                service.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                service.process.kill()
                service.process.wait(timeout=5)
    fixture.shutdown()
    fixture.server_close()
    for log in logs:
        log.close()
