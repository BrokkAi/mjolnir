#!/usr/bin/env python3
"""Adopt real container workers through the CLI across controller state loss.

Build the dev mj and mj-worker with --features test-hooks. Every daemon, CLI
and worker belongs to this driver's private named instance. The ACP peer is the
recorded reliability harness; turns are held by explicit release files.
Ignored until brokkai/mjolnir#1603 fixes conflicting test-hooks worker stamps.
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

import container_upgrades
from container_upgrades import Regression

from reliability_lab import ScenarioFailure

WAIT = 120


class AdoptionLab(Regression):
    def __init__(self, args):
        super().__init__(args)
        self.container_ids = []

    def command(self, *args, timeout=WAIT, check=True):
        result = subprocess.run([str(self.hel), "--instance", self.instance, *args],
                                env=self.environment(), capture_output=True, text=True, timeout=timeout)
        if check and result.returncode:
            raise ScenarioFailure(f"mj {args}: {result.stderr}")
        return result

    def wait(self, predicate, description):
        deadline = time.monotonic() + WAIT
        while time.monotonic() < deadline:
            try:
                if value := predicate():
                    return value
            except (OSError, ValueError, ScenarioFailure):
                pass
            time.sleep(0.05)
        raise ScenarioFailure(f"timed out: {description}")

    def authenticate(self):
        code, _ = self.wait_daemon_status(0)
        assert self.request("POST", "/auth/session", {"code": code})[0] == 204

    def relay(self, root, method, **params):
        script = r"""
import json, socket, sys, uuid
with socket.socket(socket.AF_UNIX) as peer:
    peer.settimeout(120)
    peer.connect(sys.argv[1] + "/control.sock")
    with peer.makefile("rb") as reader:
        def call(protocol, request):
            peer.sendall(json.dumps({"request_id":uuid.uuid4().hex,"protocol_version":protocol,"request":request}).encode()+b"\n")
            reply=json.loads(reader.readline())
            assert reply["result"]=="ok",reply
            return reply
        hello=call(1,{"method":"hello","params":{"controller_version":"adoption-regression","supported":{"min":1,"max":4294967295}}})
        request = {"method":sys.argv[2]}
        params = json.loads(sys.argv[3])
        if params: request["params"] = params
        print(json.dumps(call(hello["protocol_version"],request)["payload"]))
