#!/usr/bin/env python3
"""Replayable, isolated Hel runtime reliability scenarios."""

from __future__ import annotations

import argparse
import contextlib
import http.cookiejar
import json
import os
import pathlib
import pty
import re
import select
import shutil
import signal
import socket
import ssl
import sqlite3
import struct
import subprocess
import sys
import termios
import tempfile
import threading
import time
import unicodedata
import urllib.error
import urllib.request


TIMEOUT = 20.0


def render_terminal(raw: bytes, rows: int = 32, columns: int = 140) -> str:
    """Apply the small ANSI subset emitted by crossterm to a fixed screen."""
    text = raw.decode("utf-8", "replace")
    screen = [[" " for _ in range(columns)] for _ in range(rows)]
    row = 0
    column = 0
    index = 0

    def clear() -> None:
        nonlocal screen, row, column
        screen = [[" " for _ in range(columns)] for _ in range(rows)]
        row = 0
        column = 0

    while index < len(text):
        char = text[index]
        if char == "\x1b":
            if index + 1 >= len(text):
                break
            kind = text[index + 1]
            if kind == "[":
                end = index + 2
                while end < len(text) and not ("@" <= text[end] <= "~"):
                    end += 1
                if end >= len(text):
                    break
                parameters = text[index + 2 : end]
                final = text[end]
                plain = parameters.lstrip("?")
                values = [int(value) if value else 0 for value in plain.split(";")] if plain else []
                if final in ("H", "f"):
                    row = max(0, (values[0] if values and values[0] else 1) - 1)
                    column = max(0, (values[1] if len(values) > 1 and values[1] else 1) - 1)
                elif final == "A":
                    row = max(0, row - (values[0] if values and values[0] else 1))
                elif final == "B":
                    row = min(rows - 1, row + (values[0] if values and values[0] else 1))
                elif final == "C":
                    column = min(columns - 1, column + (values[0] if values and values[0] else 1))
                elif final == "D":
                    column = max(0, column - (values[0] if values and values[0] else 1))
                elif final == "G":
                    column = max(0, (values[0] if values and values[0] else 1) - 1)
                elif final == "J" and (not values or values[0] in (0, 2, 3)):
                    clear()
                elif final == "K":
                    mode = values[0] if values else 0
                    start = 0 if mode in (1, 2) else column
                    stop = columns if mode in (0, 2) else min(columns, column + 1)
                    for position in range(start, stop):
                        screen[min(row, rows - 1)][position] = " "
                elif final == "h" and "1049" in parameters:
                    clear()
                index = end + 1
                continue
            if kind == "]":
                end = index + 2
                while end < len(text) and text[end] != "\x07":
                    if text[end : end + 2] == "\x1b\\":
                        end += 1
                        break
                    end += 1
                index = min(len(text), end + 1)
                continue
            if kind == "c":
                clear()
            index += 2
            continue
        if char == "\r":
            column = 0
        elif char == "\n":
            row = min(rows - 1, row + 1)
        elif char == "\b":
            column = max(0, column - 1)
        elif char >= " " and char != "\x7f":
            if row < rows and column < columns:
                screen[row][column] = char
            width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
            column = min(columns - 1, column + width)
        index += 1
    return "\n".join("".join(line).rstrip() for line in screen)


class ScenarioFailure(RuntimeError):
    pass


