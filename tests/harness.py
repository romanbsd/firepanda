"""Shared line-delimited JSON-RPC client for the firepanda service (stdlib only)."""
import json
import selectors
import subprocess
import tempfile

PROTOCOL_VERSION = "2026-07-28"
META = {"io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION}


def message(identifier, method, **params):
    """One JSON-RPC envelope without metadata, for tests that control _meta themselves."""
    return {"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}


def structured(result):
    """Return the tool payload; results above 1 MiB omit the duplicate structuredContent."""
    if "structuredContent" in result:
        return result["structuredContent"]
    return json.loads(result["content"][0]["text"])


def run_batch(command, messages, timeout=15):
    """Feed pipelined messages on a seekable stdin and collect every reply.

    Batch input can exceed pipe capacity while responses are still being
    written, so these runs use a temporary file instead of an interactive pipe.
    """
    data = "\n".join(m if isinstance(m, str) else json.dumps(m) for m in messages) + "\n"
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as source:
        source.write(data)
        source.seek(0)
        result = subprocess.run(command, stdin=source, capture_output=True, text=True,
                                timeout=timeout, check=True)
    return [json.loads(line) for line in result.stdout.splitlines()]


def budget(method, params):
    """Reply budget: per-page timeout plus slack, and the fixed crawl/map budget."""
    seconds = params.get("arguments", {}).get("timeout", 30000) / 1000 + 10
    if method == "tools/call" and params.get("name") in ("crawl", "map"):
        return max(seconds, 130)  # fixed 120-second crawl budget plus transport slack
    return seconds


class Service:
    """One firepanda process with a sequential JSON-RPC session."""

    def __init__(self, command, *, env=None, stderr=None):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=stderr,
                                        text=True, bufsize=1, env=env)
        self.sequence = 0

    def post(self, method, **params):
        """Write one request with modern metadata; do not wait for its reply."""
        self.sequence += 1
        params["_meta"] = META
        self.process.stdin.write(json.dumps(message(self.sequence, method, **params)) + "\n")
        self.process.stdin.flush()
        return self.sequence

    def call(self, method, **params):
        identifier = self.post(method, **params)
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            assert selector.select(budget(method, params)), \
                "Service did not answer within its test budget"
        response = json.loads(self.process.stdout.readline())
        assert response["id"] == identifier, response
        assert "error" not in response, response
        return response["result"]

    def tool(self, name, **arguments):
        result = self.call("tools/call", name=name, arguments=arguments)
        assert not result.get("isError"), result
        return structured(result)
