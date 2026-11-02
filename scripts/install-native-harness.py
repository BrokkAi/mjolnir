#!/usr/bin/env python3
"""Install a pinned native harness release used by managed workers into an image.

    install-native-harness.py <kimi|grok|opencode> <runtime.json> <directory>

Each harness downloads the same per-platform artifact, from the same sources
and against the same digest, as its managed installation in
mj-core/src/harness_install.rs, and lays it out the same way under
<directory>: `bin/kimi`; `bin/grok` with its relative `bin/agent` launcher;
and `bin/opencode`, taken from `package/bin/opencode` in the npm tarball.
"""
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


def download(name, urls, destination, expected):
    """Download the first reachable url to destination and fail unless its SHA-256 is expected."""
    failures = []
    for url in urls:
        try:
            subprocess.run(["curl", "--proto", "=https", "--tlsv1.2", "-fLsS",
                            "--connect-timeout", "30", "--max-time", "600", "-o", str(destination), url],
                           check=True)
        except subprocess.CalledProcessError as error:
            failures.append(f"{url}: {error}")
            continue
        with destination.open("rb") as source:
            actual = hashlib.file_digest(source, "sha256").hexdigest()
        if actual != expected:
            raise ValueError(f"{name} checksum mismatch from {url}: expected {expected}, got {actual}")
        return
    raise RuntimeError(f"could not download {name} from any source: {'; '.join(failures)}")


def kimi(version, artifact, staging):
    filename = artifact["filename"]
    binary = staging / "bin" / "kimi"
    download("Kimi Code", [f"https://code.kimi.com/kimi-code/binaries/{version}/{filename}",
                           f"https://code.kimi.ai/kimi-code/binaries/{version}/{filename}"],
             binary, artifact["sha256"])
    binary.chmod(0o755)


def grok(version, artifact, staging):
    filename = f"grok-{version}-{artifact['target']}.zst"
    archive = staging / filename
    download("Grok", [f"https://x.ai/cli/{filename}",
                      f"https://storage.googleapis.com/grok-build-public-artifacts/cli/{filename}"],
             archive, artifact["sha256"])
    binary = staging / "bin" / "grok"
    subprocess.run(["zstd", "--quiet", "--decompress", str(archive), "-o", str(binary)], check=True)
    archive.unlink()
    binary.chmod(0o755)
    (staging / "bin" / "agent").symlink_to("grok")


def opencode(version, artifact, staging):
    target = artifact["target"]
    filename = f"cli-{target}-{version}.tgz"
    archive = staging / filename
    download("OpenCode", [f"https://registry.npmjs.org/@opencode/cli-{target}/-/{filename}"],
             archive, artifact["sha256"])
    with tarfile.open(archive) as source:
        member = source.getmember("package/bin/opencode")
        if not member.isreg():
            raise ValueError("OpenCode archive entry package/bin/opencode is not a regular file")
        with source.extractfile(member) as entry, (staging / "bin" / "opencode").open("wb") as output:
            shutil.copyfileobj(entry, output)
    archive.unlink()
    (staging / "bin" / "opencode").chmod(0o755)


def main():
    install = {"kimi": kimi, "grok": grok, "opencode": opencode}[sys.argv[1]]
    metadata = json.loads(Path(sys.argv[2]).read_text())
    destination = Path(sys.argv[3])
    system = {"Linux": "linux", "Darwin": "macos"}[platform.system()]
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine()]
    artifact = metadata["platforms"][f"{system}-{arch}"]
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        raise FileExistsError(f"{destination} already exists")
    with tempfile.TemporaryDirectory(prefix=f".{sys.argv[1]}-install-", dir=destination.parent) as scratch:
        staging = Path(scratch) / "install"
        (staging / "bin").mkdir(parents=True)
        install(metadata["version"], artifact, staging)
        os.replace(staging, destination)


if __name__ == "__main__":
    main()
