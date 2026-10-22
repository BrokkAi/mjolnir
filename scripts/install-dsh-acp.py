#!/usr/bin/env python3
"""Install the verified dsh-acp release binary used by managed workers into an image.

The metadata is mj-worker/assets/dsh/runtime.json, which also pins the binary
managed bare workers install, so the image and those workers run one release.
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


def download(url, destination, expected):
    subprocess.run(["curl", "--proto", "=https", "--tlsv1.2", "-fLsS",
                    "--connect-timeout", "30", "--max-time", "600", "-o", str(destination), url], check=True)
    with destination.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"dsh-acp checksum mismatch: expected {expected}, got {actual}")


def main():
    metadata = json.loads(Path(sys.argv[1]).read_text())
    destination = Path(sys.argv[2])
    system = {"Linux": "linux", "Darwin": "macos"}[platform.system()]
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine()]
    artifact = metadata["platforms"][f"{system}-{arch}"]
    version = metadata["version"]
    directory = f"dsh-acp-v{version}-{artifact['target']}"
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".dsh-acp-install-", dir=destination) as scratch:
        staging = Path(scratch)
        archive = staging / f"{directory}.tar.gz"
        download(f"https://github.com/BrokkAi/dsh-acp/releases/download/v{version}/{directory}.tar.gz",
                 archive, artifact["sha256"])
        with tarfile.open(archive) as bundle:
            member = bundle.getmember(f"{directory}/dsh-acp")
            if not member.isfile():
                raise ValueError(f"{directory}/dsh-acp is not a regular file")
            source = bundle.extractfile(member)
            binary = staging / "dsh-acp"
            with binary.open("wb") as target:
                target.write(source.read())
        binary.chmod(0o755)
        os.replace(binary, destination / "dsh-acp")


if __name__ == "__main__":
    main()
