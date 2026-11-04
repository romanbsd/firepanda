"""Installer integrity and input validation without network access or executable downloads."""
import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/install-lightpanda.py"
PAYLOAD = b"test-native-executable"


class Installer(unittest.TestCase):
    def install(self, output, release="1.0.2", digest=None):
        digest = digest or hashlib.sha256(PAYLOAD).hexdigest()
        metadata = {"assets": [{"name": "lightpanda-x86_64-linux", "digest": "sha256:" + digest,
                                "browser_download_url": "https://example.invalid/asset", "updated_at": "test"}]}

        def fetch(request, **_):
            if request.full_url == "https://api.github.com/repos/lightpanda-io/browser/releases/tags/1.0.2":
                return io.BytesIO(json.dumps(metadata).encode())
            if request.full_url == "https://example.invalid/asset":
                return io.BytesIO(PAYLOAD)
            self.fail("Unexpected network URL: " + request.full_url)

        with patch.object(sys, "argv", [str(SCRIPT), "--release", release, "--output", str(output)]), \
                patch("platform.system", return_value="Linux"), patch("platform.machine", return_value="x86_64"), \
                patch("urllib.request.urlopen", side_effect=fetch) as network, \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            runpy.run_path(str(SCRIPT), run_name="__main__")
        return network.call_count

    def test_sidecar_never_replaces_binary_or_truncates_version(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in ("lightpanda", "lightpanda-1.0.2", "lightpanda.json"):
                with self.subTest(name=name):
                    output = Path(directory) / name
                    self.assertEqual(self.install(output), 2)
                    self.assertEqual(output.read_bytes(), PAYLOAD)
                    self.assertEqual(output.stat().st_mode & 0o777, 0o755)
                    manifest = json.loads(output.with_name(name + ".json").read_text())
                    self.assertEqual(manifest["sha256"], hashlib.sha256(PAYLOAD).hexdigest())

    def test_invalid_tag_never_reaches_network(self):
        with tempfile.TemporaryDirectory() as directory:
            for tag in ("", ".", "..", "...", "v1-é", "v1/next", "v1\nx"):
                with self.subTest(tag=tag):
                    with self.assertRaises(SystemExit) as failure:
                        self.install(Path(directory) / "binary", release=tag)
                    self.assertEqual(failure.exception.code, 2)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_digest_failure_keeps_existing_binary_and_removes_temp(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "binary"
            output.write_bytes(b"existing")
            with self.assertRaisesRegex(SystemExit, "SHA-256 mismatch"):
                self.install(output, digest="0" * 64)
            self.assertEqual(output.read_bytes(), b"existing")
            self.assertEqual(list(Path(directory).iterdir()), [output])


if __name__ == "__main__":
    unittest.main()
