#!/usr/bin/env python3
"""Pin check: pinned agy trusts Mjolnir's MCP servers through its own grants.

In its guardian (Auto) mode agy asks before every MCP tool call, and its
request names the tool `mcp_<server>_<tool>` without the server. The worker
therefore stages `mcp(<server>/*)` grants in agy's project file
(`stage_antigravity_mcp_grants` in mj-worker/src/acp/launch.rs). This script
checks against the real agy that

  1. with the staged grants, an ACP-registered `mj-agents` server runs its tool
     with no permission request;
  2. a lookalike server `mj-agents_send` whose tool `message` produces the same
     title as `mj-agents`/`send_message` still asks, so the grant follows the
     server and not the title;
  3. without the grants the owned server asks, so (1) proves the file works.

The project file is the golden file the Rust test compares the worker's output
with: mj-worker/src/acp/testdata/antigravity_project_grants.json.

Opt-in, like the other tests/e2e scripts. Each scenario is one small model turn
on a copied login, so it spends a little of the account's quota:

    python3 tests/e2e/antigravity_owned_mcp_grants.py --login <copy of antigravity-oauth-token>

Copy the login; do not point --login at the live ~/.gemini file.
"""

import argparse
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


class Agy:
    """One `agy acp` process that denies every permission request it makes."""

    def __init__(self, agy: str, home: Path):
        self.process = subprocess.Popen(
            [agy, "acp", "--gemini-dir", str(home), "--disable-telemetry"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            env={**os.environ, "AGY_CLI_DISABLE_AUTO_UPDATE": "true"},
        )
        self.inbox: queue.Queue = queue.Queue()
        threading.Thread(target=self.pump, daemon=True).start()
        self.next_id = 0
        self.permission_titles: list[str] = []
        self.completed_tools: list[str] = []

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

    def handle(self, message: dict) -> None:
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


def run_scenario(agy: str, login: Path, grants: bool, server: str, tool: str) -> Agy:
    with tempfile.TemporaryDirectory(prefix="agy-grants-") as scratch:
        scratch = Path(scratch)
        home = scratch / "home"
        (home / "antigravity-cli").mkdir(parents=True)
        shutil.copy(login, home / "antigravity-cli/antigravity-oauth-token")
        if grants:
            (home / PROJECT_FILE).parent.mkdir(parents=True)
            shutil.copy(GOLDEN, home / PROJECT_FILE)
        workspace = scratch / "workspace"
        workspace.mkdir()
        client = Agy(agy, home)
        try:
            client.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
            session = client.call("session/new", {
                "cwd": str(workspace),
                "mcpServers": [{
                    "name": server,
                    "command": sys.executable,
                    "args": [str(Path(__file__).resolve()), "--stub", server, tool],
                    "env": [],
                }],
            })["sessionId"]
            client.call("session/set_mode", {"sessionId": session, "modeId": "auto"})
            client.call("session/prompt", {"sessionId": session, "prompt": [{
                "type": "text",
                "text": f"Call the MCP tool mcp_{server}_{tool} once with no arguments, "
                        "then reply with exactly its result.",
            }]})
        finally:
            client.close()
        return client


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1] == "--stub":
        stub_server(sys.argv[2], sys.argv[3])
        return
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--login", required=True, type=Path,
                        help="a copy of antigravity-oauth-token")
    parser.add_argument("--agy", default=shutil.which("agy") or str(Path.home() / ".local/bin/agy"))
    arguments = parser.parse_args()
    version = subprocess.run([arguments.agy, "--version"], capture_output=True, text=True).stdout.strip()
    print(f"agy {version}")

    owned = run_scenario(arguments.agy, arguments.login, True, "mj-agents", "send_message")
    assert owned.permission_titles == [], f"owned server asked: {owned.permission_titles}"
    assert owned.completed_tools == ["mcp_mj-agents_send_message"], owned.completed_tools
    print("granted owned server: ran with no permission request")

    lookalike = run_scenario(arguments.agy, arguments.login, True, "mj-agents_send", "message")
    assert lookalike.permission_titles == ["Run mcp_mj-agents_send_message"], lookalike.permission_titles
    assert lookalike.completed_tools == [], lookalike.completed_tools
    print("lookalike server with the owned title: asked and was denied")

    ungranted = run_scenario(arguments.agy, arguments.login, False, "mj-agents", "send_message")
    assert ungranted.permission_titles == ["Run mcp_mj-agents_send_message"], ungranted.permission_titles
    print("owned server without the file: asked, so the file is what grants")


if __name__ == "__main__":
    main()
