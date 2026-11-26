#!/usr/bin/env python3
"""Pin check: Mjolnir's DSH home patch disables the pinned DSH's native sub-agent rows.

Mjolnir writes `<dsh-home>/cordis.patch.yml` for every dsh-acp session
(`stage_dsh_home_patch` in mj-worker-staging/src/worker_binary/install.rs) with
the text of `dsh_native_subagents_patch` (mj-core/src/harness_runtime.rs): the
`DSH_NATIVE_SUBAGENT_PLUGIN_ROWS` disabled for a session that must not delegate
natively, or `[]` for one that may. The home patch is DSH's supported
deployment seam and applies after every bundle layer of the `sdk` profile
dsh-acp boots. This script checks against the pinned dsh-acp and its pinned
DSH runtime that

  1. every id in DSH_NATIVE_SUBAGENT_PLUGIN_ROWS exists as a row in the
     composed `sdk` profile (`dsh --profile sdk --dump-config`);
  2. with the exact patch text Mjolnir writes, every row ends up
     `disabled: true`, the dump credits the home file with the change, and DSH
     prints no warning about a patch entry that matches no row;
  3. with `[]` the rows stay enabled;
  4. over ACP through the real dsh-acp, the suppressed session's first model
     request carries no "root agent" identity text and none of the
     send_message/list_agents/wait_agent/subagent/subagent_fork/workflow
     tools, while the `[]` session carries the identity and the tools.

The row ids and the patch text come from mj-core/src/harness_runtime.rs, not
from a copy here, so a Rust-side edit fails this check. No login, quota, or
network: `DEEPSEEK_BASE_URL` points at a local fake Messages API that records
each request and answers one text block, so the composition under test is
DSH's alone. Opt-in, like the other tests/e2e scripts; run it whenever the
dsh-acp pin changes:

    python3 tests/e2e/dsh_native_subagent_patch.py [--dsh-acp <package dir or bin>]
"""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

REPO = Path(__file__).resolve().parents[2]
RUNTIME_RS = REPO / "mj-core/src/harness_runtime.rs"

# The delegation tool names the disabled rows register: send_message,
# list_agents and wait_agent come from tool-subagent-control, subagent from
# tool-subagent, subagent_fork from tool-subagent-fork, workflow from
# tool-workflow.
NATIVE_TOOL_NAMES = {"send_message", "list_agents", "wait_agent", "subagent", "subagent_fork", "workflow"}

ROOT_IDENTITY = 'You are the root agent. Your name is "lead".'


def rust_source() -> str:
    return RUNTIME_RS.read_text()


def pinned_dsh_acp_version(source: str) -> str:
    match = re.search(r'pub const DSH_ACP_VERSION: &str = "([^"]+)";', source)
    if not match:
        raise SystemExit(f"DSH_ACP_VERSION not found in {RUNTIME_RS}")
    return match.group(1)


def native_subagent_rows(source: str) -> list[str]:
    match = re.search(
        r"pub const DSH_NATIVE_SUBAGENT_PLUGIN_ROWS: &\[&str\] = &\[(.*?)\];",
        source, re.S)
    if not match:
        raise SystemExit(f"DSH_NATIVE_SUBAGENT_PLUGIN_ROWS not found in {RUNTIME_RS}")
    rows = re.findall(r'"([^"]+)"', match.group(1))
    if not rows:
        raise SystemExit("DSH_NATIVE_SUBAGENT_PLUGIN_ROWS parsed empty")
    return rows


def unescape_rust_string(literal: str) -> str:
    literal = re.sub(r"\\\n\s*", "", literal)  # line continuation
    return literal.replace("\\n", "\n").replace('\\"', '"').replace("\\\\", "\\")


def mjolnir_patch(source: str, rows: list[str], suppress: bool) -> str:
    """The text `dsh_native_subagents_patch` builds, from the same source."""
    if not suppress:
        match = re.search(r'if !suppress_native_subagents \{\s*return "(.*?)"\.to_owned\(\);', source)
        if not match:
            raise SystemExit("the empty-patch return in dsh_native_subagents_patch was not found")
        return unescape_rust_string(match.group(1))
    match = re.search(r'let mut patch = String::from\(\s*"(.*?)",\s*\);', source, re.S)
    if not match:
        raise SystemExit("the patch header in dsh_native_subagents_patch was not found")
    header = unescape_rust_string(match.group(1))
    format_match = re.search(r'patch\.push_str\(&format!\("(.*?)"\)\);', source)
    if not format_match or format_match.group(1) != "- id: {id}\\n  disabled: true\\n":
        raise SystemExit("the row format in dsh_native_subagents_patch changed")
    return header + "".join(f"- id: {row}\n  disabled: true\n" for row in rows)


