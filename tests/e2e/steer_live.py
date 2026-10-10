#!/usr/bin/env python3
"""Opt-in live steering acceptance test with isolated harness homes and daemon.

Uses paid model calls. It copies only Codex auth.json and Claude
.credentials.json into temporary profile homes. See --help for invocation.
"""

from __future__ import annotations

import argparse
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import uuid


class ScenarioFailure(RuntimeError):
    pass


class SteerLab:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.repo_root = Path(__file__).resolve().parents[2]
        self.artifacts = args.artifacts.resolve()
        self.artifacts.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.runtime = Path(tempfile.mkdtemp(prefix="mj-steer-live-"))
        os.chmod(self.runtime, 0o700)
        self.instance = args.instance or f"steer-live-{os.getpid()}-{uuid.uuid4().hex[:8]}"
        if self.instance == "default" or not self.instance.replace("-", "").isalnum():
            raise ScenarioFailure("instance must be a non-default name containing only letters, digits, or hyphens")
        self.config = self.runtime / "config"
        self.data = self.runtime / "data"
        self.profiles = self.runtime / "profiles"
        self.home = self.runtime / "ambient-home"
        self.cache = self.runtime / "cache"
        self.project = self.runtime / "project"
        self.xdg_runtime = self.runtime / "xdg-runtime"
        self.binary_dir = self.runtime / "bin"
        self.codex_home = self.profiles / "codex"
        self.claude_home = self.profiles / "claude"
        self.markers = self.runtime / "markers"
        for directory in (
            self.config, self.data, self.profiles, self.home, self.cache,
            self.project, self.markers, self.binary_dir,
            self.xdg_runtime,
        ):
            directory.mkdir(parents=True, exist_ok=True)
        self.mj_source = args.mj.resolve()
        self.worker_source = args.worker.resolve()
        self.mj = self.binary_dir / "mj"
        self.worker = self.binary_dir / "mj-worker"
        shutil.copy2(self.mj_source, self.mj)
        shutil.copy2(self.worker_source, self.worker)
        os.chmod(self.xdg_runtime, 0o700)
        self.trace_path = self.artifacts / "trace.jsonl"
        self.trace = self.trace_path.open("w", buffering=1)
        self.sessions: list[str] = []
        self.daemon_may_be_running = False
        self.workspace = "Steering live " + self.instance[-8:]
        self.viewer_url = ""
        self.viewer_api: urllib.request.OpenerDirector | None = None
        self.api_url = ""
        self.api_token = ""
        self.codex_source = args.codex_home.expanduser().resolve()
        self.claude_source = args.claude_home.expanduser().resolve()

        self.env = os.environ.copy()
        for key in (
            "CODEX_HOME", "CLAUDE_CONFIG_DIR", "CODEX_PATH", "CLAUDE_CODE_EXECUTABLE",
            "CODEX_API_KEY", "OPENAI_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "MJ_DAEMON_OWNER_PID",
            "MJ_DAEMON_EXIT_WHEN_IDLE",
            "CONTAINERS_STORAGE_CONF", "CONTAINERS_STORAGE_CONF_OVERRIDE",
            "CONTAINERS_CONF", "CONTAINERS_CONF_OVERRIDE", "CONTAINER_HOST",
            "CONTAINER_CONNECTION", "STORAGE_DRIVER",
        ):
            self.env.pop(key, None)
        self.env.update({
            "HOME": str(self.home),
            "USERPROFILE": str(self.home),
            "MJ_INSTANCE": self.instance,
            "MJ_CONFIG_DIR": str(self.config),
            "MJ_DATA_DIR": str(self.data),
            "MJ_WORKER_BINARY": str(self.worker),
            "SESSIONWIKI_DATA": str(self.runtime / "index"),
            "XDG_CACHE_HOME": str(self.cache),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_DATA_HOME": str(self.home / ".local" / "share"),
            "XDG_STATE_HOME": str(self.home / ".local" / "state"),
            "XDG_RUNTIME_DIR": str(self.xdg_runtime),
            "MJOLNIR_NO_UPDATE_CHECK": "1",
        })
        self.record("setup", "instance", {
            "instance": self.instance,
            "mj": str(self.mj),
            "worker": str(self.worker),
            "isolated_config": str(self.config),
            "isolated_data": str(self.data),
            "artifact_dir": str(self.artifacts),
        })
        self.record("setup", "binaries-copied", {
            "mj_source": str(self.mj_source),
            "worker_source": str(self.worker_source),
            "mj_sha256": self.sha256(self.mj),
            "worker_sha256": self.sha256(self.worker),
            "copies_are_stable_for_cleanup": True,
        })

    @staticmethod
    def sha256(path: Path) -> str:
        with path.open("rb") as binary:
            return hashlib.file_digest(binary, "sha256").hexdigest()

    def record(self, case: str, event: str, value: object) -> None:
        self.trace.write(json.dumps({
            "time": time.time(), "case": case, "event": event, "value": value,
        }, ensure_ascii=False) + "\n")

    def command(self, *args: str) -> list[str]:
        return [str(self.mj), "--instance", self.instance, *args]

    def cli(
        self,
        *args: str,
        timeout: float | None = None,
        expected_status: int = 0,
        case: str = "cli",
    ) -> str:
        result = subprocess.run(
            self.command(*args), env=self.env, capture_output=True,
            text=True, timeout=timeout or self.args.timeout,
        )
        self.record(case, "cli", {
            "args": args, "status": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr,
        })
        if result.returncode != expected_status:
            raise ScenarioFailure(
                f"mj {' '.join(args)} exited {result.returncode}: "
                f"{result.stderr.strip()} {result.stdout.strip()}"
            )
        return result.stdout

    def copy_credential(self, source: Path, destination: Path, name: str) -> None:
        credential = source / name
        if not credential.is_file():
            raise ScenarioFailure(f"required credential file is missing: {credential}")
        shutil.copyfile(credential, destination / name)
        os.chmod(destination / name, 0o600)

    @staticmethod
    def toml_string(value: str) -> str:
        return json.dumps(value, ensure_ascii=False)

    def make_hook(self, harness: str, event: str) -> Path:
        if harness != "claude":
            raise ValueError("the slow PreToolUse fixture is configured for Claude")
        hook = self.claude_home / "slow_pre_tool_hook.py"
        started = self.markers / f"{harness}-{event}-started"
        finished = self.markers / f"{harness}-{event}-finished"
        hook_source = (
            "#!/usr/bin/env python3\n"
            "import pathlib, sys, time\n"
            "sys.stdin.read()\n"
            f"pathlib.Path({str(started)!r}).write_text('started')\n"
            "time.sleep(8)\n"
            f"pathlib.Path({str(finished)!r}).write_text('finished')\n"
        )
        hook.write_text(hook_source, encoding="utf-8")
        hook.chmod(0o700)
        return hook

    def codex_cli_entrypoint(self) -> Path:
        pins = (self.repo_root / "mj-core/src/harness_runtime.rs").read_text(encoding="utf-8")
        versions = {}
        for name in ("CODEX_ACP_VERSION", "CODEX_CLI_VERSION"):
            match = re.search(rf'pub const {name}: &str = "([^"]+)";', pins)
            if not match:
                raise ScenarioFailure(f"could not read {name} from the harness pins")
            versions[name] = match.group(1)
        install_id = (
            f"brokkai-codex-acp-{versions['CODEX_ACP_VERSION']}"
            f"_codex-{versions['CODEX_CLI_VERSION']}"
        )
        cache_bases = [self.cache, Path.home() / ".cache"]
        configured_cache = os.environ.get("XDG_CACHE_HOME")
        if configured_cache:
            cache_bases.append(Path(configured_cache).expanduser())
        for cache_base in dict.fromkeys(path.resolve() for path in cache_bases):
            candidate = (
                cache_base / "mjolnir/harnesses/codex" / install_id
                / "node_modules/@openai/codex/bin/codex.js"
            )
            if candidate.is_file():
                return candidate
        raise ScenarioFailure(
            "the pinned Codex CLI needed for the isolated hook is absent "
            f"from the managed harness cache (install {install_id})"
        )

    def install_codex_test_wrapper(self) -> None:
        """Seed this run's managed cache with a test-local trust wrapper."""
        source_cli = self.codex_cli_entrypoint()
        source_install = source_cli.parents[4]
        install = self.cache / "mjolnir" / "harnesses" / "codex" / source_install.name
        if install.exists():
            raise ScenarioFailure(f"isolated Codex harness install already exists: {install}")
        (install / "node_modules" / "@brokkai").mkdir(parents=True)
        shutil.copy2(source_install / "mj-harness.json", install / "mj-harness.json")
        (install / "node_modules" / "@brokkai" / "codex-acp").symlink_to(
            source_install / "node_modules" / "@brokkai" / "codex-acp",
            target_is_directory=True,
        )
        wrapper = install / "node_modules" / "@openai" / "codex" / "bin" / "codex.js"
        wrapper.parent.mkdir(parents=True)
        wrapper.write_text(
            "#!/bin/sh\n"
            f"exec node {shlex.quote(str(source_cli))} "
            "--dangerously-bypass-hook-trust \"$@\"\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o700)
        result = subprocess.run(
            [str(wrapper), "--version"], capture_output=True, text=True,
            timeout=15, env=self.env,
        )
        if result.returncode or "0.160.1" not in result.stdout:
            raise ScenarioFailure(
                "isolated Codex wrapper did not launch the pinned CLI: "
                f"status={result.returncode}, stdout={result.stdout!r}, stderr={result.stderr!r}"
            )
        self.record("setup", "codex-hook-bypass-installed", {
            "install": str(install),
            "wrapper": str(wrapper),
            "source_cli": str(source_cli),
            "version": result.stdout.strip(),
            "trust_bypass_is_test_local": True,
        })

    def prepare(self) -> None:
        self.codex_home.mkdir()
        self.claude_home.mkdir()
        self.copy_credential(self.codex_source, self.codex_home, "auth.json")
        self.copy_credential(self.claude_source, self.claude_home, ".credentials.json")
        self.make_hook("claude", "tool-boundary")
        (self.codex_home / "config.toml").write_text(
            'approval_policy = "on-request"\n'
            'sandbox_mode = "workspace-write"\n'
            "[features]\n"
            "hooks = true\n",
            encoding="utf-8",
        )
        self.install_codex_test_wrapper()
        (self.claude_home / "settings.json").write_text(json.dumps({
            "permissions": {"allow": ["Bash"]},
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": f"{sys.executable} {self.claude_home / 'slow_pre_tool_hook.py'}",
                    }],
                }],
            },
        }, indent=2) + "\n", encoding="utf-8")

        repo_env = os.environ.copy()
        repo_env.update({
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(self.runtime / "gitconfig"),
        })
        for cmd in (
            ["git", "init", "--initial-branch=main"],
            ["git", "config", "user.name", "Mj steering test"],
            ["git", "config", "user.email", "steer-test@invalid"],
        ):
            subprocess.run(cmd, cwd=self.project, env=repo_env, check=True,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        (self.project / "README.md").write_text("Isolated Mj steering test repository.\n")
        subprocess.run(["git", "add", "README.md"], cwd=self.project,
                       env=repo_env, check=True, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, text=True)
        subprocess.run(["git", "commit", "-m", "initial fixture"], cwd=self.project,
                       env=repo_env, check=True, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, text=True)

        codex_home = str(self.codex_home)
        claude_home = str(self.claude_home)
        config_text = (
            "version = 13\n\n"
            "[phone]\n"
            "enabled = true\n"
            'bind = "127.0.0.1:0"\n'
            "tailscale_detect = false\n\n"
            "[profiles.codex]\n"
            'kind = "codex"\n'
            f"home = {self.toml_string(codex_home)}\n"
            "environment = { "
            'INITIAL_AGENT_MODE = "workspace-write", '
            f"XDG_CACHE_HOME = {self.toml_string(str(self.cache))} "
            "}\n\n"
            "[profiles.claude]\n"
            'kind = "claude"\n'
            f"home = {self.toml_string(claude_home)}\n\n"
            "[bundles.fixture]\n"
            "primary_repo = \"fixture\"\n\n"
            "[[bundles.fixture.repositories]]\n"
            'id = "fixture"\n'
            f"local = {self.toml_string(str(self.project))}\n"
            'destination = "fixture"\n\n'
            "[targets.localhost]\n"
            'kind = "bare"\n'
        )
        config_path = self.config / "config.toml"
        config_path.write_text(config_text, encoding="utf-8")
        for real_home in (str(self.codex_source), str(self.claude_source)):
            if real_home in config_text:
                raise ScenarioFailure("isolated config references a real harness home")
        if str(self.home) not in self.env["HOME"]:
            raise ScenarioFailure("Mj process HOME is not isolated")
        self.record("setup", "profile-audit", {
            "codex_config_home_is_copy": codex_home.startswith(str(self.runtime)),
            "claude_config_home_is_copy": claude_home.startswith(str(self.runtime)),
            "real_profile_paths_absent_from_config": True,
            "credential_files_copied": ["auth.json", ".credentials.json"],
            "codex_hook_trust_bypass_scoped_to_test_cache": True,
        })
        (self.artifacts / "isolated-config.toml").write_text(config_text, encoding="utf-8")

    def daemon_request(self, action: dict[str, object], request_id: int = 1) -> object:
        metadata_path = self.data / "daemon.json"
        deadline = time.monotonic() + 30
        while not metadata_path.is_file() and time.monotonic() < deadline:
            time.sleep(0.05)
        if not metadata_path.is_file():
            raise ScenarioFailure("isolated daemon did not publish daemon.json")
        metadata = json.loads(metadata_path.read_text())
        host, port_text = str(metadata["address"]).rsplit(":", 1)
        if host.startswith("[") and host.endswith("]"):
            host = host[1:-1]
        envelope = {
            "protocol_version": metadata["protocol_version"],
            "request_id": request_id,
            "token": metadata["token"],
            "action": action,
        }
        body = json.dumps(envelope, separators=(",", ":")).encode()
        with socket.create_connection((host, int(port_text)), timeout=10) as stream:
            stream.sendall(struct.pack(">I", len(body)) + body)

            def receive_exact(length: int) -> bytes:
                chunks = bytearray()
                while len(chunks) < length:
                    chunk = stream.recv(length - len(chunks))
                    if not chunk:
                        raise ScenarioFailure("isolated daemon response frame was truncated")
                    chunks.extend(chunk)
                return bytes(chunks)

            while True:
                length = struct.unpack(">I", receive_exact(4))[0]
                response = json.loads(receive_exact(length))
                if response.get("request_id") != request_id:
                    raise ScenarioFailure("isolated daemon crossed request identities")
                result = response.get("result")
                if not isinstance(result, dict) or "Ok" not in result:
                    raise ScenarioFailure(f"isolated daemon action failed: {response!r}")
                reply = result["Ok"]
                if reply.get("reply") != "reply_chunk":
                    return reply.get("value")

    def get_api(self, path: str) -> dict:
        request = urllib.request.Request(
            self.api_url + path,
            headers={"Authorization": "Bearer " + self.api_token},
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    def viewer_request(self, method: str, path: str, body: object | None = None) -> tuple[int, object | None]:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.viewer_url + path, data=data, method=method,
            headers={"Content-Type": "application/json"},
        )
        assert self.viewer_api is not None
        try:
            with self.viewer_api.open(request, timeout=30) as response:
                payload = response.read()
                return response.status, json.loads(payload) if payload else None
        except urllib.error.HTTPError as error:
            payload = error.read()
            try:
                value = json.loads(payload) if payload else None
            except json.JSONDecodeError:
                value = payload.decode("utf-8", "replace")
            return error.code, value

    def start(self) -> None:
        self.daemon_may_be_running = True
        self.cli("workspaces", "create", self.workspace, "--json", case="setup")
        info = json.loads(self.cli("api-info", "--json", case="setup"))
        self.api_url = info["base_url"]
        self.api_token = Path(info["token_path"]).read_text().strip()
        access = self.daemon_request({"action": "web_viewer_access"}, request_id=2)
        if not isinstance(access, dict) or "Ready" not in access:
            raise ScenarioFailure(f"isolated viewer did not become ready: {access!r}")
        ready = access["Ready"]
        self.viewer_url = ready["viewer_url"].rstrip("/")
        self.viewer_api = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar())
        )
        status, _ = self.viewer_request("POST", "/auth/session", {"code": ready["viewer_code"]})
        if status != 204:
            raise ScenarioFailure(f"isolated viewer login returned HTTP {status}")
        self.record("setup", "viewer-ready", {"viewer_url": self.viewer_url})

    def create_session(self, harness: str, title: str) -> str:
        output = self.cli(
            "new", "--workspace", self.workspace,
            "--profile", harness, "--target", "localhost",
            "--project-directory", str(self.project),
            "--no-review", "--title", title, "--json",
            timeout=max(self.args.timeout, 300), case=title,
        )
        value = json.loads(output)
        session = value.get("session_id", value.get("id"))
        if not isinstance(session, str) or not session:
            raise ScenarioFailure(f"mj new returned no session id: {value!r}")
        self.sessions.append(session)
        self.wait_session_ready(session, title)
        return session

    def audit_codex_post_tool_hook(self, session: str, case: str) -> None:
        supervisor = self.data / "workers" / session / "acp-supervisor.json"
        if not supervisor.is_file():
            raise ScenarioFailure(f"{case}: Codex ACP supervisor record is missing")
        spec = json.loads(supervisor.read_text(encoding="utf-8"))
        environment = spec.get("environment", {})
        raw_config = environment.get("CODEX_CONFIG")
        if not isinstance(raw_config, str):
            raise ScenarioFailure(f"{case}: Codex ACP has no runtime config override")
        runtime_config = json.loads(raw_config)
        profile_config_path = self.data / "workers" / session / "profile" / "config.toml"
        if not profile_config_path.is_file():
            raise ScenarioFailure(f"{case}: staged Codex profile config is missing")
        profile_config = tomllib.loads(profile_config_path.read_text(encoding="utf-8"))
        if profile_config.get("features", {}).get("hooks") is not True:
            raise ScenarioFailure(f"{case}: copied Codex profile does not enable hooks")
        post_tool_hooks = runtime_config.get("hooks", {}).get("PostToolUse")
        if not post_tool_hooks:
            raise ScenarioFailure(f"{case}: Mj PostToolUse hook is missing from Codex runtime config")
        self.record(case, "codex-post-tool-hook-configured", {
            "mj_post_tool_hook_preserved": True,
            "pre_tool_hook_tested": False,
        })

    def snapshot(self) -> dict:
        status, value = self.viewer_request("GET", "/api/snapshot")
        if status != 200 or not isinstance(value, dict):
            raise ScenarioFailure(f"invalid viewer snapshot: HTTP {status} {value!r}")
        return value

    @staticmethod
    def row(snapshot: dict, session: str) -> dict | None:
        rows = snapshot.get("sessions", [])
        if not isinstance(rows, list):
            raise ScenarioFailure("viewer snapshot has no session array")
        return next((row for row in rows if row.get("id") == session), None)

    def session_view(self, session: str) -> dict:
        value = self.get_api("/sessions/" + urllib.parse.quote(session, safe=""))
        if value.get("has_error"):
            raise ScenarioFailure(f"Mj API reports session error: {value!r}")
        return value

    def history(self, session: str) -> list[dict]:
        encoded = urllib.parse.quote(session, safe="")
        path = f"/sessions/{encoded}/history"
        page = self.get_api(path)
        items = list(page.get("items", []))
        before = page.get("before")
        seen: set[tuple[int, str]] = set()
        while before:
            cursor = (int(before["position"]), str(before["stable_id"]))
            if cursor in seen:
                raise ScenarioFailure("Mj history API repeated a pagination cursor")
            seen.add(cursor)
            query = urllib.parse.urlencode({
                "before_position": cursor[0], "before_id": cursor[1],
            })
            page = self.get_api(path + "?" + query)
            items = list(page.get("items", [])) + items
            before = page.get("before")
            if len(seen) > 100:
                raise ScenarioFailure("Mj history API exceeded 100 pages")
        return items

    def transcript(self, session: str, roles: tuple[str, ...]) -> list[dict]:
        encoded = urllib.parse.quote(session, safe="")
        items: list[dict] = []
        after_seq = 0
        while True:
            query = urllib.parse.urlencode(
                [("role", role) for role in roles] + [("after_seq", after_seq)]
            )
            page = self.get_api(f"/sessions/{encoded}/transcript?{query}")
            items.extend(page.get("items", []))
            next_after_seq = page.get("next_after_seq")
            if next_after_seq is None or next_after_seq <= after_seq:
                return items
            after_seq = next_after_seq

    @staticmethod
    def terminal_output(items: list[dict]) -> tuple[str, list[dict]]:
        output_parts: list[str] = []
        calls: list[dict] = []
        for item in items:
            body = item.get("body") or {}
            if body.get("kind") == "tool":
                call = body.get("call") or {}
                calls.append(call)
                for record in body.get("terminal_outputs", []):
                    if isinstance(record.get("output"), str):
                        output_parts.append(record["output"])
            elif body.get("kind") == "terminal_output":
                record = body.get("record") or {}
                if isinstance(record.get("output"), str):
                    output_parts.append(record["output"])
        return "\n".join(output_parts), calls

    def save_api_evidence(self, session: str, case: str, items: list[dict]) -> tuple[list[dict], dict]:
        snapshot_row = self.row(self.snapshot(), session)
        structured_transcript = self.transcript(session, ("tool", "terminal"))
        value = {
            "session": self.session_view(session),
            "viewer_snapshot": snapshot_row,
            "history": items,
            "tool_and_terminal_transcript": structured_transcript,
        }
        path = self.artifacts / f"{case}-api-evidence.json"
        path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        self.record(case, "api-evidence-saved", {
            "path": str(path), "history_item_count": len(items),
        })
        return structured_transcript, snapshot_row

    def wait_session_ready(self, session: str, case: str) -> None:
        deadline = time.monotonic() + min(self.args.timeout, 240)
        last: dict = {}
        while time.monotonic() < deadline:
            last = self.session_view(session)
            row = self.row(self.snapshot(), session) or {}
            if row.get("has_error") or row.get("state") == "error":
                raise ScenarioFailure(f"{case}: session failed during startup: {row!r}")
            if row.get("chat_phase") == "idle" and row.get("is_idle"):
                self.record(case, "session-ready", {"id": session, "state": last})
                return
            time.sleep(0.5)
        raise ScenarioFailure(f"{case}: session did not become ready: {last!r}")

    def prompt(self, session: str, command_id: str, text: str, case: str) -> None:
        self.cli(
            "prompt", "--session", session, "--command-id", command_id,
            "--json", text, timeout=45, case=case,
        )

    def steer_queued_prompt(
        self,
        session: str,
        command_id: str,
        case: str,
        *,
        must_steer_before: Path | None = None,
    ) -> dict:
        """Use the viewer's explicit steer action when the normal prompt remains queued."""
        deadline = time.monotonic() + 15
        last_row: dict = {}
        while time.monotonic() < deadline:
            last_row = self.row(self.snapshot(), session) or {}
            steering = last_row.get("steering") or {}
            if steering.get("queued_prompt_id") == command_id:
                status = steering.get("status")
                self.record(case, "steering-observed", steering)
                if status == "failed":
                    raise ScenarioFailure(f"steering failed: {steering!r}")
                if status in ("applied", "resolved", "unconfirmed"):
                    return {"path": "normal-prompt", "steering": steering}
                # A pending operation already exists; do not submit a duplicate.
                if status == "pending":
                    return {"path": "normal-prompt-pending", "steering": steering}

            queued = last_row.get("queued_prompts") or []
            target = next((
                item for item in queued
                if item.get("command_id", item.get("id")) == command_id
            ), None)
            active = last_row.get("active_prompt_id")
            if target and active and not last_row.get("steering"):
                if must_steer_before and must_steer_before.exists():
                    raise ScenarioFailure(
                        f"{case}: queued prompt remained unsteered until after the PreToolUse hook finished"
                    )
                queued_id = target.get("id", target.get("command_id"))
                body = {
                    "action": "turn-control",
                    "session_id": session,
                    "command": {
                        "type": "steer",
                        "data": {
                            "active_prompt_id": active,
                            "queued_prompt_id": queued_id,
                        },
                    },
                }
                status, response = self.viewer_request("POST", "/api/actions", body)
                self.record(case, "explicit-steer", {
                    "status": status, "response": response,
                    "active_prompt_id": active, "queued_prompt_id": queued_id,
                })
                if status not in (200, 202, 204):
                    raise ScenarioFailure(f"explicit Mj turn-control steer returned {status}: {response!r}")
                return {"path": "explicit-api", "response": response}
            time.sleep(0.1)
        raise ScenarioFailure(
            f"queued prompt was not targetable for steering: {last_row!r}"
        )

    def wait_delivery(
        self,
        session: str,
        nonce: str,
        user_command_id: str,
        case: str,
        *,
        require_midturn_applied: bool = False,
        minimum_agent_text: int = 0,
    ) -> tuple[list[dict], dict]:
        deadline = time.monotonic() + self.args.timeout
        applied_while_running = False
        idle_without_delivery_since: float | None = None
        last_row: dict = {}
        last_items: list[dict] = []
        while time.monotonic() < deadline:
            last_row = self.row(self.snapshot(), session) or {}
            last_items = self.history(session)
            agent_text = "\n".join(
                str(item.get("text", "")) for item in last_items
                if item.get("role") == "agent"
            )
            user_text = "\n".join(
                str(item.get("text", "")) for item in last_items
                if item.get("role") == "user"
            )
            steering = last_row.get("steering") or {}
            if steering.get("queued_prompt_id") == user_command_id:
                if steering.get("status") == "failed":
                    raise ScenarioFailure(f"Mj records failed steering: {steering!r}")
                if steering.get("status") == "applied" and last_row.get("chat_phase") == "running":
                    applied_while_running = True
            completed = (
                last_row.get("chat_phase") == "idle"
                and not any(
                    item.get("command_id", item.get("id")) == user_command_id
                    for item in (last_row.get("queued_prompts") or [])
                )
            )
            if completed and user_text.count(nonce) == 1 and agent_text.count(nonce) == 0:
                idle_without_delivery_since = idle_without_delivery_since or time.monotonic()
                refusal = next((
                    marker for marker in (
                        "doesn't want to take this action right now",
                        "Bash call was declined",
                    )
                    if marker.lower() in agent_text.lower()
                ), None)
                if refusal:
                    raise ScenarioFailure(
                        f"{case}: harness completed without the queued steer after {refusal!r}"
                    )
                if time.monotonic() - idle_without_delivery_since >= 12:
                    raise ScenarioFailure(
                        f"{case}: session became idle without delivering the queued nonce; "
                        f"agent text={agent_text[-1200:]!r}"
                    )
            else:
                idle_without_delivery_since = None
            if (
                completed
                and user_text.count(nonce) == 1
                and agent_text.count(nonce) == 1
                and len(agent_text) >= minimum_agent_text
            ):
                if require_midturn_applied and not applied_while_running:
                    raise ScenarioFailure(
                        "Codex steer was not observed as applied while the original turn was running"
                    )
                self.record(case, "delivery-complete", {
                    "user_nonce_count": user_text.count(nonce),
                    "assistant_nonce_count": agent_text.count(nonce),
                    "applied_while_running": applied_while_running,
                    "chat_phase": last_row.get("chat_phase"),
                })
                return last_items, {
                    "applied_while_running": applied_while_running,
                    "chat_phase": last_row.get("chat_phase"),
                    "steering": steering,
                }
            if last_row.get("has_error"):
                raise ScenarioFailure(f"session reports an error while waiting for steer: {last_row!r}")
            time.sleep(0.25)
        raise ScenarioFailure(
            f"timed out waiting for nonce {nonce}; session={last_row!r}; "
            f"transcript roles={[item.get('role') for item in last_items]}"
        )

    @staticmethod
    def text_by_role(items: list[dict], role: str) -> str:
        return "\n".join(str(item.get("text", "")) for item in items if item.get("role") == role)

    def tool_hook_markers(self, harness: str, session: str, event: str) -> tuple[Path, Path]:
        if harness != "claude":
            raise ScenarioFailure("slow-hook marker paths are only used by the Claude scenario")
        return (
            self.markers / f"{harness}-{event}-started",
            self.markers / f"{harness}-{event}-finished",
        )

    def wait_codex_shell_running(
        self, session: str, case: str, marker_name: str,
    ) -> dict:
        deadline = time.monotonic() + min(self.args.timeout, 180)
        workspace = (self.project / ".mj" / "clones" / session).resolve()
        if not workspace.is_relative_to(self.runtime.resolve()):
            raise ScenarioFailure(f"{case}: Codex clone escapes isolated test runtime: {workspace}")
        marker = workspace / marker_name
        while time.monotonic() < deadline:
            row = self.row(self.snapshot(), session) or {}
            if row.get("has_error"):
                raise ScenarioFailure(f"{case}: Codex session errored before shell start: {row!r}")
            matching_calls = []
            for item in self.transcript(session, ("tool",)):
                call = (item.get("body") or {}).get("call") or {}
                raw_input = call.get("rawInput") or {}
                command = raw_input.get("command")
                title = call.get("title")
                if marker_name in str(command or "") or marker_name in str(title or ""):
                    matching_calls.append(call)
            terminal_calls = [
                call for call in matching_calls
                if call.get("status") in ("completed", "failed", "cancelled")
            ]
            if marker.is_file():
                if terminal_calls:
                    raise ScenarioFailure(
                        "Codex shell call completed before steering: "
                        f"status={terminal_calls[0].get('status')}"
                    )
                if row.get("chat_phase") == "running" and matching_calls:
                    call = matching_calls[-1]
                    value = {
                        "tool_call_id": call.get("toolCallId"),
                        "api_tool_status": call.get("status"),
                        "chat_phase": row.get("chat_phase"),
                        "command_marker": marker.name,
                    }
                    self.record(case, "shell-command-running", value)
                    return value
            time.sleep(0.1)
        raise ScenarioFailure(f"{case}: Mj API did not observe the Codex shell command running")

    def run_tool_boundary(self, harness: str) -> dict:
        case = f"{harness}-tool-boundary"
        session = self.create_session(harness, case)
        if harness == "codex":
            self.audit_codex_post_tool_hook(session, case)
        nonce = f"STEER_LIVE_{harness.upper()}_QUEUED_{uuid.uuid4().hex[:10].upper()}"
        output_marker = f"{harness.upper()}_COMMAND_{uuid.uuid4().hex[:10].upper()}"
        command_id = f"steer-live-{self.instance}-{harness}-tool-queued"
        marker_name = f".steer-live-{self.instance}-{harness}-command-started"
        if harness == "claude":
            hook_started, hook_finished = self.tool_hook_markers(
                harness, session, "tool-boundary",
            )
            command = (
                f"printf 'TOOL_BEGIN_{output_marker}\\n' && sleep 2 && "
                f"printf 'TOOL_DONE_{output_marker}\\n'"
            )
        else:
            hook_started = hook_finished = None
            command = (
                f"printf 'TOOL_BEGIN_{output_marker}\\n' && touch {marker_name} && "
                f"sleep 20 && printf 'TOOL_DONE_{output_marker}\\n'"
            )
        tool_prompt = (
            f"Use the {'Bash' if harness == 'claude' else 'exec shell'} tool exactly once "
            "to run this exact command: "
            f"`{command}`. "
            "Do not change or retry the command. After it completes, report its exact output."
        )
        queued_prompt = f"When you can next respond, include {nonce} exactly once. Do not call any tools."
        self.prompt(session, f"steer-live-{self.instance}-{harness}-tool-first", tool_prompt, case)
        if harness == "claude":
            try:
                self.wait_file(hook_started, case)
            except ScenarioFailure:
                try:
                    items = self.history(session)
                    transcript, snapshot_row = self.save_api_evidence(session, case, items)
                    self.record(case, "pre-tool-hook-marker-missing", {
                        "api_evidence": str(self.artifacts / f"{case}-api-evidence.json"),
                        "agent_text": self.text_by_role(items, "agent")[-5000:],
                        "tool_transcript": transcript,
                        "snapshot_preview": snapshot_row.get("preview", []),
                    })
                except Exception as evidence_error:
                    self.record(case, "pre-tool-hook-evidence-error", str(evidence_error))
                raise
            if hook_finished.exists():
                raise ScenarioFailure("Claude slow hook finished before the queued prompt was submitted")
            self.record(case, "pre-tool-hook-running", {"started": True, "finished": False})
        else:
            self.wait_codex_shell_running(session, case, marker_name)
        self.prompt(session, command_id, queued_prompt, case)
        steer = self.steer_queued_prompt(
            session, command_id, case,
            must_steer_before=hook_finished if harness == "claude" else None,
        )
        items, delivery = self.wait_delivery(
            session, nonce, command_id, case,
            require_midturn_applied=(harness == "codex"),
        )
        structured_transcript, snapshot_row = self.save_api_evidence(session, case, items)
        all_text = "\n".join(str(item.get("text", "")) for item in items)
        tool_text = self.text_by_role(items, "tool")
        terminal_text, tool_calls = self.terminal_output(structured_transcript)
        # The history API can represent stdout through the viewer snapshot's
        # conversation preview while the transcript API retains only a terminal
        # reference on the tool call.
        preview_text = "\n".join(
            str(line) for line in snapshot_row.get("preview", [])
        )
        output_text = terminal_text + "\n" + preview_text
        boundary_finished = harness != "claude" or hook_finished.is_file()
        command_output_ok = (
            f"TOOL_BEGIN_{output_marker}" in output_text
            and f"TOOL_DONE_{output_marker}" in output_text
        )
        matching_calls = [
            call for call in tool_calls
            if output_marker in str((call.get("rawInput") or {}).get("command", ""))
            or output_marker in str(call.get("title", ""))
        ]
        command_completed = len(matching_calls) == 1 and matching_calls[0].get("status") == "completed"
        cancelled = any(
            marker in all_text.lower()
            for marker in (
                "doesn't want to take this action right now",
                "bash call was declined",
            )
        )
        failed_text = any(
            word in all_text.lower() for word in ("hook_cancelled", "tool_call_failed")
        ) or any(call.get("status") == "failed" for call in tool_calls)
        if not boundary_finished or not command_output_ok or not command_completed or cancelled or failed_text:
            raise ScenarioFailure(
                f"{case}: tool did not complete cleanly after steering; "
                f"boundary_finished={boundary_finished}, command_completed={command_completed}, "
                f"command_output_found={command_output_ok}, cancelled={cancelled}, "
                f"failure_text={failed_text}"
            )
        summary = {
            "harness": harness,
            "session_id": session,
            "steer_path": steer["path"],
            "steering": delivery["steering"],
            "boundary_condition": "pre_tool_hook" if harness == "claude" else "shell_command_running",
            "boundary_finished": boundary_finished,
            "tool_output_found": True,
            "failure_text_absent": True,
            "nonce_count_agent": self.text_by_role(items, "agent").count(nonce),
            "tool_transcript_excerpt": tool_text[:2000],
        }
        self.record(case, "passed", summary)
        return summary

    def run_text_only(self, harness: str, *, midturn_regression: bool = False) -> dict:
        case = f"{harness}-text-only"
        session = self.create_session(harness, case)
        nonce = f"STEER_LIVE_{harness.upper()}_TEXT_{uuid.uuid4().hex[:10].upper()}"
        command_id = f"steer-live-{self.instance}-{harness}-text-queued"
        base_prompt = (
            "Write a detailed, structured explanation of HTTP caching with at least 1000 words. "
            "Do not use tools. Continue until the explanation is complete."
        )
        queued_prompt = f"In your next response, include {nonce} exactly once. Do not call tools."
        self.prompt(session, f"steer-live-{self.instance}-{harness}-text-first", base_prompt, case)
        self.wait_for_agent_text(session, minimum=1800, case=case)
        if self.row(self.snapshot(), session).get("chat_phase") != "running":
            raise ScenarioFailure(f"{case}: long text turn ended before the queued prompt was submitted")
        self.prompt(session, command_id, queued_prompt, case)
        steer = self.steer_queued_prompt(session, command_id, case)
        items, delivery = self.wait_delivery(
            session, nonce, command_id, case,
            require_midturn_applied=midturn_regression,
            minimum_agent_text=1000,
        )
        self.save_api_evidence(session, case, items)
        tools = [item for item in items if item.get("role") == "tool"]
        if tools:
            raise ScenarioFailure(f"{case}: no-tool prompt produced tool transcript items")
        summary = {
            "harness": harness,
            "session_id": session,
            "steer_path": steer["path"],
            "steering": delivery["steering"],
            "agent_text_length": len(self.text_by_role(items, "agent")),
            "tool_item_count": len(tools),
            "nonce_count_agent": self.text_by_role(items, "agent").count(nonce),
            "applied_while_running": delivery["applied_while_running"],
            "codex_midturn_regression": midturn_regression,
        }
        self.record(case, "passed", summary)
        return summary

    def wait_file(self, path: Path, case: str) -> None:
        deadline = time.monotonic() + min(self.args.timeout, 180)
        while time.monotonic() < deadline:
            if path.is_file():
                self.record(case, "marker", {"path": path.name, "exists": True})
                return
            time.sleep(0.05)
        raise ScenarioFailure(f"{case}: timed out waiting for slow-hook marker {path.name}")

    def wait_for_agent_text(self, session: str, minimum: int, case: str) -> None:
        deadline = time.monotonic() + min(self.args.timeout, 240)
        while time.monotonic() < deadline:
            row = self.row(self.snapshot(), session) or {}
            text = self.text_by_role(self.history(session), "agent")
            if row.get("chat_phase") == "running" and len(text) >= minimum:
                self.record(case, "base-response-streaming", {
                    "chat_phase": row.get("chat_phase"),
                    "agent_text_length": len(text),
                })
                return
            if row.get("has_error"):
                raise ScenarioFailure(f"{case}: text turn failed before steering: {row!r}")
            time.sleep(0.25)
        raise ScenarioFailure(f"{case}: no long response text appeared while the turn was running")

    def run(self) -> list[dict]:
        self.prepare()
        self.start()
        results = []
        harnesses = [self.args.harness] if self.args.harness != "both" else ["claude", "codex"]
        if self.args.scenario in ("all", "tool-boundary"):
            for harness in harnesses:
                results.append(self.run_tool_boundary(harness))
        if self.args.scenario in ("all", "text-only"):
            for harness in harnesses:
                results.append(self.run_text_only(
                    harness, midturn_regression=(harness == "codex")
                ))
        if self.args.scenario == "codex-midturn":
            results.append(self.run_text_only("codex", midturn_regression=True))
        return results

    def cleanup(self) -> list[str]:
        errors: list[str] = []
        if self.daemon_may_be_running:
            known: set[str] = set(self.sessions)
            try:
                output = self.cli("sessions", "--json", case="cleanup")
                value = json.loads(output)
                rows = value if isinstance(value, list) else value.get("sessions", [])
                for row in rows:
                    session = row.get("id", row.get("session_id"))
                    if isinstance(session, str):
                        known.add(session)
                for session in sorted(known):
                    try:
                        self.cli(
                            "destroy", "--session", session, "--delete-branch", "--json",
                            case="cleanup",
                        )
                    except Exception as error:
                        errors.append(f"destroy {session}: {error}")
                if not errors and known:
                    deadline = time.monotonic() + min(self.args.timeout, 90)
                    remaining = known.copy()
                    while remaining and time.monotonic() < deadline:
                        output = self.cli("sessions", "--json", case="cleanup")
                        value = json.loads(output)
                        rows = value if isinstance(value, list) else value.get("sessions", [])
                        visible = {
                            row.get("id", row.get("session_id"))
                            for row in rows
                            if isinstance(row.get("id", row.get("session_id")), str)
                        }
                        remaining = known & visible
                        if remaining:
                            time.sleep(0.5)
                    if remaining:
                        errors.append(
                            "isolated sessions still exist after destroy: "
                            + ", ".join(sorted(remaining))
                        )
            except Exception as error:
                errors.append(f"enumerate isolated sessions: {error}")
            try:
                self.cli("daemon", "stop", case="cleanup")
            except Exception as error:
                errors.append(f"stop isolated daemon: {error}")
        container_store = self.home / ".local" / "share" / "containers" / "storage"
        if not errors and container_store.exists():
            # All sessions and the isolated daemon have been stopped above.
            # The rootless store lives under the test's private HOME. Its layer
            # files belong to the subordinate uid map, so remove each child in
            # Podman's user namespace; leave the store root for normal teardown.
            mounted = [
                Path(root)
                for root, _, _ in os.walk(container_store, followlinks=False)
                if os.path.ismount(root)
            ]
            if mounted:
                errors.append(
                    "isolated Podman store still has mounted paths: "
                    + ", ".join(str(path) for path in mounted)
                )
            else:
                podman = shutil.which("podman", path=self.env.get("PATH"))
                if not podman:
                    errors.append("isolated rootless Podman store exists but podman is unavailable")
                else:
                    overlay = container_store / "overlay"
                    if overlay.is_dir():
                        children = sorted(overlay.iterdir())
                        if children:
                            try:
                                result = subprocess.run(
                                    [podman, "unshare", "rm", "-rf", "--", *map(str, children)],
                                    env=self.env, capture_output=True, text=True, timeout=90,
                                )
                            except Exception as error:
                                errors.append(f"remove isolated Podman overlay contents: {error}")
                            else:
                                if result.returncode:
                                    errors.append(
                                        "remove isolated Podman overlay contents: "
                                        f"{result.stderr.strip() or result.stdout.strip()}"
                                    )
                        if not errors:
                            try:
                                overlay.rmdir()
                            except OSError as error:
                                errors.append(f"remove isolated Podman overlay directory: {error}")
                    if not errors:
                        self.record("cleanup", "isolated-podman-store-scoped", {
                            "store": str(container_store),
                            "private_test_home": True,
                            "mounted_paths": [],
                            "removed_in_podman_user_namespace": True,
                        })
        if not errors:
            try:
                # Harness/container overlays can contain read-only directories.
                # Destroyed sessions are confirmed absent before making this
                # test-owned runtime removable.
                for root, directories, _ in os.walk(self.runtime, topdown=True, followlinks=False):
                    current = Path(root)
                    for directory in (current, *(current / name for name in directories)):
                        mode = directory.stat(follow_symlinks=False).st_mode
                        if stat.S_ISDIR(mode):
                            directory.chmod(
                                stat.S_IMODE(mode)
                                | stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR
                            )
                shutil.rmtree(self.runtime)
                self.record("cleanup", "isolated-runtime-removed", {
                    "runtime": str(self.runtime),
                })
            except Exception as error:
                errors.append(f"remove isolated runtime: {error}")
        if errors:
            self.record("cleanup", "runtime-retained", {
                "reason": errors,
                "runtime": str(self.runtime),
                "note": "retained because daemon-owned sessions may still be using these files",
            })
        return errors


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-live", action="store_true", required=True,
                        help="confirm real, paid Codex and Claude requests")
    parser.add_argument("--mj", type=Path, required=True, help="mj binary under test")
    parser.add_argument("--worker", type=Path, required=True,
                        help="matching mj-worker binary")
    parser.add_argument("--codex-home", type=Path, default=Path.home() / ".codex",
                        help="source home from which auth.json is copied")
    parser.add_argument("--claude-home", type=Path, default=Path.home() / ".claude",
                        help="source home from which .credentials.json is copied")
    parser.add_argument("--harness", choices=("both", "codex", "claude"), default="both")
    parser.add_argument("--scenario", choices=("all", "tool-boundary", "text-only", "codex-midturn"),
                        default="all")
    parser.add_argument("--instance", help="optional isolated instance label; default is unique")
    parser.add_argument("--artifacts", type=Path, required=True,
                        help="new directory for API transcript and run trace")
    parser.add_argument("--timeout", type=int, default=660,
                        help="per-operation and scenario timeout in seconds")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    for path in (args.mj, args.worker, args.codex_home, args.claude_home):
        if not path.exists():
            raise SystemExit(f"does not exist: {path}")
    if args.scenario == "codex-midturn" and args.harness == "claude":
        raise SystemExit("codex-midturn requires --harness codex or both")
    lab = SteerLab(args)
    error: BaseException | None = None
    results: list[dict] = []
    try:
        results = lab.run()
        lab.record("run", "passed", results)
    except BaseException as caught:
        error = caught
        lab.record("run", "failed", {"error": str(caught)})
    finally:
        cleanup_errors = lab.cleanup()
        if cleanup_errors:
            lab.record("cleanup", "failed", cleanup_errors)
        lab.trace.close()
    if error:
        if isinstance(error, KeyboardInterrupt):
            raise error
        raise error
    if cleanup_errors:
        raise ScenarioFailure("cleanup failed: " + "; ".join(cleanup_errors))
    print(json.dumps({"outcome": "passed", "results": results,
                      "artifacts": str(args.artifacts.resolve())}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
