#!/usr/bin/env python3
"""Measure workspace rebuilds with warm external dependencies in a private snapshot."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", default="HEAD")
    parser.add_argument("--label", required=True)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=3)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    work = args.directory.resolve()
    source = work / args.label / "source"
    source.mkdir(parents=True, exist_ok=False)
    archive = source.parent / "source.tar"
    with archive.open("wb") as output:
        subprocess.run(["git", "archive", args.revision], cwd=root, stdout=output, check=True)
    with tarfile.open(archive) as package:
        package.extractall(source, filter="data")
    archive.unlink()
    # Bypass compiler-cache shims: a cache hit cannot measure compilation work.
    cargo = subprocess.check_output(["rustup", "which", "cargo"], text=True).strip()
    env = dict(os.environ, CARGO_TARGET_DIR=str(work / "build"),
               RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="")
    metadata = json.loads(subprocess.check_output(
        [cargo, "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=source, env=env))
    members = set(metadata["workspace_default_members"])
    packages = [p["name"] for p in metadata["packages"] if p["id"] in members]
    settings = {key: env.get(key) for key in (
        "RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_INCREMENTAL",
        "CARGO_BUILD_JOBS", "CARGO_TARGET_DIR")}
    settings["rustc"] = subprocess.check_output(["rustc", "-Vv"], text=True)
    settings["revision"] = subprocess.check_output(
        ["git", "rev-parse", args.revision], cwd=root, text=True).strip()
    (source.parent / "settings.json").write_text(json.dumps(settings, indent=2) + "\n")
    for run in range(args.runs + 1):
        # Delete only this benchmark's workspace products; preserve external dependencies.
        clean = [cargo, "clean"]
        for package in packages:
            clean.extend(["-p", package])
        log = source.parent / f"run-{run}.log"
        with log.open("w") as output:
            subprocess.run(clean, cwd=source, env=env, stdout=output, stderr=output, check=True)
            print(f"{args.label}: {'warmup' if run == 0 else f'run {run}'}", flush=True)
            subprocess.run([cargo, "build", "--locked", "--timings"],
                           cwd=source, env=env, stdout=output, stderr=output, check=True)
        report = max((work / "build/cargo-timings").glob("cargo-timing-*.html"),
                     key=lambda p: p.stat().st_mtime_ns)
        html = report.read_text()
        (source.parent / f"run-{run}.html").write_text(html)
        units = json.loads(re.search(r"const UNIT_DATA = (.*?);", html, re.S).group(1))
        summary = [{k: u[k] for k in ("name", "target", "start", "duration", "sections")}
                   for u in units if u["name"] in packages]
        (source.parent / f"run-{run}.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
