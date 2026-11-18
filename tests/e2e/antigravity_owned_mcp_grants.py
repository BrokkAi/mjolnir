#!/usr/bin/env python3
"""Pin check: pinned agy honors Mjolnir's MCP and home-read grants.

In its guardian (Auto) mode agy asks before every MCP tool call, and its
request names the tool `mcp_<server>_<tool>` without the server. The worker
therefore stages `mcp(<server>/*)` grants in agy's project file
(`stage_antigravity_grants` in mj-worker/src/acp/launch.rs). This script
checks against the real agy, in its Default mode, that

  1. with the staged grants, an ACP-registered `mj-agents` server runs its tool
     with no permission request;
  2. a lookalike server `mj-agents_send` whose tool `message` produces the same
     title as `mj-agents`/`send_message` still asks, so the grant follows the
     server and not the title;
  3. without the grants the owned server asks, so (1) proves the file works;
  4. read_file(<home>/) allows nested reads, but outside-home reads, home
     writes, and read-only commands still ask, so the read grant covers no
     write or command. (`command(gh)`, Mjolnir's GitHub CLI approval, is
     staged too but not exercised.)

The golden's @USER_HOME@ is replaced with the home agy sees plus a directory
separator. agy exposes read_file for viewing files; it has no dedicated listing
or grep/search tool. Those operations use run_command.

agy offers Auto mode only to some Google sign-ins, never to the API-key
sign-in below, so the checks run in Default mode. Default reads the same
project grants and asks before every MCP call, command, write, and read outside
them, so each grant shows as a question that does not come. What only Auto
adds, running read-only commands in its sandbox without asking, is agy's own
policy and is not covered here.

The project file is the golden file the Rust test compares the worker's output
with: mj-worker/src/acp/testdata/antigravity_project_grants.json.

No login, quota, or network: agy signs in with its `gemini-api-key` method and
a dummy key, and `GOOGLE_GEMINI_BASE_URL` sends its model calls to a local fake
Gemini API that plays the model. Each turn the fake makes the one function call
the scenario names, then answers DONE, so the permission decisions under test
are agy's alone and do not depend on a model choosing the tool. Opt-in, like
the other tests/e2e scripts; it needs only the pinned agy:

    python3 tests/e2e/antigravity_owned_mcp_grants.py [--agy <path to agy>]
"""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time

REPO = Path(__file__).resolve().parents[2]
GOLDEN = REPO / "mj-worker/src/acp/testdata/antigravity_project_grants.json"
PROJECT_FILE = Path("config/projects/default-cli-project.json")


def stub_server(name: str, tool: str) -> None:
    """A stdio MCP server with one tool, for the agent to call."""
    for line in sys.stdin:
        if not line.strip():
            continue
        message = json.loads(line)
        method, request_id = message.get("method"), message.get("id")
        if request_id is None:
            continue
        if method == "initialize":
            result = {
                "protocolVersion": message["params"].get("protocolVersion", "2025-03-26"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": name, "version": "1"},
            }
        elif method == "tools/list":
            result = {"tools": [{
                "name": tool,
                "description": "Returns PONG. Takes no input.",
                "inputSchema": {"type": "object", "properties": {}},
            }]}
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": f"PONG from {name}"}]}
        else:
            result = {}
        print(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}), flush=True)


