#!/usr/bin/env python3
"""Install the pinned, verified Antigravity CLI into agent-dev."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


def main():
    metadata = json.loads(Path(sys.argv[1]).read_text())
    destination = Path(sys.argv[2])
    system = {"Linux": "linux", "Darwin": "macos"}[platform.system()]
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine()]
    artifact = metadata["platforms"][f"{system}-{arch}"]
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".agy-install-", dir=destination) as scratch:
        staging = Path(scratch)
        archive = staging / "agy.tar.gz"
        with urllib.request.urlopen(artifact["url"]) as source, archive.open("wb") as output:
            shutil.copyfileobj(source, output)
        for algorithm in ("sha256", "sha512"):
            with archive.open("rb") as source:
                actual = hashlib.file_digest(source, algorithm).hexdigest()
            if actual != artifact[algorithm]:
                raise ValueError(f"Antigravity {algorithm} checksum mismatch: {actual}")
        binary = staging / "agy"
        with tarfile.open(archive) as source:
            entry = source.getmember("antigravity")
            if not entry.isfile():
                raise ValueError("Antigravity archive entry is not a regular file")
            with source.extractfile(entry) as payload, binary.open("wb") as output:
                shutil.copyfileobj(payload, output)
        binary.chmod(0o755)
        environment = dict(os.environ, AGY_CLI_DISABLE_AUTO_UPDATE="true")
        version = subprocess.run([str(binary), "--version"], env=environment,
                                 stdout=subprocess.PIPE, check=True).stdout.decode().strip()
        if version != metadata["version"]:
            raise ValueError(f"Antigravity version mismatch: {version}")
        os.replace(binary, destination / "agy")


if __name__ == "__main__":
    main()
