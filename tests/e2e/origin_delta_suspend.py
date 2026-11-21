#!/usr/bin/env python3
"""Suspend and resume an independent clone using real mj, daemon and worker.

Build mj with test-hooks for the post-preflight timing cases. Build the worker
separately from the same checkout using its normal features.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
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
    parser.add_argument("--case", choices=["round-trip", "untracked", "detached", "late-file", "git-error", "raw"], required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named instance")

    lab = reliability_lab.Lab(args.mj, "origin-delta-suspend", 0)
    git_server = None
    session = None
    writer = None
    checkpoint_hook = None
    try:
        lab.prepare()
        with (lab.config / "config.toml").open("a") as config:
            config.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")
        if args.case in {"late-file", "git-error"}:
            lab.hook_name = "origin_delta_after_preflight"
            checkpoint_hook = lab.hooks / f"{lab.hook_name}.continue"
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
            reply = lab.daemon_request({"action": "session_record", "arguments": {"session_id": session}})
            assert reply["value"] is not None, reply
            value = reply["value"]
            return value["record"] if isinstance(value, dict) and "record" in value else value

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
        (module_src / "module.txt").write_text("later published submodule checkout\n")
        git(module_src, "commit", "-am", "publish later module state")
        git(module_src, "push", repos / "module.git", "main")
        git(lab.project, "clone", "--bare", ".", repos / "project.git")
        git(lab.project, "remote", "add", "origin", f"{base}/project.git")
        git(lab.project, "fetch", "origin")

        created_ws = json.loads(run("workspaces", "create", "Origin delta", "--json").stdout)
        workspace = created_ws.get("id") or created_ws.get("workspace_id") or created_ws["workspace"]["id"]
        if args.case == "raw":
            git(lab.project, "branch", "pre-launch-branch")
            (lab.project / "README.md").write_text("pre-launch stash\n")
            git(lab.project, "stash", "push", "-m", "pre-launch stash")

            created = lab.daemon_request({
                "action": "start_create_session",
                "arguments": {
                    "workspace_id": workspace,
                    "profile_id": "fake",
                    "bundle_id": "",
                    "target_template_id": "localhost",
                    "title": "raw-origin-delta",
                    "project_directory": str(lab.project),
                    "create_managed_worktree": False,
                    "subagents": {"mode": "none"},
                    "no_project_memory": False,
                    "no_github_events": False,
                    "remember_launch_choices": False,
                    "remember_container_size": False,
                    "additional_mounts": [],
                },
            })
            session = created["value"]["session"]["id"]
            wait_res = lab.daemon_request({
                "action": "wait_create_session",
                "arguments": {"session_id": session},
            }, timeout=WAIT)
            assert wait_res.get("reply") == "done", (created, wait_res)
            before = wait_for(lambda: row if (row := record())["state"] == "running" else None,
                              "raw session worker ready")
            assert before.get("managed_worktree") is None, before
            launch_file = lab.project / ".git/mj/sessions" / session / "launch"
            assert launch_file.is_file(), f"missing launch record {launch_file}"

            git(lab.project, "checkout", "-b", "post-launch-branch")
            (lab.project / "post.txt").write_text("post-launch commit\n")
            git(lab.project, "add", "post.txt")
            git(lab.project, "commit", "-m", "post-launch commit")
            (lab.project / "post.txt").write_text("post-launch stash\n")
            git(lab.project, "stash", "push", "-m", "post-launch stash")
            (lab.project / "staged.txt").write_text("staged edit\n")
            git(lab.project, "add", "staged.txt")
            (lab.project / "post.txt").write_text("unstaged edit\n")
            untracked = lab.project / "untracked.txt"
            untracked.write_text("keep untracked\n")

            run("suspend", "--acknowledge-unpublished-work", "--session", session)
            stopped = wait_for(lambda: row if (row := record())["state"] == "stopped" else None,
                               "raw session to stop")
            assert launch_file.is_file(), f"launch record disappeared on suspend: {launch_file}"
            assert untracked.read_text() == "keep untracked\n"
            with zipfile.ZipFile(stopped["checkpoint"]["archive_path"]) as archive:
                manifest = json.loads(archive.read("manifest.json"))
                assert manifest["schema_version"] == 14, manifest
                repository, = manifest["repositories"]
                assert repository["origin_delta_pack_path"] in archive.namelist()
                delta = repository["origin_delta"]
                assert "refs/heads/post-launch-branch" in delta["refs"], delta["refs"]
                assert "refs/heads/pre-launch-branch" not in delta["refs"], delta["refs"]
                assert len(delta["stash"]) == 1, delta["stash"]

            (lab.project / "README.md").write_text("edited while suspended\n")
            run("resume", "--session", session, "--json")
            wait_for(lambda: record()["state"] == "running", "resumed raw worker ready")
            assert (lab.project / "README.md").read_text() == "edited while suspended\n"

            run("destroy", "--session", session)
            session = None
            wait_for(lambda: not launch_file.exists(), f"launch record removed after destroy: {launch_file}")
            return

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
            result = run("suspend", "--acknowledge-unpublished-work", "--session", session, check=False)
            assert result.returncode != 0 and "untracked.txt" in result.stderr, result
            assert record()["state"] == "running", record()
            assert pid_path.read_text() == worker_before
            assert untracked.read_text() == "keep me\n"
            untracked.unlink()
            git_directory = checkout / ".git"
            with (git_directory / "info/exclude").open("a") as excludes:
                excludes.write("\n.mj/keep/\n")
            keep = checkout / ".mj/keep"
            keep.mkdir(parents=True, exist_ok=True)
            saved = keep / "ignored.txt"
            saved.write_text("preserve ignored keep data\n")
            git(checkout, "check-ignore", saved.relative_to(checkout))
            result = run("suspend", "--acknowledge-unpublished-work", "--session", session, check=False)
            assert result.returncode != 0 and "ignored.txt" in result.stderr, result
            assert record()["state"] == "running", record()
            assert pid_path.read_text() == worker_before
            assert saved.read_text() == "preserve ignored keep data\n"
            return

        if args.case in {"detached", "late-file", "git-error"}:
            original_index = None
            if args.case in {"detached", "late-file"}:
                started = lab.runtime_root / "writer-started"
                release = lab.runtime_root / "writer-release"
                wrote = lab.runtime_root / "writer-wrote"
                writer = subprocess.Popen(
                    [sys.executable, "-c", """
