#!/usr/bin/env python3
"""Pin check: pinned agy honors Mjolnir's MCP and home-read grants.

In its guardian (Auto) mode agy asks before every MCP tool call, and its
request names the tool `mcp_<server>_<tool>` without the server. The worker
therefore stages `mcp(<server>/*)` grants in agy's project file
(`stage_antigravity_grants` in mj-worker/src/acp/launch.rs). This script
checks against the real agy that

  1. with the staged grants, an ACP-registered `mj-agents` server runs its tool
     with no permission request;
  2. a lookalike server `mj-agents_send` whose tool `message` produces the same
     title as `mj-agents`/`send_message` still asks, so the grant follows the
     server and not the title;
  3. without the grants the owned server asks, so (1) proves the file works;
  4. read_file(<home>/) allows nested reads, but outside-home reads and home
     writes still ask; commands retain agy's existing Auto-mode policy, and
     `command(gh)` is staged there as Mjolnir's GitHub CLI approval.

The golden's @USER_HOME@ is replaced with the home agy sees plus a directory
separator. agy 1.5.8 exposes read_file for viewing files; it has no dedicated
listing or grep/search tool. Those operations use run_command, which Auto mode
can approve independently of our read grant.

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

    def __init__(self, agy: str, home: Path, user_home: Path | None = None):
        self.process = subprocess.Popen(
            [agy, "acp", "--gemini-dir", str(home), "--disable-telemetry"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            env={**os.environ, "AGY_CLI_DISABLE_AUTO_UPDATE": "true",
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


def run_filesystem_scenario(agy: str, login: Path, trace: Path | None = None) -> Agy:
    # agy permits temporary storage independently of project grants. Keep the
    # fixture outside it so an ungranted outside-home read really asks.
    with tempfile.TemporaryDirectory(prefix="agy-read-grants-", dir=Path.home()) as scratch:
        scratch = Path(scratch)
        config_home = scratch / "config"
        (config_home / "antigravity-cli").mkdir(parents=True)
        shutil.copy(login, config_home / "antigravity-cli/antigravity-oauth-token")
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
        client = Agy(agy, config_home, user_home)
        try:
            client.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
            session = client.call("session/new", {"cwd": str(workspace), "mcpServers": []})["sessionId"]
            client.call("session/set_mode", {"sessionId": session, "modeId": "auto"})

            def prompt(text: str) -> None:
                client.call("session/prompt", {"sessionId": session, "prompt": [{
                    "type": "text", "text": text + " Do not retry or use another tool if denied.",
                }]})

            prompt(f"Use read_file to view {target}, then report its contents. Do not use commands.")
            assert client.permission_titles == [], client.permission_titles
            assert client.completed_tools == ["read_file"], client.completed_tools
            text = "".join(m.get("params", {}).get("update", {}).get("content", {}).get("text", "")
                           for m in client.messages)
            assert "AGY_HOME_READ_PROBE_7391" in text, text
            print("home read: nested file viewed without permission")
            for label, text in [
                ("outside-home read", f"Use read_file to read {outside}. Do not use commands."),
                ("home write", f"Use write_file to write PROBE to {user_home}/write.txt. Do not use commands."),
            ]:
                permissions_before = len(client.permission_titles)
                prompt(text)
                assert len(client.permission_titles) == permissions_before + 1, (label, client.permission_titles)
                assert client.completed_tools == ["read_file"], (label, client.completed_tools)
                print(f"{label}: asked and was denied ({client.permission_titles[-1]})")
            assert not (user_home / "write.txt").exists()
            # These read-only commands also run without a home grant in Auto
            # mode. They are not separate filesystem tools or new command grants.
            for command in [f"ls {target.parent}", f"grep AGY_HOME_READ_PROBE_7391 {target}"]:
                permissions_before = len(client.permission_titles)
                completed_before = len(client.completed_tools)
                prompt(f"Use run_command to run {command}.")
                assert len(client.permission_titles) == permissions_before, client.permission_titles
                assert len(client.completed_tools) > completed_before, client.completed_tools
                assert all(tool == "run_command" for tool in client.completed_tools[completed_before:])
                print(f"read-only command: ran under agy's existing Auto policy ({command})")
        finally:
            client.close()
            if trace:
                trace.write_text("".join(json.dumps(m) + "\n" for m in client.messages))
        return client


def run_scenario(agy: str, login: Path, grants: bool, server: str, tool: str) -> Agy:
    with tempfile.TemporaryDirectory(prefix="agy-grants-") as scratch:
        scratch = Path(scratch)
        home = scratch / "home"
        (home / "antigravity-cli").mkdir(parents=True)
        shutil.copy(login, home / "antigravity-cli/antigravity-oauth-token")
        if grants:
            (home / PROJECT_FILE).parent.mkdir(parents=True)
            stage_grants(home, Path.home())
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
    parser.add_argument("--trace", type=Path, help="write filesystem ACP evidence as JSONL")
    arguments = parser.parse_args()
    version = subprocess.run([arguments.agy, "--version"], capture_output=True, text=True).stdout.strip()
    print(f"agy {version}")

    run_filesystem_scenario(arguments.agy, arguments.login, arguments.trace)

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
