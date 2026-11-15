#!/usr/bin/env python3
"""Resume repairs a failed parent while a real child uses its Podman volume.

Uses the container-upgrade lab's deterministic ACP fixture. Build mj with
--features test-hooks; pass a compatible portable worker and a local image.
All daemon operations use --instance and private configuration/data directories.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sqlite3

import container_upgrades
from container_upgrades import Regression, WAIT


class ResumeRegression(Regression):
    def tool(self, session, action, params):
        script = r'''
import json, socket, sys, time, uuid
request = {"request_id": uuid.uuid4().hex, "created_at_ms": int(time.time() * 1000),
           "action": {"action": sys.argv[2], "params": json.loads(sys.argv[3])}}
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(float(sys.argv[4]))
    connection.connect("/var/lib/hel/workers/" + sys.argv[1] + "/subagents.sock")
    connection.sendall(json.dumps(request).encode() + b"\n")
    reply = json.loads(connection.makefile("rb").readline())
assert not reply["result"]["is_error"], reply
print(reply["result"]["message"])
'''
        return json.loads(self.podman("exec", "-i", self.container(session),
            "/usr/bin/python3", "-", session, action, json.dumps(params), str(WAIT), input=script))

    def kill_parent_worker(self, parent):
        self.podman("exec", self.container_from_database(parent), "/usr/bin/python3", "-c",
            'import os, signal, sys; os.killpg(int(open("/var/lib/hel/workers/" + sys.argv[1] + "/worker.pid").read()), signal.SIGKILL)', parent)

    def parent_profile_fault(self, parent, restore=False):
        script = r"""
import json, pathlib, sys
root = pathlib.Path("/var/lib/hel/workers") / sys.argv[1]
home = pathlib.Path(json.loads((root / "launch.json").read_text())["environment"]["CODEX_HOME"])
config = home / "config.toml"
backup = home / "config.toml.resume-original"
if sys.argv[2] == "restore":
    backup.replace(config)
else:
    config.rename(backup)
    config.write_text("[injected invalid TOML")
