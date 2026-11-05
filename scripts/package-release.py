#!/usr/bin/env python3
"""Package the tested native binary and notices; Lightpanda is installed separately."""
import argparse
import hashlib
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--output', type=Path, default=ROOT / 'dist')
parser.add_argument('--tag', help='Require the tag to match the executable version')
args = parser.parse_args()
version = subprocess.check_output([str(args.binary.resolve()), '--version'], text=True).strip().removeprefix('firepanda ')
if not re.fullmatch(r'\d+\.\d+\.\d+', version):
    parser.error('Unexpected executable version')
if args.tag and args.tag != 'v' + version:
    parser.error('Release tag must equal v' + version)
system = {'Darwin': 'macos', 'Linux': 'linux'}.get(platform.system())
if not system:
    parser.error('Unsupported operating system')
name = f'firepanda-{version}-{system}-{platform.machine()}'
args.output.mkdir(parents=True, exist_ok=True)
archive = args.output / (name + '.tar.gz')
with tempfile.TemporaryDirectory() as directory:
    stage = Path(directory) / name
    (stage / 'bin').mkdir(parents=True)
    shutil.copy2(args.binary, stage / 'bin/firepanda')
    for file in ('README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md'):
        shutil.copy2(ROOT / file, stage / file)
    for folder in ('LICENSES', 'docs'):
        shutil.copytree(ROOT / folder, stage / folder)
    (stage / 'scripts').mkdir()
    shutil.copy2(ROOT / 'scripts/install-lightpanda.py', stage / 'scripts/install-lightpanda.py')
    with tarfile.open(archive, 'w:gz') as output:
        output.add(stage, arcname=name)
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_suffix(archive.suffix + '.sha256').write_text(f'{digest}  {archive.name}\n')
print(archive)