class PtyClient:
    def __init__(self, name: str, command: list[str], env: dict[str, str], capture: pathlib.Path):
        master, slave = pty.openpty()
        os.set_blocking(master, False)
        os.set_blocking(slave, True)
        os.write(slave, b"")
        import fcntl

        # The screen reconstruction in `text` has to replay the byte stream at
        # the size the program was drawing for, so the size travels with the
        # client rather than being assumed by the renderer.
        self.rows = 32
        self.columns = 140
        fcntl.ioctl(
            slave, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.columns, 0, 0)
        )

        def own_terminal() -> None:
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.name = name
        self.master = master
        self.capture_path = capture
        self.capture = capture.open("wb")
        self.output = bytearray()
        self.lock = threading.Lock()
        self.process = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            close_fds=True,
            preexec_fn=own_terminal,
        )
        os.close(slave)
        self.reader = threading.Thread(target=self._drain, name=f"{name}-pty-reader", daemon=True)
        self.reader.start()

    def _drain(self) -> None:
        while True:
            ready, _, _ = select.select([self.master], [], [], 0.1)
            if not ready:
                if self.process.poll() is not None:
                    ready, _, _ = select.select([self.master], [], [], 0)
                    if not ready:
                        break
                continue
            try:
                chunk = os.read(self.master, 65536)
            except BlockingIOError:
                continue
            except OSError:
                break
            if not chunk:
                break
            with self.lock:
                self.output.extend(chunk)
            self.capture.write(chunk)
            self.capture.flush()

    def text(self) -> str:
        with self.lock:
            raw = bytes(self.output)
        return render_terminal(raw, self.rows, self.columns)

    def wait_for(self, marker: str, timeout: float = TIMEOUT) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if marker in self.text():
                return
            if self.process.poll() is not None:
                raise ScenarioFailure(
                    f"{self.name} exited before displaying {marker!r}: {self.text()[-4000:]}"
                )
            time.sleep(0.05)
        raise ScenarioFailure(
            f"{self.name} did not display {marker!r}: {self.text()[-4000:]}"
        )

    def send(self, data: bytes) -> None:
        os.write(self.master, data)

    def resize(self, rows: int, columns: int) -> None:
        import fcntl

        self.rows = rows
        self.columns = columns
        fcntl.ioctl(
            self.master,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", rows, columns, 0, 0),
        )

    def quit(self) -> float:
        started = time.monotonic()
        self.send(b"\x1bq")
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired as error:
            raise ScenarioFailure(f"{self.name} did not quit within two seconds") from error
        elapsed = time.monotonic() - started
        self.reader.join(timeout=1)
        self.capture.close()
        with contextlib.suppress(OSError):
            os.close(self.master)
        if self.process.returncode != 0:
            raise ScenarioFailure(f"{self.name} exited with {self.process.returncode}")
        return elapsed

    def terminate(self) -> None:
        if self.process.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=2)
        self.reader.join(timeout=1)
        if not self.capture.closed:
            self.capture.close()
        with contextlib.suppress(OSError):
            os.close(self.master)


