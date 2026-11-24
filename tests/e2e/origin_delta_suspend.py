#!/usr/bin/env python3
"""Suspend and resume an independent clone using real mj, daemon and worker.

Build mj with test-hooks for the interruption cases. Build the worker
separately from the same checkout using its normal features.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from urllib.request import Request, urlopen
import zipfile

import reliability_lab

WAIT = 120
RECOVERY_HOOKS = {
    "dead-worker-live": "origin_delta_before_live_stop",
    "dead-worker-startup": "origin_delta_after_preflight",
    "dead-worker-error": "origin_delta_after_preflight",
    "verified-before-destroy": "origin_delta_after_close_ack",
}


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
    parser.add_argument(
        "--case",
        choices=["round-trip", "untracked", "detached", "late-file", "git-error", "raw", *RECOVERY_HOOKS],
        required=True,
    )
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named instance")

    lab = reliability_lab.Lab(args.mj, "origin-delta-suspend", 0)
    git_server = None
    session = None
    writer = None
    holder = None
    checkpoint_hook = None
    try:
        lab.prepare(fake_acp_prompt_delay_ms=60_000 if args.case == "dead-worker-live" else 0)
        with (lab.config / "config.toml").open("a") as config:
            config.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")
        if args.case in {"late-file", "git-error"}:
            lab.hook_name = "origin_delta_after_preflight"
        else:
            lab.hook_name = RECOVERY_HOOKS.get(args.case)
        if lab.hook_name is not None:
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

        def terminal_request(method, path, body=None):
            endpoint = json.loads((lab.data / "api-endpoint.json").read_text())
            request = Request(
                f"http://{endpoint['address']}/api/v1{path}",
                data=None if body is None else json.dumps(body).encode(),
                headers={
                    "Authorization": f"Bearer {endpoint['token']}",
                    "Content-Type": "application/json",
                },
                method=method,
            )
            with urlopen(request, timeout=WAIT) as response:
                payload = response.read()
                return json.loads(payload) if payload else None

        def record():
            return terminal_request("GET", f"/sessions/{session}/record")["record"]

        open_fault = lab.runtime_root / "fail-harness-load"
        if args.case == "dead-worker-error":
            bridge = lab.runtime_root / "fake_acp.py"
            script = bridge.read_text()
            load = '    elif method in ("session/new", "session/load"):\n'
            assert script.count(load) == 1
            bridge.write_text(script.replace(load, load + f"""
        if method == "session/load" and os.path.exists({str(open_fault)!r}):
            send({{"jsonrpc": "2.0", "id": ident, "error": {{
                "code": -32603, "message": "injected Suspend fixture load failure"
            }}}})
            continue
"""))

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

            created = terminal_request("POST", "/sessions/create", {
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
            })
            session = created["session"]["id"]
            terminal_request("POST", f"/sessions/{session}/create/wait")
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

            run("suspend", "--session", session)
            stopped = wait_for(lambda: row if (row := record())["state"] == "stopped" else None,
                               "raw session to stop")
            assert launch_file.is_file(), f"launch record disappeared on suspend: {launch_file}"
            assert untracked.read_text() == "keep untracked\n"
            with zipfile.ZipFile(stopped["checkpoint"]["archive_path"]) as archive:
                manifest = json.loads(archive.read("manifest.json"))
                assert manifest["schema_version"] == 15, manifest
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

        # Hard-won: #1676: a retained clone could not suspend after its original worker was lost
        if args.case in RECOVERY_HOOKS:
            root = pid_path.parent
            tracked = checkout / "README.md"
            tracked.write_text("staged before interrupted Suspend\n")
            git(checkout, "add", tracked.name)
            tracked.write_text("unstaged before interrupted Suspend\n")

            def patches():
                return (
                    git(checkout, "diff", "--cached", "--binary", "--", tracked.name).stdout,
                    git(checkout, "diff", "--binary", "--", tracked.name).stdout,
                )

            def acp_requests(methods):
                log = lab.runtime_root / "fake-acp.log"
                if not log.exists():
                    return []
                requests = (json.loads(line) for line in log.read_text().splitlines(keepends=True)
                            if line.endswith("\n"))
                return [request for request in requests if request.get("method") in methods]

            def harness_opens():
                # A daemon boot also probes harness capabilities in a scratch
                # workspace. Count only opens of this session's checkout.
                return [request for request in acp_requests({"session/new", "session/load"})
                        if Path(request["params"]["cwd"]) == checkout]

            prompt = "durable Suspend fixture prompt"

            def transcript():
                return run("transcript", "--session", session, "--json").stdout

            def worker_pid():
                return int(pid_path.read_text())

            def daemon_pid():
                return json.loads((lab.data / "daemon.json").read_text())["pid"]

            def kill_owned(pid):
                assert pid in lab.owned_pids(), f"refusing to signal unowned PID {pid}"
                os.kill(pid, signal.SIGKILL)
                wait_for(lambda: not reliability_lab.process_is_running(pid), f"PID {pid} to exit")

            def start_writer():
                ready = lab.runtime_root / "writer-started"
                ready.unlink(missing_ok=True)
                process = subprocess.Popen(
                    [sys.executable, "-c", """