"""
        return json.loads(self.podman("exec", "-i", self.container_ids[-1], "/usr/bin/python3", "-",
            str(root), method, json.dumps(params), input=script))

    def state(self, root):
        return self.relay(root, "status")["data"]

    def submit(self, root, text):
        command = "adoption-prompt-" + uuid.uuid4().hex
        self.relay(root, "submit", command_id=command, command={"type": "prompt", "data": {
            "prompt": [{"type": "text", "text": text}]}})
        return command

    def release(self, text):
        (self.runtime_root / "releases" / hashlib.sha256(text.encode()).hexdigest()).touch()

    def forget(self, session, *, crash=False):
        if crash:
            metadata = json.loads((self.data / "daemon.json").read_text())
            os.kill(metadata["pid"], signal.SIGKILL)
        else:
            self.command("daemon", "stop")
        # Only this stopped private controller's record is lost. Cascades also
        # remove its projection; the worker and its native files are untouched.
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("PRAGMA foreign_keys=ON")
            database.execute("DELETE FROM sessions WHERE session_id=?", [session])
        self.command("sessions", "--json")
        self.authenticate()

    def start_adoption(self, session, suffix, *flags):
        return self.start_recovery("adopt", session, suffix, *flags)

    def start_recovery(self, action, session, suffix, *flags):
        output = (self.root / f"{suffix}.stdout").open("w")
        error = (self.root / f"{suffix}.stderr").open("w")
        process = subprocess.Popen([str(self.hel), "--instance", self.instance, "recover", action,
            "--session", session, "--target", "podman", *flags], env=self.environment(),
            stdout=output, stderr=error)
        output.close()
        error.close()
        return process

    def genesis_orphan(self, workspace, title):
        hook = "adoption_orphan_before_serving"
        for suffix in ["reached", "continue"]:
            (self.hooks / f"{hook}.{suffix}").unlink(missing_ok=True)
        status, response = self.request("POST", "/api/actions", {"action": "new", "workspace_id": workspace,
            "profile_id": "fake", "bundle_id": "fixture", "target_id": "podman", "title": title,
            "additional_mounts": [{"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"}]}, timeout=WAIT)
        assert status == 202, response
        self.wait(lambda: (self.hooks / f"{hook}.reached").exists(), "worker before first attachment")
        row = next(row for row in self.snapshot()["sessions"] if row.get("title") == title)
        session = row["id"]
        record = self.daemon_request({"action": "session_record", "arguments": {"session_id": session}})["value"]
        self.container_ids.append(record["target"]["container_id"])
        self.sessions.append(session)
        root = Path("/var/lib/hel/workers") / session
        self.forget(session, crash=True)
        (self.hooks / f"{hook}.continue").touch()
        self.wait(lambda: self.state(root).get("acp_ready"), "untracked harness ready")
        return session, root

    def run_scenario(self):
        container_upgrades.OLD_IMAGE = self.args.image
        self.prepare_container_fixture()
        bridge = self.runtime_root / "fake_acp.py"
        script = bridge.read_text().replace('        report_execution("running")\n', '''        report_execution("running")
        if text == "question blocked turn":
            send({"jsonrpc":"2.0", "id":"held-question", "method":"elicitation/create", "params":{
                "sessionId":session_id, "mode":"form", "message":"Which file?", "requestedSchema":{
                    "type":"object", "required":["file"], "properties":{"file":{"type":"string", "title":"File"}}}}})
            answer = json.loads(sys.stdin.readline())
            assert answer.get("id") == "held-question" and "result" in answer, answer
        if text in ("start background terminal", "fail background terminal"):
            report_execution("idle")
            send({"jsonrpc":"2.0", "id":ident, "result":{"stopReason":"end_turn"}})
            params = {"sessionId":session_id, "command":"/bin/sh", "args":["-c", 'while [ ! -f "$1" ]; do /usr/bin/sleep 0.05; done', "adoption-terminal", os.path.join(os.environ["MJ_FAKE_ACP_RELEASE_ROOT"], "terminal-exit")]}
            if text == "fail background terminal": params["cwd"] = "/nonexistent/adoption-terminal-test"
            send({"jsonrpc":"2.0", "id":"background-terminal", "method":"terminal/create", "params":params})
            response = json.loads(sys.stdin.readline())
            assert response.get("id") == "background-terminal", response
            assert ("result" in response) == (text == "start background terminal"), response
            continue
''')
        compile(script, str(bridge), "exec")
        bridge.write_text(script)
        config = self.config / "config.toml"
        terminal_hook = "adoption_terminal_admitted_before_spawn"
        config.write_text(config.read_text().replace("environment = {", f"environment = {{ MJ_TEST_HOOK = {json.dumps(terminal_hook)}, MJ_TEST_HOOK_DIR = {json.dumps(str(self.hooks))}, MJ_CHAOS_ISOLATED = \"1\",", 1))
        self.command("workspaces", "create", "Adoption regression")
        self.authenticate()
        workspace = self.snapshot()["workspaces"][0]["id"]
        status, response = self.request("POST", "/api/actions", {"action": "new", "workspace_id": workspace,
            "profile_id": "fake", "bundle_id": "fixture", "target_id": "podman",
            "title": "adoption-fixture", "additional_mounts": [{"source": str(self.runtime_root),
                "destination": str(self.runtime_root), "access": "rw"}]}, timeout=WAIT)
        assert status == 202, response
        row = self.wait(lambda: next((row for row in self.snapshot()["sessions"]
            if row.get("title") == "adoption-fixture" and row.get("state") == "running"), None), "worker ready")
        session = row["id"]
        record = self.daemon_request({"action": "session_record", "arguments": {"session_id": session}})["value"]
        self.container_ids.append(record["target"]["container_id"])
        self.sessions.append(session)
        root = Path("/var/lib/hel/workers") / session
        first = self.submit(root, "question blocked turn")
        self.wait(lambda: (self.state(root).get("active_prompt") or {}).get("command_id") == first, "first turn")
        def question(value):
            if isinstance(value, dict):
                if value.get("message") == "Which file?" and "id" in value: return value
                values = value.values()
            elif isinstance(value, list): values = value
            else: return None
            return next((found for item in values if (found := question(item))), None)
        pending = self.wait(lambda: question(self.request("GET", f"/api/conversations/{session}", timeout=WAIT)[1]), "live pending question")
        following = self.submit(root, "queued before recovery")
        self.wait(lambda: self.state(root).get("acknowledged_through", 0) > 0
                  or self.state(root).get("retained_through", 0) > 0, "acknowledged history")
        self.forget(session)
        cancelled = self.start_adoption(session, "cancelled")
        self.wait(lambda: "pending question" in (self.root / "cancelled.stderr").read_text(), "blocked-turn cut wait notice")
        cancelled.send_signal(signal.SIGINT)
        assert cancelled.wait(timeout=WAIT) != 0
        assert (self.state(root).get("active_prompt") or {}).get("command_id") == first
        # No further attachment or adoption after Ctrl-C: the worker must release
        # its hold and dispatch the follow-up itself when the old turn finishes.
        self.release("question blocked turn")
        self.relay(root, "respond_elicitation", elicitation_id=pending["id"], response={"action":"accept", "content":{"file":"a.c"}})
        self.wait(lambda: (self.state(root).get("active_prompt") or {}).get("command_id") == following,
                  "follow-up after cancelled cut")
        final = self.submit(root, "after the recovery cut")
        adoption = self.start_adoption(session, "adopted")
        self.wait(lambda: "waiting for" in (self.root / "adopted.stderr").read_text().lower(), "second cut wait")
        self.release("queued before recovery")
        assert adoption.wait(timeout=WAIT) == 0, (self.root / "adopted.stderr").read_text()
        self.wait(lambda: (self.state(root).get("active_prompt") or {}).get("command_id") == final, "post-cut turn")
        self.wait(lambda: "after the recovery cut" in json.dumps(self.request("GET", f"/api/conversations/{session}", timeout=WAIT)[1]), "ordinary replay after the cut")
        status, conversation = self.request("GET", f"/api/conversations/{session}", timeout=WAIT)
        assert status == 200, conversation
        assert "History before recovery unavailable" in json.dumps(conversation)
        assert "after the recovery cut" in json.dumps(conversation)
        self.release("after the recovery cut")
        self.record_action("adopted-at-idle-boundary", session=session)
        self.wait(lambda: not self.state(root).get("active_prompt"), "last turn finished")
        self.submit(root, "start background terminal")
        self.wait(lambda: (self.hooks / f"{terminal_hook}.reached").exists(), "terminal registered before spawn")
        self.wait(lambda: not self.state(root).get("active_prompt"), "terminal requested outside a turn")
        self.forget(session)
        waiting = self.start_adoption(session, "terminal-only")
        self.wait(lambda: "live harness terminal" in (self.root / "terminal-only.stderr").read_text(), "terminal blocker notice")
        notice = (self.root / "terminal-only.stderr").read_text()
        assert "only way forward" in notice
        assert f"mj --instance '{self.instance}' recover destroy --session '{session}' --target 'podman' --confirm '{session}'" in notice
        assert waiting.poll() is None
        (self.hooks / f"{terminal_hook}.continue").touch()
        (self.runtime_root / "releases" / "terminal-exit").touch()
        assert waiting.wait(timeout=WAIT) == 0, (self.root / "terminal-only.stderr").read_text()
        self.wait(lambda: not self.state(root).get("active_agent_terminals"), "terminal exit released adoption")
        self.record_action("cut-waited-for-background-terminal", session=session)
        for suffix in ["reached", "continue"]:
            (self.hooks / f"{terminal_hook}.{suffix}").unlink(missing_ok=True)
        self.submit(root, "fail background terminal")
        self.wait(lambda: (self.hooks / f"{terminal_hook}.reached").exists(), "failed terminal registered before spawn")
        self.wait(lambda: not self.state(root).get("active_prompt"), "failed terminal has no running turn")
        self.forget(session)
        failed_spawn = self.start_adoption(session, "failed-terminal")
        self.wait(lambda: "live harness terminal" in (self.root / "failed-terminal.stderr").read_text(), "failed terminal holds the cut until spawn returns")
        (self.hooks / f"{terminal_hook}.continue").touch()
        assert failed_spawn.wait(timeout=WAIT) == 0, (self.root / "failed-terminal.stderr").read_text()
        assert not self.state(root).get("active_agent_terminals")
        assert not self.state(root).get("tools_in_flight")
        self.record_action("failed-spawn-released-registration", session=session)
        # Once the writer has accepted the install, EOF cannot release either
        # owner before commit/publication and the final reply attempt.
        self.hook_name = "adoption_install_write_accepted"
        self.forget(session)
        submitted = self.start_adoption(session, "accepted-before-disconnect")
        self.wait(lambda: (self.hooks / "adoption_install_write_accepted.reached").exists(), "install write accepted")
        submitted.send_signal(signal.SIGINT)
        assert submitted.wait(timeout=WAIT) != 0
        destroy = self.start_recovery("destroy", session, "destroy-after-write", "--confirm", session)
        self.wait(lambda: "client request" in json.dumps(self.daemon_request({"action":"upgrade_blockers"})), "destroy waiting for install ownership")
        assert destroy.poll() is None
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            assert database.execute("SELECT count(*) FROM sessions WHERE session_id=?", [session]).fetchone()[0] == 0
        (self.hooks / "adoption_install_write_accepted.continue").touch()
        assert destroy.wait(timeout=WAIT) != 0, (self.root / "destroy-after-write.stderr").read_text()
        self.wait(lambda: self.daemon_request({"action":"session_record", "arguments":{"session_id":session}})["value"]["state"] == "running", "accepted install published after disconnect")
        self.record_action("accepted-install-survived-disconnect", session=session)
        self.hook_name = None
        # Pause a second real worker before it can receive its first ACK.
        # Losing the controller here leaves an intact journal. Adoption must
        # replay it ordinarily and leave its later turn running.
        hook = "adoption_orphan_before_serving"
        config = self.config / "config.toml"
        config.write_text(config.read_text().replace(json.dumps(terminal_hook), json.dumps(hook)))
        self.command("daemon", "restart")
        self.authenticate()
        session, root = self.genesis_orphan(workspace, "genesis-orphan")
        active = self.submit(root, "genesis held turn")
        self.wait(lambda: (self.state(root).get("active_prompt") or {}).get("command_id") == active, "untracked turn")
        assert self.state(root)["acknowledged_through"] == 0
        adoption = self.start_adoption(session, "genesis")
        assert adoption.wait(timeout=WAIT) == 0, (self.root / "genesis.stderr").read_text()
        assert (self.state(root).get("active_prompt") or {}).get("command_id") == active
        rejected = self.start_adoption(session, "already-adopted")
        assert rejected.wait(timeout=WAIT) != 0
        assert (self.state(root).get("active_prompt") or {}).get("command_id") == active
        self.wait(lambda: "genesis held turn" in json.dumps(self.request("GET", f"/api/conversations/{session}", timeout=WAIT)[1]), "ordinary replay from genesis")
        status, conversation = self.request("GET", f"/api/conversations/{session}", timeout=WAIT)
        assert status == 200, conversation
        assert "History before recovery unavailable" not in json.dumps(conversation)
        self.release("genesis held turn")
        self.record_action("genesis-replayed-without-interrupting", session=session)
        # The adoption winner owns its worker; a later destructive orphan
        # action cannot treat the same resource as untracked.
        assert self.command("recover", "destroy", "--session", session, "--target", "podman",
                            "--confirm", session, check=False).returncode != 0
        self.hook_name = "adoption_cut_before_install"
        session, root = self.genesis_orphan(workspace, "genesis-destroy-race")
        racing = self.start_adoption(session, "destroy-race")
        self.wait(lambda: (self.hooks / "adoption_cut_before_install.reached").exists(), "adoption before guarded install")
        self.command("recover", "destroy", "--session", session, "--target", "podman", "--confirm", session)
        (self.hooks / "adoption_cut_before_install.continue").touch()
        assert racing.wait(timeout=WAIT) != 0
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            assert database.execute("SELECT count(*) FROM sessions WHERE session_id=?", [session]).fetchone()[0] == 0
        self.record_action("destroy-won-before-genesis-install", session=session)
        self.command("daemon", "stop")

    def cleanup(self):
        try:
            self.command("daemon", "stop", check=False)
        finally:
            for container in self.container_ids:
                self.podman("rm", "--force", container, check=False)
            for session in self.sessions:
                for volume in self.podman("volume", "ls", "--filter", f"label=dev.mj.session={session}", "--format", "{{.Name}}", check=False).split():
                    self.podman("volume", "rm", "--force", volume, check=False)
            self.cleanup_owned()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--image", default="ghcr.io/brokkai/mjolnir/agent-dev:latest")
    parser.add_argument("--instance", default="adoption-quiescent-test")
    parser.add_argument("--run-ignored", action="store_true",
                        help="Run the ignored regression after brokkai/mjolnir#1603 is fixed")
    args = parser.parse_args()
    # Ignored: brokkai/mjolnir#1603 makes real test-hooks workers fail build verification.
    if not args.run_ignored:
        print("SKIP adoption_quiescent: brokkai/mjolnir#1603 (use --run-ignored once fixed)")
        return
    if args.instance == "default":
        parser.error("use a separate named test instance")
    args.old_mj = args.mj
    lab = AdoptionLab(args)
    try:
        lab.run_scenario()
        print(f"PASS {lab.root}")
    finally:
        lab.cleanup()


if __name__ == "__main__":
    main()
