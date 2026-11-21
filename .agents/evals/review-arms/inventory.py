"""Runs inside a review container: pins the review start or inventories the checkout.

Reads a JSON request on stdin: {"mode": "pin"|"audit", "cwd": ..., "base": ...}.
Prints a JSON inventory. Read-only except for the pin's one update-ref.
"""
import hashlib
import io
import json
import os
import subprocess
import sys

request = json.load(sys.stdin)


def git(*args, input=None):
    return subprocess.check_output(["git", "-C", request["cwd"], *args], input=input, stderr=subprocess.PIPE)


def text(*args):
    return git(*args).decode().strip()


def symlink_targets(links):
    if not links:
        return {}
    blobs = io.BytesIO(git("cat-file", "--batch", input=b"".join(oid + b"\n" for oid in links.values())))
    targets = {}
    for path, oid in links.items():
        header = blobs.readline().split()
        if len(header) != 3 or header[:2] != [oid, b"blob"]:
            raise ValueError(f"missing symlink blob for {path!r}")
        size = int(header[2])
        target = blobs.read(size)
        if len(target) != size or blobs.read(1) != b"\n":
            raise ValueError(f"truncated symlink blob for {path!r}")
        targets[path] = os.fsdecode(target)
    return targets


start_refs = [r for r in text("for-each-ref", "--format=%(refname)").splitlines()
              if r.startswith("refs/mj/session-start/")]
pinned = None
if request["mode"] == "pin":
    assert len(start_refs) == 1, start_refs
    git("update-ref", start_refs[0], request["base"])
    pinned = start_refs[0]

objects = {}
for line in text("cat-file", "--batch-all-objects", "--batch-check=%(objectname) %(objecttype)").splitlines():
    oid, kind = line.split()
    objects[oid] = kind
commits = sorted(oid for oid, kind in objects.items() if kind == "commit")
refs = text("for-each-ref", "--format=%(refname) %(objectname)").splitlines()
head = text("rev-parse", "HEAD")
tracked_paths, links = [], {}
for row in git("ls-tree", "-rz", head).split(b"\0"):
    if row:
        info, name = row.split(b"\t", 1)
        path = os.fsdecode(name)
        mode, _, oid = info.split()
        tracked_paths.append(path)
        if mode == b"120000":
            links[path] = oid
print(json.dumps({
    "pinned_ref": pinned,
    "head": head,
    "tracked_symlinks": list(links),
    "tracked_symlink_targets": symlink_targets(links),
    "tracked_paths": tracked_paths,
    "head_tree": text("rev-parse", "HEAD^{tree}"),
    "base_tree": text("rev-parse", request["base"] + "^{tree}"),
    "session_start": {r: text("rev-parse", r) for r in start_refs},
    "diff_sha256": hashlib.sha256(git("diff", "--binary", "--full-index", "--no-ext-diff",
                                      "--no-textconv", request["base"], "HEAD")).hexdigest(),
    "diff_stat": text("diff", "--shortstat", request["base"], "HEAD"),
    "refs": refs,
    "object_count": len(objects),
    "objects_sha256": hashlib.sha256("\n".join(sorted(objects)).encode()).hexdigest(),
    "commits": commits,
    "remotes": text("remote").splitlines(),
    "alternates": "alternate:" in text("count-objects", "-v"),
    "status": text("status", "--porcelain=v1", "--untracked-files=all"),
    # Builds and test runs leave ignored output (target/, __pycache__/) however they were started.
    "ignored": text("status", "--porcelain=v1", "--ignored=traditional", "--untracked-files=normal"),
}))
