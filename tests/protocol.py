"""Black-box MCP contract tests. No browser, Node, or third-party Python packages."""
import json
import os
import subprocess
import sys
import unittest

from harness import META, PROTOCOL_VERSION, message, run_batch

BINARY = sys.argv[1]
sys.argv = sys.argv[:1]


def run(messages):
    return run_batch([BINARY, "--lightpanda", "/missing/lightpanda"], messages)


class Protocol(unittest.TestCase):
    def test_version_matches_mcp_identity(self):
        cli = subprocess.run([BINARY, "--version"], capture_output=True, text=True, timeout=5, check=True)
        version = cli.stdout.strip().split()[1]
        response = run([message(1, "initialize", protocolVersion="2025-11-25", capabilities={},
                                clientInfo={"name": "test", "version": "1"})])[0]
        self.assertEqual(response['result']['serverInfo']['version'], version)

    def test_idle_timeout_cli_validation(self):
        for value in ("", "+1", "-1", "1.5", " 1", "1 ", "nan", "86401", str(2**64)):
            result = subprocess.run([BINARY, "--idle-timeout", value], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 2, (value, result))
        for value in ("0", "1", "60", "86400"):
            result = subprocess.run([BINARY, "--idle-timeout", value], input="", capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, (value, result))
        self.assertEqual(subprocess.run([BINARY, "--idle-timeout"], capture_output=True, timeout=5).returncode, 2)

    def test_modern_stateless_and_legacy(self):
        out = run([
            message(1, "tools/list", _meta=META),
            message(2, "server/discover", _meta=META),
            message(3, "tools/list"),
            message(4, "initialize", protocolVersion="2025-11-25", capabilities={}, clientInfo={"name": "test", "version": "1"}),
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            message(5, "tools/list"), message(6, "ping"),
            message(7, "ping", _meta={"io.modelcontextprotocol/protocolVersion": "2099-01-01"}),
        ])
        self.assertEqual([x["id"] for x in out], list(range(1, 8)))
        self.assertEqual({t["name"] for t in out[0]["result"]["tools"]}, {"scrape", "crawl", "map"})
        self.assertEqual(out[0]["result"]["resultType"], "complete")
        self.assertIn(PROTOCOL_VERSION, out[1]["result"]["supportedVersions"])
        self.assertEqual(out[2]["error"]["code"], -32600)
        self.assertNotIn("resultType", out[4]["result"])
        self.assertEqual(out[5]["result"], {})
        self.assertEqual(out[6]["error"]["code"], -32022)
        for tool in out[0]["result"]["tools"]:
            properties = tool["inputSchema"]["properties"]
            for name in ("waitForSelector", "waitForText"):
                self.assertEqual(properties[name]["type"], "string")
                self.assertEqual(properties[name]["minLength"], 1)

    def test_invalid_requests_recover(self):
        out = run(["{", "[]", message(1, "unknown", _meta=META),
                   message(2, "ping", _meta=[]), "x" * (1024 * 1024 + 1),
                   message(3, "ping", _meta=META)])
        self.assertEqual([x["error"]["code"] for x in out[:-1]], [-32700, -32600, -32601, -32602, -32600])
        self.assertEqual(out[-1]["id"], 3)

    def test_validation_before_starting_browser(self):
        args = [
            {}, {"url": "file:///etc/passwd"}, {"url": "https://user:pass@example.com"},
            {"url": "+https://example.com"}, {"url": "https://example.com:99999"},
            {"url": "http://example.com\\@other.test"}, {"url": "https://example.com", "formats": ["pdf"]},
            {"url": "https://example.com", "timeout": "100"}, {"url": "https://example.com", "waitFor": -1},
            {"url": "https://example.com", "onlyMainContent": "true"},
            {"url": "https://example.com", "formats": []}, {"url": "https://example.com", "unknown": True},
        ]
        out = run([message(i, "tools/call", _meta=META, name="scrape", arguments=a) for i, a in enumerate(args)])
        self.assertEqual([x["id"] for x in out], list(range(len(args))))
        self.assertTrue(all(x["error"]["code"] == -32602 for x in out))
        out = run([message(1, "tools/call", _meta=META, name="scrape", arguments={"url": "https://example.com"})])
        self.assertTrue(out[0]["result"]["isError"])
        self.assertIn("Cannot start Lightpanda", out[0]["result"]["content"][0]["text"])

    def test_depth_limit_and_large_url_recover(self):
        # Both inputs are below the byte limit but used to exercise recursive code.
        deeply_nested = '{"jsonrpc":"2.0","id":1,"method":"ping","params":{"deep":' + '[' * 5000 + '0' + ']' * 5000 + '}}'
        out = run([deeply_nested, message(2, "tools/call", _meta=META, name="scrape", arguments={
            "url": "https://example.com/" + "x" * 8100, "timeout": 100}), message(3, "ping", _meta=META)])
        self.assertEqual(out[0]["error"]["code"], -32600)
        self.assertIn("nesting", out[0]["error"]["message"])
        self.assertTrue(out[1]["result"]["isError"])  # valid URL reaches the missing browser
        self.assertEqual(out[2]["id"], 3)

    def test_unsigned_integer_boundaries(self):
        values = [-1, 0, 2**31, 2**63, 2**64 - 1, 1.5, True]
        for field in ("timeout", "waitFor", "limit", "maxDepth"):
            invalid = [v for v in values if v not in (0,)] if field in ("waitFor", "maxDepth") else values
            out = run([message(i, "tools/call", _meta=META, name="crawl", arguments={
                "url": "https://example.com", field: v}) for i, v in enumerate(invalid)])
            self.assertEqual([x["id"] for x in out], list(range(len(invalid))))
            self.assertTrue(all(x["error"]["code"] == -32602 for x in out), (field, out))

    def test_readiness_validation_before_browser(self):
        for field, maximum in (("waitForSelector", 2048), ("waitForText", 4096)):
            values = [None, False, 1, [], {}, "", "x" * (maximum + 1), "日" * maximum]
            out = run([message(i, "tools/call", _meta=META, name="scrape", arguments={
                "url": "https://example.com", field: value}) for i, value in enumerate(values)])
            self.assertEqual([x["id"] for x in out], list(range(len(values))))
            self.assertTrue(all(x.get("error", {}).get("code") == -32602 for x in out), (field, out))

    def test_invalid_ipv6_rejected_before_browser_start(self):
        urls = ["http://[dead]/", "http://[:::]/", "http://[1:2:3]/"]
        out = run([message(i, "tools/call", _meta=META, name="scrape", arguments={"url": url})
                   for i, url in enumerate(urls)])
        self.assertEqual([x["id"] for x in out], list(range(len(urls))))
        self.assertTrue(all(x.get("error", {}).get("code") == -32602 for x in out), out)

    def test_closed_stdout_exits_without_waiting_for_stdin(self):
        process = subprocess.Popen([BINARY], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            process.stdout.close()
            process.stdin.write(json.dumps(message(1, "ping", _meta=META)) + "\n")
            process.stdin.flush()
            try:
                self.assertEqual(process.wait(timeout=3), 1)
            except subprocess.TimeoutExpired:
                # Preserve a useful stack if the intermittent closed-reader timeout
                # recurs. Inspect only the child created by this test.
                if os.path.exists("/usr/bin/sample"):
                    sample = subprocess.run(["/usr/bin/sample", str(process.pid), "1", "-file", "/dev/stdout"],
                                            capture_output=True, text=True, timeout=5)
                    print(sample.stdout, file=sys.stderr)
                raise
        finally:
            process.stdin.close()
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            process.stderr.close()

    def test_idle_sigterm_and_pipelined_input(self):
        for _ in range(10):
            process = subprocess.Popen([BINARY], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                process.stdin.write(json.dumps(message(1, "ping", _meta=META)) + "\n")
                process.stdin.flush()
                self.assertEqual(json.loads(process.stdout.readline())["id"], 1)
                process.terminate()
                self.assertEqual(process.wait(timeout=3), 0)
            finally:
                process.stdin.close()
                if process.poll() is None:
                    process.kill()
                    process.wait()
                process.stdout.close()
                process.stderr.close()
        out = run([message(i, "ping", _meta=META) for i in range(100)])
        self.assertEqual([r["id"] for r in out], list(range(100)))


if __name__ == "__main__":
    unittest.main()
