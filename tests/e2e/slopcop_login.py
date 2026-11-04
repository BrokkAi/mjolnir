#!/usr/bin/env python3
"""A container session's `slopcop` signs in with the host's slopcop login.

Real daemon and worker in a named instance, Podman, and the deterministic ACP
fixture. The host must already be signed in to slopcop: the session's wrapper
must then answer `slopcop whoami` with the host's service, account, workspace
and plan. `whoami` only reads the account; no check runs and nothing is billed.

Build mj with --features test-hooks and the portable worker
(scripts/build-linux-worker.sh), then use an image that has slopcop:

    python3 tests/e2e/slopcop_login.py --mj target/debug/mj \
        --worker target/worker/x86_64-unknown-linux-musl/debug/mj-worker \
        --image ghcr.io/brokkai/mjolnir/agent-dev:latest --instance slopcop-login
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys

from container_upgrades import OLD_IMAGE, WAIT, Regression
from reliability_lab import Lab, ScenarioFailure


WHOAMI_FIELDS = ["service", "account", "workspace", "plan"]


def whoami(command):
    """The identity `command whoami` reports, limited to the fields compared."""
    result = subprocess.run([*command, "whoami", "--format", "json"], capture_output=True,
                            text=True, timeout=WAIT)
    if result.returncode:
        raise ScenarioFailure(f"{command[-1]} whoami: {result.stderr.strip()}")
    answer = json.loads(result.stdout)
    return {field: answer.get(field) for field in WHOAMI_FIELDS}


class SlopcopLogin(Regression):
    """The container-upgrade lab's Podman fixture, driven to one slopcop check."""

    def __init__(self, args):
        self.args = args
        self.instance = args.instance
        self.sessions = []
        Lab.__init__(self, args.mj, "slopcop-login", 1397)

    def create_session(self, workspace, mounts, title):
        status, response = self.request("POST", "/api/actions", {"action": "new", "workspace_id": workspace,
            "profile_id": "fake", "bundle_id": "fixture", "target_id": "podman", "title": title,
            "additional_mounts": mounts,
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3}})
        assert status == 202, response
        snapshot = self.wait_snapshot(lambda value: any(row.get("title") == title and row.get("state") == "running"
                                                        for row in value.get("sessions", [])), title + " ready")
        session = next(row["id"] for row in snapshot["sessions"] if row.get("title") == title and row["state"] == "running")
        self.sessions.append(session)
        return session

    def serve_project_over_http(self):
        """Serve the fixture repository as Git's static HTTP protocol, which
        needs no `git daemon` on the host, and make it the project's remote."""
        bare = self.runtime_root / "project.git"
        self.git_output(["clone", "--quiet", "--bare", str(self.project), str(bare)])
        self.git_output(["update-server-info"], bare)
        port = self.free_port()
        server = subprocess.Popen([sys.executable, "-m", "http.server", str(port), "--bind", "0.0.0.0",
                                   "--directory", str(self.runtime_root)],
                                  stdout=(self.root / "http-server.stdout").open("w"),
                                  stderr=(self.root / "http-server.stderr").open("w"), start_new_session=True)
        self.record_process("started", "http-server", server.pid)
        self.git_output(["remote", "set-url", "origin", f"http://host.containers.internal:{port}/project.git"],
                        self.project)

    def run_scenario(self):
        try:
            host = whoami(["slopcop"])
        except (FileNotFoundError, ScenarioFailure) as error:
            raise ScenarioFailure(f"sign in to slopcop on this host first: {error}") from error
        self.prepare_container_fixture()
        config = self.config / "config.toml"
        config.write_text(config.read_text().replace(OLD_IMAGE, self.args.image))
        self.serve_project_over_http()
        self.cli("workspaces", "create", "slopcop login")
        self.authenticate()
        workspace = self.snapshot()["workspaces"][0]["id"]
        mounts = [{"source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw"},
                  {"source": str(self.runtime_root / "build-cache"), "destination": "/build-cache", "access": "rw"}]
        session = self.create_session(workspace, mounts, "slopcop-login")
        container = self.container(session)
        # Run the wrapper as the account that runs the agent's commands.
        owner = self.podman("exec", container, "stat", "-c", "%u", f"/var/lib/hel/workers/{session}").strip()
        login = f"/var/lib/hel/profiles/{session}/slopcop-login"
        self.wait(lambda: subprocess.run(["podman", "exec", "--user", owner, container, "test", "-s", login],
                                         capture_output=True, timeout=WAIT).returncode == 0,
                  "slopcop login sync into the session")
        session_answer = whoami(["podman", "exec", "--user", owner, container,
                                 f"/var/lib/hel/workers/{session}/bin/slopcop"])
        if session_answer != host:
            raise ScenarioFailure(f"session whoami {session_answer} differs from the host's {host}")
        self.record_action("slopcop-login-reached-container-session", session=session,
                           account=host["account"], workspace=host["workspace"])
        self.trace["outcome"] = "passed"
        self.trace["finished_at"] = self.timestamp()
        self.write_trace()
        print(json.dumps({"instance": self.instance, "session": session, "result": "passed",
                          "artifacts": str(self.root)}), flush=True)


def main():
    """Run the scenario in its named instance and always clean it up."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named test instance")
    scenario = SlopcopLogin(args)
    try:
        scenario.run_scenario()
    finally:
        scenario.cleanup()


if __name__ == "__main__":
    main()