def clean_env(**overrides: str) -> dict:
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("DSH_", "DEEPSEEK_"))}
    env.update(overrides)
    return env


def default_dsh_acp(pin: str) -> Path:
    """The npx cache entry for the pinned dsh-acp, if one is installed."""
    for package in sorted(Path.home().glob(".npm/_npx/*/node_modules/@brokkai/dsh-acp")):
        try:
            version = json.loads((package / "package.json").read_text())["version"]
        except (OSError, json.JSONDecodeError, KeyError):
            continue
        if version == pin:
            return package
    raise SystemExit(
        f"no installed @brokkai/dsh-acp matches the pin {pin} under ~/.npm/_npx; "
        f"run `npx -y @brokkai/dsh-acp@{pin} --version` once or pass --dsh-acp")


def resolve_dsh_js(node: str, dsh_acp_bin: Path) -> Path:
    """The pinned DSH entry point, resolved the way dsh-acp's launcher does."""
    script = (
        "const {createRequire}=require('node:module');"
        "console.log(createRequire(process.argv[1]).resolve('@deepseek-ai/dsh/package.json'))"
    )
    result = subprocess.run([node, "-e", script, str(dsh_acp_bin)],
                            capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"could not resolve @deepseek-ai/dsh from {dsh_acp_bin}: {result.stderr.strip()}")
    dsh_js = Path(result.stdout.strip()).parent / "lib/bin.js"
    if not dsh_js.is_file():
        raise SystemExit(f"@deepseek-ai/dsh has no lib/bin.js at {dsh_js}")
    return dsh_js


class CompositionRow:
    def __init__(self):
        self.disabled = False
        self.section = ""


def run_dump(node: str, dsh_js: Path, home: Path) -> dict[str, CompositionRow]:
    """The composed `sdk` profile's rows, from `dsh --dump-config`."""
    result = subprocess.run(
        [node, str(dsh_js), "--profile", "sdk", "--dump-config"],
        capture_output=True, text=True, timeout=180,
        env=clean_env(DSH_HOME=str(home)))
    if result.returncode != 0:
        raise SystemExit(f"dsh --dump-config failed:\n{result.stderr}")
    patch_warning = re.search(r'patch: entry .* not found', result.stderr)
    if patch_warning:
        raise SystemExit(f"DSH warned about the home patch: {patch_warning.group(0)}\n{result.stderr}")
    rows: dict[str, CompositionRow] = {}
    row = None
    section = ""
    for line in result.stdout.splitlines():
        if line.startswith("# == "):
            section = line[len("# == "):]
            row = None
            continue
        id_match = re.match(r"- id: (\S+)\s*$", line)
        if id_match:
            row = CompositionRow()
            row.section = section
            rows.setdefault(id_match.group(1), row)
            continue
        if line and not line.startswith(" "):
            row = None
            continue
        if row is not None and line == "  disabled: true":
            row.disabled = True
    return rows


