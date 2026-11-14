"""Runs inside a review container: pins the review start or inventories the checkout.

Reads a JSON request on stdin: {"mode": "pin"|"audit", "cwd": ..., "base": ...}.
Prints a JSON inventory. Read-only except for the pin's one update-ref.
"""
import hashlib
import json
import subprocess
import sys

request = json.load(sys.stdin)


def git(*args):
    return subprocess.check_output(["git", "-C", request["cwd"], *args], stderr=subprocess.PIPE)


def text(*args):
    return git(*args).decode().strip()


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
print(json.dumps({
    "pinned_ref": pinned,
    "head": text("rev-parse", "HEAD"),
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
}))
