#!/usr/bin/env python3
"""Compute the worker compatibility identity from committed Cargo inputs."""

from __future__ import annotations

import argparse
import hashlib
import os
import subprocess
import sys
from pathlib import Path


def git_output(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(root), *args],
        check=False,
        capture_output=True,
        text=True,
    )


def require_identity(value: str, source: str) -> str:
    value = value.strip().lower()
    if len(value) != 40 or any(character not in "0123456789abcdef" for character in value):
        raise ValueError(f"{source} must be a full 40-character hexadecimal digest")
    return value


def worker_inputs_id(package_root: Path, fallback_revision: str) -> str:
    override = os.environ.get("MJ_WORKER_INPUTS_ID")
    if override is not None:
        return require_identity(override, "MJ_WORKER_INPUTS_ID")

    fallback_revision = require_identity(fallback_revision, "fallback revision")
    if (package_root / ".cargo_vcs_info.json").is_file():
        return fallback_revision

    try:
        top_level = git_output(package_root, "rev-parse", "--show-toplevel")
    except OSError:
        return fallback_revision
    if top_level.returncode != 0:
        return fallback_revision
    repository_root = Path(top_level.stdout.strip()).resolve()
    try:
        head = git_output(repository_root, "rev-parse", "--verify", "HEAD^{commit}")
    except OSError:
        return fallback_revision
    if head.returncode != 0:
        return fallback_revision
    head_revision = head.stdout.strip()

    list_path = package_root / "worker-build-inputs.txt"
    relative_list_path = list_path.resolve().relative_to(repository_root).as_posix()
    committed_list = git_output(
        repository_root, "cat-file", "-e", f"{head_revision}:{relative_list_path}"
    )
    if committed_list.returncode == 0:
        contents = git_output(
            repository_root, "show", f"{head_revision}:{relative_list_path}"
        )
        if contents.returncode != 0:
            raise RuntimeError(f"read committed worker input list {relative_list_path}")
        lines = contents.stdout.splitlines()
    else:
        # The new list is absent from HEAD while this change is being built.
        # Once committed, edits to the working copy never affect the stamp.
        lines = list_path.read_text(encoding="utf-8").splitlines()

    paths = []
    for line in lines:
        path = line.strip()
        if not path or path.startswith("#"):
            continue
        if Path(path).is_absolute() or ".." in Path(path).parts:
            raise ValueError(f"worker input path must stay inside the workspace: {path}")
        paths.append(Path(path).as_posix())
    if not paths:
        raise ValueError("worker input path list is empty")
    if len(paths) != len(set(paths)):
        raise ValueError("worker input path list contains duplicates")

    identity_lines = []
    for path in sorted(paths):
        object_id = git_output(
            repository_root, "rev-parse", "--verify", f"{head_revision}:{path}"
        )
        if object_id.returncode != 0:
            raise RuntimeError(f"worker input path {path!r} is missing from HEAD")
        identity_lines.append(f"{path} {object_id.stdout.strip()}")
    return hashlib.sha1(("\n".join(identity_lines) + "\n").encode("utf-8")).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--package-root", type=Path, required=True)
    parser.add_argument("--fallback-revision", required=True)
    args = parser.parse_args()
    try:
        print(worker_inputs_id(args.package_root.resolve(), args.fallback_revision))
    except (OSError, RuntimeError, ValueError) as error:
        print(f"worker input identity: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
