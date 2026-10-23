#!/usr/bin/env python3
"""Opt-in live regression for mid-turn and idle mailbox child reports."""

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
    parser.add_argument("--harness", choices=("codex", "claude"), default="codex")
    parser.add_argument("--profile-home", type=Path,
                        help="source harness home; defaults to ~/.codex or ~/.claude")
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--child-model", default="gpt-6-luna")
    parser.add_argument("--no-github-events", action="store_true",
                        help="start the parent with --no-github-events; child reports must still arrive")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("a named isolated instance is required")
    root = args.root.resolve()
    required_root = Path("/home/jonathan/.cache/mjolnir/build").resolve()
    if not root.is_relative_to(required_root):
        parser.error(f"--root must be under {required_root}")
    source_home = args.profile_home or (Path.home() / (".codex" if args.harness == "codex" else ".claude"))
    source_home = source_home.expanduser().resolve()
    credential = "auth.json" if args.harness == "codex" else ".credentials.json"
    if not (source_home / credential).is_file():
        parser.error(f"{args.harness} credentials not found: {source_home / credential}")
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    config, data, profile, project, home = [root / name for name in
                                            ("config", "data", "profile", "project", "home")]
    for directory in (config, data, profile, project):
        directory.mkdir()
    home.mkdir(mode=0o700)
    shutil.copy2(source_home / credential, profile / credential)
    (profile / credential).chmod(0o600)
    if args.harness == "codex":
        (profile / "config.toml").write_text('model = ' + json.dumps(args.model) + '\n')
    (project / "README.md").write_text("Isolated parent report notification regression.\n")
    subprocess.run(["git", "init", "-q", str(project)], check=True)
    subprocess.run(["git", "-C", str(project), "add", "README.md"], check=True)
    subprocess.run(["git", "-C", str(project), "-c", "user.name=Mj test", "-c",
                    "user.email=mj-test@example.invalid", "commit", "-qm", "Test fixture"], check=True)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    (config / "config.toml").write_text(f'''version = 18
[phone]
enabled = true
bind = "127.0.0.1:{port}"
tailscale_detect = false
[github.watch]
enabled = true
[profiles.live]
kind = "{args.harness}"
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
    isolated_config = (config / "config.toml").read_text()
    assert str(source_home) not in isolated_config, "isolated config contains the real harness home"
    assert str(profile) in isolated_config, "isolated config does not point at the copied profile"
    env = os.environ.copy()
    env.update(MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data), MJ_INSTANCE=args.instance,
               MJ_WORKER_BINARY=str(args.worker.resolve()), MJOLNIR_NO_UPDATE_CHECK="1",
               HOME=str(home))
    for key in ("MJ_SESSION_ID", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "OPENAI_API_KEY",
                "CODEX_API_KEY", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN", "MJ_DAEMON_OWNER_PID", "MJ_DAEMON_EXIT_WHEN_IDLE"):
        env.pop(key, None)
    env["CODEX_HOME" if args.harness == "codex" else "CLAUDE_CONFIG_DIR"] = str(profile)
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
        # The worker serves only its own protocol, which hello names.
        hello = {"request_id": uuid.uuid4().hex, "protocol_version": 1,
                 "request": {"method": "hello", "params": {
                     "controller_version": "parent-report-e2e",
                     "supported": {"min": 1, "max": 4294967295}}}}
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
                with connection.makefile("rb") as reader:
                    connection.sendall(json.dumps(hello).encode() + b"\n")
                    greeting = json.loads(reader.readline())
                    assert greeting["result"] == "ok", greeting
                    request = {"request_id": uuid.uuid4().hex,
                               "protocol_version": greeting["protocol_version"],
                               "request": {"method": "status"}}
                    connection.sendall(json.dumps(request).encode() + b"\n")
                    reply = json.loads(reader.readline())
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        assert reply["result"] == "ok", reply
        return reply["payload"]["data"]

    def waiting(session):
        state = status(session)
        return state if state and state["execution"] == "idle" and state.get("active_prompt") is None \
            and state.get("harness_turn") is None else None

    def relations(parent):
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            return [json.loads(row[0]) for row in database.execute(
                "SELECT record_json FROM subagent_sessions WHERE parent_session_id=? ORDER BY child_session_id", (parent,))]

    def parent_commands():
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            return [row[0] for row in database.execute(
                "SELECT command_id FROM session_turn_usage WHERE session_id=? ORDER BY completed_ordinal", (parent,))]

    def finished_events():
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            rows = database.execute(
                "SELECT event_key, event_json, wake, accepted_at, accepted_command_id "
                "FROM mailbox_outbox WHERE target_session_id=? AND event_key LIKE 'subagent-finished-%' "
                "ORDER BY created_at, event_key", (parent,)).fetchall()
        return [{"event_key": key, "event": json.loads(event), "wake": bool(wake),
                 "accepted_at": accepted_at, "accepted_command_id": command_id}
                for key, event, wake, accepted_at, command_id in rows]

    def report_for(child_id):
        rows = [row for row in finished_events()
                if row["event"].get("body", {}).get("type") == "subagent_finished"
                and row["event"]["body"].get("report", {}).get("child_session_id") == child_id]
        return rows[0] if rows else None

    def check_report_event(entry, child_id, task_name, token):
        assert entry, f"no SubagentFinished outbox event for {task_name} ({child_id})"
        event = entry["event"]
        report = event.get("body", {}).get("report", {})
        assert event.get("wake") is True and entry["wake"], entry
        assert event.get("source") == "mjolnir", entry
        assert entry["event_key"].startswith("subagent-finished-"), entry
        assert entry["accepted_at"], entry
        assert (entry["accepted_command_id"] or "").startswith("mailbox-"), entry
        assert report.get("child_session_id") == child_id, report
        assert report.get("task_name") == task_name, report
        assert report.get("state") and report.get("finished") is True, report
        assert token in (report.get("output") or ""), report
        assert isinstance(report.get("report_dir"), str) and report["report_dir"], report
        return entry

    parent = None
    child_ids = []
    gates = [project / "release-a", project / "release-b"]
    observations = []

    def observe(label, state):
        observations.append({"label": label, "at": time.time(), "parent": parent,
                             "continuation": state.get("continuation"), "activity": state.get("activity"),
                             "active_prompt": state.get("active_prompt"),
                             "queued_prompts": state.get("queued_prompts", [])})
        (root / "observations.json").write_text(json.dumps(observations, indent=2) + "\n")
        print(label, flush=True)

    try:
        cli("workspaces", "create", "Parent report notification regression")
        prompt = f'''This is an isolated regression experiment. Do not commit or modify source code.
Spawn exactly two straightforward Mjolnir children named delayed-a and delayed-b.
Each child must wait for its gate in one shell command, checking once a second,
then hand back exactly its assigned report token. A gate: {gates[0]}, token:
REPORT_A_1302. B gate: {gates[1]}, token: REPORT_B_1302.
After both are spawned, write a marker file at {project / 'midturn-ready'}.
Keep doing independent work as four separate shell tool calls, each running
`sleep 15`. After every call, read any Mjolnir mailbox context delivered by its
PostToolUse/PostToolBatch hook. When A's SubagentFinished report arrives, write
REPORT_A_1302 to {project / 'report-a'} during this same turn, then continue the
remaining sleep calls. End this first turn only after all four calls finish and
the A token is written. B will still be gated; end with a brief waiting status.
Never inspect either gate. After user steering, write STEERING_ACK_1302 to
{project / 'steering-ack'} and end briefly while B is still running. When B's
SubagentFinished event wakes you, read its embedded report, write REPORT_B_1302
to {project / 'report-b'}, and give your final answer.
'''
        new_command = ["new", "--workspace", "Parent report notification regression", "--target", "localhost",
                       "--project-directory", str(project), "--model", args.model,
                       "--effort", "medium", "--subagents", "mjolnir", "--review", "off",
                       "--no-project-memory", "--json"]
        if args.no_github_events:
            new_command.append("--no-github-events")
        created = json.loads(cli(*new_command, prompt))
        parent = created["session_id"]
        print("parent:", parent, flush=True)
        children = eventually("two children", lambda: (r if len(r := relations(parent)) == 2 else None))
        child_ids = [child["child_session_id"] for child in children]
        child_a = next(child for child in children if child.get("task_name") == "delayed-a")
        child_b = next(child for child in children if child.get("task_name") == "delayed-b")
        eventually("parent starts independent work", lambda: (project / "midturn-ready").exists())
        first_active = status(parent)
        assert first_active and first_active.get("active_prompt"), first_active
        initial_command = first_active["active_prompt"]["command_id"]
        time.sleep(2)
        gates[0].touch()
        event_a = eventually("A report accepted into parent's mailbox", lambda: report_for(child_a["child_session_id"]))
        event_a = check_report_event(event_a, child_a["child_session_id"], "delayed-a", "REPORT_A_1302")
        eventually("parent wrote A token during first turn", lambda: (project / "report-a").exists())
        active_after_a = status(parent)
        assert active_after_a and active_after_a.get("active_prompt", {}).get("command_id") == initial_command, active_after_a
        assert not parent_commands(), "A's report started a new turn while the original turn was active"
        assert "REPORT_A_1302" in (project / "report-a").read_text()
        observe("A report handled through mailbox hook inside initial turn", active_after_a)

        first = eventually("initial parent turn ended", lambda: waiting(parent))
        initial_turns = eventually("initial turn persisted", lambda: (
            commands if (commands := parent_commands()) == [initial_command] else None))
        assert "REPORT_A_1302" in (project / "report-a").read_text()
        # Idle parent must remain parked between turns for 30 seconds.
        for _ in range(30):
            time.sleep(1)
            state = waiting(parent)
            assert state and parent_commands() == initial_turns, state
        observe("30 seconds idle without auto-continue", state)
        cli("daemon", "restart")
        observe("idle state survives daemon restart", eventually("idle after restart", lambda: waiting(parent)))
        child_b_state = status(child_b["child_session_id"])
        assert child_b_state and child_b_state.get("active_prompt"), child_b_state
        cli("message", "--session", parent, "User steering: write STEERING_ACK_1302 to "
            + str(project / "steering-ack") + ". Do this small marker write yourself, spawn no more children, leave B running, then end your turn with a brief waiting status.")
        eventually("steering acknowledged", lambda: (project / "steering-ack").exists())
        second = eventually("parent idle after steering", lambda: waiting(parent))
        steering_turns = eventually("steering turn persisted", lambda: (
            commands if len(commands := parent_commands()) == 2 else None))
        assert steering_turns[0] == initial_command, steering_turns
        observe("steering acknowledged; B remains gated", second)
        gates[1].touch()
        event_b = eventually("B report accepted into parent's mailbox", lambda: report_for(child_b["child_session_id"]))
        event_b = check_report_event(event_b, child_b["child_session_id"], "delayed-b", "REPORT_B_1302")
        wake_state = eventually("B mailbox event wakes the idle parent", lambda: (
            state if (state := status(parent)) and state.get("active_prompt") else None))
        assert wake_state["active_prompt"]["command_id"].startswith("mailbox-wake-"), wake_state
        eventually("parent wrote B token", lambda: (project / "report-b").exists())
        assert "REPORT_B_1302" in (project / "report-b").read_text()
        final = eventually("final parent turn ended", lambda: (
            state if (state := status(parent)) and state.get("active_prompt") is None
            and state["execution"] == "idle" and state.get("harness_turn") is None else None))
        observe("B report handled by its idle wake turn", final)
        commands = eventually("three persisted parent turns", lambda: (
            rows if len(rows := parent_commands()) >= 3 else None))
        assert not any(command.startswith("auto-continue-") for command in commands), commands
        assert len(commands) == 3, commands  # Initial work, user steering, and B mailbox wake.
        assert commands[0] == initial_command, commands
        assert sum(command.startswith("mailbox-wake-") for command in commands) == 1, commands
        assert commands[2].startswith("mailbox-wake-"), commands
        assert not any(command.startswith("subagent-wait-") for command in commands), commands
        all_events = finished_events()
        assert len(all_events) == 2, all_events
        assert {event["event"]["body"]["report"]["child_session_id"] for event in all_events} == set(child_ids), all_events
        (root / "parent-turns.json").write_text(json.dumps(commands, indent=2) + "\n")
        (root / "parent-mailbox-events.json").write_text(
            json.dumps(all_events, indent=2) + "\n")
        transcript = cli("transcript", "--session", parent, "--limit", "1000", "--json")
        (root / "parent-transcript.json").write_text(transcript)
        json.loads(transcript)
        print("PASS: A delivered mid-turn; B woke idle parent; exactly three turns", flush=True)
    finally:
        for gate in gates:
            gate.touch(exist_ok=True)
        try:
            if parent:
                (root / "parent-transcript.json").write_text(
                    cli("transcript", "--session", parent, "--limit", "1000", "--json", check=False))
                cli("suspend", "--session", parent, "--acknowledge-unpublished-work", check=False)
                for child in child_ids:
                    cli("wait", "--session", child, "--timeout", "90", "--json", check=False)
                    cli("destroy", "--session", child, check=False)
                cli("destroy", "--session", parent, check=False)

                def sessions_destroyed():
                    with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                        owned = [parent, *child_ids]
                        placeholders = ",".join("?" for _ in owned)
                        remaining = database.execute(
                            f"SELECT session_id FROM sessions WHERE session_id IN ({placeholders})", owned).fetchall()
                        return not remaining

                eventually("test parent and children destroyed", sessions_destroyed, timeout=120)
        finally:
            cli("daemon", "stop", check=False)


if __name__ == "__main__":
    main()