class Lab:
    def __init__(self, hel: pathlib.Path, scenario: str, seed: int):
        self.hel = hel.resolve()
        self.scenario = scenario
        self.seed = seed
        self.repo_root = pathlib.Path(__file__).resolve().parents[2]
        artifact_parent = self.repo_root / "target" / "reliability-artifacts"
        artifact_parent.mkdir(parents=True, exist_ok=True)
        stamp = f"{scenario}-seed-{seed}-{os.getpid()}"
        self.root = artifact_parent / stamp
        self.root.mkdir(mode=0o700)
        self.runtime_root = pathlib.Path(tempfile.mkdtemp(prefix=f"hel-r-{os.getpid()}-"))
        self.config = self.runtime_root / "config"
        self.data = self.runtime_root / "data"
        self.profile = self.runtime_root / "profile"
        self.project = self.runtime_root / "project"
        self.hooks = self.runtime_root / "hooks"
        for directory in [self.config, self.data, self.profile, self.project, self.hooks]:
            directory.mkdir()
        self.hook_name: str | None = None
        self.trace_path = self.root / "trace.json"
        self.trace: dict[str, object] = {
            "format_version": 1,
            "commit": self.git_output(["rev-parse", "HEAD"]).strip(),
            "scenario": scenario,
            "seed": seed,
            "started_at": self.timestamp(),
            "finished_at": None,
            "actions": [],
            "process_events": [],
            "revisions": {"web": [], "tui-1": [], "tui-2": []},
            "outcome": "running",
            "artifacts": {
                "trace": "trace.json",
                "integrity": "integrity.txt",
                "tui_1": "tui-1.capture",
                "tui_2": "tui-2.capture",
                "browser_transcript": "browser-transcript.json",
                "controller_log": "controller.log",
                "daemon_log": "daemon.log",
                "process_tree": "process-tree.txt",
            },
        }
        self.clients: list[PtyClient] = []
        self.daemon_pid: int | None = None
        self.cookie_jar = http.cookiejar.CookieJar()
        self.http = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.cookie_jar))
        self.base_url = ""
        self.write_trace()

    @staticmethod
    def timestamp() -> str:
        return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    def git_output(self, args: list[str], cwd: pathlib.Path | None = None) -> str:
        result = subprocess.run(
            ["git", *args],
            cwd=cwd or self.repo_root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        return result.stdout

    def record_action(self, action: str, **details: object) -> None:
        actions = self.trace["actions"]
        assert isinstance(actions, list)
        actions.append({"at": self.timestamp(), "action": action, **details})
        self.write_trace()

    def record_process(self, event: str, name: str, pid: int) -> None:
        events = self.trace["process_events"]
        assert isinstance(events, list)
        events.append({"at": self.timestamp(), "event": event, "name": name, "pid": pid})
        self.write_trace()

    def write_trace(self) -> None:
        temporary = self.trace_path.with_suffix(".json.new")
        temporary.write_text(json.dumps(self.trace, indent=2, sort_keys=True) + "\n")
        temporary.replace(self.trace_path)

    def environment(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "MJ_CONFIG_DIR": str(self.config),
                "MJ_DATA_DIR": str(self.data),
                "MJ_CHAOS_ISOLATED": "1",
                "RUST_LOG": "hel=debug,hel_cli=debug",
            }
        )
        env.pop("MJ_DAEMON_EXIT_WHEN_IDLE", None)
        if self.hook_name is not None:
            env["MJ_TEST_HOOK"] = self.hook_name
            env["MJ_TEST_HOOK_DIR"] = str(self.hooks)
        return env

    @staticmethod
    def free_port() -> int:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            return int(listener.getsockname()[1])

    def prepare(self, *, phone_tls: bool = False, fake_acp_delay_ms: int = 0) -> int:
        self.git_output(["init", "--initial-branch=main"], self.project)
        self.git_output(["config", "user.name", "Hel Reliability"], self.project)
        self.git_output(["config", "user.email", "reliability@invalid"], self.project)
        (self.project / "README.md").write_text("isolated Hel reliability repository\n")
        self.git_output(["add", "README.md"], self.project)
        self.git_output(["commit", "-m", "initial fixture"], self.project)

        bridge = self.runtime_root / "fake_acp.py"
        fixture_bin = self.runtime_root / "bin"
        fixture_bin.mkdir()
        (fixture_bin / "python3").symlink_to(sys.executable)
        bridge.write_text(
            """#!/usr/bin/env python3
import json
import os
import select
import sys
import time

session_id = "reliability-native"
log_path = os.environ["MJ_FAKE_ACP_LOG"]

def send(payload):
    sys.stdout.write(json.dumps(payload, separators=(",", ":")) + "\\n")
    sys.stdout.flush()

def log(payload):
    with open(log_path, "a", encoding="utf-8") as output:
        output.write(json.dumps(payload, sort_keys=True) + "\\n")

def delay():
    delay_ms = int(os.environ.get("MJ_FAKE_ACP_DELAY_MS", "0"))
    if delay_ms > 0:
        time.sleep(delay_ms / 1000)

def wait_for_prompt_cancel():
    delay_ms = int(os.environ.get("MJ_FAKE_ACP_DELAY_MS", "0"))
    if delay_ms <= 0:
        return False
    ready, _, _ = select.select([sys.stdin], [], [], delay_ms / 1000)
    if not ready:
        return False
    cancellation = json.loads(sys.stdin.readline())
    log(cancellation)
    if cancellation.get("method") != "session/cancel":
        raise RuntimeError("expected session/cancel while prompt was delayed")
    return True

for line in sys.stdin:
    message = json.loads(line)
    log(message)
    method = message.get("method")
    ident = message.get("id")
    if method == "initialize":
        result = {"protocolVersion": 1}
    elif method in ("session/new", "session/load"):
        delay()
        rollout_dir = os.path.join(os.environ["CODEX_HOME"], "sessions", "2026", "08", "30")
        os.makedirs(rollout_dir, exist_ok=True)
        rollout = os.path.join(rollout_dir, "rollout-" + session_id + ".jsonl")
        with open(rollout, "a", encoding="utf-8") as output:
            output.write(json.dumps({
                "type": "session_meta",
                "payload": {"session_id": session_id},
            }, separators=(",", ":")) + "\\n")
        result = {"sessionId": session_id}
    elif method == "session/prompt":
        blocks = message.get("params", {}).get("prompt", [])
        text = " ".join(block.get("text", "") for block in blocks if block.get("type") == "text")
        memory_end = "</mj-project-memory>"
        if memory_end in text:
            text = text.rsplit(memory_end, 1)[1].strip()
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": "checking deterministic fixture"},
                },
            },
        })
        if wait_for_prompt_cancel():
            result = {"stopReason": "cancelled"}
        else:
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "reliability reply: " + text},
                    },
                },
            })
            result = {"stopReason": "end_turn"}
    elif ident is None:
        continue
    else:
        result = {}
    send({"jsonrpc": "2.0", "id": ident, "result": result})
"""
        )
        bridge.chmod(0o700)
        port = self.free_port()
        tls_config = ""
        if phone_tls:
            certificate = self.runtime_root / "viewer-cert.pem"
            private_key = self.runtime_root / "viewer-key.pem"
            result = subprocess.run(
                [
                    "openssl",
                    "req",
                    "-x509",
                    "-newkey",
                    "rsa:2048",
                    "-nodes",
                    "-keyout",
                    str(private_key),
                    "-out",
                    str(certificate),
                    "-days",
                    "1",
                    "-subj",
                    "/CN=127.0.0.1",
                    "-addext",
                    "subjectAltName=IP:127.0.0.1",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                raise ScenarioFailure(f"create browser TLS fixture: {result.stderr.strip()}")
            private_key.chmod(0o600)
            tls_context = ssl.create_default_context()
            tls_context.check_hostname = False
            tls_context.verify_mode = ssl.CERT_NONE
            self.http = urllib.request.build_opener(
                urllib.request.HTTPCookieProcessor(self.cookie_jar),
                urllib.request.HTTPSHandler(context=tls_context),
            )
            tls_config = (
                f"tls_cert = {json.dumps(str(certificate))}\n"
                f"tls_key = {json.dumps(str(private_key))}\n"
            )
        config = f'''version = 1

[phone]
enabled = true
bind = "127.0.0.1:{port}"
tailscale_detect = false
{tls_config}

[profiles.fake]
kind = "codex"
home = {json.dumps(str(self.profile))}
executable = {json.dumps(str(bridge))}
environment = {{ MJ_FAKE_ACP_LOG = {json.dumps(str(self.runtime_root / "fake-acp.log"))}, MJ_FAKE_ACP_DELAY_MS = {json.dumps(str(fake_acp_delay_ms))}, PATH = {json.dumps(str(fixture_bin))} }}

[bundles.fixture]
primary_repo = "fixture"

[[bundles.fixture.repositories]]
id = "fixture"
local = {json.dumps(str(self.project))}
destination = "fixture"

[targets.localhost]
kind = "local-bare"
'''
        (self.config / "config.toml").write_text(config)
        self.record_action("prepared", port=port)
        return port

    def command(self, *args: str, timeout: float = TIMEOUT, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [str(self.hel), *args],
            env=self.environment(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
        if check and result.returncode != 0:
            raise ScenarioFailure(
                f"Hel {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}"
            )
        return result

    def start_tui(self, name: str) -> PtyClient:
        client = PtyClient(
            name,
            [str(self.hel)],
            self.environment(),
            self.root / f"{name}.capture",
        )
        self.clients.append(client)
        self.record_process("started", name, client.process.pid)
        return client

    def wait_daemon_status(self, port: int) -> tuple[str, int]:
        deadline = time.monotonic() + TIMEOUT
        last = ""
        while time.monotonic() < deadline:
            result = self.command("daemon", "status", timeout=3, check=False)
            last = result.stdout + result.stderr
            match = re.search(
                rf"web viewer (?:http|https)://127\.0\.0\.1:{port}; viewer code ([0-9]{{6}})",
                result.stdout,
            )
            if result.returncode == 0 and match:
                metadata = json.loads((self.data / "daemon.json").read_text())
                self.daemon_pid = int(metadata["pid"])
                self.record_process("observed", "daemon", self.daemon_pid)
                return match.group(1), self.daemon_pid
            time.sleep(0.1)
        raise ScenarioFailure(f"daemon/web viewer did not become ready: {last[-4000:]}")

    def daemon_request(self, action: dict[str, object], request_id: int = 99) -> object:
        metadata = json.loads((self.data / "daemon.json").read_text())
        envelope = {
            "protocol_version": metadata["protocol_version"],
            "request_id": request_id,
            "token": metadata["token"],
            "action": action,
        }
        body = json.dumps(envelope, separators=(",", ":")).encode()
        host, port_text = str(metadata["address"]).rsplit(":", 1)
        with socket.create_connection((host, int(port_text)), timeout=5) as stream:
            stream.sendall(struct.pack(">I", len(body)) + body)
            header = stream.recv(4)
            if len(header) != 4:
                raise ScenarioFailure("daemon closed before its response frame")
            remaining = struct.unpack(">I", header)[0]
            chunks = bytearray()
            while len(chunks) < remaining:
                chunk = stream.recv(remaining - len(chunks))
                if not chunk:
                    raise ScenarioFailure("daemon response frame was truncated")
                chunks.extend(chunk)
        response = json.loads(chunks)
        result = response.get("result")
        if not isinstance(result, dict) or "Ok" not in result:
            raise ScenarioFailure(f"daemon action failed: {response!r}")
        return result["Ok"]

    def request(self, method: str, path: str, body: object | None = None) -> tuple[int, object | None]:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.base_url + path,
            data=data,
            method=method,
            headers={"Content-Type": "application/json"},
        )
        try:
            with self.http.open(request, timeout=5) as response:
                payload = response.read()
                value = json.loads(payload) if payload else None
                return response.status, value
        except urllib.error.HTTPError as error:
            payload = error.read()
            try:
                value = json.loads(payload) if payload else None
            except json.JSONDecodeError:
                value = payload.decode("utf-8", "replace")
            return error.code, value

    def snapshot(self) -> dict[str, object]:
        status, value = self.request("GET", "/api/snapshot")
        if status != 200 or not isinstance(value, dict):
            raise ScenarioFailure(f"invalid snapshot response: {status} {value!r}")
        revision = value.get("revision")
        if not isinstance(revision, int):
            raise ScenarioFailure(f"snapshot has invalid revision: {revision!r}")
        revisions = self.trace["revisions"]
        assert isinstance(revisions, dict)
        web = revisions["web"]
        assert isinstance(web, list)
        if web and revision < web[-1]:
            raise ScenarioFailure(f"web revision moved backwards from {web[-1]} to {revision}")
        if not web or revision != web[-1]:
            web.append(revision)
            self.write_trace()
        return value

    def record_client_revision(self, client: str, revision: int) -> None:
        revisions = self.trace["revisions"]
        assert isinstance(revisions, dict)
        observed = revisions[client]
        assert isinstance(observed, list)
        if observed and revision < observed[-1]:
            raise ScenarioFailure(
                f"{client} convergence revision moved backwards from {observed[-1]} to {revision}"
            )
        if not observed or observed[-1] != revision:
            observed.append(revision)
            self.write_trace()

    def wait_snapshot(self, predicate, description: str) -> dict[str, object]:
        deadline = time.monotonic() + TIMEOUT
        last: dict[str, object] = {}
        while time.monotonic() < deadline:
            last = self.snapshot()
            if predicate(last):
                return last
            time.sleep(0.1)
        raise ScenarioFailure(f"timed out waiting for {description}: {json.dumps(last)[:4000]}")

    @staticmethod
    def session(snapshot: dict[str, object], session_id: str) -> dict[str, object] | None:
        sessions = snapshot.get("sessions", [])
        assert isinstance(sessions, list)
        return next((item for item in sessions if item.get("id") == session_id), None)

    def owned_pids(self) -> list[int]:
        owned: list[int] = []
        expected = {
            f"MJ_CONFIG_DIR={self.config}".encode(),
            f"MJ_DATA_DIR={self.data}".encode(),
        }
        for entry in pathlib.Path("/proc").iterdir():
            if not entry.name.isdigit() or int(entry.name) == os.getpid():
                continue
            try:
                environment = set((entry / "environ").read_bytes().split(b"\0"))
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                continue
            if expected.issubset(environment):
                owned.append(int(entry.name))
        return sorted(owned)

    def capture_process_tree(self) -> None:
        result = subprocess.run(
            ["ps", "-eo", "pid=,ppid=,pgid=,sid=,stat=,etimes=,args="],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        (self.root / "process-tree.txt").write_text(result.stdout)

    def preserve_runtime(self) -> None:
        destination = self.root / "runtime"
        if self.runtime_root.exists():
            def ignore_sockets(directory: str, names: list[str]) -> list[str]:
                return [
                    name
                    for name in names
                    if pathlib.Path(directory, name).is_socket()
                ]

            shutil.copytree(
                self.runtime_root,
                destination,
                dirs_exist_ok=True,
                ignore=ignore_sockets,
            )
        daemon_log = self.data / "daemon.log"
        if daemon_log.exists():
            shutil.copy2(daemon_log, self.root / "daemon.log")
        logs = self.data / "logs"
        if logs.is_dir():
            for candidate in logs.glob("*.log"):
                with contextlib.suppress(OSError, UnicodeError):
                    if 'command="daemon-run"' in candidate.read_text():
                        shutil.copy2(candidate, self.root / "controller.log")
                        break

    def remove_runtime(self) -> None:
        if self.runtime_root.exists():
            shutil.rmtree(self.runtime_root)

    def integrity(self) -> None:
        database = self.data / "mj.sqlite3"
        connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
        try:
            result = connection.execute("PRAGMA integrity_check").fetchall()
            foreign_keys = connection.execute("PRAGMA foreign_key_check").fetchall()
        finally:
            connection.close()
        body = f"integrity_check={result!r}\nforeign_key_check={foreign_keys!r}\n"
        (self.root / "integrity.txt").write_text(body)
        if result != [("ok",)] or foreign_keys:
            raise ScenarioFailure(f"SQLite integrity failed: {body}")

    def stop_daemon(self) -> None:
        if self.daemon_pid is None:
            return
        self.command("daemon", "stop", check=False)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if not pathlib.Path(f"/proc/{self.daemon_pid}").exists():
                self.record_process("stopped", "daemon", self.daemon_pid)
                return
            time.sleep(0.05)
        raise ScenarioFailure(f"daemon {self.daemon_pid} did not stop within five seconds")

    def cleanup_owned(self) -> None:
        for client in self.clients:
            client.terminate()
        pids = self.owned_pids()
        for pid in pids:
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + 2
        while pids and time.monotonic() < deadline:
            pids = [pid for pid in pids if pathlib.Path(f"/proc/{pid}").exists()]
            time.sleep(0.05)
        for pid in pids:
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.kill(pid, signal.SIGKILL)

    def run(self) -> None:
        port = self.prepare()
        first = self.start_tui("tui-1")
        first.wait_for("Workspaces")
        first.send(b"\r\r")
        first.wait_for("Sessions")
        code, _ = self.wait_daemon_status(port)
        self.base_url = f"http://127.0.0.1:{port}"
        status, _ = self.request("POST", "/auth/session", {"code": code})
        if status != 204:
            raise ScenarioFailure(f"web login returned {status}")
        self.record_action("web-login")

        second = self.start_tui("tui-2")
        second.wait_for("Workspaces")
        second.send(b"\r")
        second.wait_for("Sessions")
        deadline = time.monotonic() + 5
        attached = 0
        while time.monotonic() < deadline:
            status_text = self.command("daemon", "status").stdout
            match = re.search(r"; ([0-9]+) attached clients?;", status_text)
            attached = int(match.group(1)) if match else 0
            if attached >= 2:
                break
            time.sleep(0.1)
        if attached < 2:
            raise ScenarioFailure(f"daemon saw only {attached} attached dashboard clients")

        snapshot = self.snapshot()
        workspaces = snapshot.get("workspaces", [])
        if not isinstance(workspaces, list) or len(workspaces) != 1:
            raise ScenarioFailure(f"expected one workspace: {workspaces!r}")
        title = f"reliability-{self.seed}"
        action = {
            "action": "new",
            "workspace_id": workspaces[0]["id"],
            "profile_id": "fake",
            "bundle_id": "fixture",
            "target_id": "localhost",
            "title": title,
            "project_directory": str(self.project),
        }
        status, _ = self.request("POST", "/api/actions", action)
        if status != 202:
            raise ScenarioFailure(f"new action returned {status}")
        self.record_action("new-session", title=title)
        snapshot = self.wait_snapshot(
            lambda value: any(
                item.get("title") == title and item.get("state") == "running" and not item.get("has_error")
                for item in value.get("sessions", [])
            ),
            "running session",
        )
        session = next(item for item in snapshot["sessions"] if item["title"] == title)
        session_id = str(session["id"])
        first.wait_for(title)
        second.wait_for(title)
        revision = int(snapshot["revision"])
        self.record_client_revision("tui-1", revision)
        self.record_client_revision("tui-2", revision)

        prompt = f"prompt seed={self.seed}"
        status, _ = self.request(
            "POST",
            "/api/actions",
            {"action": "prompt", "session_id": session_id, "text": prompt},
        )
        if status != 202:
            raise ScenarioFailure(f"prompt action returned {status}")
        self.record_action("prompt", session_id=session_id, text=prompt)
        reply = f"reliability reply: {prompt}"

        transcript: dict[str, object] = {}
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            self.snapshot()
            status, value = self.request("GET", f"/api/conversations/{session_id}")
            if status == 200 and isinstance(value, dict):
                transcript = value
                lines = [line for entry in value.get("entries", []) for line in entry.get("lines", [])]
                if lines.count(prompt) == 1 and lines.count(reply) == 1:
                    break
            time.sleep(0.1)
        else:
            raise ScenarioFailure(f"transcript did not converge exactly once: {transcript!r}")
        (self.root / "browser-transcript.json").write_text(
            json.dumps(transcript, indent=2, sort_keys=True) + "\n"
        )
        first.wait_for(reply)
        second.wait_for(reply)
        converged = self.snapshot()
        revision = int(converged["revision"])
        self.record_client_revision("tui-1", revision)
        self.record_client_revision("tui-2", revision)
        self.record_action("clients-converged", session_id=session_id, clients=3)

        status, _ = self.request(
            "POST", "/api/actions", {"action": "close", "session_id": session_id}
        )
        if status != 202:
            raise ScenarioFailure(f"close action returned {status}")
        self.record_action("close", session_id=session_id)
        self.wait_snapshot(
            lambda value: (self.session(value, session_id) or {}).get("state") == "stopped",
            "stopped session",
        )

        quit_one = first.quit()
        self.record_process("stopped", "tui-1", first.process.pid)
        quit_two = second.quit()
        self.record_process("stopped", "tui-2", second.process.pid)
        self.record_action("dashboards-quit", tui_1_seconds=quit_one, tui_2_seconds=quit_two)
        self.stop_daemon()
        self.integrity()
        leaks = self.owned_pids()
        if leaks:
            raise ScenarioFailure(f"owned processes remained after cleanup: {leaks}")
        self.capture_process_tree()
        self.trace["finished_at"] = self.timestamp()
        self.trace["outcome"] = "passed"
        self.write_trace()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument("--hel", required=True, type=pathlib.Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    lab = Lab(args.hel, args.scenario, args.seed)
    print(f"reliability: artifacts={lab.root}", flush=True)
    try:
        lab.run()
    except BaseException as error:
        lab.capture_process_tree()
        lab.trace["finished_at"] = lab.timestamp()
        lab.trace["outcome"] = "failed"
        lab.trace["failure"] = str(error)
        lab.write_trace()
        print(f"reliability: failed: {error}", file=sys.stderr)
        print(
            f"replay: tests/e2e/run-reliability.sh --scenario {args.scenario} "
            f"--seed {args.seed} {args.hel}",
            file=sys.stderr,
        )
        lab.cleanup_owned()
        with contextlib.suppress(Exception):
            lab.integrity()
        lab.preserve_runtime()
        lab.remove_runtime()
        return 1
    finally:
        lab.capture_process_tree()
    lab.preserve_runtime()
    lab.remove_runtime()
    print(
        f"reliability: passed scenario={args.scenario} seed={args.seed} clients=3 leaks=0",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
