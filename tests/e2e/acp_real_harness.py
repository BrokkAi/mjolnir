#!/usr/bin/env python3
"""Opt-in ACP acceptance test with a real harness, daemon, and container worker.

Requires Python 3.11+, a signed-in profile, Docker or Podman, and built mj and
portable mj-worker binaries. Uses paid model calls. See --help for invocation.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request


def relay(input_fd: int, output_fd: int) -> None:
    """The disposable consumer owns both adapter pipes; killing it closes them."""
    def copy(source: int, destination: int) -> None:
        while chunk := os.read(source, 65536):
            pending = memoryview(chunk)
            while pending:
                pending = pending[os.write(destination, pending):]

    output = threading.Thread(target=copy, args=(output_fd, 1), daemon=True)
    output.start()
    copy(0, input_fd)
    os.close(input_fd)
    output.join()


class Client:
    def __init__(self, lab: Lab, name: str):
        self.lab = lab
        self.name = name
        self.next_id = 0
        self.messages: queue.Queue = queue.Queue()
        self.updates: list[dict] = []
        self.stopped = False
        self.stderr = (lab.root / f"{name}.stderr").open("w")
        self.adapter = subprocess.Popen(
            lab.command("acp", "--workspace", "acp-test", "--profile", "real",
                        "--target", lab.target, "--bundle", "fixture"),
            env=lab.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.stderr,
        )
        self.consumer = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--relay",
             str(self.adapter.stdin.fileno()), str(self.adapter.stdout.fileno())],
            pass_fds=(self.adapter.stdin.fileno(), self.adapter.stdout.fileno()),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
        )
        # Only the consumer keeps these ends. Its death must actually deliver
        # EOF to mj acp, even though the test supervisor remains alive.
        self.adapter.stdin.close()
        self.adapter.stdout.close()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        lab.clients.append(self)

    def read(self) -> None:
        try:
            for line in self.consumer.stdout:
                message = json.loads(line)
                self.lab.record(self.name, "receive", message)
                self.messages.put(message)
        except Exception as error:
            self.messages.put(error)
        finally:
            self.messages.put(EOFError(f"{self.name}: consumer output closed"))

    def send(self, method: str, params: dict, *, notification: bool = False) -> int:
        self.next_id += 1
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notification:
            message["id"] = self.next_id
        self.lab.record(self.name, "send", message)
        self.consumer.stdin.write((json.dumps(message) + "\n").encode())
        self.consumer.stdin.flush()
        return self.next_id

    def response(self, request_id: int) -> dict:
        deadline = time.monotonic() + self.lab.timeout
        while True:
            message = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            if isinstance(message, Exception):
                raise message
            if message.get("method") == "session/update":
                self.updates.append(message["params"])
                continue
            assert message.get("id") == request_id, message
            assert "error" not in message, message
            return message["result"]

    def new_session(self) -> str:
        initialized = self.response(self.send("initialize", {
            "protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "mj-acp-real-harness-test", "version": "1"},
        }))
        assert initialized["protocolVersion"] == 1, initialized
        result = self.response(self.send("session/new", {
            "cwd": str(self.lab.root), "mcpServers": [],
        }))
        session = result["sessionId"]
        self.lab.sessions.append(session)
        return session

    def prompt(self, session: str, text: str) -> int:
        return self.send("session/prompt", {
            "sessionId": session, "prompt": [{"type": "text", "text": text}],
        })

    def stop(self, *, crash: bool = False) -> None:
        if self.stopped:
            return
        if self.consumer.poll() is None:
            if crash:
                self.consumer.kill()
            else:
                self.consumer.stdin.close()
            self.consumer.wait(timeout=60)
        status = self.adapter.wait(timeout=60)
        self.reader.join(timeout=5)
        self.stderr.close()
        assert status == 0, f"{self.name}: adapter exited {status}; see stderr"
        self.stopped = True
        self.lab.record(self.name, "exit", {"consumer_killed": crash, "adapter_status": status})


class Lab:
    def __init__(self, args: argparse.Namespace):
        self.root = args.artifacts.resolve()
        self.root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.timeout = args.timeout
        self.binary = args.mj.resolve()
        self.target = args.engine
        self.instance = f"acp-real-{os.getpid()}"
        self.sessions: list[str] = []
        self.clients: list[Client] = []
        self.lock = threading.Lock()
        self.trace = (self.root / "transcript.jsonl").open("w", buffering=1)
        self.env = dict(os.environ, MJ_INSTANCE=self.instance,
                        MJ_CONFIG_DIR=str(self.root / "config"),
                        MJ_DATA_DIR=str(self.root / "data"),
                        SESSIONWIKI_DATA=str(self.root / "index"),
                        MJ_WORKER_BINARY=str(args.worker.resolve()),
                        MJOLNIR_NO_UPDATE_CHECK="1")
        self.env.pop("MJ_DAEMON_OWNER_PID", None)
        self.env.pop("MJ_DAEMON_EXIT_WHEN_IDLE", None)
        config = self.root / "config"
        config.mkdir()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        quote = json.dumps
        image_id = subprocess.run(
            [args.engine, "image", "inspect", args.image, "--format", "{{.Id}}"],
            capture_output=True, text=True, check=True, timeout=30,
        ).stdout.strip()
        (config / "config.toml").write_text(f'''version = 13
[phone]
bind = "127.0.0.1:{port}"
tailscale_detect = false
[profiles.real]
kind = {quote(args.harness)}
home = {quote(str(args.profile_home.resolve()))}
[machines.local]
kind = "local"
build_cache = {{ enabled = false }}
[targets.{self.target}]
kind = {quote(args.engine)}
image = {quote(image_id)}
pull_policy = "never"
[bundles.fixture]
primary_repo = "fixture"
repositories = [{{ id = "fixture", github = {quote(args.repository)}, destination = "fixture" }}]
''')
        self.base_url = ""
        self.token = ""
        binaries = {}
        for path in (self.binary, args.worker.resolve()):
            with path.open("rb") as binary:
                binaries[str(path)] = hashlib.file_digest(binary, "sha256").hexdigest()
        self.record("setup", "environment", {
            "instance": self.instance, "binaries": binaries, "harness": args.harness,
            "engine": args.engine, "image": args.image, "image_id": image_id,
            "repository": args.repository,
        })

    def command(self, *args: str) -> list[str]:
        return [str(self.binary), "--instance", self.instance, *args]

    def record(self, case: str, event: str, value: object) -> None:
        with self.lock:
            self.trace.write(json.dumps({"time": time.time(), "case": case,
                                         "event": event, "value": value}) + "\n")

    def cli(self, *args: str, expected_status: int = 0) -> dict:
        result = subprocess.run(self.command(*args), env=self.env, capture_output=True,
                                text=True, timeout=self.timeout)
        self.record("cli", "result", {"args": args, "status": result.returncode,
                                      "stdout": result.stdout, "stderr": result.stderr})
        assert result.returncode == expected_status, result.stderr
        return json.loads(result.stdout) if "--json" in args and result.stdout.strip() else {}

    def get(self, path: str) -> dict:
        request = urllib.request.Request(self.base_url + path,
                                         headers={"Authorization": "Bearer " + self.token})
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    def until(self, description: str, probe):
        deadline = time.monotonic() + self.timeout
        last = None
        while time.monotonic() < deadline:
            last = probe()
            if last:
                self.record("assert", description, last)
                print(description, flush=True)
                return last
            time.sleep(1)
        raise TimeoutError(description)

    def session_when(self, session: str, predicate) -> dict | None:
        value = self.get("/sessions/" + session)
        assert not value["has_error"], value
        assert not value.get("pending_elicitations"), value
        return value if predicate(value) else None

    def idle(self, session: str) -> dict:
        return self.until(f"{session}: idle", lambda: self.session_when(
            session, lambda row: row["is_idle"]))

    def indexed(self, session: str, marker: str) -> None:
        def find():
            page = self.get("/wiki/search?" + urllib.parse.urlencode({"q": marker}))
            return next((row for row in page["rows"]
                         if row["tool"] == "mjolnir" and row.get("hel_session_id") == session), None)
        self.until(f"{session}: searchable for {marker}", find)

    def running_sleep(self, session: str) -> dict | None:
        if not self.session_when(session, lambda row: row["chat_phase"] == "running"):
            return None
        page = self.get(f"/sessions/{session}/transcript?role=tool")
        return next((item for item in page["items"]
                     if item["body"].get("call", {}).get("status") == "in_progress"
                     and "sleep 120" in json.dumps(item["body"])), None)

    def run(self) -> None:
        self.cli("workspaces", "create", "acp-test", "--json")
        info = self.cli("api-info", "--json")
        self.base_url = info["base_url"]
        self.token = Path(info["token_path"]).read_text().strip()
        for case in ("complete", "cancel", "consumer-death"):
            client = Client(self, case)
            session = client.new_session()
            marker = "ACPREAL" + case.replace("-", "").upper() + str(os.getpid())
            if case == "complete":
                prompt = f"Reply with exactly {marker}. Do not run tools or change files."
            else:
                prompt = (f"This is a cancellation test, marker {marker}. Run `sleep 120` "
                          "in the terminal, wait for it to finish, then reply with the marker. "
                          "Do not modify any files or start background tasks.")
            request_id = client.prompt(session, prompt)
            if case == "complete":
                result = client.response(request_id)
                assert result["stopReason"] == "end_turn", result
                assert any(marker in update.get("update", {}).get("content", {}).get("text", "")
                           for update in client.updates), client.updates
                client.stop()
            else:
                self.until(f"{session}: harness running sleep before {case}",
                           lambda: self.running_sleep(session))
                if case == "cancel":
                    client.send("session/cancel", {"sessionId": session}, notification=True)
                    result = client.response(request_id)
                    assert result["stopReason"] == "cancelled", result
                    client.stop()
                else:
                    client.stop(crash=True)
            self.idle(session)
            if case != "complete":
                result = self.cli("wait", "--session", session, "--timeout", "10", "--json",
                                  expected_status=1)
                assert result["outcome"] == "cancelled", result
            listed = self.cli("sessions", "--json")
            assert any(row["id"] == session for row in listed["sessions"]), listed
            self.indexed(session, marker)
            if case == "consumer-death":
                # Keep-on-exit leaves a live session. Exercise an actual
                # checkpoint/release/resume, then prove a new prompt works.
                self.cli("suspend", "--session", session,
                         "--acknowledge-unpublished-work", "--json")
                self.until(f"{session}: suspended", lambda: self.session_when(
                    session, lambda row: row["lifecycle"] == "suspended"))
                self.cli("resume", "--session", session, "--json")
                self.idle(session)
                reply = self.cli("prompt", "--session", session, "--wait", "--json",
                                 "--timeout", str(self.timeout),
                                 f"Reply with exactly {marker}RESUMED. Do not run tools.")
                assert reply["outcome"] == "finished", reply
                assert marker + "RESUMED" in reply["final_message"], reply
            self.record(case, "passed", {"session": session, "marker": marker})

    def cleanup(self) -> None:
        errors = []
        for client in self.clients:
            try:
                client.stop()
            except Exception as error:
                errors.append(str(error))
        try:
            # Enumerate this fresh instance too, covering an accepted creation
            # whose ACP reply never arrived.
            rows = self.cli("sessions", "--json")["sessions"] if self.base_url else []
            for row in rows:
                session = row["id"]
                self.cli("destroy", "--session", session, "--delete-branch", "--json")
            if self.base_url:
                self.until("all test sessions destroyed",
                           lambda: not self.get("/sessions")["sessions"])
        except Exception as error:
            errors.append(str(error))
        try:
            self.cli("daemon", "stop")
        except Exception as error:
            errors.append(str(error))
        if errors:
            raise RuntimeError("cleanup failed: " + "; ".join(errors))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, default=Path("target/debug/mj"))
    parser.add_argument("--worker", type=Path, required=True, help="Portable musl worker binary")
    parser.add_argument("--profile-home", type=Path, required=True, help="Signed-in harness home")
    parser.add_argument("--harness", choices=("codex", "claude", "kimi", "grok", "muse"), default="codex")
    parser.add_argument("--engine", choices=("docker", "podman"), default="docker")
    parser.add_argument("--image", default="ghcr.io/brokkai/mjolnir/agent-dev:latest")
    parser.add_argument("--repository", default="octocat/Hello-World")
    parser.add_argument("--artifacts", type=Path, required=True, help="New directory; never reuse live data")
    parser.add_argument("--timeout", type=int, default=660)
    args = parser.parse_args()
    for path in (args.mj, args.worker, args.profile_home):
        if not path.exists():
            parser.error(f"does not exist: {path}")
    lab = Lab(args)
    print(f"artifacts={lab.root} instance={lab.instance}", flush=True)
    try:
        lab.run()
    except BaseException:
        try:
            lab.cleanup()
        except Exception as error:
            print(error, file=sys.stderr)
        raise
    else:
        lab.cleanup()
    print("PASS: completion, cancellation, consumer death, search, and resume", flush=True)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--relay"]:
        relay(int(sys.argv[2]), int(sys.argv[3]))
    else:
        main()
