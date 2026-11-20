"""Host-side evidence that must survive destruction of the engine workspace."""
from collections import defaultdict
import hashlib
import json
import os


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def save_bytes(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(path.name + ".tmp")
    with temp.open("wb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    temp.replace(path)
    sync_directory(path.parent)


def save_json(path, value):
    save_bytes(path, (json.dumps(value, sort_keys=True, allow_nan=False) + "\n").encode("utf-8"))


def cross_query_reuse_summary(provenances):
    measured, reused = defaultdict(set), defaultdict(set)
    for provenance in provenances:
        for turn in provenance.get("turns", ()):
            if "cross_query_reuse" not in turn:
                continue
            identity = (turn["session"], turn["turn"])
            for step in provenance.get("steps") or ("documented",):
                measured[step].add(identity)
                if turn["cross_query_reuse"]:
                    reused[step].add(identity)
    return {"enforced": False, "by_step": {
        step: {"measured_turns": len(measured[step]), "reuse_turns": len(reused[step])}
        for step in sorted(measured)
    }}


def file_digests(directory):
    """Flush and index the complete captured tree before any destroy request."""
    hashes = {}
    directories = [directory]
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"unexpected symlink in engine evidence: {path}")
        if path.is_dir():
            directories.append(path)
        elif path.is_file():
            with path.open("rb") as source:
                hashes[path.relative_to(directory).as_posix()] = hashlib.file_digest(source, "sha256").hexdigest()
                os.fsync(source.fileno())
        else:
            raise ValueError(f"unexpected file type in engine evidence: {path}")
    for path in reversed(directories):
        sync_directory(path)
    return hashes