from pathlib import Path
import sys
import time

with open(sys.argv[2], "r+b", buffering=0) as tracked:
    first = tracked.read(1)
    Path(sys.argv[1]).touch()
    while True:
        tracked.seek(0)
        tracked.write(first)
        time.sleep(0.02)
""", str(ready), tracked.name],
                    cwd=checkout, env=dict(env, MJ_SESSION_ID=session), start_new_session=True,
                )
                return process, ready

            hook_reached = lab.hooks / f"{lab.hook_name}.reached"

            def pause_suspend():
                checkpoint_hook.unlink(missing_ok=True)
                hook_reached.unlink(missing_ok=True)
                run("suspend", "--session", session)
                wait_for(hook_reached.exists, lab.hook_name)
                assert hook_reached.read_text().strip() == f"pid={daemon_pid()}"
                row = record()
                assert row["state"] == "closing", row
                return row

            def restart_daemon(pid):
                kill_owned(pid)
                checkpoint_hook.touch()
                run("workspaces", "list", "--json")
                assert daemon_pid() != pid

            live_race = args.case == "dead-worker-live"
            retry_error = args.case == "dead-worker-error"
            if not live_race:
                run("prompt", "--session", session, prompt)
            if retry_error:
                # Publish a real retained-target Error via the existing failed
                # harness startup path, with the lab daemon offline during the boot.
                run("daemon", "stop")
                subprocess.run(
                    [str(args.worker.resolve()), "worker", "stop", "--root", str(root)],
                    env=env, check=True, timeout=WAIT,
                )
                open_fault.touch()
                result = subprocess.run(
                    [str(root / "hel"), "worker", "run", "--root", str(root),
                     "--config", str(root / "launch.json")],
                    env=env, capture_output=True, text=True, timeout=WAIT,
                )
                assert result.returncode != 0, result
                exit_record = json.loads((root / "worker-exit.json").read_bytes())
                assert "injected Suspend fixture load failure" in exit_record["harness_startup_failure"]["message"], exit_record
                open_fault.unlink()
                run("workspaces", "list", "--json")
                failed = wait_for(lambda: row if (row := record())["state"] == "error" else None,
                                  "typed harness failure to leave the session in Error")
                assert failed["target"]["worker_root"] == str(root), failed
                assert not reliability_lab.process_is_running(worker_pid())

            if live_race or retry_error:
                # An independently launched checkout holder lacks session
                # ownership. Suspension must leave it alive and report its PID.
                holder = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(3600)", str(lab.runtime_root)],
                    cwd=checkout, env=env, start_new_session=True,
                )
                writer, ready = start_writer()
                wait_for(ready.exists, "marked writer to start before refusal")
                expected = patches()
                before_refusal = harness_opens()
                old_worker = worker_pid() if live_race else None
                assert pause_suspend().get("checkpoint") is None
                if old_worker is not None:
                    kill_owned(old_worker)
                checkpoint_hook.touch()
                after = wait_for(
                    lambda: row if "cannot prove these processes belong to this session"
                    in ((row := record()).get("last_checkpoint_error") or "") else None,
                    "unknown-holder refusal to be recorded",
                )
                assert after["state"] == ("error" if retry_error else "running"), after
                assert str(holder.pid) in after["last_checkpoint_error"], after
                assert after.get("checkpoint") is None, after
                assert after["target"]["worker_root"] == str(root), after
                assert patches() == expected
                assert holder.poll() is None, "capture signalled the unknown holder"
                assert writer.wait(timeout=WAIT) < 0, "capture left the marked writer running"
                wait_for(lambda: len(harness_opens()) > len(before_refusal),
                         "refusal rollback to restart Harness")
                holder.terminate()
                holder.wait(timeout=WAIT)
                if retry_error:
                    # The rollback starts a Harness even as the published state
                    # stays Error. Exercise the next retry with no live worker.
                    kill_owned(worker_pid())

            if live_race:
                run("message", "--session", session, prompt)
                wait_for(lambda: any(prompt in json.dumps(request)
                                    for request in acp_requests({"session/prompt"})),
                         "active prompt to reach the harness")

            journal_marker = "checking deterministic fixture" if live_race else f"reliability reply: {prompt}"
            wait_for(lambda: journal_marker in transcript(), "durable relay transcript before capture")
            writer, ready = start_writer()
            wait_for(ready.exists, "marked writer to start before capture")
            expected = patches()
            opens = harness_opens()
            old_daemon = daemon_pid()
            old_worker = worker_pid() if not retry_error else None
            closing = pause_suspend()
            checkpoint = closing.get("checkpoint")
            if args.case == "verified-before-destroy":
                assert checkpoint is not None, closing
                archive = Path(checkpoint["archive_path"])
                assert hashlib.sha256(archive.read_bytes()).hexdigest() == checkpoint["sha256"]
                assert worker_pid() != old_worker
                assert json.loads((root / "launch.json").read_text())["run_mode"] == "checkpoint_only"
                relay = json.loads((root / "relay-state.json").read_text())
                assert relay["execution"] in {"closing", "closed"}, relay["execution"]
                restart_daemon(old_daemon)
            else:
                assert checkpoint is None, closing
                if old_worker is not None:
                    kill_owned(old_worker)
                if args.case == "dead-worker-startup":
                    restart_daemon(old_daemon)
                else:
                    checkpoint_hook.touch()
                    assert daemon_pid() == old_daemon

            def stopped_record():
                row = record()
                assert row["state"] != "error" and not row.get("last_checkpoint_error"), row
                return row if row["state"] == "stopped" else None

            stopped = wait_for(stopped_record, "interrupted Suspend to complete")
            assert not stopped.get("last_error") and not stopped.get("last_checkpoint_error"), stopped
            assert not checkout.exists(), checkout
            assert writer.wait(timeout=WAIT) < 0, "capture left the marked writer running"
            assert harness_opens() == opens, harness_opens()
            saved = stopped["checkpoint"]
            archive = Path(saved["archive_path"])
            assert hashlib.sha256(archive.read_bytes()).hexdigest() == saved["sha256"]
            assert set(archive.parent.glob("*.zip")) == {archive}
            with zipfile.ZipFile(archive) as bundle:
                canonical = json.loads(bundle.read("canonical/session.json"))
            assert canonical["event_frontier"] == saved["event_frontier"]
            assert journal_marker in json.dumps(canonical["transcript"])
            if checkpoint is not None:
                assert saved == checkpoint, (saved, checkpoint)
                daemon_logs = lab.data / "logs"
                exports = [line for log in daemon_logs.glob("mj-daemon-*.log")
                           for line in log.read_text().splitlines()
                           if session in line and "checkpoint archive exported on the target" in line]
                assert len(exports) == 1, exports
                replacement_log, = daemon_logs.glob(f"mj-daemon-*-{daemon_pid()}.log")
                starts = [line for line in replacement_log.read_text().splitlines()
                          if session in line and "start detached Mjolnir worker" in line]
                assert not starts, starts
            if live_race:
                assert not acp_requests({"session/cancel"}), "dead prompt was treated as a live CancelTurn"
            run("resume", "--session", session, "--queue", "start", "--json")
            wait_for(lambda: record()["state"] == "running", "resumed worker ready")
            assert patches() == expected
            assert journal_marker in transcript()
            if live_race:
                wait_for(lambda: any("interrupted turn recovery" in json.dumps(request)
                                    for request in acp_requests({"session/prompt"})),
                         "resumed harness to receive the existing restart continuation")
            return

        if args.case == "untracked":
            untracked = checkout / "untracked.txt"
            untracked.write_text("keep me\n")
            result = run("suspend", "--session", session, check=False)
            assert result.returncode != 0 and "untracked.txt" in result.stderr, result
            assert record()["state"] == "running", record()
            assert pid_path.read_text() == worker_before
            assert untracked.read_text() == "keep me\n"
            untracked.unlink()
            git_directory = checkout / ".git"
            with (git_directory / "info/exclude").open("a") as excludes:
                excludes.write("\n.mj/keep/\n")
            keep = checkout / ".mj/keep" / session
            keep.mkdir(parents=True, exist_ok=True)
            saved = keep / "ignored.txt"
            saved.write_text("preserve ignored keep data\n")
            git(checkout, "check-ignore", saved.relative_to(checkout))
            result = run("suspend", "--session", session, check=False)
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

            run("suspend", "--session", session)
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

        run("suspend", "--session", session)
        stopped = wait_for(lambda: row if (row := record())["state"] == "stopped" else None,
                           "session to stop")
        assert not checkout.exists(), str(checkout)
        with zipfile.ZipFile(stopped["checkpoint"]["archive_path"]) as archive:
            manifest = json.loads(archive.read("manifest.json"))
            assert manifest["schema_version"] == 15, manifest
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
        if holder is not None and holder.poll() is None:
            holder.terminate()
            holder.wait(timeout=WAIT)
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
