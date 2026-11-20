#!/usr/bin/env python3
"""Stages the Go packages that are green, so work in progress can be committed
without committing anything broken.

Several people or agents write runtime packages in one working tree at once.
Their files are only on local disk until committed, and a package may be
half-edited at any moment. This script takes a snapshot of each candidate
directory through the Git index, checks the snapshot itself (not the working
tree, which may have moved on), and leaves staged exactly the directories
that pass:

    gofmt clean, `go build`, `go vet`, and `go test` with the port's tags

A directory that fails is unstaged again, and the check is repeated, because a
package that passed may import one that was just dropped.

    python3 go/port/checkpoint.py            # stage what is green, print a summary
    python3 go/port/checkpoint.py --list     # only say what would be staged

It never commits. Review `git status`, then commit and push.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
TAGS = "mj_debug mj_test_support mj_test_hooks mj_test_identity mj_golden mj_github_history"
# Paths that ride along with any checkpoint: the oracle modules the packages'
# tests are recorded from. They are Rust, behind features, and not built by CI.
EXTRA = ["go/port/oracle/src", "go/port/oracle/Cargo.toml", "go/port/oracle/Cargo.lock", "go.mod", "go.sum"]


def run(args, cwd=ROOT, env=None, timeout=1800):
    return subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def candidates() -> list[str]:
    found = []
    for base in ("go/rs", "go/crates"):
        full = os.path.join(ROOT, base)
        if not os.path.isdir(full):
            continue
        if base == "go/rs":
            found.append(base)
        for name in sorted(os.listdir(full)):
            path = os.path.join(full, name)
            if os.path.isdir(path) and name != "testdata":
                found.append(f"{base}/{name}")
    for name in sorted(os.listdir(os.path.join(ROOT, "go"))):
        if name.startswith("mj") or name in ("cmd", "assets", "parity"):
            found.append(f"go/{name}")
    return found


def compress_corpora() -> None:
    """Recorded oracle answers are kept gzip-compressed (the test helper reads
    <corpus>.jsonl.gz). Compress any that were recorded uncompressed."""
    import gzip

    for current, names, files in os.walk(os.path.join(ROOT, "go")):
        names[:] = [n for n in names if not n.startswith("target")]
        if os.path.basename(current) != "oracle" or os.path.basename(os.path.dirname(current)) != "testdata":
            continue
        for name in files:
            if not name.endswith(".jsonl"):
                continue
            plain = os.path.join(current, name)
            with open(plain, "rb") as source:
                data = source.read()
            with open(plain + ".gz", "wb") as target:
                with gzip.GzipFile(filename="", mode="wb", fileobj=target, compresslevel=9, mtime=0) as packed:
                    packed.write(data)
            os.remove(plain)


def go_dirs(snapshot: str, directory: str) -> list[str]:
    """Package directories under `directory` in the snapshot. `go/rs` itself
    is one package; its subdirectories are candidates of their own."""
    top = os.path.join(snapshot, directory)
    out = []
    if not os.path.isdir(top):
        return out
    for current, names, files in os.walk(top):
        names[:] = [n for n in names if n != "testdata"]
        rel = os.path.relpath(current, snapshot)
        if directory == "go/rs" and rel != "go/rs":
            names[:] = []
            continue
        if any(f.endswith(".go") for f in files):
            out.append(rel)
    return out


def check(snapshot: str, directory: str, env: dict) -> str | None:
    """None when every package under `directory` is green, else the reason."""
    packages = go_dirs(snapshot, directory)
    if not packages:
        return "no Go files"
    for package in packages:
        files = [os.path.join(package, f) for f in os.listdir(os.path.join(snapshot, package)) if f.endswith(".go")]
        result = run(["gofmt", "-l", *files], cwd=snapshot)
        if result.stdout.strip() or result.returncode != 0:
            return "gofmt: " + (result.stdout.strip() or result.stderr.strip()).splitlines()[0]
        for step in (["go", "build", "-tags", TAGS, f"./{package}/"], ["go", "build", f"./{package}/"],
                     ["go", "vet", "-tags", TAGS, f"./{package}/"]):
            result = run(step, cwd=snapshot, env=env)
            if result.returncode != 0:
                lines = [l for l in (result.stderr + result.stdout).splitlines() if l and not l.startswith("#")]
                return f"{step[1]}: " + (lines[0] if lines else "failed")
        for goos in ("darwin", "windows"):
            result = run(["go", "build", f"./{package}/"], cwd=snapshot, env={**env, "GOOS": goos})
            if result.returncode != 0:
                lines = [l for l in (result.stderr + result.stdout).splitlines() if l and not l.startswith("#")]
                return f"build for {goos}: " + (lines[0] if lines else "failed")
        try:
            result = run(["go", "test", "-count=1", "-tags", TAGS, f"./{package}/"], cwd=snapshot, env=env, timeout=1200)
        except subprocess.TimeoutExpired:
            return "go test: timed out"
        if result.returncode != 0:
            lines = [l for l in result.stdout.splitlines() if l.startswith(("--- FAIL", "FAIL", "panic:"))]
            return "go test: " + (lines[0] if lines else (result.stderr.strip().splitlines() or ["failed"])[0])
    return None


def main(argv: list[str]) -> int:
    list_only = "--list" in argv
    env = dict(os.environ)
    for name in ("MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_SESSION_ID", "MJ_SESSION_BASH_ENV", "MJ_ORACLE"):
        env.pop(name, None)
    env.update({"CGO_ENABLED": "0", "LC_ALL": "C.UTF-8", "LANGUAGE": "en"})

    staged = [d for d in candidates() if os.path.isdir(os.path.join(ROOT, d))]
    compress_corpora()
    run(["git", "add", "--", *staged, *[e for e in EXTRA if os.path.exists(os.path.join(ROOT, e))]])
    dropped: dict[str, str] = {}
    for _ in range(8):
        snapshot = tempfile.mkdtemp(prefix="mj-go-checkpoint-")
        try:
            result = run(["git", "checkout-index", "-a", f"--prefix={snapshot}/"])
            if result.returncode != 0:
                print(result.stderr, file=sys.stderr)
                return 1
            failed = {}
            for directory in staged:
                reason = check(snapshot, directory, env)
                if reason is not None:
                    failed[directory] = reason
        finally:
            shutil.rmtree(snapshot, ignore_errors=True)
        if not failed:
            break
        for directory, reason in failed.items():
            dropped[directory] = reason
            run(["git", "reset", "-q", "--", directory])
        staged = [d for d in staged if d not in failed]
    else:
        print("checkpoint: did not settle", file=sys.stderr)
        return 1

    changed = run(["git", "diff", "--cached", "--stat", "--", "go"]).stdout.strip().splitlines()
    print(f"green and staged: {len(staged)} directories")
    for directory in staged:
        print(f"  {directory}")
    if dropped:
        print(f"not staged: {len(dropped)}")
        for directory, reason in sorted(dropped.items()):
            print(f"  {directory}: {reason[:160]}")
    print(changed[-1] if changed else "nothing new to commit")
    if list_only:
        run(["git", "reset", "-q", "--", "go", "go.mod", "go.sum"])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
