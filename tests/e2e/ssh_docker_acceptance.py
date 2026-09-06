#!/usr/bin/env python3
"""Opt-in acceptance driver for the disposable SSH Docker target.

The EC2 host is deliberately owned by ``ssh_docker_lab.py``.  This program
only consumes its private ledger, exercises Mjolnir, and leaves cloud cleanup
to the caller.  It uses the same daemon IPC framing as the reliability lab so
session creation can include additional mounts, which the web action does not
currently expose.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time
import tomllib
from typing import Any


SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from reliability_lab import Lab, ScenarioFailure  # noqa: E402
from ssh_docker_lab import (  # noqa: E402
    CommandError,
    LabError,
    load_json,
    run_command,
    ssh,
)


DEFAULT_ARTIFACT_DIR = pathlib.Path("target/ssh-docker-e2e/acceptance")
DEFAULT_IMAGE = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
DEFAULT_TIMEOUT = 300.0
CONTAINER_WORKSPACE = "/workspace"


class AcceptanceFailure(RuntimeError):
    """A bounded acceptance failure with an artifact directory to inspect."""


def private_dir(path: pathlib.Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)


def private_write(path: pathlib.Path, text: str) -> None:
    private_dir(path.parent)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(text, encoding="utf-8")
    temporary.chmod(0o600)
    temporary.replace(path)


def record_json(path: pathlib.Path, value: Any) -> None:
    private_write(path, json.dumps(value, indent=2, sort_keys=True) + "\n")


def result_text(path: pathlib.Path, result: subprocess.CompletedProcess[str]) -> None:
    private_write(path, result.stdout + ("\n" + result.stderr if result.stderr else ""))


def parse_profile(source: pathlib.Path, profile: str) -> tuple[str, pathlib.Path]:
    """Read only the non-secret profile identity and home path."""

    try:
        document = tomllib.loads(source.read_text(encoding="utf-8"))
        section = document["profiles"][profile]
        kind = section["kind"]
        home = pathlib.Path(section["home"]).expanduser()
    except (KeyError, TypeError, tomllib.TOMLDecodeError) as error:
        raise AcceptanceFailure(f"config has no usable profiles.{profile} section") from error
    if not isinstance(kind, str) or not isinstance(section.get("home"), str):
        raise AcceptanceFailure(f"profiles.{profile} has invalid kind/home fields")
    if not home.is_dir():
        raise AcceptanceFailure(f"profile {profile} home is not a directory")
    return kind, home


def codex3_config_lines(source: pathlib.Path) -> list[str]:
    """Write the real profile home into isolated controller configuration."""

    kind, home = parse_profile(source, "codex3")
    if kind != "codex":
        raise AcceptanceFailure(f"profiles.codex3 is not a Codex profile ({kind!r})")
    return ["[profiles.codex3]", 'kind = "codex"', f'home = {json.dumps(str(home))}']


def resource_name(session_id: str) -> str:
    readable = "".join(character.lower() for character in session_id if character.isascii() and character.isalnum())[:12]
    digest = hashlib.sha256(session_id.encode()).digest()
    return f"mj-{readable}-{digest[0]:02x}{digest[1]:02x}{digest[2]:02x}"


def shell_remote(argv: list[str]) -> str:
    return shlex.join(argv)


def scp_arguments(ledger: dict[str, Any], address: str, source: pathlib.Path, destination: str) -> list[str]:
    connection = ledger["connection"]
    return [
        "scp",
        "-4",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        f"UserKnownHostsFile={connection['known_hosts']}",
        "-i",
        str(connection["private_key"]),
        str(source),
        f"{connection['user']}@{address}:{destination}",
    ]


class AcceptanceLab:
    """Small wrapper around reliability_lab.Lab with a caller-selected root."""

    def __init__(self, binary: pathlib.Path, artifact: pathlib.Path, worker: pathlib.Path, seed: int):
        private_dir(artifact)
        self.lab = Lab(binary.resolve(), "ssh-docker-acceptance", seed)
        binary = binary.resolve()
        worker = worker.resolve()
        generated_root = self.lab.root
        if generated_root != artifact:
            with contextlib.suppress(OSError):
                shutil.rmtree(generated_root)
        self.lab.root = artifact
        self.lab.trace_path = artifact / "trace.json"
        for directory in (self.lab.config, self.lab.data, self.lab.profile, self.lab.project, self.lab.hooks):
            private_dir(directory)
        self.lab.trace["artifacts"] = {"trace": "trace.json"}
        self.lab.trace["runtime"] = {
            "config": str(self.lab.config),
            "data": str(self.lab.data),
            "profile_home": "source config profile home",
            "project": str(self.lab.project),
        }
        self.lab.write_trace()
        self.binary = binary
        self.worker = worker
        self.artifact = artifact
        self.ledger: dict[str, Any] = {}
        self.address = ""
        self.remote_root = ""
        self.workspace_id = ""
        self.port = 0
        self.daemon_started = False
        self.session_ids: list[str] = []

        # Lab.start_tui uses Lab.environment directly. Include the exact
        # portable worker there as well as in controller CLI subprocesses.
        base_environment = self.lab.environment

        def environment() -> dict[str, str]:
            env = base_environment()
            env["MJ_WORKER_BINARY"] = str(self.worker)
            return env

        self.lab.environment = environment  # type: ignore[method-assign]

    @property
    def env(self) -> dict[str, str]:
        env = self.lab.environment()
        env["MJ_WORKER_BINARY"] = str(self.worker)
        env["MJ_TEST_ACCEPTANCE"] = "1"
        return env

    def command(self, *args: str, timeout: float = 60, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [str(self.binary), *args],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
            check=False,
        )
        if check and result.returncode:
            raise AcceptanceFailure(f"mj {' '.join(args)} failed ({result.returncode}): {result.stderr[-4000:]}")
        return result

    def load_ledger(self, path: pathlib.Path) -> None:
        self.ledger = load_json(path)
        connection = self.ledger.get("connection")
        if not isinstance(connection, dict):
            raise AcceptanceFailure("ledger has no connection object")
        address = connection.get("address")
        if not isinstance(address, str) or not address:
            raise AcceptanceFailure("ledger has no public address; run the lab status command first")
        self.address = address
        self.remote_root = (
            f"/home/{connection.get('user', 'ubuntu')}/"
            f"mj-acceptance-{self.ledger.get('run_tag', os.getpid())}-{os.getpid()}"
        )

    def prepare_profile_fixture(self, source_config: pathlib.Path) -> None:
        self.fixture = self.lab.project
        self.git(["init", "--initial-branch=main"])
        self.git(["config", "user.name", "Mjolnir Acceptance"])
        self.git(["config", "user.email", "acceptance@invalid"])
        (self.fixture / "original.txt").write_text("original attachment fixture\n", encoding="utf-8")
        self.git(["add", "original.txt"])
        self.git(["commit", "-m", "acceptance fixture"])
        (self.artifact / "attachment").mkdir(exist_ok=True)
        (self.artifact / "attachment" / "original.txt").write_text("original attachment fixture\n", encoding="utf-8")

    def git(self, args: list[str]) -> None:
        result = subprocess.run(["git", *args], cwd=self.lab.project, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=False)
        if result.returncode:
            raise AcceptanceFailure(f"git {' '.join(args)} failed: {result.stderr[-2000:]}")

    def write_config(self, source_config: pathlib.Path) -> None:
        if not self.port:
            self.port = Lab.free_port()
        connection = self.ledger["connection"]
        extra_args = [
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=yes",
            "-o", f"UserKnownHostsFile={connection['known_hosts']}",
        ]
        lines = [
            "version = 1",
            "",
            "[phone]",
            "enabled = true",
            f'bind = "127.0.0.1:{self.port}"',
            "tailscale_detect = false",
            "",
            *codex3_config_lines(source_config),
            "",
            "[bundles.acceptance]",
            'primary_repo = "fixture"',
            "",
            "[[bundles.acceptance.repositories]]",
            'id = "fixture"',
            f'local = {json.dumps(str(self.fixture))}',
            'destination = "fixture"',
            "",
            "[targets.ssh-docker]",
            'kind = "ssh-docker"',
            f'host = {json.dumps(self.address)}',
            f'user = {json.dumps(str(connection.get("user", "ubuntu")))}',
            f'identity_file = {json.dumps(str(connection["private_key"]))}',
            f"extra_args = {json.dumps(extra_args)}",
            f'image = {json.dumps(DEFAULT_IMAGE)}',
            'pull_policy = "missing"',
            'platform = "linux/amd64"',
            'cpus = "1"',
            'memory = "3g"',
        ]
        private_write(self.lab.config / "config.toml", "\n".join(lines) + "\n")

    def remote(self, command: str, name: str, timeout: float = 30) -> str:
        try:
            output = ssh(self.ledger, self.address, command, timeout)
        except (LabError, CommandError) as error:
            raise AcceptanceFailure(f"remote {name} failed: {error}") from error
        private_write(self.artifact / "remote" / f"{name}.log", output)
        return output

    def upload_remote_fixture(self) -> None:
        self.remote(
            shell_remote(
                [
                    "mkdir",
                    "-p",
                    self.remote_root,
                    f"{self.remote_root}/attach-1",
                    f"{self.remote_root}/attach-2",
                    f"{self.remote_root}/overlay-1",
                    f"{self.remote_root}/overlay-2",
                ]
            ),
            "fixture-mkdir",
        )
        source = self.artifact / "attachment" / "original.txt"
        for ordinal in (1, 2):
            for kind in ("attach", "overlay"):
                result = run_command(
                    scp_arguments(
                        self.ledger,
                        self.address,
                        source,
                        f"{self.remote_root}/{kind}-{ordinal}/original.txt",
                    ),
                    timeout=60,
                )
                result_text(self.artifact / "remote" / f"upload-{kind}-{ordinal}.log", result)

    def start_daemon(self) -> None:
        result = self.command("daemon", "restart", timeout=60)
        result_text(self.artifact / "daemon-restart.log", result)
        self.daemon_started = True
        self.lab.daemon_pid = None
        self.lab.base_url = f"http://127.0.0.1:{self.port}"
        code, _ = self.lab.wait_daemon_status(self.port)
        status, value = self.lab.request("POST", "/auth/session", {"code": code})
        if status != 204:
            raise AcceptanceFailure(f"web authentication failed ({status}): {value!r}")

    def reauthenticate(self) -> None:
        self.lab.trace["revisions"]["web"] = []
        code, _ = self.lab.wait_daemon_status(self.port)
        status, value = self.lab.request("POST", "/auth/session", {"code": code})
        if status != 204:
            raise AcceptanceFailure(f"web reauthentication failed ({status}): {value!r}")

    def ipc(self, action: dict[str, Any], request_id: int, timeout: float = DEFAULT_TIMEOUT) -> Any:
        """Send one daemon frame with a lifecycle-sized read timeout."""

        metadata = json.loads((self.lab.data / "daemon.json").read_text(encoding="utf-8"))
        envelope = {
            "protocol_version": metadata["protocol_version"],
            "request_id": request_id,
            "token": metadata["token"],
            "action": action,
        }
        body = json.dumps(envelope, separators=(",", ":")).encode()
        host, port_text = str(metadata["address"]).rsplit(":", 1)
        with socket.create_connection((host, int(port_text)), timeout=8) as stream:
            stream.settimeout(timeout)
            stream.sendall(struct.pack(">I", len(body)) + body)
            header = self._read_exact(stream, 4)
            remaining = struct.unpack(">I", header)[0]
            response_body = self._read_exact(stream, remaining)
        response = json.loads(response_body)
        result = response.get("result")
        if not isinstance(result, dict) or "Ok" not in result:
            raise AcceptanceFailure(f"daemon action failed: {response!r}")
        return result["Ok"]

    @staticmethod
    def _read_exact(stream: socket.socket, size: int) -> bytes:
        result = bytearray()
        while len(result) < size:
            chunk = stream.recv(size - len(result))
            if not chunk:
                raise AcceptanceFailure("daemon response frame was truncated")
            result.extend(chunk)
        return bytes(result)

    def workspace(self) -> str:
        result = self.ipc({"action": "list_workspaces"}, 10)
        value = result.get("value") if isinstance(result, dict) else None
        if not isinstance(value, list):
            raise AcceptanceFailure(f"unexpected workspace reply: {result!r}")
        wanted = "ssh-docker-acceptance"
        for entry in value:
            workspace = entry.get("workspace", {}) if isinstance(entry, dict) else {}
            if workspace.get("name") == wanted:
                self.workspace_id = str(workspace["id"])
                return self.workspace_id
        result = self.ipc({"action": "create_workspace", "arguments": {"name": wanted}}, 11)
        workspace = result.get("value") if isinstance(result, dict) else None
        if not isinstance(workspace, dict) or not workspace.get("id"):
            raise AcceptanceFailure(f"unexpected create-workspace reply: {result!r}")
        self.workspace_id = str(workspace["id"])
        return self.workspace_id

    def attach_tui(self) -> None:
        """Attach one reliability_lab PTY so reconnect/cleanup include a host client."""

        client = self.lab.start_tui("acceptance-tui")
        client.wait_for("Sessions", timeout=60)

    def create_session(self, ordinal: int, target_id: str = "ssh-docker") -> str:
        source = f"{self.remote_root}/attach-{ordinal}"
        request = {
            "workspace_id": self.workspace_id,
            "profile_id": "codex3",
            "bundle_id": "acceptance",
            "project_directory": None,
            "target_template_id": target_id,
            "additional_mounts": [
                {"source": source, "destination": "/mnt/attachment", "read_only": True},
                {
                    "source": f"{self.remote_root}/overlay-{ordinal}",
                    "destination": "/mnt/overlay",
                    "read_only": False,
                },
            ],
            "allow_dirty_local": False,
            "resource_allocation": None,
            "title": f"ssh-docker acceptance {ordinal}",
            "session_title_override": f"ssh-docker acceptance {ordinal}",
        }
        result = self.ipc({"action": "start_create_session", "arguments": request}, 100 + ordinal)
        value = result.get("value") if isinstance(result, dict) else None
        session = value.get("session") if isinstance(value, dict) else None
        session_id = session.get("id") if isinstance(session, dict) else None
        if not isinstance(session_id, str) or not session_id:
            raise AcceptanceFailure(f"unexpected registered-session reply: {result!r}")
        self.session_ids.append(session_id)
        self.lab.record_action("registered-session", ordinal=ordinal, session_id=session_id)
        self.ipc({"action": "wait_create_session", "arguments": {"session_id": session_id}}, 110 + ordinal)
        self.lab.record_action("new-session", ordinal=ordinal, session_id=session_id)
        self.wait_state(session_id, "running")
        return session_id

    def wait_state(self, session_id: str, state: str, timeout: float = DEFAULT_TIMEOUT) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        last: dict[str, Any] = {}
        while time.monotonic() < deadline:
            snapshot = self.lab.snapshot()
            last = next((item for item in snapshot.get("sessions", []) if item.get("id") == session_id), {})
            if last.get("state") == state:
                return last
            if last.get("has_error"):
                raise AcceptanceFailure(f"session {session_id} entered an error state: {last!r}")
            time.sleep(2)
        raise AcceptanceFailure(f"session {session_id} did not reach {state}: {last!r}")

    def prompt_and_verify(self, session_id: str, ordinal: int, timeout: float = DEFAULT_TIMEOUT) -> None:
        before_ordinal = self.wait_state(session_id, "running").get("latest_event_ordinal", 0)
        expected = f"ssh-docker acceptance generated file for session {ordinal}"
        prompt = (
            f"Create {CONTAINER_WORKSPACE}/fixture/generated.txt and /mnt/overlay/generated.txt, "
            f"putting exactly {expected!r} in each. Do not modify any existing file. "
            "In particular, leave /mnt/attachment/original.txt unchanged. "
            "Read all three files back after writing and report what you verified."
        )
        status, value = self.lab.request("POST", "/api/actions", {"action": "prompt", "session_id": session_id, "text": prompt})
        if status != 202:
            raise AcceptanceFailure(f"prompt action returned {status}: {value!r}")
        self.lab.record_action("prompt", session_id=session_id, ordinal=ordinal)
        container = resource_name(session_id)
        workspace_file = f"{CONTAINER_WORKSPACE}/fixture/generated.txt"
        script = shell_remote([
            "sh", "-lc",
            f"test -f {workspace_file} && "
            f"test \"$(cat {workspace_file})\" = {shlex.quote(expected)} && "
            "test -f /mnt/overlay/generated.txt && "
            f"test \"$(cat /mnt/overlay/generated.txt)\" = {shlex.quote(expected)} && "
            "test \"$(cat /mnt/attachment/original.txt)\" = 'original attachment fixture'",
        ])
        deadline = time.monotonic() + timeout
        last_error = "agent has not finished"
        while time.monotonic() < deadline:
            try:
                output = self.remote(shell_remote(["docker", "exec", container, *shlex.split(script)]), f"verify-{ordinal}", timeout=20)
                if output is not None:
                    break
            except AcceptanceFailure as error:
                last_error = str(error)
                time.sleep(3)
        else:
            raise AcceptanceFailure(f"session {session_id} did not produce the expected remote files: {last_error}")
        source = self.remote(shell_remote(["cat", f"{self.remote_root}/attach-{ordinal}/original.txt"]), f"source-{ordinal}")
        if source != "original attachment fixture\n":
            raise AcceptanceFailure(f"read-only attachment source changed for session {ordinal}")
        overlay_source = self.remote(
            shell_remote(["sh", "-lc", f"test ! -e {shlex.quote(self.remote_root)}/overlay-{ordinal}/generated.txt && cat {shlex.quote(self.remote_root)}/overlay-{ordinal}/original.txt"]),
            f"overlay-source-{ordinal}",
        )
        if overlay_source != "original attachment fixture\n":
            raise AcceptanceFailure(f"writable overlay source changed for session {ordinal}")
        status, transcript = self.lab.request("GET", f"/api/conversations/{session_id}")
        if status != 200:
            raise AcceptanceFailure(f"conversation read failed: {status}")
        record_json(self.artifact / f"conversation-{ordinal}.json", transcript)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            row = self.wait_state(session_id, "running")
            if row.get("is_idle") and row.get("latest_event_ordinal", 0) > before_ordinal:
                status, transcript = self.lab.request("GET", f"/api/conversations/{session_id}")
                if status != 200:
                    raise AcceptanceFailure(f"completed conversation read failed: {status}")
                record_json(self.artifact / f"conversation-{ordinal}.json", transcript)
                return
            time.sleep(2)
        raise AcceptanceFailure(f"session {session_id} did not finish its coding turn")

    def checkpoint_close_resume(self, session_id: str) -> None:
        before = self.container_id(session_id)
        result = self.ipc({"action": "checkpoint_session", "arguments": {"session_id": session_id}}, 300)
        record_json(self.artifact / "checkpoint.json", result)
        self.ipc({"action": "close_session", "arguments": {"session_id": session_id}}, 301)
        self.wait_state(session_id, "stopped", timeout=DEFAULT_TIMEOUT)
        resume = {
            "session_id": session_id,
            "workspace_id": self.workspace_id,
            "profile_id": "codex3",
            "target_template_id": "ssh-docker",
            "additional_mounts": None,
            "resource_allocation": None,
            "discard_queue": False,
            "repository_preflight": None,
        }
        self.ipc({"action": "resume_session", "arguments": resume}, 302)
        self.wait_state(session_id, "running", timeout=DEFAULT_TIMEOUT)
        after = self.container_id(session_id)
        if before == after:
            raise AcceptanceFailure("checkpoint resume reused the original container")
        record_json(self.artifact / "resume-container-ids.json", {"before": before, "after": after})
        container = resource_name(session_id)
        self.remote(
            shell_remote(
                [
                    "docker",
                    "exec",
                    container,
                    "sh",
                    "-lc",
                    "test -f /workspace/fixture/generated.txt && test ! -e /mnt/overlay/generated.txt",
                ]
            ),
            "resume-workspace",
        )
        self.lab.record_action("checkpoint-close-resume", session_id=session_id)

    def container_id(self, session_id: str) -> str:
        return self.remote(
            shell_remote(["docker", "inspect", "--format", "{{.Id}}", resource_name(session_id)]),
            f"container-{session_id[:8]}",
        ).strip()

    def verify_removed(self, session_id: str) -> None:
        name = resource_name(session_id)
        script = (
            f'test -z "$(docker ps -aq --filter label=dev.mj.session={session_id})" && '
            f'test -z "$(docker volume ls -q --filter label=dev.mj.session={session_id})" && '
            f'test ! -e "$HOME/.cache/mjolnir/docker-overlays/{name}" && '
            f'test ! -e "$HOME/.cache/mjolnir/git/sessions/{session_id}"'
        )
        deadline = time.monotonic() + 60
        while True:
            try:
                self.remote(shell_remote(["sh", "-c", script]), f"removed-{session_id[:8]}")
                return
            except AcceptanceFailure:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(2)

    def run_extra(self) -> None:
        """Exercise failures and orphan adoption with a separate controller database."""
        self.doctor()
        config_path = self.lab.config / "config.toml"
        config = config_path.read_text()
        target = config.split("[targets.ssh-docker]", 1)[1]
        broken = target.replace(DEFAULT_IMAGE, "mj-acceptance-missing-image:never")
        broken = broken.replace('pull_policy = "missing"', 'pull_policy = "never"')
        private_write(config_path, config + "\n[targets.broken]\n" + broken)
        self.start_daemon()
        self.workspace()
        self.attach_tui()
        try:
            self.create_session(1, "broken")
        except AcceptanceFailure as error:
            if "image" not in str(error).lower():
                raise
            private_write(self.artifact / "expected-launch-failure.log", str(error))
            failed_session = self.session_ids[-1]
            self.verify_removed(failed_session)
            deadline = time.monotonic() + 15
            while any(row["id"] == failed_session for row in self.lab.snapshot()["sessions"]):
                if time.monotonic() >= deadline:
                    raise AcceptanceFailure("failed launch left a provisioning session in controller state")
                time.sleep(1)
        else:
            raise AcceptanceFailure("missing-image launch unexpectedly succeeded")

        first = self.create_session(1)
        second = self.create_session(2)
        self.remote(shell_remote([
            "docker", "exec", resource_name(first), "sh", "-c",
            "if printf 'forbidden\\n' >/mnt/attachment/original.txt 2>/dev/null; then exit 1; fi; test \"$(cat /mnt/attachment/original.txt)\" = 'original attachment fixture'",
        ]), "verify-read-only-attachment")
        # Codex writes its native rollout on the first turn. Materialize both
        # conversations before testing worker restart and checkpoint import.
        self.prompt_and_verify(first, 1)
        self.prompt_and_verify(second, 2)
        self.checkpoint_close_resume(first)
        self.remote(shell_remote([
            "docker", "exec", resource_name(first), "sh", "-c",
            "test \"$(cat /workspace/fixture/generated.txt)\" = 'ssh-docker acceptance generated file for session 1'",
        ]), "verify-checkpoint-content")
        identities = {session_id: self.container_id(session_id) for session_id in (first, second)}
        record_json(self.artifact / "before-daemon-interruption.json", self.lab.snapshot())
        try:
            self.remote("sudo systemctl stop docker.socket docker.service", "stop-docker", timeout=60)
            started = time.monotonic()
            snapshot = self.lab.snapshot()
            elapsed = time.monotonic() - started
            record_json(self.artifact / "snapshot-during-interruption.json", {"elapsed_seconds": elapsed, "snapshot": snapshot})
            if elapsed > 5:
                raise AcceptanceFailure("viewer blocked while remote Docker was unavailable")
            result = self.command("doctor", "--json", timeout=60, check=False)
            result_text(self.artifact / "doctor-daemon-stopped.log", result)
            checks = json.loads(result.stdout)
            check = next(item for item in checks if item["id"] == "runtime.ssh-docker.ssh-docker")
            if check["status"] != "fixable":
                raise AcceptanceFailure("doctor did not report the stopped remote Docker daemon")
        finally:
            self.remote("sudo systemctl start docker.service", "start-docker", timeout=60)
        self.command("daemon", "restart", timeout=60)
        self.reauthenticate()
        for session_id in (first, second):
            self.wait_state(session_id, "running")
            if identities[session_id] != self.container_id(session_id):
                raise AcceptanceFailure("runtime recovery replaced a surviving container")
        self.lab.record_action("docker-daemon-interruption-recovered")

        self.ipc({"action": "close_session", "arguments": {"session_id": first}}, 501)
        self.wait_state(first, "stopped")
        self.verify_removed(first)
        if identities[second] != self.container_id(second):
            raise AcceptanceFailure("closing one session disturbed the other")
        self.ipc({"action": "close_session", "arguments": {"session_id": second}}, 503)
        self.wait_state(second, "stopped")
        self.verify_removed(second)
        self.run_orphan()

    def run_orphan(self) -> None:
        # A prior graceful restart checkpoints existing sessions and permits
        # journal pruning. Create a fresh session to test loss of its controller
        # before any checkpoint; checkpoint-based restore is tested separately.
        orphan = self.create_session(2)
        self.prompt_and_verify(orphan, 2)
        config = (self.lab.config / "config.toml").read_text()
        metadata = json.loads((self.lab.data / "daemon.json").read_text())
        daemon_pid = int(metadata["pid"])
        if daemon_pid not in self.lab.owned_pids():
            raise AcceptanceFailure("refusing to interrupt an unowned controller")
        os.kill(daemon_pid, signal.SIGKILL)
        self.lab.record_action("controller-crash", pid=daemon_pid, session_id=orphan)
        self.daemon_started = False
        self.lab.daemon_pid = None
        self.stop(finalize=False)
        self.lab.clients.clear()

        # Keep the original database intact while a fresh database discovers
        # and adopts the worker using ownership labels and its durable journal.
        self.lab.config = self.lab.runtime_root / "recovery-config"
        self.lab.data = self.lab.runtime_root / "recovery-data"
        private_dir(self.lab.config)
        private_dir(self.lab.data)
        self.lab.trace["recovery_runtime"] = {
            "config": str(self.lab.config), "data": str(self.lab.data),
        }
        self.lab.write_trace()
        previous_port = self.port
        self.port = Lab.free_port()
        recovery_config = config.replace(f"127.0.0.1:{previous_port}", f"127.0.0.1:{self.port}")
        private_write(self.lab.config / "config.toml", recovery_config)
        self.lab.daemon_pid = None
        self.daemon_started = False
        self.adopt_existing(orphan)

    def adopt_existing(self, session_id: str) -> None:
        if session_id not in self.session_ids:
            self.session_ids.append(session_id)
        scan = self.command("recover", "scan", "--json", timeout=90)
        result_text(self.artifact / "orphan-scan.json", scan)
        if session_id not in scan.stdout:
            raise AcceptanceFailure("fresh controller did not discover the orphan worker")
        adopted = self.command("recover", "adopt", "--session", session_id, "--target", "ssh-docker", timeout=DEFAULT_TIMEOUT)
        result_text(self.artifact / "orphan-adoption.log", adopted)
        self.lab.trace["revisions"]["web"] = []
        self.start_daemon()
        self.wait_state(session_id, "running")
        self.prompt_and_verify(session_id, 2)
        self.ipc({"action": "close_session", "arguments": {"session_id": session_id}}, 502)
        self.wait_state(session_id, "stopped")
        self.verify_removed(session_id)
        self.lab.record_action("fresh-controller-adoption-and-close", session_id=session_id)
        self.lab.trace["outcome"] = "passed"
        self.lab.trace["finished_at"] = self.lab.timestamp()
        self.lab.write_trace()

    def restart_and_recover(self) -> None:
        result = self.command("daemon", "restart", timeout=60)
        result_text(self.artifact / "daemon-restart-after-resume.log", result)
        self.reauthenticate()
        snapshot = self.lab.snapshot()
        record_json(self.artifact / "snapshot-after-reconnect.json", snapshot)
        recovery = self.command("recover", "scan", "--json", timeout=60)
        result_text(self.artifact / "recovery-scan.log", recovery)
        if recovery.stdout:
            with contextlib.suppress(json.JSONDecodeError):
                record_json(self.artifact / "recovery-scan.json", json.loads(recovery.stdout))
        # The extra phase stops this controller and exercises adoption using
        # a fresh database, where the surviving worker is actually an orphan.

    def cleanup_remote(self) -> None:
        if not self.remote_root:
            return
        # Never remove a mount source while one of our exact session
        # containers can still reference it. The EC2 lab owner can tear down
        # the host safely if a failed run leaves a worker behind.
        try:
            for session_id in self.session_ids:
                containers = self.remote(
                    shell_remote(
                        [
                            "docker",
                            "ps",
                            "-aq",
                            "--filter",
                            f"label=dev.mj.session={session_id}",
                        ]
                    ),
                    f"cleanup-check-{session_id[:8]}",
                    timeout=20,
                )
                if containers.strip():
                    self.lab.trace["remote_cleanup"] = {"status": "retained-for-ec2-teardown", "session_id": session_id}
                    self.lab.write_trace()
                    return
            self.remote(shell_remote(["rm", "-rf", "--", self.remote_root]), "cleanup-remote", timeout=30)
            self.lab.trace["remote_cleanup"] = {"status": "complete"}
        except AcceptanceFailure as error:
            self.lab.trace["remote_cleanup"] = {"status": "retained-for-ec2-teardown", "error": str(error)}
        self.lab.write_trace()

    def stop(self, finalize: bool = True) -> None:
        for client in self.lab.clients:
            client.terminate()
        try:
            if self.lab.daemon_pid is not None or self.daemon_started:
                result = self.command("daemon", "stop", timeout=30, check=False)
                result_text(self.artifact / "daemon-stop.log", result)
        finally:
            # Standalone recovery commands can also leave SSH transport
            # helpers, even when this phase never started a daemon.
            self.lab.cleanup_owned()
        if self.lab.owned_pids():
            raise AcceptanceFailure("test processes survived bounded cleanup; runtime retained")
        self.lab.capture_process_tree()
        if finalize:
            self.lab.preserve_runtime()
            self.lab.remove_runtime()

    def doctor(self) -> None:
        result = self.command("doctor", "--json", "--smoke", timeout=DEFAULT_TIMEOUT, check=False)
        result_text(self.artifact / "doctor.log", result)
        if result.stdout:
            with contextlib.suppress(json.JSONDecodeError):
                record_json(self.artifact / "doctor.json", json.loads(result.stdout))
        if result.returncode:
            raise AcceptanceFailure(f"mj doctor failed ({result.returncode}); see {self.artifact / 'doctor.log'}")

    def run(self) -> None:
        self.doctor()
        self.start_daemon()
        self.workspace()
        self.attach_tui()
        first = self.create_session(1)
        second = self.create_session(2)
        self.prompt_and_verify(first, 1)
        self.prompt_and_verify(second, 2)
        self.checkpoint_close_resume(first)
        self.restart_and_recover()
        self.ipc({"action": "close_session", "arguments": {"session_id": first}}, 400)
        self.ipc({"action": "close_session", "arguments": {"session_id": second}}, 401)
        self.wait_state(first, "stopped", timeout=DEFAULT_TIMEOUT)
        self.wait_state(second, "stopped", timeout=DEFAULT_TIMEOUT)
        self.verify_removed(first)
        self.verify_removed(second)
        self.lab.trace["outcome"] = "passed"
        self.lab.trace["finished_at"] = self.lab.timestamp()
        self.lab.write_trace()

def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("doctor", "run", "extra"), help="acceptance phase to execute")
    parser.add_argument("--ledger", type=pathlib.Path, help="private EC2 ledger.json")
    parser.add_argument("--artifact-dir", type=pathlib.Path, default=DEFAULT_ARTIFACT_DIR)
    parser.add_argument("--hel", type=pathlib.Path, default=pathlib.Path("target/debug/mj"))
    parser.add_argument("--worker", type=pathlib.Path, default=pathlib.Path("target/x86_64-unknown-linux-musl/debug/mj-worker"))
    parser.add_argument("--config", type=pathlib.Path, default=pathlib.Path.home() / ".config/mjolnir/config.toml")
    parser.add_argument("--seed", type=int, default=20260906)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    artifact = args.artifact_dir.resolve()
    if (artifact / "trace.json").exists():
        print(f"acceptance: artifact directory already contains a trace; choose a new --artifact-dir: {artifact}", file=sys.stderr)
        return 2
    private_dir(artifact)
    ledger = args.ledger.resolve() if args.ledger else artifact / "ledger.json"
    if not ledger.exists():
        print(f"acceptance: ledger does not exist: {ledger}", file=sys.stderr)
        return 2
    if not args.hel.is_file() or not args.worker.is_file():
        print("acceptance: --hel and --worker must name built files", file=sys.stderr)
        return 2
    if not args.config.is_file():
        print(f"acceptance: config does not exist: {args.config}", file=sys.stderr)
        return 2
    harness = AcceptanceLab(args.hel, artifact, args.worker, args.seed)
    exit_code = 0
    try:
        harness.load_ledger(ledger)
        harness.prepare_profile_fixture(args.config)
        harness.write_config(args.config)
        if args.command == "doctor":
            harness.doctor()
        else:
            harness.upload_remote_fixture()
            if args.command == "extra":
                harness.run_extra()
            else:
                harness.run()
    except (AcceptanceFailure, ScenarioFailure, LabError, CommandError, OSError, subprocess.TimeoutExpired) as error:
        harness.lab.trace["outcome"] = "failed"
        harness.lab.trace["failure"] = str(error)
        harness.lab.trace["finished_at"] = harness.lab.timestamp()
        harness.lab.write_trace()
        print(f"acceptance: failed: {error}; artifacts={artifact}", file=sys.stderr)
        exit_code = 1
    finally:
        try:
            harness.stop()
            harness.cleanup_remote()
        except Exception as error:
            harness.lab.trace["outcome"] = "failed"
            harness.lab.trace["cleanup_error"] = str(error)
            harness.lab.write_trace()
            print(f"acceptance: cleanup failed: {error}; artifacts={artifact}", file=sys.stderr)
            exit_code = 1
    if exit_code == 0:
        print(f"acceptance: passed; artifacts={artifact}")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
