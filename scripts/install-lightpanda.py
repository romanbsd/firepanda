#!/usr/bin/env python3
"""Install the native upstream binary after checking GitHub's asset digest."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import tempfile
import urllib.request

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--release", default="nightly", help="GitHub release tag (default: nightly)")
parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[1] / ".local/bin/lightpanda")
args = parser.parse_args()
system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "AMD64": "x86_64"}.get(platform.machine())
if not system or not arch:
    parser.error("Only macOS/Linux on arm64 or x86_64 are supported")
name = f"lightpanda-{arch}-{system}"
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.release):
    parser.error("Invalid release tag")


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "firepanda-native-installer"})
    return urllib.request.urlopen(request, timeout=120)


with fetch("https://api.github.com/repos/lightpanda-io/browser/releases/tags/" + args.release) as response:
    release = json.load(response)
asset = next((a for a in release["assets"] if a["name"] == name), None)
if not asset or not asset.get("digest", "").startswith("sha256:"):
    raise SystemExit("Release asset or its SHA-256 digest is unavailable; no executable installed")
args.output.parent.mkdir(parents=True, exist_ok=True)
temporary = None
try:
    digest = hashlib.sha256()
    with tempfile.NamedTemporaryFile(dir=args.output.parent, delete=False) as output:
        temporary = Path(output.name)
        with fetch(asset["browser_download_url"]) as response:
            while chunk := response.read(1024 * 1024):
                digest.update(chunk)
                output.write(chunk)
    actual = digest.hexdigest()
    if actual != asset["digest"].removeprefix("sha256:"):
        raise SystemExit("SHA-256 mismatch; executable rejected")
    temporary.chmod(0o755)
    os.replace(temporary, args.output)
    manifest = {"release": args.release, "asset": name, "sha256": actual, "updated_at": asset["updated_at"]}
    # Append to the full name: replacing a suffix can collide with the binary
    # itself (e.g. --output lightpanda.json) or with another versioned install.
    manifest_path = args.output.parent / (args.output.name + ".json")
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Installed {args.output}\nSHA-256: {actual}")
finally:
    if temporary is not None:
        temporary.unlink(missing_ok=True)
