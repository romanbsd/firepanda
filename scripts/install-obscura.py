#!/usr/bin/env python3
"""Install the Obscura headless browser after checking the release archive digest."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import tarfile
import tempfile
import urllib.request

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--release", default="latest", help="GitHub release tag (default: latest)")
parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[1] / ".local/bin/obscura")
parser.add_argument("--stealth", action="store_true", help="Install the stealth transport variant")
parser.add_argument("--no-render", action="store_true", help="Install the variant without the render layer")
args = parser.parse_args()
system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "AMD64": "x86_64"}.get(platform.machine())
if not system or not arch:
    parser.error("Only macOS/Linux on arm64 or x86_64 are supported")
suffix = ("-no-render" if args.no_render else "") + ("-stealth" if args.stealth else "")
name = f"obscura-{arch}-{system}{suffix}.tar.gz"
if args.release != "latest" and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.release):
    parser.error("Invalid release tag")
release_url = (
    "https://api.github.com/repos/h4ckf0r0day/obscura/releases/latest"
    if args.release == "latest"
    else "https://api.github.com/repos/h4ckf0r0day/obscura/releases/tags/" + args.release
)


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "firepanda-native-installer"})
    return urllib.request.urlopen(request, timeout=120)


with fetch(release_url) as response:
    release = json.load(response)
asset = next((a for a in release["assets"] if a["name"] == name), None)
if not asset or not asset.get("digest", "").startswith("sha256:"):
    raise SystemExit("Release asset or its SHA-256 digest is unavailable; no executable installed")
args.output.parent.mkdir(parents=True, exist_ok=True)
archive = None
try:
    digest = hashlib.sha256()
    with tempfile.NamedTemporaryFile(dir=args.output.parent, delete=False) as output:
        archive = Path(output.name)
        with fetch(asset["browser_download_url"]) as response:
            while chunk := response.read(1024 * 1024):
                digest.update(chunk)
                output.write(chunk)
    actual = digest.hexdigest()
    if actual != asset["digest"].removeprefix("sha256:"):
        raise SystemExit("SHA-256 mismatch; executable rejected")
    with tempfile.TemporaryDirectory(dir=args.output.parent) as directory:
        staged = Path(directory)
        with tarfile.open(archive) as bundle:
            members = {Path(member.name).name: member for member in bundle.getmembers() if member.isfile()}
            for binary in ("obscura", "obscura-worker"):
                if binary not in members:
                    raise SystemExit(f"Release archive is missing {binary}; nothing installed")
                with bundle.extractfile(members[binary]) as source, open(staged / binary, "wb") as target:
                    while chunk := source.read(1024 * 1024):
                        target.write(chunk)
                (staged / binary).chmod(0o755)
        os.replace(staged / "obscura", args.output)
        os.replace(staged / "obscura-worker", args.output.with_name("obscura-worker"))
    manifest = {"release": release["tag_name"], "asset": name, "sha256": actual, "updated_at": asset["updated_at"]}
    # Append to the full name: replacing a suffix can collide with the binary
    # itself (e.g. --output obscura.json) or with another versioned install.
    manifest_path = args.output.parent / (args.output.name + ".json")
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Installed {args.output}\nSHA-256: {actual}")
finally:
    if archive is not None:
        archive.unlink(missing_ok=True)
