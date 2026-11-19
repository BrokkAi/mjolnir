#!/usr/bin/env python3
"""Suspend and resume an independent clone using real mj, daemon and worker."""
import argparse
import json
from pathlib import Path
import subprocess
import time
import zipfile

import reliability_lab

WAIT = 120


def wait_for(predicate, description):
    until = time.monotonic() + WAIT
    while time.monotonic() < until:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for {description}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", required=True, type=Path)
    parser.add_argument("--worker", required=True, type=Path)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--case", choices=["round-trip", "untracked"], required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named instance")

    lab = reliability_lab.Lab(args.mj, "origin-delta-suspend", 0)
    git_server = None
    session = None
    try:
        lab.prepare()
        with (lab.config / "config.toml").open("a") as config:
            config.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")
        env = dict(lab.environment(), MJ_INSTANCE=args.instance,
                   MJ_WORKER_BINARY=str(args.worker.resolve()))

        def run(*arguments, check=True):
            result = subprocess.run(
                [str(args.mj.resolve()), "--instance", args.instance, *arguments],
                env=env, capture_output=True, text=True, timeout=WAIT,
            )
            if check:
                assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result

        def git(path, *arguments, check=True):
            result = subprocess.run(
                ["git", "-C", str(path), *map(str, arguments)],
                env=env, capture_output=True, text=True, timeout=WAIT,
            )
            if check:
                assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result

        def record():
            return lab.daemon_request({"action": "session_record", "arguments": {"session_id": session}})

        repos = lab.runtime_root / "origins"
        repos.mkdir()
        module_src = lab.runtime_root / "module"
        module_src.mkdir()
        git(module_src, "init", "--initial-branch=main")
        (module_src / "module.txt").write_text("published submodule\n")
        git(module_src, "add", "module.txt")
        git(module_src, "commit", "-m", "publish module")
        git(module_src, "clone", "--bare", ".", repos / "module.git")

        port = lab.free_port()
        git_server = subprocess.Popen(
            ["git", "daemon", "--reuseaddr", "--export-all", "--listen=127.0.0.1",
             f"--port={port}", f"--base-path={repos}", str(repos)],
            cwd=lab.runtime_root, env=env, start_new_session=True,
        )
        base = f"git://127.0.0.1:{port}"
        wait_for(lambda: git(lab.project, "ls-remote", f"{base}/module.git",
                             check=False).returncode == 0, "fixture Git origin")
        git(lab.project, "submodule", "add", f"{base}/module.git", "vendor/module")
        git(lab.project, "commit", "-m", "register module")
        git(lab.project, "clone", "--bare", ".", repos / "project.git")
        git(lab.project, "remote", "add", "origin", f"{base}/project.git")
        git(lab.project, "fetch", "origin")

        run("workspaces", "create", "Origin delta")
        session = json.loads(run(
            "new", "--workspace", "Origin delta", "--profile", "fake", "--target", "localhost",
            "--project-directory", str(lab.project), "--subagents", "none", "--json",
        ).stdout)["session_id"]
        before = wait_for(lambda: row if (row := record())["state"] == "running" else None,
                          "session worker ready")
        assert before["managed_worktree"]["kind"] == "clone", before
        checkout = Path(before["managed_worktree"]["worktree_root"])
        pid_path = Path(before["target"]["worker_root"]) / "worker.pid"
        worker_before = pid_path.read_text()

        if args.case == "untracked":
            untracked = checkout / "untracked.txt"
            untracked.write_text("keep me\n")
            result = run("suspend", "--session", session, check=False)
            assert result.returncode != 0 and "untracked.txt" in result.stderr, result
            assert record()["state"] == "running", record()
            assert pid_path.read_text() == worker_before
            assert untracked.read_text() == "keep me\n"
            return

        git(checkout, "submodule", "update", "--init")
        (checkout / "local.txt").write_text("unpushed commit\n")
        git(checkout, "add", "local.txt")
        git(checkout, "commit", "-m", "unpushed")
        (checkout / "local.txt").write_text("saved stash\n")
        git(checkout, "stash", "push", "-m", "suspend stash")
        nested = checkout / "nested"
        git(checkout, "worktree", "add", "-b", "nested-lane", nested)
        (nested / "local.txt").write_text("nested tracked edit\n")
        git(checkout, "config", "mj.fixtureOnly", "discarded")
        expected = [
            git(checkout, "rev-parse", "HEAD").stdout,
            git(checkout, "stash", "list", "--format=%H:%gs").stdout,
            git(nested, "rev-parse", "--abbrev-ref", "HEAD").stdout,
            git(nested, "diff").stdout,
            git(checkout / "vendor/module", "rev-parse", "HEAD").stdout,
        ]

        run("suspend", "--session", session)
        stopped = wait_for(lambda: row if (row := record())["state"] == "stopped" else None,
                           "session to stop")
        assert not checkout.exists(), str(checkout)
        with zipfile.ZipFile(stopped["checkpoint"]["archive_path"]) as archive:
            manifest = json.loads(archive.read("manifest.json"))
            assert manifest["schema_version"] == 13, manifest
            repository, = manifest["repositories"]
            assert repository["origin_delta_pack_path"] in archive.namelist()
            assert "untracked_tar_path" not in repository

        run("resume", "--session", session, "--json")
        wait_for(lambda: record()["state"] == "running", "resumed worker ready")
        actual = [
            git(checkout, "rev-parse", "HEAD").stdout,
            git(checkout, "stash", "list", "--format=%H:%gs").stdout,
            git(nested, "rev-parse", "--abbrev-ref", "HEAD").stdout,
            git(nested, "diff").stdout,
            git(checkout / "vendor/module", "rev-parse", "HEAD").stdout,
        ]
        assert actual == expected, (actual, expected)
        assert git(checkout, "config", "--get", "mj.fixtureOnly", check=False).returncode == 1
    finally:
        if git_server is not None:
            git_server.terminate()
            git_server.wait(timeout=WAIT)
        if "run" in locals():
            try:
                if session is not None:
                    run("destroy", "--session", session, check=False)
            finally:
                run("daemon", "stop", check=False)
        lab.cleanup_owned()


if __name__ == "__main__":
    main()
