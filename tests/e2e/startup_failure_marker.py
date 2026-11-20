#!/usr/bin/env python3
"""Stop relay recovery after a replacement worker's ACP session/load fails."""

import argparse
import json
from pathlib import Path
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
    assert script.count(branch) == 1
    script = script.replace(branch, branch + f"""
        if method == "session/load" and not os.path.exists({str(entered)!r}):
            open({str(entered)!r}, "w").close()
            while not os.path.exists({str(release)!r}):
                time.sleep(0.02)
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
        lab.wait_snapshot(
            lambda snapshot: (lab.session(snapshot, session) or {}).get("capabilities", {}).get("prompt"),
            "the original worker's configured session",
        )
        original = record()
        assert original["native_session_id"], original
        root = Path(original["target"]["worker_root"])
        launch = root / "launch.json"
        worker_cwd = Path(json.loads(launch.read_text())["cwd"])
        original_pid = int((root / "worker.pid").read_text())

        # Let the process fail while the daemon is offline. The next daemon
        # has no relay snapshot and must discover the durable exit record.
        cli("daemon", "stop")
        subprocess.run(
            [str(args.worker.resolve()), "worker", "stop", "--root", str(root)],
            env=worker_env, check=True, timeout=reliability_lab.TIMEOUT,
        )
        with (lab.root / "failed-worker.log").open("w") as log:
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

        # The relay accepts durable work while ACP is still opening. Hold the
        # fault until both deliveries land so a later manual start can replay them.
        relay = Relay(root / "control.sock")
        relay.request("hello", {
            "controller_version": "startup-failure-test", "supported": {"min": 1, "max": 4294967295},
        })
        prompt = "queued before the harness startup fault"
        mail = "accepted mail before the harness startup fault"
        relay.submit("startup-failure-mail", "deliver_mailbox_event", {"event": {
            "key": "startup-failure-mail", "source": "mjolnir", "wake": False,
            "created_at_ms": int(time.time() * 1000),
            "body": {"type": "parent_message", "text": mail},
        }})
        relay.submit("startup-failure-prompt", "prompt", {
            "prompt": [{"type": "text", "text": prompt}],
        })
        release.touch()
        assert failed_worker.wait(timeout=reliability_lab.TIMEOUT) != 0

        exit_path = root / "worker-exit.json"
        startup_path = root / "worker-startup.json"
        exit_bytes = exit_path.read_bytes()
        boot = startup_path.stat()
        assert CAUSE in json.loads(exit_bytes)["harness_startup_failure"]["message"]
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