"""
        self.podman("exec", self.container_from_database(parent), "/usr/bin/python3", "-c",
                    script, parent, "restore" if restore else "break")

    # Hard-won: 94d7 resume: Resume of a failed session deleted a container its live sub-agents used and restored older work
    def run_regression(self):
        self.prepare_container_fixture()
        bridge = self.runtime_root / "fake_acp.py"
        options = [{"id": "model", "name": "Model", "category": "model", "type": "select",
                    "currentValue": "tiny", "options": [{"value": "tiny", "name": "Tiny fixture"}]},
                   {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
                    "currentValue": "high", "options": [{"value": "high", "name": "High"}]}]
        script = bridge.read_text().replace(
            '            "sessionId": session_id,\n            "modes":',
            '            "sessionId": session_id,\n            "configOptions": ' + repr(options) + ',\n            "modes":')
        script = script.replace('    elif method == "session/set_mode":',
            '    elif method == "session/set_config_option":\n        result = {"configOptions": ' + repr(options) + '}\n    elif method == "session/set_mode":')
        script = script.replace('def wait_for_prompt_cancel():\n',
            'def wait_for_prompt_cancel():\n    if "Keep working during parent repair." in text:\n        open(' + repr(str(self.runtime_root / "child-prompt.txt")) + ', "w").write(text)\n')
        compile(script, str(bridge), "exec")
        bridge.write_text(script)
        config = self.config / "config.toml"
        config.write_text(config.read_text() + '\n[subagents.straightforward]\nmodel = "tiny"\neffort = "high"\n')
        self.cli("workspaces", "create", "Resume shared environment")
        self.authenticate()
        workspace = self.snapshot()["workspaces"][0]["id"]
        # The fixture and release markers must be visible in both worker roots.
        mounts = [{"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"}]
        status, response = self.request("POST", "/api/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "fixture", "target_id": "podman", "title": "resume-parent",
            "additional_mounts": mounts, "subagents": {"mode": "mjolnir"},
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3},
        }, timeout=WAIT)
        assert status == 202, response
        def parent_ready():
            value = self.snapshot()
            return value if any(row.get("title") == "resume-parent" and row.get("state") == "running"
                                for row in value.get("sessions", [])) else None
        snapshot = self.wait(parent_ready, "parent ready")
        parent = next(row["id"] for row in snapshot["sessions"] if row.get("title") == "resume-parent")
        self.sessions.append(parent)
        self.cli("checkpoint", "--session", parent)
        before = self.inspect(parent)
        native = self.worker_status(parent)["state"]["native_session_id"]
        workspace_path = self.record(parent)["container_workspace"]
        self.podman("exec", self.container(parent), "sh", "-c", 'printf newer >"$1/resume-sentinel"', "sh", workspace_path)
        child = self.tool(parent, "spawn", {"task_name": "live-child", "task_type": "straightforward", "instructions": "Keep working during parent repair."})["child_session_id"]
        active = self.wait(lambda: self.working(child), "child active turn")
        for dead in [False, True]:
            # Inject the durable result of failed harness preparation while the
            # daemon is stopped. Workers and the child's turn remain independent.
            self.cli("daemon", "stop")
            if dead:
                self.kill_parent_worker(parent)
            with sqlite3.connect(self.data / "mj.sqlite3") as database:
                database.execute("UPDATE sessions SET state='error',last_error='injected harness preparation failure' WHERE session_id=?", (parent,))
                if dead:
                    # The second repair must work without any recovery archive.
                    database.execute("DELETE FROM session_checkpoints WHERE session_id=?", (parent,))
            self.cli("resume", "--session", parent)
            self.authenticate()
            self.wait(lambda: self.record(parent)["state"] == "running", "parent repaired")
            after = self.inspect(parent)
            assert after["Id"] == before["Id"]
            assert after["Mounts"] == before["Mounts"]
            assert self.worker_status(parent)["state"]["native_session_id"] == native
            assert self.working(child)["command_id"] == active["command_id"]
            assert self.podman("exec", self.container(parent), "cat", workspace_path + "/resume-sentinel") == "newer"
        # A real profile-preparation failure leaves a live relay with no coordinator.
        # Once the fixture is repaired, Resume must replace only that failed boot.
        self.cli("daemon", "stop")
        self.kill_parent_worker(parent)
        self.parent_profile_fault(parent)
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='error' WHERE session_id=?", (parent,))
        self.cli("resume", "--session", parent)
        self.authenticate()
        def failed_boot():
            row = self.session(self.snapshot(), parent)
            return (row and row.get("state") == "error" and not row.get("operation")
                    and (self.worker_status(parent)["state"].get("harness_preparation") or {}).get("state") == "failed")
        self.wait(failed_boot, "terminal harness preparation failure")
        self.parent_profile_fault(parent, restore=True)
        self.cli("resume", "--session", parent)
        self.wait(lambda: self.record(parent)["state"] == "running", "failed live worker repaired")
        assert self.inspect(parent)["Id"] == before["Id"]
        assert self.worker_status(parent)["state"]["native_session_id"] == native
        assert self.working(child)["command_id"] == active["command_id"]
        assert self.podman("exec", self.container(parent), "cat", workspace_path + "/resume-sentinel") == "newer"
        child_prompt = (self.runtime_root / "child-prompt.txt").read_text()
        marker = self.runtime_root / "releases" / hashlib.sha256(child_prompt.encode()).hexdigest()
        marker.touch()
        # A handback reminder can start immediately after this turn completes.
        # Ask the worker about the original command, not a transient idle gap.
        def child_finished():
            current = self.working(child)
            return current is None or current["command_id"] != active["command_id"]
        self.wait(child_finished, "original child turn completion")
        self.worker_status(child)
        print(json.dumps({"result": "passed", "instance": self.instance, "parent": parent, "child": child, "artifacts": str(self.root)}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--image", default="ghcr.io/brokkai/mjolnir/agent-dev:latest")
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named test instance")
    container_upgrades.OLD_IMAGE = args.image
    regression = ResumeRegression(args)
    try:
        regression.run_regression()
    finally:
        regression.cleanup()


if __name__ == "__main__":
    main()
