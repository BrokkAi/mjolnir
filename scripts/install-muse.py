#!/usr/bin/env python3
"""Install the same verified Muse runtime used by managed workers into an image."""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile


def download(url, destination, expected):
    subprocess.run(["curl", "--proto", "=https", "--tlsv1.2", "-fLsS",
                    "--connect-timeout", "30", "--max-time", "600", "-o", str(destination), url], check=True)
    with destination.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"Muse checksum mismatch: expected {expected}, got {actual}")


def main():
    metadata = json.loads(Path(sys.argv[1]).read_text())
    destination = Path(sys.argv[2])
    system = {"Linux": "linux", "Darwin": "macos"}[platform.system()]
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine()]
    artifact = metadata["platforms"][f"{system}-{arch}"]
    adapter, muse = metadata["adapter_version"], metadata["muse_version"]
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".muse-install-", dir=destination) as scratch:
        staging = Path(scratch)
        archive = staging / "adapter.tar.gz"
        download(f"https://github.com/BrokkAi/muse-acp/releases/download/v{adapter}/muse-acp-v{adapter}-{artifact['adapter_target']}.tar.gz",
                 archive, artifact["adapter_sha256"])
        subprocess.run(["tar", "-xzf", str(archive), "--strip-components=1", "-C", str(staging)], check=True)
        binary = staging / "muse"
        download(f"https://lookaside.facebook.com/lookaside/muse/download/?channel=muse&version={muse}&file=muse-{artifact['muse_target']}",
                 binary, artifact["muse_sha256"])
        binary.chmod(0o755)
        for name in ("muse", "muse-acp"):
            os.replace(staging / name, destination / name)
        notices = destination.parent / "share" / "licenses" / "muse-acp"
        notices.mkdir(parents=True, exist_ok=True)
        for name in ("LICENSE", "NOTICE"):
            os.replace(staging / name, notices / name)


if __name__ == "__main__":
    main()
