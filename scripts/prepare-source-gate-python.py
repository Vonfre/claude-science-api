#!/usr/bin/env python3
"""Prepare pinned wheel payloads for the macOS arm64 Python 3.9 source gate.

Fetch wheels separately using the lock; this installer never contacts a network.
"""
import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import pwd
import re
import shutil
import stat
import tempfile
import zipfile


def prepare(wheel_dir, destination, lock):
    entries = []
    for line in lock.read_text().splitlines():
        if not line.strip() or line.startswith('#'):
            continue
        match = re.fullmatch(r'([\w-]+)==([\w.]+) --hash=sha256:([0-9a-f]{64})', line)
        if not match:
            raise ValueError('invalid dependency lock')
        name, version, digest = match.groups()
        candidates = list(wheel_dir.glob(name.replace('-', '_') + '-' + version + '-*.whl'))
        if len(candidates) != 1:
            raise ValueError('exactly one locked wheel required: ' + name)
        payload = candidates[0].read_bytes()
        if hashlib.sha256(payload).hexdigest() != digest:
            raise ValueError('wheel digest mismatch: ' + name)
        entries.append((candidates[0], payload))
    if not entries:
        raise ValueError('empty dependency lock')
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists() or destination.is_symlink():
        raise ValueError('destination already exists; use a fresh versioned cache')
    staging = Path(tempfile.mkdtemp(prefix='.wheel-stage-', dir=destination.parent))
    try:
        import io
        for wheel, payload in entries:
            with zipfile.ZipFile(io.BytesIO(payload)) as archive:
                for item in archive.infolist():
                    path = PurePosixPath(item.filename)
                    mode = item.external_attr >> 16
                    if (path.is_absolute() or '..' in path.parts or '\\' in item.filename
                            or stat.S_ISLNK(mode) or not path.parts):
                        raise ValueError('unsafe wheel member')
                    target = staging.joinpath(*path.parts)
                    if item.is_dir():
                        target.mkdir(parents=True, exist_ok=True)
                        continue
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with target.open('xb') as output:
                        output.write(archive.read(item))
                    target.chmod(0o600)
        os.rename(staging, destination)
    finally:
        if staging.exists():
            shutil.rmtree(staging)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wheel-dir', type=Path, required=True)
    args = parser.parse_args()
    root = Path(pwd.getpwuid(os.geteuid()).pw_dir)
    destination = root / 'Library/Caches/SciPort/source-gate/python3.9-wheels-v1/site-packages'
    prepare(args.wheel_dir, destination, Path(__file__).resolve().parents[1] / 'quality/source-gate-python.lock')
    print('Prepared pinned source-gate dependencies (no global packages modified).')


if __name__ == '__main__':
    main()
