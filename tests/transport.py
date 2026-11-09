"""Adversarial CDP peers over actual Unix-socket WebSockets; no third-party packages."""
import base64
import hashlib
import json
import selectors
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

BINARY = sys.argv[1]
sys.argv = sys.argv[:1]

from harness import META, message as envelope


def frame(payload, opcode=1, final=True):
    if isinstance(payload, dict):
        payload = json.dumps(payload).encode()
    header = bytes([(0x80 if final else 0) | opcode])
    length = len(payload)
    if length < 126:
        header += bytes([length])
    elif length <= 65535:
        header += bytes([126]) + struct.pack("!H", length)
    else:
        header += bytes([127]) + struct.pack("!Q", length)
    return header + payload


def exact(connection, size):
    result = bytearray()
    while len(result) < size:
        chunk = connection.recv(size - len(result))
        if not chunk:
            raise EOFError
        result.extend(chunk)
    return result


def message(connection):
    while True:
        first, second = exact(connection, 2)
        length = second & 127
        if length == 126:
            length = struct.unpack("!H", exact(connection, 2))[0]
        elif length == 127:
            length = struct.unpack("!Q", exact(connection, 8))[0]
        mask = exact(connection, 4) if second & 128 else b""
        payload = exact(connection, length)
        if mask:
            payload = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        if first & 15 == 8:
            raise EOFError
        if first & 15 == 1:
            return json.loads(payload)


class Peer:
    def __init__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="fp-cdp-", dir="/tmp")
        self.path = self.directory.name + "/cdp.sock"
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(self.path)
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.stop = threading.Event()
        self.entered = threading.Event()
        self.mode = "valid"
        self.connection = None
        self.errors = []
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stop.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            self.connection = connection
            try:
                connection.settimeout(5)
                # One-byte reads deliberately avoid swallowing the first WS frame.
                header = bytearray()
                while not header.endswith(b"\r\n\r\n"):
                    header.extend(exact(connection, 1))
                key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n")
                           if line.lower().startswith(b"sec-websocket-key:"))
                accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
                while not self.stop.is_set():
                    request = message(connection)
                    method = request["method"]
                    reply = {"id": request["id"], "result": {}}
                    if "sessionId" in request:
                        reply["sessionId"] = request["sessionId"]
                    if method == "Target.createBrowserContext":
                        reply["result"] = {"browserContextId": "context"}
                    elif method == "Target.createTarget":
                        reply["result"] = {"targetId": "page"}
                    elif method == "Target.attachToTarget":
                        reply["result"] = {"sessionId": "session"}
                    elif method == "Network.enable" and self.mode == "no_network":
                        reply.pop("result")
                        reply["error"] = {"code": -32601, "message": "unsupported"}
                    elif method == "Page.navigate":
                        self.entered.set()
                        if self.mode in ("pings", "fragments"):
                            # Keep a blocking read active past its inactivity timeout.
                            if self.mode == "fragments":
                                connection.sendall(frame(b"{", final=False))
                            until = time.monotonic() + 4
                            while not self.stop.is_set() and time.monotonic() < until:
                                connection.sendall(frame(b"", opcode=9 if self.mode == "pings" else 0, final=self.mode == "pings"))
                                time.sleep(0.02)
                            continue
                        # The navigate reply commits the main frame. Status and
                        # load events follow it; a load sent before the reply
                        # is an about:blank event and is ignored.
                        reply["result"] = {"frameId": "main"}
                        connection.sendall(frame(reply))
                        if self.mode in ("status", "fractional_status"):
                            for frame_id, status in (("main", 201 if self.mode == "status" else 201.5), ("child", 503)):
                                connection.sendall(frame({"method": "Network.responseReceived", "sessionId": "session", "params": {
                                    "type": "Document", "frameId": frame_id, "response": {"status": status}}}))
                        connection.sendall(frame({"method": "Page.loadEventFired", "sessionId": "session"}))
                        continue
                    elif method == "Target.disposeBrowserContext" and self.mode == "dispose_error":
                        reply.pop("result")
                        reply["error"] = {"code": -32000, "message": "dispose failed"}
                    elif method == "Runtime.evaluate":
                        if self.mode == "missing_result":
                            reply.pop("result")
                        elif self.mode == "conflicting_result":
                            reply["error"] = {"code": -1, "message": "broken"}
                        elif self.mode == "missing_format":
                            reply["result"] = {"result": {"value": {"metadata": {"url": "https://example.com/"}}}}
                        elif self.mode == "deep_json":
                            connection.sendall(frame(b"[" * 5000 + b"0" + b"]" * 5000))
                            continue
                        else:
                            reply["result"] = {"result": {"value": {"metadata": {"url": "https://example.com/", "title": "Mock"}, "markdown": "hello"}}}
                            if self.mode == "bad_warnings":
                                reply["result"]["result"]["value"]["warnings"] = [False]
                    connection.sendall(frame(reply))
            except (OSError, EOFError):
                pass
            except Exception as error:
                self.errors.append(error)
            finally:
                connection.close()
                self.connection = None

    def close(self):
        self.stop.set()
        self.listener.close()
        if self.connection:
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.thread.join(timeout=6)
        self.directory.cleanup()
        assert not self.thread.is_alive(), "CDP peer thread leaked"
        assert not self.errors, self.errors