class FakeGemini:
    """A local Gemini API whose model makes one scripted function call per turn.

    A request that offers tools gets the scripted call once; the request that
    carries agy's answer to it, and every other request (agy's own small-model
    bookkeeping), gets the text DONE. The answers agy sent back are kept, so a
    scenario can check what a tool returned.
    """

    def __init__(self):
        self.lock = threading.Lock()
        self.pending: tuple[str, dict] | None = None
        self.answers: list[dict] = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args) -> None:
                pass

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length) or b"{}")
                if ":streamGenerateContent" in self.path:
                    data = f"data: {json.dumps(fake.reply(body))}\r\n\r\n".encode()
                    kind = "text/event-stream"
                    status = 200
                elif ":generateContent" in self.path:
                    data, kind, status = json.dumps(fake.reply(body)).encode(), "application/json", 200
                else:
                    data = json.dumps({"error": {"code": 404, "status": "NOT_FOUND",
                                                 "message": f"fake Gemini has no {self.path}"}}).encode()
                    kind, status = "application/json", 404
                self.send_response(status)
                self.send_header("Content-Type", kind)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{server.server_address[1]}"
        threading.Thread(target=server.serve_forever, daemon=True).start()

    def script(self, name: str, args: dict) -> None:
        with self.lock:
            self.pending = (name, args)

    def reply(self, body: dict) -> dict:
        contents = body.get("contents") or [{}]
        answered = [part["functionResponse"] for part in contents[-1].get("parts", [])
                    if "functionResponse" in part]
        with self.lock:
            self.answers.extend(answered)
            if body.get("tools") and self.pending and not answered:
                name, args = self.pending
                self.pending = None
                part = {"functionCall": {"name": name, "args": args}}
            else:
                part = {"text": "DONE"}
        return {
            "candidates": [{"content": {"role": "model", "parts": [part]},
                            "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2},
        }


class Agy:
    """One `agy acp` process that denies every permission request it makes."""

    def __init__(self, agy: str, home: Path, model: FakeGemini, user_home: Path | None = None):
        self.process = subprocess.Popen(
            [agy, "acp", "--gemini-dir", str(home), "--disable-telemetry"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            env={**os.environ, "AGY_CLI_DISABLE_AUTO_UPDATE": "true",
                 "GEMINI_API_KEY": "fake-key", "GOOGLE_GEMINI_BASE_URL": model.url,
                 **({("USERPROFILE" if os.name == "nt" else "HOME"): str(user_home)}
                    if user_home is not None else {})},
        )
        self.inbox: queue.Queue = queue.Queue()
        threading.Thread(target=self.pump, daemon=True).start()
        self.next_id = 0
        self.permission_titles: list[str] = []
        self.completed_tools: list[str] = []
        self.messages: list[dict] = []

    def pump(self) -> None:
        for line in self.process.stdout:
            self.inbox.put(json.loads(line))

    def send(self, message: dict) -> None:
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, method: str, params: dict, timeout: float = 300) -> dict:
        self.next_id += 1
        mine = self.next_id
        self.send({"jsonrpc": "2.0", "id": mine, "method": method, "params": params})
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                message = self.inbox.get(timeout=1)
            except queue.Empty:
                continue
            if message.get("id") == mine and "method" not in message:
                if "error" in message:
                    raise SystemExit(f"{method} failed: {message['error']}")
                return message["result"]
            self.handle(message)
        raise SystemExit(f"timed out waiting for {method}")

    def start(self, workspace: Path, servers: list[dict]) -> str:
        """Sign in with the dummy key and open a Default-mode session."""
        self.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        self.call("authenticate", {"methodId": "gemini-api-key"})
        session = self.call("session/new", {"cwd": str(workspace), "mcpServers": servers})["sessionId"]
        self.call("session/set_mode", {"sessionId": session, "modeId": "default"})
        return session

    def handle(self, message: dict) -> None:
        self.messages.append(message)
        method = message.get("method")
        if method == "session/request_permission":
            params = message["params"]
            self.permission_titles.append(params["toolCall"].get("title", ""))
            reject = next(o for o in params["options"] if o["kind"].startswith("reject"))
            self.send({"jsonrpc": "2.0", "id": message["id"],
                       "result": {"outcome": {"outcome": "selected", "optionId": reject["optionId"]}}})
        elif method == "session/update":
            update = message["params"]["update"]
            if update.get("sessionUpdate") == "tool_call_update" and update.get("status") == "completed":
                self.completed_tools.append(update.get("name", ""))
        elif method and "id" in message:
            self.send({"jsonrpc": "2.0", "id": message["id"],
                       "error": {"code": -32601, "message": "unsupported"}})

    def close(self) -> None:
        self.process.stdin.close()
        self.process.terminate()
        self.process.wait()


def stage_grants(config_home: Path, user_home: Path) -> None:
    project = json.loads(GOLDEN.read_text())
    rules = project["permissionGrants"]["permissionGrants"]["allow"]
    rules[:] = [rule.replace("@USER_HOME@", str(user_home) + os.sep) for rule in rules]
    (config_home / PROJECT_FILE).parent.mkdir(parents=True, exist_ok=True)
    (config_home / PROJECT_FILE).write_text(json.dumps(project))


def run_filesystem_scenario(agy: str, trace: Path | None = None) -> Agy:
    # agy permits temporary storage independently of project grants. Keep the
    # fixture outside it so an ungranted outside-home read really asks.
    with tempfile.TemporaryDirectory(prefix="agy-read-grants-", dir=Path.home()) as scratch:
        scratch = Path(scratch)
        config_home = scratch / "config"
        config_home.mkdir()
        user_home = scratch / "user-home"
        target = user_home / "other-repository/nested/probe.txt"
        target.parent.mkdir(parents=True)
        target.write_text("AGY_HOME_READ_PROBE_7391\n")
        outside = scratch / "user-home-sibling/probe.txt"
        outside.parent.mkdir()
        outside.write_text("OUTSIDE_HOME\n")
        workspace = scratch / "workspace"
        workspace.mkdir()
        stage_grants(config_home, user_home)
        model = FakeGemini()
        client = Agy(agy, config_home, model, user_home)
        try:
            session = client.start(workspace, [])

            def turn(tool: str, args: dict) -> None:
                model.script(tool, args)
                client.call("session/prompt", {"sessionId": session, "prompt": [{
                    "type": "text", "text": f"Use {tool}.",
                }]})

            turn("read_file", {"Path": str(target)})
            assert client.permission_titles == [], client.permission_titles
            assert client.completed_tools == ["read_file"], client.completed_tools
            assert "AGY_HOME_READ_PROBE_7391" in json.dumps(model.answers), model.answers
            print("home read: nested file viewed without permission")
            for label, tool, args in [
                ("outside-home read", "read_file", {"Path": str(outside)}),
                ("home write", "write_file", {"Path": str(user_home / "write.txt"), "Content": "PROBE"}),
            ]:
                permissions_before = len(client.permission_titles)
                turn(tool, args)
                assert len(client.permission_titles) == permissions_before + 1, (label, client.permission_titles)
                assert client.completed_tools == ["read_file"], (label, client.completed_tools)
                print(f"{label}: asked and was denied ({client.permission_titles[-1]})")
            assert not (user_home / "write.txt").exists()
            # Listing and searching the granted directory are commands, which
            # the read grant does not cover.
            for command in [f"ls {target.parent}", f"grep AGY_HOME_READ_PROBE_7391 {target}"]:
                permissions_before = len(client.permission_titles)
                turn("run_command", {"CommandLine": command, "toolSummary": "Read-only command",
                                     "WaitMsBeforeAsync": 5000})
                assert len(client.permission_titles) == permissions_before + 1, client.permission_titles
                assert client.completed_tools == ["read_file"], client.completed_tools
                print(f"read-only command: asked and was denied ({command})")
        finally:
            client.close()
            if trace:
                trace.write_text("".join(json.dumps(m) + "\n" for m in client.messages))
        return client


def run_scenario(agy: str, grants: bool, server: str, tool: str) -> Agy:
    with tempfile.TemporaryDirectory(prefix="agy-grants-") as scratch:
        scratch = Path(scratch)
        home = scratch / "home"
        home.mkdir()
        if grants:
            stage_grants(home, Path.home())
        workspace = scratch / "workspace"
        workspace.mkdir()
        model = FakeGemini()
        client = Agy(agy, home, model)
        try:
            session = client.start(workspace, [{
                "name": server,
                "command": sys.executable,
                "args": [str(Path(__file__).resolve()), "--stub", server, tool],
                "env": [],
            }])
            model.script(f"mcp_{server}_{tool}", {"toolSummary": "Stub call"})
            client.call("session/prompt", {"sessionId": session, "prompt": [{
                "type": "text", "text": f"Call the MCP tool mcp_{server}_{tool} once.",
            }]})
        finally:
            client.close()
        return client


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1] == "--stub":
        stub_server(sys.argv[2], sys.argv[3])
        return
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--agy", default=shutil.which("agy") or str(Path.home() / ".local/bin/agy"))
    parser.add_argument("--trace", type=Path, help="write filesystem ACP evidence as JSONL")
    arguments = parser.parse_args()
    version = subprocess.run([arguments.agy, "--version"], capture_output=True, text=True).stdout.strip()
    print(f"agy {version}")

    run_filesystem_scenario(arguments.agy, arguments.trace)

    owned = run_scenario(arguments.agy, True, "mj-agents", "send_message")
    assert owned.permission_titles == [], f"owned server asked: {owned.permission_titles}"
    assert owned.completed_tools == ["mcp_mj-agents_send_message"], owned.completed_tools
    print("granted owned server: ran with no permission request")

    lookalike = run_scenario(arguments.agy, True, "mj-agents_send", "message")
    assert lookalike.permission_titles == ["Run mcp_mj-agents_send_message"], lookalike.permission_titles
    assert lookalike.completed_tools == [], lookalike.completed_tools
    print("lookalike server with the owned title: asked and was denied")

    ungranted = run_scenario(arguments.agy, False, "mj-agents", "send_message")
    assert ungranted.permission_titles == ["Run mcp_mj-agents_send_message"], ungranted.permission_titles
    print("owned server without the file: asked, so the file is what grants")


if __name__ == "__main__":
    main()
