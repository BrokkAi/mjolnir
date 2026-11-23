#!/usr/bin/env python3
"""Real old/new daemons, workers, Podman, Git and a deterministic ACP fixture.

Build mj with --features test-hooks and provide its matching portable worker.
The old mj must be the official 2.51.0 release, with its bundled worker beside it.
Every invocation uses the named instance and this driver's private directories.
Pass --settings-only for the size and mounts case on the current build. The
Podman host must delegate CPU and memory controllers for that case.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import uuid

from reliability_lab import Lab, ScenarioFailure
from test_hook_chaos import wait_hook


WAIT = 120
OLD_IMAGE = "ghcr.io/brokkai/mjolnir/agent-dev:2.51.0"
NEW_IMAGE = "ghcr.io/brokkai/mjolnir/agent-dev:2.52.0"
SOURCE_HOOK = "container_upgrade_source_retained_before_replacement"
READY_HOOK = "container_upgrade_worker_ready_before_retirement"


class Regression(Lab):
    def __init__(self, args):
        self.args = args
        self.instance = args.instance
        self.sessions = []
        super().__init__(args.mj, "container-upgrades", 1326)

    def environment(self):
        env = super().environment()
        env.update(MJ_INSTANCE=self.instance, MJ_WORKER_BINARY=str(self.args.worker.resolve()),
                   GH_CONFIG_DIR=str(self.runtime_root / "empty-gh"))
        for name in ["GH_TOKEN", "GITHUB_TOKEN", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "MJ_SESSION_ID"]:
            env.pop(name, None)
        return env

    def cli(self, *arguments, old=False, check=True):
        binary = self.args.old_mj if old else self.args.mj
        env = self.environment()
        if old:
            env.pop("MJ_WORKER_BINARY", None)
        result = subprocess.run([str(binary.resolve()), "--instance", self.instance, *arguments],
                                env=env, capture_output=True, text=True, timeout=WAIT)
        with (self.root / "cli.jsonl").open("a") as output:
            output.write(json.dumps({"old": old, "arguments": arguments, "status": result.returncode,
                                     "stdout": result.stdout, "stderr": result.stderr}) + "\n")
        if check and result.returncode:
            raise ScenarioFailure(f"mj {arguments}: {result.stderr}")
        return result

    def podman(self, *arguments, input=None, check=True):
        result = subprocess.run(["podman", *arguments], input=input, capture_output=True, text=True, timeout=WAIT)
        if check and result.returncode:
            raise ScenarioFailure(f"podman {arguments[:3]}: {result.stderr}")
        return result.stdout

    def authenticate(self):
        code, _ = self.wait_daemon_status(0)
        assert self.request("POST", "/auth/session", {"code": code})[0] == 204

    def record(self, session):
        reply = self.daemon_request({"action": "session_record", "arguments": {"session_id": session}})
        value = reply["value"]
        return value.get("record", value)

    def container(self, session):
        return self.record(session)["target"]["container_id"]

    def inspect(self, session):
        record = json.loads(self.podman("inspect", self.container(session)))[0]
        # Engine Config.Env can contain credentials; never save it in artifacts.
        return {key: record[key] for key in ["Id", "Name", "Image", "Mounts", "State"]}

    def worker_status(self, session):
        script = r'''
import json, socket, sys, uuid
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(30)
    connection.connect("/var/lib/hel/workers/" + sys.argv[1] + "/control.sock")
    with connection.makefile("rb") as reader:
        request = {"request_id":uuid.uuid4().hex,"protocol_version":1,"request":{"method":"hello","params":{"controller_version":"container-upgrade-regression","supported":{"min":1,"max":4294967295}}}}
        connection.sendall(json.dumps(request).encode()+b"\n")
        hello = json.loads(reader.readline())
        assert hello["result"] == "ok", hello
        request = {"request_id":uuid.uuid4().hex,"protocol_version":hello["protocol_version"],"request":{"method":"status"}}
        connection.sendall(json.dumps(request).encode()+b"\n")
        status = json.loads(reader.readline())
        assert status["result"] == "ok", status
        print(json.dumps({"hello":hello["payload"],"state":status["payload"]["data"]}))
'''
        return json.loads(self.podman("exec", "-i", self.container(session), "/usr/bin/python3", "-", session, input=script))

    def wait(self, predicate, description):
        deadline = time.monotonic() + WAIT
        last = None
        while time.monotonic() < deadline:
            try:
                last = predicate()
                if last:
                    return last
            except (OSError, ValueError, ScenarioFailure):
                pass
            time.sleep(0.1)
        raise ScenarioFailure(f"timed out waiting for {description}: {last}")

    def working(self, session):
        return self.worker_status(session)["state"].get("active_prompt")

    def prompt(self, session, text):
        command = "container-regression-" + uuid.uuid4().hex
        self.daemon_request({"action": "submit_session_command", "arguments": {
            "session_id": session, "command_id": command,
            "command": {"type": "prompt", "data": {"prompt": [{"type": "text", "text": text}]}},
        }}, timeout=WAIT)
        self.wait(lambda: self.working(session), f"accepted prompt {text}")
        return command

    def release(self, session, text):
        marker = self.runtime_root / "releases" / hashlib.sha256(text.encode()).hexdigest()
        marker.touch()
        self.wait(lambda: not self.working(session), f"prompt {text} completion")

    def restart(self, hook=None):
        self.hook_name = hook
        started = time.monotonic()
        self.cli("daemon", "restart")
        self.authenticate()
        elapsed = time.monotonic() - started
        assert elapsed < 30, f"daemon replacement waited on worker work: {elapsed:.1f}s"
        return elapsed

    def crash(self, hook):
        pid = wait_hook(self, hook)
        metadata = json.loads((self.data / "daemon.json").read_text())
        assert metadata["pid"] == pid, "refusing to kill a process outside this instance's daemon"
        os.kill(pid, signal.SIGKILL)
        self.record_process("killed", hook, pid)
        (self.hooks / f"{hook}.continue").touch()
        self.wait(lambda: not Path(f"/proc/{pid}").exists()
                  or Path(f"/proc/{pid}/stat").read_text().split()[2] == "Z", "isolated daemon exit")

    def upgrade(self, session):
        status, selection = self.request("GET", f"/api/v1/sessions/{session}/container-upgrade")
        assert status == 200, selection
        assert selection["available"], selection
        assert selection["current_image"] != selection["target_image"]
        assert "writable layer" in selection["warning"]
        status, response = self.request("POST", f"/api/v1/sessions/{session}/container-upgrade", {
            "selection": selection, "acknowledge_writable_layer_loss": True,
        })
        assert status == 202, response
        self.record_action("upgrade-accepted", session=session, selection=selection)

    def pending(self, session):
        with sqlite3.connect(f"file:{self.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            return database.execute("SELECT phase FROM session_restart_intents WHERE session_id=?", (session,)).fetchone()

    def container_from_database(self, session):
        with sqlite3.connect(f"file:{self.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            target, = database.execute("SELECT resource_id FROM session_targets WHERE session_id=?", (session,)).fetchone()
            return target

    def create_session(self, workspace, mounts, title, allocation=None):
        body = {"action": "new", "workspace_id": workspace, "profile_id": "fake", "bundle_id": "fixture",
                "target_id": "podman", "title": title, "additional_mounts": mounts}
        if allocation is not None:
            body["resource_allocation"] = allocation
        status, response = self.request("POST", "/api/actions", body)
        assert status == 202, response
        def ready():
            return next((row["id"] for row in self.snapshot()["sessions"]
                         if row.get("title") == title and row.get("state") == "running"), None)
        session = self.wait(ready, title + " ready")
        self.sessions.append(session)
        return session

    def legacy_intent(self, session, *, removed=False):
        self.cli("daemon", "stop")
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            if removed:
                checkpoint, = database.execute("SELECT sha256 FROM session_checkpoints WHERE session_id=?", (session,)).fetchone()
                digest = checkpoint
                database.execute("UPDATE sessions SET state='destroying' WHERE session_id=?", (session,))
            else:
                digest = None
            database.execute("""INSERT INTO session_restart_intents
                (session_id,operation_id,phase,updated_at,checkpoint_sha256,checkpoint_started,container_recreation_acknowledged,automatic)
                VALUES (?,?,?,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?,?,1,1)""",
                (session, "legacy-automatic-" + uuid.uuid4().hex,
                 "recreating_container" if removed else "stopping", digest, int(removed)))

    def check_storage(self, session, previous, *, manual):
        current = self.inspect(session)
        key = lambda mount: (mount["Destination"], mount.get("Name"), mount["Source"], mount["RW"])
        assert sorted(map(key, current["Mounts"])) == sorted(map(key, previous["Mounts"]))
        workspace = self.record(session)["container_workspace"]
        script = 'set -eu; test "$(cat "$1/uncovered-cache/marker")" = workspace; test "$(cat /build-cache/marker)" = cache; '
        script += 'test -e /manually-installed-tool' if manual else 'test ! -e /manually-installed-tool'
        self.podman("exec", self.container(session), "sh", "-c", script, "mj-regression", workspace)
        return current

    def prepare_container_fixture(self, image=OLD_IMAGE):
        self.prepare(fake_acp_prompt_delay_ms=0)
        (self.runtime_root / "empty-gh").mkdir()
        (self.runtime_root / "releases").mkdir()
        (self.runtime_root / "build-cache").mkdir()
        # Container paths are the same absolute paths as on the host bind.
        python = self.runtime_root / "bin/python3"
        python.unlink()
        python.write_text('#!/bin/sh\nexec /usr/bin/python3 "$@"\n')
        python.chmod(0o700)
        bridge = self.runtime_root / "fake_acp.py"
        script = bridge.read_text().replace('session_id = "reliability-native"',
            'session_id = "fixture-" + __import__("hashlib").sha256(os.environ.get("CODEX_HOME", "").encode()).hexdigest()[:24]')
        script = script.replace('def report_execution(status, goal_cleared=False):', 'def report_execution(status, goal_cleared=True):')
        script = script.replace('def wait_for_prompt_cancel():\n', '''def wait_for_prompt_cancel():
    release = os.path.join(os.environ["MJ_FAKE_ACP_RELEASE_ROOT"], __import__("hashlib").sha256(text.encode()).hexdigest())
    while not os.path.exists(release):
        ready, _, _ = select.select([sys.stdin], [], [], 0.05)
        if ready:
            request = json.loads(sys.stdin.readline())
            log(request)
            if request.get("method") != "session/cancel": raise RuntimeError(request)
            return True
    os.unlink(release)
    return False

def unused_timer_wait():
''')
        bridge.write_text(script)
        compile(script, str(bridge), "exec")
        port = self.free_port()
        server = subprocess.Popen(["git", "daemon", "--reuseaddr", "--export-all", "--listen=0.0.0.0",
            f"--port={port}", f"--base-path={self.runtime_root}", str(self.project)],
            stdout=(self.root / "git-daemon.stdout").open("w"), stderr=(self.root / "git-daemon.stderr").open("w"), start_new_session=True)
        self.record_process("started", "git-daemon", server.pid)
        origin = f"git://host.containers.internal:{port}/project"
        self.git_output(["remote", "add", "origin", origin], self.project)
        config = self.config / "config.toml"
        body = config.read_text()
        body = body.replace(f'image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"', f'image = "{image}"', 1)
        body = body.replace('[targets.podman]\nkind = "podman"', '[targets.podman]\nkind = "podman"\nworkspace_storage = { kind = "podman-volume" }')
        body = body.replace('environment = { MJ_FAKE_ACP_LOG', f'environment = {{ MJ_FAKE_ACP_RELEASE_ROOT = {json.dumps(str(self.runtime_root / "releases"))}, MJ_FAKE_ACP_LOG')
        config.write_text(body + '\n[github.watch]\nenabled = false\n[machines.local]\nkind = "local"\nbuild_cache = { enabled = false }\n')

    def run_settings_replacement(self):
        self.prepare_container_fixture("ghcr.io/brokkai/mjolnir/agent-dev:latest")
        config = self.config / "config.toml"
        config.write_text(config.read_text().replace("[targets.podman]\n", '[targets.podman]\ncpus = "2.5"\nmemory = "5g"\n'))
        self.cli("workspaces", "create", "Container settings replacement")
        self.authenticate()
        workspace = self.snapshot()["workspaces"][0]["id"]
        fixture = {"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"}
        initial = {"kind": "container", "cpus": 2, "memory_bytes": 4 << 30}
        session = self.create_session(workspace, [fixture], "settings-preservation", initial)
        record = self.record(session)
        original = self.inspect(session)
        path = record["container_workspace"]
        argv = ["sh", "-c", 'set -eu; checkout=$(find "$1" -maxdepth 3 -name .git -print -quit); cd "${checkout%/.git}"; shift; exec sh -ec "$1"',
                "mj-regression", path]
        self.podman("exec", self.container(session), *argv,
                    'printf "\nignored-build\n" >>.gitignore; mkdir ignored-build; printf preserved >ignored-build/output; git check-ignore ignored-build/output')
        mount_source = self.runtime_root / "extra"
        mount_source.mkdir()
        (mount_source / "marker").write_text("mounted")
        mount = {"source": str(mount_source), "destination": "/mnt/extra", "access": "ro"}
        size = {"kind": "container", "cpus": 3, "memory_bytes": 8 << 30}
        move = json.loads(self.cli("move", "--session", session, "--cpus", "3", "--memory-gib", "8", "--dry-run", "--json").stdout)
        assert move["container_upgrade"]["settings"]["resource_allocation"] == size, move
        status, preview = self.request("POST", "/api/actions", {
            "action": "set-container-settings", "session_id": session,
            "resource_allocation": size, "mounts": [fixture, mount]}, timeout=WAIT)
        assert status == 200 and preview["available"], (status, preview)
        assert preview["settings"]["resource_allocation"] == size, preview
        assert "writable layer" in preview["warning"], preview
        assert self.inspect(session)["Id"] == original["Id"]
        upgrade_path = f"/api/v1/sessions/{session}/container-upgrade"
        def confirm(selection):
            status, body = self.request("POST", upgrade_path, {
                "selection": selection, "acknowledge_writable_layer_loss": True}, timeout=WAIT)
            assert status == 202, body
        self.prompt(session, "hold during settings preview retry")
        confirm(preview)
        status, retained = self.request("GET", upgrade_path)
        assert status == 200 and retained == preview, retained
        confirm(retained)
        self.release(session, "hold during settings preview retry")
        self.wait(lambda: not self.pending(session) and self.inspect(session)["Id"] != original["Id"],
                  "settings replacement finished")
        replacement = self.inspect(session)
        assert replacement["Id"] != original["Id"]
        assert replacement["Image"] == original["Image"]
        key = lambda mount: (mount["Destination"], mount.get("Name"), mount["Source"], mount["RW"])
        assert set(map(key, original["Mounts"])) < set(map(key, replacement["Mounts"]))
        assert self.record(session)["target_runtime"]["container_settings"]["resource_allocation"] == size
        self.podman("exec", self.container(session), *argv,
                    'git check-ignore ignored-build/output; test "$(cat ignored-build/output)" = preserved')
        limits = self.podman("exec", self.container(session), "sh", "-ec",
                             'cat /sys/fs/cgroup/memory.max /sys/fs/cgroup/cpu.max; cat /mnt/extra/marker')
        memory, cpu, marker = limits.splitlines()
        quota, period = map(int, cpu.split())
        assert (int(memory), quota, marker) == (8 << 30, 3 * period, "mounted"), limits
        status, clear = self.request("POST", "/api/actions", {
            "action": "set-container-settings", "session_id": session,
            "resource_allocation": None, "mounts": [fixture, mount]}, timeout=WAIT)
        assert status == 200 and clear["available"], (status, clear)
        confirm(clear)
        self.wait(lambda: not self.pending(session) and self.inspect(session)["Id"] != replacement["Id"],
                  "template limits restored")
        limits = self.podman("exec", self.container(session), "cat",
                             "/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/cpu.max")
        memory, cpu = limits.splitlines()
        quota, period = map(int, cpu.split())
        assert int(memory) == 5 << 30 and quota * 2 == 5 * period, limits
        with sqlite3.connect(f"file:{self.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            assert not database.execute("SELECT 1 FROM session_moves WHERE session_id=?", (session,)).fetchone()
        self.record_action("container-settings-replacement-preserved-storage", session=session)
        self.trace["outcome"] = "passed"
        self.trace["finished_at"] = self.timestamp()
        self.write_trace()
        print(json.dumps({"instance": self.instance, "result": "passed", "artifacts": str(self.root)}), flush=True)

    def run_regression(self):
        self.prepare_container_fixture()
        self.cli("workspaces", "create", "Container upgrade regression", old=True)
        mounts = [{"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"},
                  {"source": str(self.runtime_root / "build-cache"), "destination": "/build-cache", "access": "rw"}]
        # Daemon protocol registration exposes attached directories on both releases.
        self.authenticate()
        workspace = self.snapshot()["workspaces"][0]["id"]
        session = self.create_session(workspace, mounts, "container-preservation")
        record = self.record(session)
        assert record.get("build_cache") is None
        assert not (record.get("target_runtime") or {}).get("tool_cache")
        original = self.inspect(session)
        workspace_path = record["container_workspace"]
        self.podman("exec", self.container(session), "sh", "-c",
            'set -eu; mkdir -p "$1/uncovered-cache"; printf workspace >"$1/uncovered-cache/marker"; printf cache >/build-cache/marker; printf manual >/manually-installed-tool', "mj-regression", workspace_path)
        first = self.prompt(session, "hold across daemon upgrade and cancellation")
        old_status = self.worker_status(session)
        assert "2.51.0" in json.dumps(old_status["hello"]), old_status["hello"]
        # A normal new-client request exercises automatic daemon replacement.
        self.cli("sessions", "--json")
        self.authenticate()
        assert "2.52.0" in self.cli("daemon", "status").stdout
        assert self.working(session)["command_id"] == first
        assert self.inspect(session)["Id"] == original["Id"]
        self.check_storage(session, original, manual=True)
        assert "2.51.0" in json.dumps(self.worker_status(session)["hello"])
        self.record_action("compatible-old-worker-reconnected", session=session)
        # Old automatic scheduling accepted this while the worker was busy.
        self.legacy_intent(session)
        self.cli("sessions", "--json")
        self.authenticate()
        self.wait(lambda: not self.pending(session), "legacy automatic request cancellation")
        assert self.working(session)["command_id"] == first
        assert self.inspect(session)["Id"] == original["Id"]
        config = self.config / "config.toml"
        config.write_text(config.read_text().replace(OLD_IMAGE, NEW_IMAGE))
        self.wait(lambda: self.request("GET", f"/api/v1/sessions/{session}/container-upgrade")[1].get("target_image") == NEW_IMAGE, "configuration refresh")
        newer = self.create_session(workspace, mounts, "new-default-image")
        assert self.inspect(newer)["Image"] != original["Image"]
        assert self.inspect(session)["Id"] == original["Id"]
        self.cli("upgrade-container", "--session", session)
        assert not self.pending(session), "CLI preview accepted an upgrade without acknowledgment"
        self.cli("upgrade-container", "--session", session, "--acknowledge-writable-layer-loss")
        self.wait(lambda: self.pending(session), "CLI durable upgrade acceptance")
        self.cli("upgrade-container", "--session", session, "--cancel")
        self.wait(lambda: not self.pending(session), "CLI upgrade cancellation")
        assert self.record(session).get("last_error") is None, "cancelling a pending upgrade was reported as a session failure"
        assert self.working(session)["command_id"] == first
        self.upgrade(session)
        self.wait(lambda: self.pending(session), "durable pending upgrade")
        elapsed = self.restart()
        assert self.pending(session)
        assert self.working(session)["command_id"] == first
        assert self.inspect(session)["Id"] == original["Id"]
        assert self.request("DELETE", f"/api/v1/sessions/{session}/container-upgrade")[0] == 202
        self.wait(lambda: not self.pending(session), "upgrade cancellation persistence")
        assert self.record(session).get("last_error") is None, "cancelling a pending upgrade was reported as a session failure"
        assert self.working(session)["command_id"] == first
        self.check_storage(session, original, manual=True)
        self.release(session, "hold across daemon upgrade and cancellation")
        self.daemon_request({"action": "restart_session", "arguments": {"session_id": session, "acknowledge_container_recreation": True}}, timeout=WAIT)
        assert self.inspect(session)["Id"] == original["Id"]
        self.check_storage(session, original, manual=True)
        self.restart(SOURCE_HOOK)
        self.prompt(session, "finish before explicit replacement")
        self.upgrade(session)
        assert self.inspect(session)["Id"] == original["Id"]
        self.release(session, "finish before explicit replacement")
        self.crash(SOURCE_HOOK)
        self.restart(READY_HOOK)
        wait_hook(self, READY_HOOK)
        latest = self.prompt(session, "new work after replacement worker startup")
        self.crash(READY_HOOK)
        self.restart()
        self.wait(lambda: not self.pending(session), "interrupted upgrade recovery")
        assert self.working(session)["command_id"] == latest, "recovery replayed old state over accepted work"
        replacement = self.check_storage(session, original, manual=False)
        assert replacement["Id"] != original["Id"]
        self.release(session, "new work after replacement worker startup")
        # Simulate the old destructive path after container removal but before
        # volume deletion. Surviving files outside checkpoint coverage win.
        self.legacy_intent(session, removed=True)
        self.podman("container", "rm", "--force", self.container_from_database(session))
        self.cli("sessions", "--json")
        self.authenticate()
        self.wait(lambda: not self.pending(session), "started legacy recreation recovery")
        self.check_storage(session, original, manual=False)
        self.record_action("legacy-checkpoint-recovered-with-surviving-storage", session=session)
        self.record_action("storage-and-accepted-work-preserved", source=original["Id"], replacement=replacement["Id"], handoff_seconds=elapsed)
        self.trace["outcome"] = "passed"
        self.trace["finished_at"] = self.timestamp()
        self.write_trace()
        print(json.dumps({"instance": self.instance, "session": session, "result": "passed", "artifacts": str(self.root)}), flush=True)

    def cleanup(self):
        try:
            self.cli("daemon", "stop", check=False)
        finally:
            # Only sessions created in this driver's isolated store may be removed.
            database = self.data / "mj.sqlite3"
            if database.exists():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
                    owned = [row[0] for row in connection.execute("SELECT session_id FROM sessions")]
                for session in owned:
                    assert session.replace("-", "").isalnum()
                    for kind in ["container", "volume"]:
                        names = self.podman(kind, "ls", "--filter", f"label=dev.mj.session={session}", "--format", "{{.ID}}" if kind == "container" else "{{.Name}}", check=False).split()
                        for name in names:
                            self.podman(kind, "rm", "--force", name, check=False)
            self.cleanup_owned()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--old-mj", type=Path)
    parser.add_argument("--settings-only", action="store_true", help="run the settings replacement case on the current build")
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named test instance")
    if not args.settings_only and args.old_mj is None:
        parser.error("--old-mj is required for the old-to-new regression")
    regression = Regression(args)
    try:
        if args.settings_only:
            regression.run_settings_replacement()
        else:
            regression.run_regression()
    finally:
        regression.cleanup()


if __name__ == "__main__":
    main()
