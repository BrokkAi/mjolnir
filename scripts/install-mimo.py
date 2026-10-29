#!/usr/bin/env python3
"""Install the verified native MiMo Code release used by managed workers into an image.

The `@mimo-ai/cli` npm package is only a launcher that downloads this archive
on first run without a published checksum, so the image installs the archive
directly against the digest pinned in mj-core/assets/mimo/runtime.json.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import zipfile


def download(url, destination, expected):
    """Download url to destination and fail unless its SHA-256 is expected."""
    subprocess.run(["curl", "--proto", "=https", "--tlsv1.2", "-fLsS",
                    "--connect-timeout", "30", "--max-time", "600", "-o", str(destination), url], check=True)
    with destination.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"MiMo Code checksum mismatch: expected {expected}, got {actual}")


def extract(archive, staging):
    """Extract the root `mimo` executable from a release archive into staging."""
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            source.extract("mimo", staging)
    else:
        with tarfile.open(archive) as source:
            source.extract("mimo", staging, filter="data")
    return staging / "mimo"


def main():
    """Install this platform's pinned `mimo`: install-mimo.py <runtime.json> <bin directory>."""
    metadata = json.loads(Path(sys.argv[1]).read_text())
    destination = Path(sys.argv[2])
    system = {"Linux": "linux", "Darwin": "macos"}[platform.system()]
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine()]
    artifact = metadata["platforms"][f"{system}-{arch}"]
    version = metadata["version"]
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".mimo-install-", dir=destination) as scratch:
        staging = Path(scratch)
        archive = staging / artifact["archive"]
        download(f"https://github.com/XiaomiMiMo/MiMo-Code/releases/download/v{version}/{artifact['archive']}",
                 archive, artifact["sha256"])
        binary = extract(archive, staging)
        binary.chmod(0o755)
        os.replace(binary, destination / "mimo")


if __name__ == "__main__":
    main()
