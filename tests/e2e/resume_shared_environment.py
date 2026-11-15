#!/usr/bin/env python3
"""Resume repairs failed parents while real children use their retained environments.

Uses the container-upgrade lab's deterministic ACP fixture. Build mj with
--features test-hooks; pass a compatible portable worker and a local image.
All daemon operations use --instance and private configuration/data directories.
"""
import argparse
import hashlib
import json
import os
import shutil
import signal
from pathlib import Path
import sqlite3
import subprocess

import container_upgrades
from container_upgrades import Regression, WAIT
from reliability_lab import ScenarioFailure, process_is_running


class ResumeRegression(Regression):
    def worker_python(self, session, script, *arguments):
        target = self.record(session).get("target")
        if not target:
            raise ScenarioFailure("worker has no recorded target yet")
        if target["kind"] == "local-bare":
            result = subprocess.run(["/usr/bin/python3", "-", target["worker_root"], *arguments],
                                    input=script, capture_output=True, text=True,
                                    env=self.environment(), timeout=WAIT)
            if result.returncode:
                raise ScenarioFailure(result.stderr)
            return result.stdout
        if target["kind"] != "local-podman":
            raise ScenarioFailure("fixture supports local bare and Podman workers only")
        return self.podman("exec", "-i", target["container_id"], "/usr/bin/python3", "-",
                           "/var/lib/hel/workers/" + session, *arguments, input=script)

    def lose_bare_parent_root(self, parent, root, child_pid):
        # Stop exactly the private parent's worker/supervisor/harness groups
        # before deleting its runtime. The child's group is independent.
        groups = set()
        selected = []
        for pid, description in self.owned_processes():
            try:
                environment = set(Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"))
                command = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                ours = f"MJ_SESSION_ID={parent}".encode() in environment
                ours |= (b"--root" in command and str(root).encode() in command)
                if ours:
                    selected.append(pid)
                    groups.add(os.getpgid(pid))
            except (FileNotFoundError, ProcessLookupError):
                continue
        assert groups and os.getpgrp() not in groups
        assert os.getpgid(int(child_pid)) not in groups
        for group in groups:
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.wait(lambda: not any(process_is_running(pid) for pid in selected), "bare parent process groups stopped")
        shutil.rmtree(root)

    def wait_recovery_notice(self, parent, copy_time):
        def visible():
            status, conversation = self.request("GET", f"/api/conversations/{parent}", timeout=WAIT)
            text = json.dumps(conversation)
            return (status == 200 and copy_time in text and "files are current" in text)
        self.wait(visible, "timestamped native-history recovery notice")

    def worker_status(self, session):
        script = r'''
import json, socket, sys, uuid
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(float(sys.argv[2]))
    connection.connect(sys.argv[1] + "/control.sock")
    with connection.makefile("rb") as reader:
        request = {"request_id":uuid.uuid4().hex,"protocol_version":1,"request":{"method":"hello","params":{"controller_version":"resume-regression","supported":{"min":1,"max":4294967295}}}}
        connection.sendall(json.dumps(request).encode()+b"\n")
        hello = json.loads(reader.readline())
        assert hello["result"] == "ok", hello
        request = {"request_id":uuid.uuid4().hex,"protocol_version":hello["protocol_version"],"request":{"method":"status"}}
        connection.sendall(json.dumps(request).encode()+b"\n")
        status = json.loads(reader.readline())
        assert status["result"] == "ok", status
        print(json.dumps({"hello":hello["payload"],"state":status["payload"]["data"]}))
'''
        return json.loads(self.worker_python(session, script, str(WAIT)))

    def wait_child_started(self, child):
        def settled():
            if self.record(child)["state"] != "running":
                return False
            with sqlite3.connect(f"file:{self.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                return database.execute("SELECT 1 FROM worker_restart_intents WHERE session_id=?", (child,)).fetchone() is None
        self.wait(settled, "child startup durably completed")

    def finish_child_turn(self, child, active, prompt_file):
        text = prompt_file.read_text()
        marker = self.runtime_root / "releases" / hashlib.sha256(text.encode()).hexdigest()
        marker.touch()
        def completed():
            current = self.working(child)
            return current is None or current["command_id"] != active["command_id"]
        self.wait(completed, "original child turn completion")
        self.worker_status(child)

    # Hard-won: 07d2 readiness failure: Resume must reattach a live bare worker too.
    def bare_regression(self, workspace):
        status, response = self.request("POST", "/api/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "bare-fixture", "target_id": "localhost", "title": "resume-bare",
            "project_directory": str(self.runtime_root / "bare-project"),
            "create_managed_worktree": True, "subagents": {"mode": "mjolnir"},
        }, timeout=WAIT)
        assert status == 202, response
        def ready():
            value = self.snapshot()
            return next((row["id"] for row in value.get("sessions", [])
                         if row.get("title") == "resume-bare" and row.get("state") == "running"), None)
        parent = self.wait(ready, "managed bare worker ready")
        self.sessions.append(parent)
        record = self.record(parent)
        checkout = Path(record["managed_worktree"]["worktree_root"])
        root = Path(record["target"]["worker_root"])
        native_home = Path(json.loads((root / "launch.json").read_text())["environment"]["CODEX_HOME"])
        self.cli("checkpoint", "--session", parent)
        (checkout / "resume-worktree-sentinel").write_text("newer work")
        (native_home / "resume-native-sentinel").write_text("newer native state")
        pid = (root / "worker.pid").read_text()
        native = self.worker_status(parent)["state"]["native_session_id"]
        child = self.tool(parent, "spawn", {"task_name": "bare-child", "task_type": "straightforward",
                          "instructions": "Keep working during bare parent repair."})["child_session_id"]
        active = self.wait(lambda: self.working(child), "bare child active turn")
        self.wait_child_started(child)
        child_root = Path(self.record(child)["target"]["worker_root"])
        child_pid = (child_root / "worker.pid").read_text()
        self.cli("daemon", "stop")
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='error',last_error='injected readiness timer failure after handoff' WHERE session_id=?", (parent,))
        self.cli("resume", "--session", parent)
        self.authenticate()
        self.wait(lambda: self.record(parent)["state"] == "running", "bare worker reattached")
        assert self.record(parent)["target"]["worker_root"] == str(root)
        assert (root / "worker.pid").read_text() == pid
        assert (child_root / "worker.pid").read_text() == child_pid
        assert self.worker_status(parent)["state"]["native_session_id"] == native
        assert (checkout / "resume-worktree-sentinel").read_text() == "newer work"
        assert (native_home / "resume-native-sentinel").read_text() == "newer native state"
        assert self.working(child)["command_id"] == active["command_id"]
        # The checkout and child remain current when only the parent runtime
        # and native home disappear. Restore parent history, never the worktree.
        copy_time = self.record(parent)["checkpoint"]["created_at"]
        self.cli("daemon", "stop")
        assert not child_root.is_relative_to(root)
        self.lose_bare_parent_root(parent, root, child_pid)
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='error',last_error='injected parent runtime loss' WHERE session_id=?", (parent,))
        self.cli("resume", "--session", parent)
        self.authenticate()
        self.wait(lambda: self.record(parent)["state"] == "running", "missing bare parent data recovered")
        assert (checkout / "resume-worktree-sentinel").read_text() == "newer work"
        assert (child_root / "worker.pid").read_text() == child_pid
        assert self.worker_status(parent)["state"]["native_session_id"] == native
        assert self.working(child)["command_id"] == active["command_id"]
        self.wait_recovery_notice(parent, copy_time)
        self.finish_child_turn(child, active, self.runtime_root / "bare-child-prompt.txt")
        return parent, child

    # Hard-won: 337bc5e review: a live worker hid missing bundle checkouts.
    def live_missing_checkouts_regression(self, workspace):
        status, response = self.request("POST", "/api/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "multiple", "target_id": "podman", "title": "resume-multiple",
            "additional_mounts": [{"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"}],
            "subagents": {"mode": "mjolnir"},
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3},
        }, timeout=WAIT)
        assert status == 202, response
        parent = self.wait(lambda: next((row["id"] for row in self.snapshot().get("sessions", [])
                            if row.get("title") == "resume-multiple" and row.get("state") == "running"), None), "multi-repository parent ready")
        self.sessions.append(parent)
        container = self.container(parent)
        root = self.record(parent)["container_workspace"]
        self.podman("exec", container, "sh", "-c", 'printf archived-second >"$1/second/README.md"', "fixture", root)
        self.cli("checkpoint", "--session", parent)
        self.podman("exec", container, "sh", "-c", 'printf current-third >"$1/current/README.md"', "fixture", root)
        text = "hold parent work while its checkout is reconstructed"
        command = self.prompt(parent, text)
        pid_script = 'import pathlib,sys; print((pathlib.Path(sys.argv[1]) / "worker.pid").read_text())'
        parent_pid = self.worker_python(parent, pid_script)
        child = self.tool(parent, "spawn", {"task_name": "checkout-child", "task_type": "straightforward",
                          "instructions": "Keep working during parent repair."})["child_session_id"]
        active = self.wait(lambda: self.working(child), "checkout child busy")
        self.wait_child_started(child)
        child_pid = self.worker_python(child, pid_script)
        self.cli("daemon", "stop")
        self.podman("exec", container, "/usr/bin/python3", "-c", "import shutil,sys; [shutil.rmtree(sys.argv[1]+'/'+name) for name in ['fixture','second']]", root)
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='error' WHERE session_id=?", (parent,))
        self.cli("resume", "--session", parent)
        self.authenticate()
        self.wait(lambda: self.podman("exec", container, "test", "-d", root + "/second/.git") == "", "both checkouts republished")
        assert self.worker_python(parent, pid_script) == parent_pid
        assert self.working(parent)["command_id"] == command
        assert self.worker_python(child, pid_script) == child_pid
        assert self.working(child)["command_id"] == active["command_id"]
        self.release(parent, text)
        self.wait(lambda: self.record(parent)["state"] == "running" and self.worker_python(parent, pid_script) != parent_pid, "idle parent restarted after checkout recovery")
        assert self.podman("exec", container, "cat", root + "/second/README.md") == "archived-second"
        assert self.podman("exec", container, "cat", root + "/current/README.md") == "current-third"
        assert self.worker_python(child, pid_script) == child_pid
        self.finish_child_turn(child, active, self.runtime_root / "child-prompt.txt")
        return parent

    def retained_bind_regression(self, workspace):
        directory = self.runtime_root / "bound-checkout"
        directory.mkdir()
        status, response = self.request("POST", "/api/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "fixture", "target_id": "podman-bind", "title": "resume-bound",
            "additional_mounts": [
                {"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"},
                {"source": str(directory), "destination": "/workspace", "access": "rw"}],
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3},
        }, timeout=WAIT)
        assert status == 202, response
        def ready():
            return next((row["id"] for row in self.snapshot().get("sessions", [])
                         if row.get("title") == "resume-bound" and row.get("state") == "running"), None)
        parent = self.wait(ready, "bound checkout parent ready")
        self.sessions.append(parent)
        self.cli("checkpoint", "--session", parent)
        copy_time = self.record(parent)["checkpoint"]["created_at"]
        checkout = directory / Path(self.record(parent)["container_workspace"]).relative_to("/workspace") / "fixture"
        readme = checkout / "README.md"
        assert readme.is_file()
        readme.write_text("newer tracked work after checkpoint")
        index = self.git_output(["ls-files", "--stage"], checkout)
        container = self.container(parent)
        self.cli("daemon", "stop")
        self.podman("container", "rm", "--force", container)
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='error' WHERE session_id=?", (parent,))
        self.cli("resume", "--session", parent)
        self.authenticate()
        self.wait(lambda: self.record(parent)["state"] == "running", "container recovered around bound checkout")
        assert readme.read_text() == "newer tracked work after checkpoint"
        assert self.git_output(["ls-files", "--stage"], checkout) == index
        self.wait_recovery_notice(parent, copy_time)
        return parent

    def tool(self, session, action, params):
        script = r'''
import json, socket, sys, time, uuid
request = {"request_id": uuid.uuid4().hex, "created_at_ms": int(time.time() * 1000),
           "action": {"action": sys.argv[2], "params": json.loads(sys.argv[3])}}
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(float(sys.argv[4]))
    connection.connect(sys.argv[1] + "/subagents.sock")
    connection.sendall(json.dumps(request).encode() + b"\n")
    reply = json.loads(connection.makefile("rb").readline())
assert not reply["result"]["is_error"], reply
print(reply["result"]["message"])
'''
        return json.loads(self.worker_python(session, script, action, json.dumps(params), str(WAIT)))

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
            'def wait_for_prompt_cancel():\n    if "Keep working during" in text and "parent repair." in text:\n        name = "bare-child-prompt.txt" if "bare parent repair." in text else "child-prompt.txt"\n        open(' + repr(str(self.runtime_root)) + ' + "/" + name, "w").write(text)\n')
        compile(script, str(bridge), "exec")
        bridge.write_text(script)
        bare_project = self.runtime_root / "bare-project"
        self.git_output(["clone", "--no-hardlinks", str(self.project), str(bare_project)], self.runtime_root)
        origin = self.git_output(["remote", "get-url", "origin"], self.project).strip()
        self.git_output(["remote", "set-url", "origin", origin.replace("host.containers.internal", "127.0.0.1")], bare_project)
        config = self.config / "config.toml"
        config.write_text(config.read_text() + '\n[bundles.bare-fixture]\nprimary_repo = "fixture"\n[[bundles.bare-fixture.repositories]]\nid = "fixture"\nlocal = ' + json.dumps(str(bare_project)) + '\ndestination = "fixture"\n')
        config = self.config / "config.toml"
        multiple = '\n[bundles.multiple]\nprimary_repo = "fixture"\n'
        for name in ["fixture", "second", "current"]:
            multiple += '\n[[bundles.multiple.repositories]]\nid = ' + json.dumps(name) + '\nlocal = ' + json.dumps(str(self.project)) + '\ndestination = ' + json.dumps(name) + '\n'
        config.write_text(config.read_text() + multiple)
        config.write_text(config.read_text() + '\n[subagents.straightforward]\nmodel = "tiny"\neffort = "high"\n')
        config.write_text(config.read_text() + '\n[targets.podman-bind]\nkind = "podman"\nworkspace_storage = { kind = "container-layer" }\nimage = ' + json.dumps(self.args.image) + '\npull_policy = "never"\n')
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
        self.wait_child_started(child)
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
        self.finish_child_turn(child, active, self.runtime_root / "child-prompt.txt")
        # A missing container does not authorize replacing a surviving volume.
        self.cli("checkpoint", "--session", parent)
        volume = self.record(parent)["target"]["workspace_storage"]["name"]
        copy_time = self.record(parent)["checkpoint"]["created_at"]
        old_container = self.inspect(parent)["Id"]
        self.prompt(parent, "work lost with removed fixture container")
        discarded = self.prompt(parent, "queued prompt explicitly discarded during replacement")
        self.wait(lambda: any(item["command_id"] == discarded for item in self.worker_status(parent)["state"]["queued_prompts"]), "prompt queued before container loss")
        container_name = self.container(parent)
        self.cli("daemon", "stop")
        self.podman("container", "rm", "--force", container_name)
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='error' WHERE session_id=?", (parent,))
        config = self.config / "config.toml"
        good_config = config.read_text()
        config.write_text(good_config.replace(json.dumps(self.args.image), '"localhost/mj-resume-missing-image:review-regression"'))
        self.cli("resume", "--session", parent)
        self.authenticate()
        self.wait(lambda: self.record(parent)["state"] == "error" and "image" in (self.record(parent).get("last_error") or "").lower(), "failed container recreation remains retryable")
        self.cli("daemon", "stop")
        config.write_text(good_config)
        self.cli("workspaces", "list")
        self.authenticate()
        status, response = self.request("POST", "/api/actions", {
            "action": "resume", "session_id": parent, "workspace_id": workspace,
            "profile_id": "fake", "target_id": "podman", "queue": "discard",
        }, timeout=WAIT)
        assert status == 202, response
        self.wait(lambda: self.record(parent)["state"] == "running", "container recreated on retained volume")
        assert not self.worker_status(parent)["state"]["queued_prompts"]
        assert not self.working(parent), "discarded prompts must not start after replacement"
        assert self.inspect(parent)["Id"] != old_container
        assert self.record(parent)["target"]["workspace_storage"]["name"] == volume
        assert self.podman("exec", self.container(parent), "cat", workspace_path + "/resume-sentinel") == "newer"
        self.wait_recovery_notice(parent, copy_time)
        # Proven absence of all components and no archive stops offering Resume.
        container_name = self.container(parent)
        self.cli("daemon", "stop")
        self.podman("container", "rm", "--force", container_name)
        self.podman("volume", "rm", volume)
        with sqlite3.connect(self.data / "mj.sqlite3") as database:
            database.execute("DELETE FROM session_checkpoints WHERE session_id=?", (parent,))
            database.execute("UPDATE sessions SET state='error' WHERE session_id=?", (parent,))
        self.cli("resume", "--session", parent, check=False)
        self.authenticate()
        self.wait(lambda: self.record(parent)["state"] == "lost", "absent target published")
        assert self.record(parent).get("target") is None
        row = self.session(self.snapshot(), parent)
        assert not row["capabilities"]["resume"] and row["capabilities"]["destroy"]
        refusal = self.cli("resume", "--session", parent, check=False)
        assert refusal.returncode != 0 and "destroy" in refusal.stderr.lower(), refusal.stderr
        multiple_parent = self.live_missing_checkouts_regression(workspace)
        bound_parent = self.retained_bind_regression(workspace)
        bare_parent, bare_child = self.bare_regression(workspace)
        print(json.dumps({"result": "passed", "instance": self.instance, "parent": parent, "child": child,
                          "bare_parent": bare_parent, "bare_child": bare_child, "bound_parent": bound_parent, "multiple_parent": multiple_parent, "artifacts": str(self.root)}), flush=True)


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
    except Exception:
        # Capture only platform facts, never engine environment/credentials.
        diagnostics = []
        for session in regression.sessions:
            try:
                container = regression.container_from_database(session)
                probe = subprocess.run(["podman", "exec", container, "/bin/sh", "-c",
                    "command -v uname; uname -sm; /usr/bin/uname -sm; printf 'PATH=%s\\n' \"$PATH\""],
                    capture_output=True, text=True, timeout=WAIT)
                diagnostics.append({"session": session, "status": probe.returncode,
                    "stdout": probe.stdout[:4000], "stderr": probe.stderr[:4000]})
            except Exception as error:
                diagnostics.append({"session": session, "error": str(error)})
        (regression.root / "failure-platform.json").write_text(json.dumps(diagnostics, indent=2))
        raise
    finally:
        regression.cleanup()


if __name__ == "__main__":
    main()
