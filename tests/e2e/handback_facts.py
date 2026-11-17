#!/usr/bin/env python3
"""Opt-in live regression: a child's report carries what Mjolnir observed.

A real parent spawns a real child that starts a detached `setsid sleep`, leaves
an uncommitted file and hands back claiming everything is committed. The test
checks, against the machine's own process table, that the parent's report lists
the sleep as running and detached and the file as uncommitted, in the one
message (the worker observes both when the turn ends and publishes them with
the turn's outcome), and that the stored outcome carries the same facts. With `--kill-daemon-at-handback` the daemon is killed
between the child's handback and its report, and the next daemon's report must
still carry the real facts. It runs on whichever harness `--harness` names.

The store is read through Node's `node:sqlite` (Node 22.5 or newer): the
system SQLite behind some hosts' Python cannot open the daemon's strict tables.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time

NODE_SQL = """
import { DatabaseSync } from "node:sqlite";
const [path, sql, params] = process.argv.slice(1);
const db = new DatabaseSync(path, { readOnly: true });
console.log(JSON.stringify(db.prepare(sql).all(...JSON.parse(params)).map(r => Object.values(r))));
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--harness", choices=("codex", "claude"), required=True)
    parser.add_argument("--profile-home", type=Path, required=True,
                        help="a copy of the harness profile home holding its login")
    parser.add_argument("--root", type=Path, required=True,
                        help="a new directory under /home/jonathan/.cache/mjolnir/build")
    parser.add_argument("--instance", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--child-model", required=True)
    parser.add_argument("--kill-daemon-at-handback", action="store_true",
                        help="kill -9 the daemon the moment the child's handback is recorded, "
                             "restart it, and require the report's facts to be real")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("a named isolated instance is required")
    root = args.root.resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    config, data, profile, project, home = [root / name for name in
                                            ("config", "data", "profile", "project", "home")]
    for directory in (config, data, profile, project):
        directory.mkdir()
    home.mkdir(mode=0o700)
    credential = "auth.json" if args.harness == "codex" else ".credentials.json"
    shutil.copy2(args.profile_home / credential, profile / credential)
    (profile / credential).chmod(0o600)
    if args.harness == "codex":
        (profile / "config.toml").write_text("model = " + json.dumps(args.model) + "\n")
    (project / "README.md").write_text("handback facts\n")
    for command in (["init", "-q"], ["add", "README.md"],
                    ["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-qm", "fixture"]):
        subprocess.run(["git", "-C", str(project), *command], check=True)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    (config / "config.toml").write_text(f'''version = 18
[phone]
enabled = true
bind = "127.0.0.1:{port}"
tailscale_detect = false
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
[subagents]
max_concurrent = 2
[subagents.straightforward]
model = {json.dumps(args.child_model)}
effort = "low"
[subagents.subtle]
model = {json.dumps(args.model)}
effort = "low"
''')
    assert str(args.profile_home) not in (config / "config.toml").read_text()
    # Start from nothing inherited: a live session's own variables would point
    # the isolated instance at real profile homes.
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "TERM": "xterm", "LANG": "C.UTF-8",
           "MJ_CONFIG_DIR": str(config), "MJ_DATA_DIR": str(data), "MJ_INSTANCE": args.instance,
           "MJ_WORKER_BINARY": str(args.worker.resolve()), "MJOLNIR_NO_UPDATE_CHECK": "1",
           "CODEX_HOME" if args.harness == "codex" else "CLAUDE_CONFIG_DIR": str(profile)}
    if "XDG_CACHE_HOME" in os.environ:
        env["XDG_CACHE_HOME"] = os.environ["XDG_CACHE_HOME"]
    mj = [str(args.mj.resolve()), "--instance", args.instance]

    def cli(*command, check=True):
        result = subprocess.run(mj + list(command), env=env, text=True, capture_output=True, timeout=180)
        if check and result.returncode:
            raise RuntimeError(f"{command}: {result.stderr or result.stdout}")
        return result.stdout

    def query(sql, params=()):
        result = subprocess.run(
            ["node", "--no-warnings", "--input-type=module", "-e", NODE_SQL,
             str(data / "mj.sqlite3"), sql, json.dumps(list(params))],
            text=True, capture_output=True, timeout=60)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout)

    def eventually(label, predicate, timeout=600):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.5)
        raise RuntimeError(f"timed out: {label}")

    def alive(pid):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
        except OSError:
            return False
        return not stat.rsplit(")", 1)[1].split()[0].startswith("Z")

    def daemon_pids():
        found = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                cmdline = (entry / "cmdline").read_bytes().split(b"\0")
                environ = (entry / "environ").read_bytes().split(b"\0")
            except OSError:
                continue
            if b"daemon-run" in cmdline and f"MJ_DATA_DIR={data}".encode() in environ:
                found.append(int(entry.name))
        return found

    parent = None
    child = None
    sleep_pid = None
    try:
        cli("workspaces", "create", "handback facts")
        prompt = f'''Isolated regression. Spawn exactly one straightforward Mjolnir child named facts-child. Its instructions, verbatim:
"Run `setsid nohup sleep 600 >/dev/null 2>&1 &` as its own Bash tool call. Then create a file notes.txt containing the word draft in your current working directory, and do not commit it. Then hand back with exactly this report text: Everything is committed and nothing is left running."
After spawning, end your turn with a one-line status. Each time a message from Mjolnir about the child arrives, reply with one line.'''
        created = json.loads(cli("new", "--workspace", "handback facts", "--target", "localhost",
                                 "--project-directory", str(project), "--model", args.model,
                                 "--effort", "low", "--subagents", "mjolnir", "--review", "off",
                                 "--no-project-memory", "--json", prompt))
        parent = created["session_id"]
        child = eventually("child spawned", lambda: (rows[0][0] if (rows := query(
            "SELECT child_session_id FROM subagent_sessions WHERE parent_session_id=?", [parent])) else None))

        def events(prefix):
            return [json.loads(row[0]) for row in query(
                "SELECT event_json FROM mailbox_outbox WHERE target_session_id=? AND event_key LIKE ?",
                [parent, prefix + "%"])]

        if args.kill_daemon_at_handback:
            # The child has handed back; its turn has not ended and no report
            # exists. Killing the daemon here leaves the finish to the next one,
            # which reads the facts from the outcome its worker published.
            eventually("handback recorded", lambda: query(
                "SELECT 1 FROM subagent_handbacks WHERE child_session_id=? AND handback_command_id IS NOT NULL",
                [child]), timeout=600)
            for pid in daemon_pids():
                os.kill(pid, 9)
            eventually("daemon gone", lambda: not daemon_pids(), timeout=30)
            rows = query("SELECT last_turn_outcome_json FROM materialized_sessions WHERE session_id=?", [child])
            recorded = bool(rows and rows[0][0])
            reported = events("subagent-finished-")
            print(f"daemon killed at handback; turn outcome already recorded: {recorded}; "
                  f"report already sent: {bool(reported)}", flush=True)
            assert not reported, "the window was missed: the report was already sent"
            # Any ordinary command starts the next daemon, whose reconcile and
            # delegation loop pick the finish up again.
            cli("workspaces", "list")
        finish = eventually("finish report", lambda: events("subagent-finished-"))[0]
        report = finish["body"]["report"]
        facts = report["facts"]
        assert facts["processes"]["status"] == "known", facts
        processes = facts["processes"]["value"]
        sleeping = [p for p in processes["shown"] if "sleep 600" in p["command"]]
        assert sleeping and sleeping[0]["detached"], processes
        sleep_pid = sleeping[0]["pid"]
        # The processes and the working tree arrive in the one message, as the
        # worker observed them (or unavailable with why, which this run must not see).
        assert facts["worktree"]["status"] == "known", facts
        tree = facts["worktree"]["value"]
        assert tree["untracked"] >= 1 and any(p["path"] == "notes.txt" for p in tree["shown"]), tree
        # The facts are part of the outcome record that ended the turn: the same
        # observation is stored with it.
        rows = query("SELECT last_turn_outcome_json FROM materialized_sessions WHERE session_id=?", [child])
        outcome = json.loads(rows[0][0]) if rows and rows[0][0] else None
        assert outcome and outcome.get("work_facts") == facts, (outcome, facts)
        # The child's claim and Mjolnir's observation disagree, and both are in the text.
        text = subprocess.run(
            mj + ["transcript", "--session", parent, "--limit", "1000"],
            env=env, text=True, capture_output=True, timeout=120).stdout
        assert "Mjolnir observed this when it recorded this finish" in text, text[-3000:]
        assert "detached from the harness: " in text and "sleep 600" in text, text[-3000:]
        assert "notes.txt" in text and "still being read" not in text, text[-3000:]
        lines = text.splitlines()
        first = next(i for i, line in enumerate(lines) if "Mjolnir observed this when it recorded" in line)
        print("REPORT TEXT THE PARENT RECEIVED:\n" + "\n".join(lines[max(0, first - 3):first + 18]), flush=True)
        print(f"PASS ({args.harness}): the report listed the detached sleep and the untracked file together", flush=True)
    finally:
        if sleep_pid and alive(sleep_pid):
            os.kill(sleep_pid, 9)
        for session in (child, parent):
            if session:
                cli("destroy", "--session", session, check=False)
        cli("daemon", "stop", check=False)
        shutil.rmtree(profile, ignore_errors=True)


if __name__ == "__main__":
    main()
