"""Build the host-only orphan repository and read original source blobs."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory, TemporaryFile


OID = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
PILOT_REPOS = {"mjolnir", "bifrost-dev"}
PRIVATE_PATHS = (".agents/evals/review-arms", ".rql-census", ".rql-review-arms")


def git(repo, *args, **kwargs):
    return subprocess.run(
        ["git", "--no-replace-objects", "-C", repo, *args],
        check=True, stderr=subprocess.PIPE, **kwargs,
    )


def git_output(repo, *args, **kwargs):
    return git(repo, *args, stdout=subprocess.PIPE, **kwargs).stdout


def blob_reader(repo, commit):
    def read(path):
        entries = git_output(repo, "--literal-pathspecs", "ls-tree", "-z", commit, "--", path)
        if not entries:
            return None
        mode, _, oid = entries.split(b"\t", 1)[0].split()
        if mode not in {b"100644", b"100755"}:
            return None
        return git_output(repo, "cat-file", "blob", oid.decode("ascii"))
    return read


def eligible_diffs(sidecar, pilot_diffs):
    """Keep only identities from the frozen eligible-label sidecar."""
    by_key = {}
    for line in sidecar.splitlines():
        row = json.loads(line)
        repo, identifier, commit = row["repo"], row["id"], row["intro"]
        if (repo not in PILOT_REPOS or not isinstance(identifier, str) or not identifier
                or not isinstance(commit, str) or not OID.fullmatch(commit)):
            raise ValueError("invalid introducing tree identity in frozen eligible labels")
        key = f"{repo}/{identifier}"
        if key in by_key:
            raise ValueError(f"duplicate key in frozen eligible labels: {key}")
        by_key[key] = {"repo": repo, "id": identifier, "intro": commit, "key": key}
    for diff in pilot_diffs:
        match = by_key.get(diff["key"])
        if not match or any(match[key] != diff[key] for key in ("repo", "id", "intro")):
            raise ValueError(f"pilot diff absent from frozen eligible labels: {diff['key']}")
    return list(by_key.values())


def manifest_sources(manifest, repos):
    sources = {}
    owners = {}
    for diff in manifest["eligible_diffs"]:
        commit, repo = diff["intro"], diff["repo"]
        if commit in owners and owners[commit] != repo:
            raise ValueError(f"introducing tree {commit} is assigned to multiple repositories")
        owners[commit] = repo
        sources[commit] = Path(repos[repo]).resolve()
    return sources



def orphan_ref(commit):
    return f"refs/heads/intro-{commit}"


def parentless_snapshot(source, destination, commit):
    """Copy exactly the tracked tree objects and publish one deterministic root."""
    if not isinstance(commit, str) or not OID.fullmatch(commit):
        raise ValueError("expected a full introducing commit ID")
    tree = git_output(source, "rev-parse", f"{commit}^{{tree}}").strip()
    if git_output(source, "ls-tree", "-z", commit, "--", *PRIVATE_PATHS):
        raise ValueError(f"introducing tree {commit} contains study material")
    # A tree root packs trees and blobs, with no parent commit or source refs.
    with TemporaryFile() as pack:
        git(source, "pack-objects", "--stdout", "--revs", input=tree + b"\n", stdout=pack)
        pack.seek(0)
        git(destination, "index-pack", "--stdin", stdin=pack, stdout=subprocess.PIPE)
    identity = {
        "GIT_AUTHOR_NAME": "RQL pilot", "GIT_COMMITTER_NAME": "RQL pilot",
        "GIT_AUTHOR_EMAIL": "rql-pilot@localhost", "GIT_COMMITTER_EMAIL": "rql-pilot@localhost",
        "GIT_AUTHOR_DATE": "@1 +0000", "GIT_COMMITTER_DATE": "@1 +0000",
    }
    orphan = git_output(destination, "-c", "commit.gpgsign=false", "commit-tree", tree.decode("ascii"),
                        input=b"Source snapshot\n", env={**os.environ, **identity}).strip().decode("ascii")
    if git_output(destination, "rev-parse", f"{orphan}^{{tree}}").strip() != tree:
        raise RuntimeError("orphan snapshot tree does not match the introducing tree")
    if git_output(destination, "rev-list", "--parents", "-n", "1", orphan).split() != [orphan.encode()]:
        raise RuntimeError("snapshot must contain one parentless commit")
    git_output(destination, "update-ref", orphan_ref(commit), orphan)
    return {"orphan": orphan, "tree": tree.decode("ascii")}


def prepared_snapshot(source, destination, commit):
    tree = git_output(source, "rev-parse", f"{commit}^{{tree}}").strip()
    orphan = git_output(destination, "rev-parse", orphan_ref(commit)).strip().decode("ascii")
    if (git_output(destination, "rev-parse", f"{orphan}^{{tree}}").strip() != tree
            or git_output(destination, "rev-list", "--parents", "-n", "1", orphan).split() != [orphan.encode()]):
        raise RuntimeError("prepared snapshot must be a parentless commit with the introducing tree")
    return {"orphan": orphan, "tree": tree.decode("ascii")}


@contextmanager
def verifier_snapshot(repo, orphan):
    """Materialize the verifier's Rust source from Git blobs without checkout filters."""
    files = []
    for item in git_output(repo, "ls-tree", "-rz", "--full-tree", orphan).split(b"\0"):
        if not item:
            continue
        meta, name = item.split(b"\t", 1)
        mode, _, oid = meta.split()
        if mode in {b"100644", b"100755"} and name.endswith(b".rs"):
            files.append((os.fsdecode(name), oid))
    with TemporaryDirectory(prefix="rql-verifier-") as directory, TemporaryFile() as objects:
        root = Path(directory)
        if files:
            git(repo, "cat-file", "--batch", input=b"".join(oid + b"\n" for _, oid in files), stdout=objects)
        objects.seek(0)
        for name, oid in files:
            digest, kind, length = objects.readline().split()
            if (digest, kind) != (oid, b"blob"):
                raise ValueError(f"verifier snapshot did not return the frozen blob for {name!r}")
            data = objects.read(int(length))
            if len(data) != int(length) or objects.read(1) != b"\n":
                raise ValueError(f"verifier snapshot returned an incomplete blob for {name!r}")
            path = root / name
            if not path.is_relative_to(root) or ".." in path.parts:
                raise ValueError(f"invalid verifier source path {name!r}")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        yield root
