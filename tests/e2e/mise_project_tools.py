#!/usr/bin/env python3
"""Real Podman, worker, daemon and Codex: install a cloned repository's tools.

Build mj and a portable mj-worker, then pass their paths and a logged-in Codex
profile directory. No prompts or paid model calls are made. Every mj invocation
uses --instance mise-w (or another explicitly named isolated instance) and private
configuration/data directories; the daemon is stopped and containers destroyed
before exit. Text evidence is copied to --artifacts.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time


WORKER_QUERY = r'''
import json, socket, sys
root = "/var/lib/hel/workers/" + sys.argv[1]
with socket.socket(socket.AF_UNIX) as connection:
    connection.connect(root + "/control.sock")
    reader = connection.makefile("rb")
    def request(method, params=None):
        request = {"method":method}
        if params is not None: request["params"] = params
        envelope = {"request_id":method,"protocol_version":protocol,"request":request}
        connection.sendall(json.dumps(envelope).encode()+b"\n")
        response = json.loads(reader.readline())
        assert response["result"] == "ok", response
        return response
    protocol = 1
    hello = request("hello", {"controller_version":"mise-e2e","supported":{"min":1,"max":4294967295}})
    protocol = hello["protocol_version"]
    if len(sys.argv) > 2:
        request("submit", {"command_id":"mise-e2e-versions","command":{"type":"run_user_shell","data":{"command":sys.argv[2]}}})
    print(json.dumps(request("status")))
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--profile-home", type=Path, required=True)
    parser.add_argument("--instance", default="mise-w")
    parser.add_argument("--artifacts", type=Path, required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use an isolated named instance")
    args.artifacts.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="mise-w-"))
    config, data = root / "config", root / "data"
    config.mkdir()
    data.mkdir()
    profile = root / "profile"
    profile.mkdir(mode=0o700)
    # Exercise the real harness without inheriting plugins, hooks or MCP
    # integrations from the source profile. Never rewrite its live login.
    shutil.copyfile(args.profile_home.resolve() / "auth.json", profile / "auth.json")
    (profile / "auth.json").chmod(0o600)
    (profile / "config.toml").write_text("")
    # Deliberately do not inherit session instance/config/profile variables.
    env = {name: os.environ[name] for name in ["HOME", "PATH", "USER", "LOGNAME", "LANG", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] if name in os.environ}
    env.update(MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data),
               MJ_INSTANCE=args.instance, MJ_WORKER_BINARY=str(args.worker.resolve()))
    sessions = []
    server = None
    last_logged = {}

    def run(argv, *, check=True, input=None):
        result = subprocess.run(argv, env=env, capture_output=True, text=True, input=input)
        key = tuple(map(str, argv))
        outcome = (result.returncode, result.stdout, result.stderr)
        if last_logged.get(key) != outcome:
            with (args.artifacts / "commands.jsonl").open("a") as log:
                log.write(json.dumps({"argv":list(key), "status":result.returncode,
                                      "stdout":result.stdout,"stderr":result.stderr}) + "\n")
            last_logged[key] = outcome
        if check and result.returncode:
            raise RuntimeError(f"{argv}: {result.stderr}")
        return result.stdout

    def cli(*argv, check=True):
        return run([str(args.mj.resolve()), "--instance", args.instance, *argv], check=check)

    def wait(predicate, description):
        deadline = time.monotonic() + 1200
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.2)
        raise RuntimeError("timed out: " + description)

    def container(session):
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            row = database.execute("SELECT resource_id FROM session_targets WHERE session_id=?", (session,)).fetchone()
        return row[0] if row else None

    def journal(cid, worker_root):
        # The agent's mailbox messages are journaled when the worker queues
        # them; no prompt is sent, so none is delivered to the model.
        return run(["podman", "exec", cid, "sh", "-c", f"cat {worker_root}/relay-journal/*.jsonl 2>/dev/null; true"])

    def mailbox_messages(cid, worker_root):
        """The agent-bound project-tools messages (their texts), in journal order."""
        texts = []
        for line in journal(cid, worker_root).splitlines():
            if '"source":"project-tools"' in line:
                texts.extend(find_texts(json.loads(line)))
        return texts

    def find_texts(value):
        if isinstance(value, dict):
            if value.get("source") == "project-tools":
                body = value["body"]
                return [body["data"]["text"] if "data" in body else body["text"]]
            return [t for item in value.values() for t in find_texts(item)]
        if isinstance(value, list):
            return [t for item in value for t in find_texts(item)]
        return []

    try:
        for recipe in ["tools", "old-node", "broken", "empty"]:
            repo = root / recipe
            repo.mkdir()
            if recipe == "tools":
                (repo / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.99.1"\nprofile = "minimal"\n')
                (repo / ".nvmrc").write_text("24\n")
            elif recipe == "old-node":
                (repo / ".nvmrc").write_text("18\n")
            elif recipe == "broken":
                (repo / "mise.toml").write_text('[tools]\n"core:mj-e2e-nonexistent-tool" = "1"\n')
            (repo / "README.md").write_text("mise session fixture\n")
            run(["git", "init", "--initial-branch=main", str(repo)])
            run(["git", "-C", str(repo), "add", "."])
            run(["git", "-C", str(repo), "-c", "user.name=Mise Test", "-c", "user.email=mise@example.invalid", "commit", "-m", "recipe"])
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        server = subprocess.Popen(["git", "daemon", "--reuseaddr", "--export-all", "--listen=0.0.0.0", f"--port={port}", f"--base-path={root}", str(root)], env=env, start_new_session=True,
                                  stdout=(args.artifacts / "git-daemon.log").open("w"), stderr=subprocess.STDOUT)
        text = f'''version = 12
[profiles.test]
kind = "codex"
home = {json.dumps(str(profile))}
[targets.podman]
kind = "podman"
image = "ghcr.io/brokkai/mjolnir/session-base:latest"
pull_policy = "never"
workspace_storage = {{ kind = "podman-volume" }}
[targets.docker]
kind = "docker"
pull_policy = "never"
[machines.local]
kind = "local"
build_cache = {{ enabled = false }}
[github.watch]
enabled = false
[jev]
enabled = false
'''
        for recipe in ["tools", "old-node", "broken", "empty"]:
            repo = root / recipe
            run(["git", "-C", str(repo), "remote", "add", "origin", f"git://host.containers.internal:{port}/{recipe}"])
            text += f'''\n[bundles.{recipe}]
primary_repo = "fixture"
[[bundles.{recipe}.repositories]]
id = "fixture"
local = {json.dumps(str(repo))}
destination = "fixture"
'''
        (config / "config.toml").write_text(text)
        cli("workspaces", "create", "Mise worker")
        for recipe in ["tools", "old-node", "broken", "empty"]:
            session = json.loads(cli("--workspace", "Mise worker", "new", "--profile", "test", "--target", "podman", "--bundle", recipe,
                                     "--subagents", "none", "--review", "off", "--no-project-memory", "--no-github-events", "--no-github-credentials", "--json"))["session_id"]
            sessions.append(session)
            def ready():
                record = json.loads(cli("sessions", "--session", session, "--json"))
                if record.get("has_error"):
                    raise RuntimeError(record)
                return record["state"] == "running"
            wait(ready, f"{recipe} session ready")
            cid = container(session)
            worker_root = f"/var/lib/hel/workers/{session}"
            at_ready = mailbox_messages(cid, worker_root)
            if recipe == "tools":
                # The session is running before the toolchain download ends:
                # the agent was told what is installing and nothing more yet.
                assert len(at_ready) == 1 and "being installed in the background" in at_ready[0], at_ready
                assert "rust" in at_ready[0] and "node" in at_ready[0] and "second message" in at_ready[0], at_ready
            if recipe != "empty":
                messages = wait(lambda: len(m := mailbox_messages(cid, worker_root)) >= 2 and m, f"{recipe} install result message")
                (args.artifacts / f"{recipe}-mailbox.json").write_text(json.dumps(messages, indent=2))
                assert "being installed in the background" in messages[0], messages
                assert len(messages) == 2, messages
                if recipe == "broken":
                    assert "did not finish" in messages[1] and "project-tools.log" in messages[1] and "mise install" in messages[1] and "failed" in messages[1], messages
                else:
                    assert "finished installing" in messages[1], messages
            else:
                assert not at_ready and not mailbox_messages(cid, worker_root), at_ready
            (args.artifacts / f"{recipe}-journal.jsonl").write_text(journal(cid, worker_root))
            # Hard-won: #1493: generic images have no /home/hel before bootstrap writes Git config.
            run(["podman", "exec", cid, "sh", "-c", 'test -d "$HOME" && test "$(stat -c %u:%g "$HOME")" = "$(id -u):$(id -g)"'])
            run(["podman", "exec", cid, "python3", "-c",
                 "import json,sys; s=json.load(open(sys.argv[1])); assert 'MJ_SESSION_PROJECT_TOOLS_ROOTS' not in s['environment']",
                 worker_root + "/acp-supervisor.json"])
            startup = run(["podman", "exec", cid, "cat", worker_root + "/worker-startup.json"])
            (args.artifacts / f"{recipe}-startup.json").write_text(startup)
            if recipe != "empty":
                assert "Install project tools" in startup, startup
            else:
                assert "Install project tools" not in startup, startup
            if recipe == "tools":
                # Same durable user-shell command the TUI's ! command sends.
                run(["podman", "exec", "-i", cid, "python3", "-", session,
                     "cargo --version; node --version; test -z \"${MJ_SESSION_PROJECT_TOOLS_ROOTS+x}\""], input=WORKER_QUERY)
                def versions():
                    transcript = cli("transcript", "--session", session, "--json")
                    return transcript if "cargo 1.99.1" in transcript and "v24." in transcript else None
                transcript = wait(versions, "cargo/node user-shell transcript")
                (args.artifacts / "versions-transcript.json").write_text(transcript)
                print("tools: Install project tools; user shell: cargo 1.99.1, node v24.x", flush=True)
            elif recipe == "old-node":
                supervisor = json.loads(run(["podman", "exec", cid, "python3", "-c",
                    'import json,sys; s=json.load(open(sys.argv[1])); print(json.dumps({"command":s["command"],"args":s["args"],"environment":{k:s["environment"][k] for k in ["PATH","CODEX_PATH"] if k in s["environment"]}}))',
                    worker_root + "/acp-supervisor.json"]))
                node = supervisor["command"]
                version = run(["podman", "exec", cid, node, "--version"]).strip()
                assert int(version.lstrip("v").split(".")[0]) >= 22, (node, version)
                assert supervisor["environment"]["PATH"].split(":")[0].endswith("/shims"), supervisor
                run(["podman", "exec", "-i", cid, "python3", "-", session, "node --version"], input=WORKER_QUERY)
                transcript = wait(lambda: (value if "v18." in (value := cli("transcript", "--session", session, "--json")) else None), "Node18 tool transcript")
                (args.artifacts / "old-node-transcript.json").write_text(transcript)
                (args.artifacts / "old-node-supervisor.json").write_text(json.dumps(supervisor, indent=2))
                print(f"old-node: bridge started on {version}; session user shell: node v18.x; shims first", flush=True)
            elif recipe == "broken":
                transcript = cli("transcript", "--session", session, "--json")
                assert "project-tools.log" in transcript and "mise install" in transcript, transcript
                (args.artifacts / "failure-transcript.json").write_text(transcript)
                print("broken: session running; visible mise install failure notice names project-tools.log", flush=True)
            else:
                print("empty: session running; no tools step", flush=True)
            if recipe != "empty":
                log = run(["podman", "exec", cid, "cat", worker_root + "/project-tools.log"])
                (args.artifacts / f"{recipe}-project-tools.log").write_text(log)
    finally:
        for session in sessions:
            cli("destroy", "--session", session, check=False)
        cli("daemon", "stop", check=False)
        for log in (data / "logs").glob("*.log"):
            shutil.copyfile(log, args.artifacts / log.name)
        if server:
            os.killpg(server.pid, signal.SIGTERM)
            server.wait()
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
