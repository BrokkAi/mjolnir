#!/usr/bin/env python3
"""Opt-in real Codex regression for #1302; all sessions use copied login data."""

import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--profile-home", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--child-model", default="gpt-6-luna")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("a named isolated instance is required")
    root = args.root.resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    config, data, profile, project = [root / name for name in ("config", "data", "profile", "project")]
    for directory in (config, data, profile, project):
        directory.mkdir()
    shutil.copy2(args.profile_home / "auth.json", profile / "auth.json")
    (profile / "auth.json").chmod(0o600)
    (profile / "config.toml").write_text('model = ' + json.dumps(args.model) + '\n')
    (project / "README.md").write_text("Isolated Codex notification-wait regression.\n")
    subprocess.run(["git", "init", "-q", str(project)], check=True)
    subprocess.run(["git", "-C", str(project), "add", "README.md"], check=True)
    subprocess.run(["git", "-C", str(project), "-c", "user.name=Mj test", "-c",
                    "user.email=mj-test@example.invalid", "commit", "-qm", "Test fixture"], check=True)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    (config / "config.toml").write_text(f'''version = 17
[phone]
enabled = true
bind = "127.0.0.1:{port}"
tailscale_detect = false
[profiles.live]
kind = "codex"
home = {json.dumps(str(profile))}
[targets.localhost]
kind = "bare"
[targets.podman]
kind = "podman"
pull_policy = "never"
[targets.docker]
kind = "docker"
pull_policy = "never"
[continuation]
enabled = true
[subagents]
max_concurrent = 2
[subagents.straightforward]
model = {json.dumps(args.child_model)}
effort = "high"
[subagents.subtle]
model = {json.dumps(args.model)}
effort = "medium"
''')
    env = os.environ.copy()
    env.update(MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data), MJ_INSTANCE=args.instance,
               MJ_WORKER_BINARY=str(args.worker.resolve()), MJOLNIR_NO_UPDATE_CHECK="1")
    for key in ("MJ_SESSION_ID", "CODEX_HOME", "OPENAI_API_KEY", "CODEX_API_KEY",
                "MJ_DAEMON_OWNER_PID", "MJ_DAEMON_EXIT_WHEN_IDLE"):
        env.pop(key, None)
    mj = [str(args.mj.resolve()), "--instance", args.instance]

    def cli(*command, check=True):
        result = subprocess.run(mj + list(command), env=env, text=True, capture_output=True, timeout=120)
        with (root / "cli.jsonl").open("a") as log:
            log.write(json.dumps({"command": command, "status": result.returncode,
                                  "stdout": result.stdout, "stderr": result.stderr}) + "\n")
        if check and result.returncode:
            raise RuntimeError(f"{command}: {result.stderr or result.stdout}")
        if not check and result.returncode:
            print(f"cleanup/diagnostic {command} failed: {result.stderr or result.stdout}", flush=True)
        return result.stdout

    def eventually(label, predicate, timeout=240):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.5)
        raise RuntimeError(f"timed out: {label}")

    sockets = {}

    def status(session):
        if session not in sockets:
            found = [p for p in data.rglob("control.sock") if session in p.parts]
            if not found:
                return None
            sockets[session] = found[0]
        request = {"request_id": uuid.uuid4().hex, "protocol_version": 39,
                   "request": {"method": "status"}}
        try:
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(5)
                # Like mj-core's local_sockets, use the basename so long data
                # directories do not exceed sockaddr_un.sun_path. This driver
                # is single-threaded and restores cwd before doing other work.
                previous = Path.cwd()
                try:
                    os.chdir(sockets[session].parent)
                    connection.connect(sockets[session].name)
                finally:
                    os.chdir(previous)
                connection.sendall(json.dumps(request).encode() + b"\n")
                reply = json.loads(connection.makefile("rb").readline())
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        assert reply["result"] == "ok", reply
        return reply["payload"]["data"]

    def waiting(session):
        state = status(session)
        return state if state and state["continuation"].get("awaiting_children_since_ms") is not None \
            and state["execution"] == "idle" and state.get("active_prompt") is None \
            and state.get("harness_turn") is None else None

    def relations(parent):
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            return [json.loads(row[0]) for row in database.execute(
                "SELECT record_json FROM subagent_sessions WHERE parent_session_id=? ORDER BY child_session_id", (parent,))]

    parent = None
    gates = [project / "release-a", project / "release-b"]
    observations = []

    def observe(label, state):
        observations.append({"label": label, "at": time.time(), "parent": parent,
                             "continuation": state["continuation"], "activity": state.get("activity"),
                             "active_prompt": state.get("active_prompt"),
                             "queued_prompts": state.get("queued_prompts", [])})
        (root / "observations.json").write_text(json.dumps(observations, indent=2) + "\n")
        print(label, flush=True)

    try:
        cli("workspaces", "create", "Parent wait regression")
        prompt = f'''This is an isolated regression experiment. Do not commit or modify source code.
Spawn exactly two straightforward Mjolnir children, named delayed-a and delayed-b.
Each child must wait for its gate file, then hand back exactly its report token.
Child A gate: {gates[0]}; report token: REPORT_A_1302.
Child B gate: {gates[1]}; report token: REPORT_B_1302.
Tell each child to wait in one shell command using Python with a one-second sleep
between file existence checks; the external test driver will create the gate.
After spawning, you have no independent work: call await_children and end your turn
as its instructions say. Never suspend the session or close a child before collecting
its report. On report notification, call wait, collect every ready report, and await
again if another child is unfinished. One wait collects every ready report: make
exactly one wait call per notification, then await_children and end the turn if
a child remains unfinished. Do not inspect gate files or poll tool wrappers
yourself. User steering may arrive while you wait. Only after both reports are
collected, write both tokens to {project / 'reports-collected'} and give your final answer.
'''
        created = json.loads(cli("new", "--workspace", "Parent wait regression",
                                 "--target", "localhost", "--project-directory", str(project),
                                 "--model", args.model, "--effort", "medium", "--subagents", "mjolnir",
                                 "--no-review", "--no-project-memory", "--json", prompt))
        parent = created["session_id"]
        print("parent:", parent, flush=True)
        first = eventually("parent intentionally waiting", lambda: waiting(parent))
        observe("initial wait", first)
        children = eventually("two children", lambda: (r if len(r := relations(parent)) == 2 else None))
        completed = first["continuation"].get("completed_command_id")
        # A real parent must stay between turns, without empty polling requests.
        for _ in range(30):
            time.sleep(1)
            state = waiting(parent)
            assert state and state["continuation"].get("completed_command_id") == completed, state
        observe("30 seconds without another parent turn", state)
        cli("daemon", "restart")
        observe("wait survives daemon restart", eventually("wait after restart", lambda: waiting(parent)))
        for child in children:
            child_state = status(child["child_session_id"])
            assert child_state and child_state.get("active_prompt"), child_state
        cli("prompt", "--session", parent, "User steering: write STEERING_ACK_1302 to "
            + str(project / "steering-ack") + ". Do this small marker write yourself, spawn no more children, leave both children running, and use await_children again.")
        eventually("steering acknowledged", lambda: (project / "steering-ack").exists())
        second = eventually("parent awaits after steering", lambda: waiting(parent))
        assert second["continuation"].get("completed_command_id") != completed
        observe("user steering resumes parent without stopping children", second)
        gates[0].touch()
        eventually("first report collected", lambda: any(r.get("reported_finish") for r in relations(parent)))
        third = eventually("parent awaits remaining child", lambda: waiting(parent))
        observe("first report collected; awaiting second", third)
        gates[1].touch()
        eventually("both reports collected", lambda: (project / "reports-collected").exists())
        text = (project / "reports-collected").read_text()
        assert "REPORT_A_1302" in text and "REPORT_B_1302" in text, text
        assert all(r.get("reported_finish") for r in relations(parent))
        final = eventually("final parent turn ended", lambda: (
            state if (state := status(parent)) and state.get("active_prompt") is None
            and state["continuation"].get("awaiting_children_since_ms") is None else None))
        observe("both reports collected; task finished", final)
        def parent_commands():
            with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                return [row[0] for row in database.execute(
                    "SELECT command_id FROM session_turn_usage WHERE session_id=? ORDER BY completed_ordinal", (parent,))]

        commands = eventually("four persisted parent turns", lambda: (
            rows if len(rows := parent_commands()) >= 4 else None))
        assert not any(command.startswith("auto-continue-") for command in commands), commands
        assert len(commands) == 4, commands  # Initial task, steering, and two report notifications.
        assert len([command for command in commands if command.startswith("subagent-wait-")]) == 2, commands
        (root / "parent-turns.json").write_text(json.dumps(commands, indent=2) + "\n")
        transcript = cli("transcript", "--session", parent, "--limit", "1000", "--json")
        (root / "parent-transcript.json").write_text(transcript)
        print("PASS: idle parent yields, steering and reports resume it, reports collected once", flush=True)
    finally:
        for gate in gates:
            gate.touch(exist_ok=True)
        try:
            if parent:
                (root / "parent-transcript.json").write_text(
                    cli("transcript", "--session", parent, "--limit", "1000", "--json", check=False))
                cli("suspend", "--session", parent, "--acknowledge-unpublished-work")
                stopped = json.loads(cli("wait", "--session", parent, "--timeout", "90", "--json"))
                assert stopped["outcome"] == "stopped", stopped

                def persisted_stop():
                    with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                        row = database.execute("SELECT state FROM sessions WHERE session_id=?", (parent,)).fetchone()
                        return row and row[0] == "stopped"

                eventually("test parent suspension persisted", persisted_stop, timeout=90)
        finally:
            cli("daemon", "stop", check=False)


if __name__ == "__main__":
    main()
