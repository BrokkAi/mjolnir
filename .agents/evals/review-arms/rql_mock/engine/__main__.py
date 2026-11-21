"""Prepare or verify snapshots, or capture and tear down the engine sessions."""
import argparse
from pathlib import Path
import sys

from . import CONFIG, frozen_inputs, load, save, teardown
from .runner import EngineRunner
from .snapshot import git_output, manifest_sources, orphan_ref, parentless_snapshot, prepared_snapshot


def audit(destination, sources):
    records = {commit: prepared_snapshot(source, destination, commit)
               for commit, source in sorted(sources.items())}
    expected_refs = {orphan_ref(commit) for commit in sources}
    refs = set(git_output(destination, "for-each-ref", "--format=%(refname)").decode().splitlines())
    if refs != expected_refs or not refs:
        raise ValueError("snapshot refs must match the manifest exactly and be nonempty")
    expected_objects = {record["orphan"] for record in records.values()}
    objects = git_output(destination, "cat-file", "--batch-all-objects",
                         "--batch-check=%(objecttype) %(objectname)").decode().splitlines()
    commits = {oid for kind, oid in map(str.split, objects) if kind == "commit"}
    if commits != expected_objects:
        raise ValueError("snapshot repository contains an unexpected commit object")
    if (destination / "objects/info/alternates").exists():
        raise ValueError("snapshot repository uses alternate objects")
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["prepare", "verify", "teardown"])
    parser.add_argument("--config", type=Path, default=CONFIG)
    args = parser.parse_args()
    config = load(args.config)
    if args.operation == "teardown":
        # Old active records may predate the per-session provider pin.
        manifest = load(Path(config["manifest"]))
        version = manifest["model_versions"]["claude_provider_id"]
        accepted, failures = teardown(args.config.parent / "engine-cache",
                                      EngineRunner({**config, "model_version": version}))
        print(f"Engine session destroy requests accepted: {accepted}")
        for message in failures:
            print(message, file=sys.stderr)
        return int(bool(failures))
    destination = Path(config["snapshot_repo"]).resolve()
    if destination.name != "engine-trees.git":
        parser.error("snapshot_repo must end in engine-trees.git")
    manifest, _, _ = frozen_inputs(Path(config["manifest"]))
    sources = manifest_sources(manifest, config["repos"])
    if not sources:
        parser.error("no introducing commits in the manifest")
    if args.operation == "prepare":
        destination.mkdir(parents=True, exist_ok=True)
        if not (destination / "HEAD").is_file():
            git_output(destination, "init", "--bare", "--template=")
        for commit, source in sorted(sources.items()):
            parentless_snapshot(source, destination, commit)
        # git daemon advertises HEAD for initial clone and restricts upload-pack
        # to explicitly exported repositories and the supplied strict path.
        git_output(destination, "symbolic-ref", "HEAD", orphan_ref(min(sources)))
    records = audit(destination, sources)
    if args.operation == "prepare":
        save(destination.parent / "engine-trees.json", records)
        (destination / "git-daemon-export-ok").touch()
    print(f"verified {len(records)} introducing tree snapshots")


if __name__ == "__main__":
    sys.exit(main())
