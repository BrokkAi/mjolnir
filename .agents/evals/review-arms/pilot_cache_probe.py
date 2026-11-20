"""Run in the sealed container to enumerate commits in its Git cache."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile

CACHE = Path("/run/hel/git-cache")


def git(cwd, *args):
    return subprocess.check_output(["git", "-c", "safe.directory=*", "-C", str(cwd), *args],
                                   stderr=subprocess.PIPE).decode()


def objects(repo):
    lines = git(repo, "cat-file", "--batch-all-objects", "--batch-check=%(objectname) %(objecttype)")
    return {oid for oid, kind in (line.split() for line in lines.splitlines()) if kind == "commit"}


def attest(cwd, intro, base, cache=CACHE):
    allowed = set(git(cwd, "rev-list", intro, base).splitlines())
    workspace = objects(cwd)
    snapshots = {}
    with tempfile.TemporaryDirectory(prefix="pilot-cache-") as tmp:
        for index, entry in enumerate(sorted(cache.iterdir()) if cache.exists() else []):
            if entry.is_symlink():
                raise ValueError(f"unexpected symlink in Git cache: {entry}")
            if entry.is_file() and entry.suffix == ".bundle":
                bare = Path(tmp) / str(index)
                git(tmp, "init", "--bare", "--quiet", str(bare))
                git(bare, "bundle", "unbundle", str(entry))
                commits = objects(bare)
            elif entry.is_dir() and entry.suffix == ".git":
                commits = objects(entry)
            else:
                raise ValueError(f"unexpected Git cache entry: {entry}")
            snapshots[entry.name] = {"commits": sorted(commits), "outside_pin": sorted(commits - allowed)}
    return {"cache_present": cache.exists(), "pin": intro, "base": base,
            "workspace_commits": sorted(workspace),
            "workspace_outside_pin": sorted(workspace - allowed),
            "snapshots": snapshots, "passed": workspace == allowed and all(
                not snapshot["outside_pin"] and (base != intro or snapshot["commits"] == [intro])
                for snapshot in snapshots.values())}


if __name__ == "__main__":
    print(json.dumps(attest(*sys.argv[1:])))