class FakeMessages:
    """A local DeepSeek Messages API that records requests and ends each turn."""

    def __init__(self):
        self.bodies: list[dict] = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args) -> None:
                pass

            def do_GET(self) -> None:
                data = json.dumps({"data": [{"id": "deepseek-flash"}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length) or b"{}")
                fake.bodies.append(body)
                events = [
                    ("message_start", {"type": "message_start", "message": {
                        "id": "msg_fake", "type": "message", "role": "assistant",
                        "content": [], "model": body.get("model", "deepseek-flash"),
                        "stop_reason": None, "stop_sequence": None,
                        "usage": {"input_tokens": 1, "output_tokens": 1}}}),
                    ("content_block_start", {"type": "content_block_start", "index": 0,
                                             "content_block": {"type": "text", "text": ""}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                             "delta": {"type": "text_delta", "text": "FAKE OK"}}),
                    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                    ("message_delta", {"type": "message_delta",
                                       "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                       "usage": {"output_tokens": 2}}),
                    ("message_stop", {"type": "message_stop"}),
                ]
                data = "".join(f"event: {kind}\ndata: {json.dumps(payload)}\n\n"
                               for kind, payload in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{server.server_address[1]}"
        threading.Thread(target=server.serve_forever, daemon=True).start()


class DshAcp:
    """One dsh-acp ACP process against the fake Messages API."""

    def __init__(self, node: str, dsh_acp_bin: Path, scratch: Path, model: FakeMessages, patch: str):
        workspace = scratch / "workspace"
        workspace.mkdir()
        data_dir = scratch / "data"
        dsh_home = data_dir / "dsh-home"  # the layout Mjolnir stages into
        dsh_home.mkdir(parents=True)
        (dsh_home / "cordis.patch.yml").write_text(patch)
        user_home = scratch / "user-home"
        user_home.mkdir()
        env = clean_env(
            HOME=str(user_home),
            DEEPSEEK_API_KEY="fake-key",
            DEEPSEEK_BASE_URL=model.url,
            DSH_ACP_DATA_DIR=str(data_dir),
            DSH_ACP_DSH_HOME=str(dsh_home),
            DSH_ACP_SYSTEM_PROMPT="",  # dsh_acp_script clears a login-shell value
        )
        self.process = subprocess.Popen(
            [node, str(dsh_acp_bin)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, env=env)
        self.workspace = workspace
        self.inbox: queue.Queue = queue.Queue()
        self.stderr_lines: list[str] = []
        threading.Thread(target=self.pump, daemon=True).start()
        threading.Thread(target=self.err_pump, daemon=True).start()
        self.next_id = 0
        self.permission_titles: list[str] = []

    def pump(self) -> None:
        for line in self.process.stdout:
            self.inbox.put(json.loads(line))

    def err_pump(self) -> None:
        for line in self.process.stderr:
            self.stderr_lines.append(line)

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
                if self.process.poll() is not None:
                    raise SystemExit(f"dsh-acp exited during {method}:\n{self.stderr()}")
                continue
            if message.get("id") == mine and "method" not in message:
                if "error" in message:
                    raise SystemExit(f"{method} failed: {message['error']}")
                return message["result"]
            self.handle(message)
        raise SystemExit(f"timed out waiting for {method}:\n{self.stderr()}")

    def handle(self, message: dict) -> None:
        method = message.get("method")
        if method == "session/request_permission":
            params = message["params"]
            self.permission_titles.append(params["toolCall"].get("title", ""))
            reject = next(o for o in params["options"] if o["kind"].startswith("reject"))
            self.send({"jsonrpc": "2.0", "id": message["id"],
                       "result": {"outcome": {"outcome": "selected", "optionId": reject["optionId"]}}})
        elif method and "id" in message:
            self.send({"jsonrpc": "2.0", "id": message["id"],
                       "error": {"code": -32601, "message": "unsupported"}})

    def stderr(self) -> str:
        return "".join(self.stderr_lines)

    def run_turn(self) -> None:
        """Open a session and run one prompt the fake answers with plain text."""
        self.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        session = self.call("session/new", {"cwd": str(self.workspace), "mcpServers": []})["sessionId"]
        result = self.call("session/prompt", {"sessionId": session, "prompt": [
            {"type": "text", "text": "Say hi."},
        ]})
        if result.get("stopReason") != "end_turn":
            raise SystemExit(f"the turn did not end cleanly: {result}")
        if self.permission_titles:
            raise SystemExit(f"a plain-text turn asked for permission: {self.permission_titles}")
        patch_warning = re.search(r"patch: entry .* not found", self.stderr())
        if patch_warning:
            raise SystemExit(f"DSH warned about the home patch: {patch_warning.group(0)}")

    def close(self) -> None:
        self.process.stdin.close()
        self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def request_text(body: dict) -> str:
    """The decoded system prompt and message text of a Messages request."""
    parts = []
    system = body.get("system")
    if isinstance(system, str):
        parts.append(system)
    for message in body.get("messages", []):
        content = message.get("content")
        if isinstance(content, str):
            parts.append(content)
        else:
            for block in content or []:
                if isinstance(block, dict) and block.get("type") == "text":
                    parts.append(block.get("text", ""))
    return "\n".join(parts)


def run_acp_scenario(node: str, dsh_acp_bin: Path, patch: str) -> tuple[dict, str]:
    with tempfile.TemporaryDirectory(prefix="dsh-native-subagents-") as scratch:
        model = FakeMessages()
        client = DshAcp(node, dsh_acp_bin, Path(scratch), model, patch)
        try:
            client.run_turn()
        finally:
            client.close()
    if not model.bodies:
        raise SystemExit(f"the fake Messages API saw no request:\n{client.stderr()}")
    return model.bodies[0], client.stderr()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--dsh-acp",
                        help="path to the pinned @brokkai/dsh-acp package directory or its bin/dsh-acp.cjs")
    arguments = parser.parse_args()

    node = shutil.which("node")
    if node is None:
        raise SystemExit("dsh-acp needs Node.js on PATH")
    source = rust_source()
    pin = pinned_dsh_acp_version(source)
    rows = native_subagent_rows(source)

    if arguments.dsh_acp:
        candidate = Path(arguments.dsh_acp)
        if candidate.is_dir():
            candidate = candidate / "bin/dsh-acp.cjs"
        dsh_acp_bin = candidate
    else:
        dsh_acp_bin = default_dsh_acp(pin) / "bin/dsh-acp.cjs"
    if not dsh_acp_bin.is_file():
        raise SystemExit(f"dsh-acp launcher not found at {dsh_acp_bin}")
    version = subprocess.run([node, str(dsh_acp_bin), "--version"],
                             capture_output=True, text=True).stdout.strip()
    if version != f"dsh-acp {pin}":
        raise SystemExit(f"{dsh_acp_bin} is {version}, not the pinned dsh-acp {pin}")
    dsh_js = resolve_dsh_js(node, dsh_acp_bin)
    dsh_version = json.loads((dsh_js.parent.parent / "package.json").read_text())["version"]
    print(f"dsh-acp {pin} with @deepseek-ai/dsh {dsh_version} ({dsh_js.parent.parent})")

    suppress_patch = mjolnir_patch(source, rows, True)
    empty_patch = mjolnir_patch(source, rows, False)
    with tempfile.TemporaryDirectory(prefix="dsh-native-subagents-") as scratch:
        scratch = Path(scratch)
        empty_home = scratch / "empty-home"
        empty_home.mkdir()
        (empty_home / "cordis.patch.yml").write_text(empty_patch)
        enabled = run_dump(node, dsh_js, empty_home)
    missing = [row for row in rows if row not in enabled]
    assert not missing, f"rows missing from the pinned sdk composition: {missing}"
    print(f"all {len(rows)} DSH_NATIVE_SUBAGENT_PLUGIN_ROWS ids exist in the pinned sdk composition")
    still_enabled = [row for row in rows if enabled[row].disabled]
    assert not still_enabled, f"rows already disabled without suppression: {still_enabled}"
    print("with `[]`, every native sub-agent row loads enabled")

    with tempfile.TemporaryDirectory(prefix="dsh-native-subagents-") as scratch:
        scratch = Path(scratch)
        suppressed_home = scratch / "suppressed-home"
        suppressed_home.mkdir()
        patch_file = suppressed_home / "cordis.patch.yml"
        patch_file.write_text(suppress_patch)
        suppressed = run_dump(node, dsh_js, suppressed_home)
    not_disabled = [row for row in rows if row not in suppressed or not suppressed[row].disabled]
    assert not not_disabled, f"rows the home patch did not disable: {not_disabled}"
    not_credited = [row for row in rows if str(patch_file) not in suppressed[row].section]
    assert not not_credited, f"rows the dump does not credit to the home patch: {not_credited}"
    print(f"with Mjolnir's patch, every row is disabled: true, credited to {patch_file.name}, with no DSH warning")

    body, _ = run_acp_scenario(node, dsh_acp_bin, empty_patch)
    text = request_text(body)
    tools = {tool["name"] for tool in body.get("tools", [])}
    assert ROOT_IDENTITY in text, f"no root-agent identity in the [] request:\n{text[:2000]}"
    missing_tools = {"send_message", "list_agents", "subagent"} - tools
    assert not missing_tools, f"native delegation tools missing with []: {missing_tools}"
    print("with `[]`, the model request carries the root-agent identity and the native delegation tools")

    body, _ = run_acp_scenario(node, dsh_acp_bin, suppress_patch)
    text = request_text(body)
    tools = {tool["name"] for tool in body.get("tools", [])}
    assert "root agent" not in text, f"root-agent identity survived the patch:\n{text[:2000]}"
    leaked = tools & NATIVE_TOOL_NAMES
    assert not leaked, f"native delegation tools survived the patch: {sorted(leaked)}"
    print("with Mjolnir's patch, the model request has no root-agent identity and no native delegation tools")


if __name__ == "__main__":
    main()
