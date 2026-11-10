"""Installer integrity and input validation without network access or executable downloads."""
import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/install-lightpanda.py"
OBSCURA_SCRIPT = Path(__file__).resolve().parents[1] / "scripts/install-obscura.py"
PAYLOAD = b"test-native-executable"


def obscura_archive():
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as bundle:
        for name in ("obscura", "obscura-worker"):
            data = b"binary:" + name.encode()
            member = tarfile.TarInfo(name)
            member.size = len(data)
            member.mode = 0o755
            bundle.addfile(member, io.BytesIO(data))
    return buffer.getvalue()


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


class ObscuraInstaller(unittest.TestCase):
    def install(self, output, digest=None):
        archive = obscura_archive()
        digest = digest or hashlib.sha256(archive).hexdigest()
        metadata = {"tag_name": "v0.2.6", "assets": [{
            "name": "obscura-x86_64-linux.tar.gz", "digest": "sha256:" + digest,
            "browser_download_url": "https://example.invalid/obscura.tar.gz", "updated_at": "test"}]}

        def fetch(request, **_):
            if request.full_url == "https://api.github.com/repos/h4ckf0r0day/obscura/releases/latest":
                return io.BytesIO(json.dumps(metadata).encode())
            if request.full_url == "https://example.invalid/obscura.tar.gz":
                return io.BytesIO(archive)
            self.fail("Unexpected network URL: " + request.full_url)

        with patch.object(sys, "argv", [str(OBSCURA_SCRIPT), "--output", str(output)]), \
                patch("platform.system", return_value="Linux"), patch("platform.machine", return_value="x86_64"), \
                patch("urllib.request.urlopen", side_effect=fetch) as network, \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            try:
                runpy.run_path(str(OBSCURA_SCRIPT), run_name="__main__")
            except SystemExit as failure:
                if failure.code not in (None, 0):
                    raise
        return network

    def test_installs_both_binaries_and_a_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "obscura"
            self.install(output)
            for name in ("obscura", "obscura-worker"):
                binary = Path(directory) / name
                self.assertEqual(binary.read_bytes(), b"binary:" + name.encode())
                self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
            manifest = json.loads(Path(directory, "obscura.json").read_text())
            self.assertEqual(manifest["release"], "v0.2.6")
            self.assertEqual(manifest["sha256"], hashlib.sha256(obscura_archive()).hexdigest())

    def test_digest_failure_installs_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "obscura"
            with self.assertRaisesRegex(SystemExit, "SHA-256 mismatch"):
                self.install(output, digest="0" * 64)
            self.assertEqual(list(Path(directory).iterdir()), [])


if __name__ == "__main__":
    unittest.main()
