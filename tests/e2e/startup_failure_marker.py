#!/usr/bin/env python3
"""Stop relay recovery after a replacement worker's harness fails to start
(its ACP session/load or its preparation fails), and finish an interrupted
Suspend without the harness that failed."""

import argparse
import json
from pathlib import Path
import sqlite3
import subprocess
import time

from mailbox_harnesses import Relay
import reliability_lab

CAUSE = "fixture transient load failure"
# Observe past the relay manager's 60-second worker-restart cooldown.
RECOVERY_OBSERVATION_SECONDS = 65


def run(lab, args):
    port = lab.prepare()
    bridge = lab.runtime_root / "fake_acp.py"
    entered = lab.runtime_root / "failed-load-entered"
    release = lab.runtime_root / "fail-load-now"
    branch = '    elif method in ("session/new", "session/load"):\n'
    script = bridge.read_text()
    # A completed turn makes the first recovery copy due immediately. Leave
    # Codex's goal state unsynchronized so checkpoint admission defers the
    # copy regardless of when the daemon observes the turn.
    clear_goal = 'report_execution("idle", goal_cleared=True)'
    assert script.count(clear_goal) == 1
    script = script.replace(clear_goal, 'report_execution("idle")')
    assert script.count(branch) == 1
    script = script.replace(branch, branch + f"""
        if method == "session/load" and not os.path.exists({str(entered)!r}):
            open({str(entered)!r}, "w").close()
            while not os.path.exists({str(release)!r}):
                time.sleep(0.02)
            for _ in range(80):
                print("fixture bridge diagnostic " + "x" * 1024, file=sys.stderr)
            send({{"jsonrpc": "2.0", "id": ident, "error": {{
                "code": -32603, "message": {CAUSE!r}
            }}}})
            continue
""")
    compile(script, str(bridge), "exec")
    bridge.write_text(script)
    with (lab.config / "config.toml").open("a") as config:
        config.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")

    env = {**lab.environment(), "MJ_WORKER_BINARY": str(args.worker.resolve())}
    worker_env = {**env, "MJ_INSTANCE": args.instance}

    def cli(*arguments):
        result = subprocess.run(
            [str(lab.hel), "--instance", args.instance, *arguments],
            env=env, capture_output=True, text=True, timeout=reliability_lab.TIMEOUT,
        )
        assert result.returncode == 0, (arguments, result.stdout, result.stderr)
        return result.stdout

    def authenticate():
        code, _ = lab.wait_daemon_status(port)
        status, body = lab.request("POST", "/auth/session", {"code": code})
        assert status == 204, (status, body)

    def record():
        return lab.daemon_request({
            "action": "session_record", "arguments": {"session_id": session},
        })["value"]

    def bridge_requests():
        return [
            message for line in (lab.runtime_root / "fake-acp.log").read_text().splitlines()
            if Path((message := json.loads(line)).get("params", {}).get("cwd", "")) == worker_cwd
            or message.get("params", {}).get("sessionId") == original["native_session_id"]
        ]

    failed_worker = None
    try:
        cli("workspaces", "create", "Startup failure marker")
        authenticate()
        session = json.loads(cli(
            "new", "--workspace", "Startup failure marker", "--profile", "fake",
            "--target", "localhost", "--project-directory", str(lab.project),
            "--subagents", "none", "--json",
        ))["session_id"]
        before = "prompt before the startup fault"
        lab.submit_prompt(session, before)
        reply = f"reliability reply: {before}"

        # The viewer accepts the action before the relay receives it. Wait
        # for the reply to appear before asking whether the turn has ended.
        def original_reply_visible(_snapshot):
            status, body = lab.request("GET", f"/api/conversations/{session}")
            return status == 200 and any(reply in entry["lines"] for entry in body["entries"])

        lab.wait_snapshot(original_reply_visible, "the original worker's reply")
        completed = json.loads(cli("wait", "--session", session, "--json"))
        assert completed["outcome"] == "finished", completed
        assert completed["final_message"] == reply, completed
        original_row = lab.session(lab.snapshot(), session)
        assert not original_row["has_checkpoint"], original_row
        lab.record_action(
            "original-turn-completed", reply=completed["final_message"],
            has_checkpoint=original_row["has_checkpoint"],
        )
        original = record()
        assert original["native_session_id"], original
        root = Path(original["target"]["worker_root"])
        launch = root / "launch.json"
        worker_cwd = Path(json.loads(launch.read_text())["cwd"])
        original_pid = int((root / "worker.pid").read_text())

        exit_path = root / "worker-exit.json"
        startup_path = root / "worker-startup.json"

        def fail_next_boot(label, while_opening=None):
            """Let the next boot fail to open its harness while the daemon is
            offline. The next daemon has no relay snapshot and must discover
            the durable exit record."""
            nonlocal failed_worker
            cli("daemon", "stop")
            subprocess.run(
                [str(args.worker.resolve()), "worker", "stop", "--root", str(root)],
                env=worker_env, check=True, timeout=reliability_lab.TIMEOUT,
            )
            entered.unlink(missing_ok=True)
            release.unlink(missing_ok=True)
            with (lab.root / f"{label}.log").open("w") as log:
                failed_worker = subprocess.Popen(
                    [str(root / "hel"), "worker", "run", "--root", str(root), "--config", str(launch)],
                    env=worker_env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
                )
            deadline = time.monotonic() + reliability_lab.TIMEOUT
            while not entered.exists():
                assert failed_worker.poll() is None, "worker exited before the injected load fault"
                assert time.monotonic() < deadline, "replacement never attempted session/load"
                time.sleep(0.05)
            assert failed_worker.pid != original_pid
            if while_opening is not None:
                while_opening()
            release.touch()
            assert failed_worker.wait(timeout=reliability_lab.TIMEOUT) != 0
            exit_record = json.loads(exit_path.read_bytes())
            assert CAUSE in exit_record["harness_startup_failure"]["message"]
            return exit_record

        def harness_opens():
            return [request["method"] for request in bridge_requests()
                    if request["method"] in ("session/new", "session/load")]

        def mark_closing():
            # A Suspend interrupted after it recorded `Closing`: the next
            # daemon resumes the close against the dead worker.
            database = sqlite3.connect(lab.data / "mj.sqlite3")
            try:
                with database:
                    database.execute(
                        "UPDATE sessions SET state='closing' WHERE session_id=?", (session,)
                    )
            finally:
                database.close()

        prompt = "queued before the harness startup fault"
        mail = "accepted mail before the harness startup fault"

        deliveries = 0

        def deliver_while_opening():
            # The relay accepts durable work while ACP is still opening. Hold
            # the fault until both deliveries land so a later manual start can
            # replay them.
            nonlocal deliveries
            deliveries += 1
            relay = Relay(root / "control.sock")
            relay.request("hello", {
                "controller_version": "startup-failure-test", "supported": {"min": 1, "max": 4294967295},
            })
            relay.submit(f"startup-failure-mail-{deliveries}", "deliver_mailbox_event", {"event": {
                "key": f"startup-failure-mail-{deliveries}", "source": "mjolnir", "wake": False,
                "created_at_ms": int(time.time() * 1000),
                "body": {"type": "parent_message", "text": mail},
            }})
            relay.submit(f"startup-failure-prompt-{deliveries}", "prompt", {
                "prompt": [{"type": "text", "text": prompt}],
            })

        exit_record = fail_next_boot("failed-worker", deliver_while_opening)
        exit_bytes = exit_path.read_bytes()
        boot = startup_path.stat()
        assert CAUSE in exit_record["reason"].splitlines()[0], exit_record["reason"]
        assert "fixture bridge diagnostic" in exit_record["reason"], exit_record["reason"]
        cli("workspaces", "list")
        authenticate()
        snapshot = lab.wait_snapshot(
            lambda snapshot: (lab.session(snapshot, session) or {}).get("state") == "error",
            "relay recovery to publish Error from the dead worker",
        )
        failed = lab.session(snapshot, session)
        assert not failed["has_checkpoint"], failed
        assert CAUSE in record()["last_error"], record()

        deadline = time.monotonic() + RECOVERY_OBSERVATION_SECONDS
        while time.monotonic() < deadline:
            assert exit_path.read_bytes() == exit_bytes, "a subsequent boot replaced the exit record"
            current_boot = startup_path.stat()
            assert (current_boot.st_ino, current_boot.st_mtime_ns) == (boot.st_ino, boot.st_mtime_ns)
            assert not (root / "control.sock").exists(), "failed worker is still serving"
            time.sleep(0.25)
        opens = [request["method"] for request in bridge_requests()
                 if request["method"] in ("session/new", "session/load")]
        assert opens == ["session/new", "session/load"], opens
        assert record()["state"] == "error"
        lab.record_action("terminal-startup-failure", faulted_boots=1, retries=0, cause=record()["last_error"])

        # Resume repairs a retained Error target from its worker root.
        cli("resume", "--session", session, "--queue", "start")
        lab.wait_snapshot(
            lambda snapshot: (row := lab.session(snapshot, session) or {}).get("state") == "running"
            and not row.get("operation")
            and any(prompt in json.dumps(request) for request in bridge_requests()
                    if request["method"] == "session/prompt"),
            "Resume to replay the queued prompt",
        )
        restored_prompts = [request for request in bridge_requests() if request["method"] == "session/prompt"]
        assert sum(prompt in json.dumps(request) for request in restored_prompts) == 1, restored_prompts
        assert any(mail in json.dumps(request) for request in restored_prompts), restored_prompts
        resumed = record()
        assert resumed["target"]["worker_root"] == str(root), resumed
        assert resumed["native_session_id"] == original["native_session_id"], resumed
        assert json.loads(exit_path.read_bytes()) is None, "Resume left a stale failure marker"
        lab.record_action("resume-replayed-prompt-and-mail")
        finished = json.loads(cli("wait", "--session", session, "--json"))
        assert finished["outcome"] == "finished", finished

        # Hard-won: #1617: a worker whose harness could not be prepared kept serving a relay with no harness
        # A replacement worker whose harness cannot even be prepared exits
        # with the same typed marker, so recovery stops at Error instead of
        # attaching to a session that can never run a turn.
        cli("daemon", "stop")
        subprocess.run(
            [str(args.worker.resolve()), "worker", "stop", "--root", str(root)],
            env=worker_env, check=True, timeout=reliability_lab.TIMEOUT,
        )
        # A launch that names no harness home cannot locate the harness's
        # credentials, which fails preparation after the relay is served.
        unpreparable_launch = lab.root / "unpreparable-launch.json"
        unpreparable = json.loads(launch.read_text())
        unpreparable["harness_home"] = ""
        for environment in ("environment", "target_environment"):
            unpreparable.get(environment, {}).pop("CODEX_HOME", None)
        unpreparable_launch.write_text(json.dumps(unpreparable))
        with (lab.root / "unpreparable-worker.log").open("w") as log:
            unprepared_worker = subprocess.Popen(
                [str(root / "hel"), "worker", "run", "--root", str(root),
                 "--config", str(unpreparable_launch)],
                env=worker_env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
            )
        assert unprepared_worker.wait(timeout=reliability_lab.TIMEOUT) != 0
        exit_bytes = exit_path.read_bytes()
        marker = json.loads(exit_bytes)["harness_startup_failure"]["message"]
        assert marker.startswith("harness preparation failed at command-permissions: "), marker
        assert not (root / "control.sock").exists(), "the unprepared worker left its socket"
        boot = startup_path.stat()
        cli("workspaces", "list")
        authenticate()
        lab.wait_snapshot(
            lambda snapshot: (lab.session(snapshot, session) or {}).get("state") == "error",
            "relay recovery to publish Error from the unprepared worker",
        )
        assert marker in record()["last_error"], record()
        deadline = time.monotonic() + RECOVERY_OBSERVATION_SECONDS
        while time.monotonic() < deadline:
            assert exit_path.read_bytes() == exit_bytes, "a subsequent boot replaced the exit record"
            current_boot = startup_path.stat()
            assert (current_boot.st_ino, current_boot.st_mtime_ns) == (boot.st_ino, boot.st_mtime_ns)
            time.sleep(0.25)
        assert record()["state"] == "error"
        lab.record_action("unpreparable-harness-ended-worker", cause=record()["last_error"])
        cli("resume", "--session", session, "--queue", "start")
        lab.wait_snapshot(
            lambda snapshot: (row := lab.session(snapshot, session) or {}).get("state") == "running"
            and not row.get("operation"),
            "Resume to restart the harness after the preparation failure",
        )
        assert json.loads(exit_path.read_bytes()) is None, "Resume left a stale failure marker"
        lab.record_action("resume-after-preparation-failure")

        # Hard-won: #1619: an interrupted Suspend whose restarted harness failed to open ended in Error without a checkpoint
        # A close that cannot save the session without its harness, here
        # because a prompt is still waiting for it, ends in Error rather than
        # leaving a running session on a worker that cannot run it.
        opens = harness_opens()
        prompt = "queued before the closing session's harness fault"
        mail = "accepted mail before the closing session's harness fault"
        fail_next_boot("failed-closing-worker-with-prompt", deliver_while_opening)
        mark_closing()
        cli("workspaces", "list")
        authenticate()
        unable = "without its harness, which failed to open"
        lab.wait_snapshot(
            lambda snapshot: (row := record())["state"] in ("running", "stopped")
            or unable in (row.get("last_error") or ""),
            "the interrupted close to publish its failure",
        )
        stranded = record()
        assert stranded["state"] == "error", stranded
        assert unable in stranded["last_error"], stranded
        assert harness_opens() == opens + ["session/load"], harness_opens()
        lab.record_action("closing-without-harness-kept-pending-prompt", cause=stranded["last_error"])
        cli("resume", "--session", session, "--queue", "start")
        lab.wait_snapshot(
            lambda snapshot: (row := lab.session(snapshot, session) or {}).get("state") == "running"
            and not row.get("operation")
            and any(prompt in json.dumps(request) for request in bridge_requests()
                    if request["method"] == "session/prompt"),
            "Resume to replay the prompt the close could not save",
        )
        finished = json.loads(cli("wait", "--session", session, "--json"))
        assert finished["outcome"] == "finished", finished
        lab.record_action("resume-after-closing-failure-replayed-prompt")

        # Hard-won: #1619: an interrupted Suspend whose restarted harness failed to open ended in Error without a checkpoint
        # With nothing waiting on the harness, the close finishes from a
        # worker started without it, and never opens the harness again.
        opens = harness_opens()
        fail_next_boot("failed-closing-worker")
        mark_closing()
        cli("workspaces", "list")
        authenticate()
        lab.wait_snapshot(
            lambda snapshot: record()["state"] in ("error", "running", "stopped"),
            "the interrupted close to finish",
        )
        suspended = record()
        assert suspended["state"] == "stopped", suspended
        assert suspended["checkpoint"], suspended
        assert not suspended.get("last_error"), suspended
        assert harness_opens() == opens + ["session/load"], harness_opens()
        lab.record_action("closing-without-harness-suspended", checkpoint=bool(suspended["checkpoint"]))
        cli("destroy", "--session", session)
    finally:
        if (lab.data / "daemon.json").exists():
            cli("daemon", "stop")
        if failed_worker is not None and failed_worker.poll() is None:
            release.touch()
            subprocess.run(
                [str(args.worker.resolve()), "worker", "stop", "--root", str(root)],
                env=worker_env, check=True, timeout=reliability_lab.TIMEOUT,
            )
            failed_worker.wait(timeout=reliability_lab.TIMEOUT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named instance")
    lab = reliability_lab.Lab(args.mj, "startup-failure-marker", 0)
    try:
        run(lab, args)
    except BaseException as error:
        lab.trace.update(outcome="failed", failure=str(error))
        raise
    else:
        lab.trace["outcome"] = "passed"
        print(lab.root, flush=True)
    finally:
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()
        lab.cleanup_owned()
        lab.preserve_runtime()
        lab.remove_runtime()


if __name__ == "__main__":
    main()