class Transport(unittest.TestCase):
    def setUp(self):
        self.peer = Peer()
        self.process = subprocess.Popen([BINARY, "--socket", self.peer.path], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, text=True)
        self.sequence = 0

    def tearDown(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process.stdout.close()
        self.peer.close()
        self.assertEqual(self.process.returncode, 0)

    def send(self, timeout=1000):
        self.sequence += 1
        self.process.stdin.write(json.dumps(envelope(
            self.sequence, "tools/call", _meta=META, name="scrape",
            arguments={"url": "https://example.com", "timeout": timeout})) + "\n")
        self.process.stdin.flush()

    def receive(self):
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(3), "CDP peer extended the operation deadline")
        response = json.loads(self.process.stdout.readline())
        self.assertNotIn("error", response)
        return response["result"]

    def test_malformed_reply_and_recovery(self):
        for mode in ("missing_result", "conflicting_result", "missing_format", "deep_json", "bad_warnings"):
            with self.subTest(mode=mode):
                self.peer.mode = mode
                self.send()
                self.assertTrue(self.receive()["isError"])
                self.peer.mode = "valid"
                self.send()
                self.assertEqual(json.loads(self.receive()["content"][0]["text"])["markdown"], "hello")

    def test_status_is_observed_main_document_only_and_optional(self):
        self.peer.mode = "status"
        self.send()
        self.assertEqual(json.loads(self.receive()["content"][0]["text"])["metadata"]["statusCode"], 201)
        for mode in ("valid", "no_network", "fractional_status"):
            self.peer.mode = mode
            self.send()
            self.assertNotIn("statusCode", json.loads(self.receive()["content"][0]["text"])["metadata"])

    def test_control_frame_and_fragment_deadlines(self):
        for mode in ("pings", "fragments"):
            with self.subTest(mode=mode):
                self.peer.mode = mode
                self.peer.entered.clear()
                start = time.monotonic()
                self.send(timeout=150)
                self.assertTrue(self.peer.entered.wait(2))
                self.assertTrue(self.receive()["isError"])
                self.assertLess(time.monotonic() - start, 2)

    def test_dispose_failure_still_returns_the_page(self):
        self.peer.mode = "dispose_error"
        self.send()
        body = self.receive()
        self.assertFalse(body.get("isError"), body)
        self.assertEqual(json.loads(body["content"][0]["text"])["markdown"], "hello")

    def test_sigterm_during_fragment_stream(self):
        self.peer.mode = "fragments"
        self.send(timeout=120000)
        self.assertTrue(self.peer.entered.wait(2))
        self.process.terminate()
        self.process.wait(timeout=3)


if __name__ == "__main__":
    unittest.main()