import os
from pathlib import Path
import sys
import time

started, release, wrote, target = map(Path, sys.argv[1:])
started.write_text(str(os.getpid()))
while not release.exists():
    time.sleep(0.01)
target.write_text("written after preflight\\n")
wrote.touch()
while True:
    if target.name == "README.md":
        target.write_text("written after preflight\\n")
    time.sleep(0.02)
""", str(started), str(release), str(wrote),
                     "README.md" if args.case == "detached" else "late.txt"],
                    cwd=checkout, env=dict(env, MJ_SESSION_ID=session), start_new_session=True,
                )
                wait_for(started.exists, "detached writer to start")
                if args.case == "detached":
                    release.touch()
                    wait_for(wrote.exists, "detached writer to write tracked data")

            run("suspend", "--acknowledge-unpublished-work", "--session", session)
            if checkpoint_hook is not None:
                wait_for((lab.hooks / f"{lab.hook_name}.reached").exists, "completed suspend preflight")
                if args.case == "late-file":
                    release.touch()
                    wait_for(wrote.exists, "background write after preflight")
                else:
                    index = checkout / ".git/index"
                    original_index = index.read_bytes()
                    index.write_bytes(b"injected corrupt Git index")
                checkpoint_hook.touch()

            if args.case == "detached":
                wait_for(lambda: record()["state"] == "stopped", "suspend after writer stop")
                assert writer.wait(timeout=WAIT) < 0, "detached writer was not terminated"
                assert not checkout.exists(), str(checkout)
            else:
                after = wait_for(
                    lambda: row if (row := record())["state"] == "running"
                    and pid_path.exists() and pid_path.read_text() != worker_before else None,
                    "agent restarted inside the source checkout",
                )
                assert checkout.is_dir(), after
                assert after.get("last_checkpoint_error"), after
                if args.case == "late-file":
                    assert "late.txt" in after["last_checkpoint_error"], after
                    assert writer.wait(timeout=WAIT) < 0
                    assert (checkout / "late.txt").read_text() == "written after preflight\n"
                else:
                    assert "index" in after["last_checkpoint_error"].lower(), after
                    assert index.read_bytes() == b"injected corrupt Git index"
                    index.write_bytes(original_index)
                    git(checkout, "status", "--porcelain")
            return

        git(checkout, "submodule", "update", "--init")
        git(checkout / "vendor/module", "switch", "--detach", "origin/main")
        assert git(checkout / "vendor/module", "rev-parse", "HEAD").stdout != git(
            checkout, "rev-parse", ":vendor/module"
        ).stdout
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

        run("suspend", "--acknowledge-unpublished-work", "--session", session)
        stopped = wait_for(lambda: row if (row := record())["state"] == "stopped" else None,
                           "session to stop")
        assert not checkout.exists(), str(checkout)
        with zipfile.ZipFile(stopped["checkpoint"]["archive_path"]) as archive:
            manifest = json.loads(archive.read("manifest.json"))
            assert manifest["schema_version"] == 14, manifest
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
        if checkpoint_hook is not None and checkpoint_hook.parent.exists():
            checkpoint_hook.touch(exist_ok=True)
        if writer is not None and writer.poll() is None:
            writer.terminate()
            writer.wait(timeout=WAIT)
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
