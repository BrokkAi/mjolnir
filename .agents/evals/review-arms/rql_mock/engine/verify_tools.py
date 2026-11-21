"""Inspect the pinned Claude SDK's API request with a loopback-only network."""
import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Thread

from rql_engine_tools import ENGINE_SETTINGS, ENGINE_TOOLS, STAGED_ENGINE_SETTINGS


RUN_SDK = """
const {createSdkMcpServer, query, tool} = await import(process.argv[1]);
const cwd = process.argv[2];
// Named like mj's server, with the tools the frozen worker pre-approves, so the
// probe checks that the staged allow rules cannot bring them back.
const probe = createSdkMcpServer({
    name: "mj-agents",
    alwaysLoad: true,
    tools: ["send_message", "fetch", "ask_oracle"].map(
        (name) => tool(name, "Local verification tool", {}, async () => ({content: []}))),
});
const session = query({
    prompt: "Read the local sample",
    options: {
        cwd,
        model: "sonnet",
        settingSources: ["user", "project", "local"],
        tools: {type: "preset", preset: "claude_code"},
        disallowedTools: ["Agent", "Task", "ListAgents", "SendMessage", "Workflow"],
        permissionMode: "bypassPermissions",
        allowDangerouslySkipPermissions: true,
        sandbox: {enabled: false},
        mcpServers: {"mj-agents": probe},
        maxTurns: 3,
    },
});
for await (const event of session) {
    if (event.type === "system" && event.subtype === "init") {
        console.log(JSON.stringify({
            tools: event.tools, permissionMode: event.permissionMode, mcpServers: event.mcp_servers,
        }));
    }
}
"""


def probe(sdk):
    requests, results = [], {}

    class Stub(BaseHTTPRequestHandler):
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path.split("?", 1)[0] != "/v1/messages":
                self.send_error(404)
                return

            requests.append(sorted(tool["name"] for tool in body["tools"]))
            for message in body["messages"]:
                for block in message["content"] if isinstance(message["content"], list) else ():
                    if block["type"] == "tool_result":
                        results[block["tool_use_id"]] = bool(block.get("is_error"))
            if results:
                blocks = [{"type": "text", "text": "Probe finished"}]
                stop = "end_turn"
            else:
                blocks = [
                    {"type": "tool_use", "id": tool_id, "name": name, "input": inputs}
                    for tool_id, name, inputs in (
                        ("toolu_read_inside", "Read", {"file_path": str(inside)}),
                        ("toolu_write_inside", "Write", {"file_path": str(inside_write), "content": "control"}),
                        ("toolu_read_outside", "Read", {"file_path": str(outside)}),
                        ("toolu_write_outside", "Write", {"file_path": str(outside_write), "content": "blocked"}),
                    )
                ]
                stop = "tool_use"
            events = [{"type": "message_start", "message": {
                "id": "msg_probe", "type": "message", "role": "assistant", "content": [],
                "model": body["model"], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 1, "output_tokens": 0},
            }}]
            for index, block in enumerate(blocks):
                inputs = block.pop("input", None)
                if inputs is not None:
                    block["input"] = {}
                events.append({"type": "content_block_start", "index": index, "content_block": block})
                if inputs is not None:
                    events.append({"type": "content_block_delta", "index": index, "delta": {
                        "type": "input_json_delta", "partial_json": json.dumps(inputs),
                    }})
                events.append({"type": "content_block_stop", "index": index})
            events += [{"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                        "usage": {"output_tokens": 1}}, {"type": "message_stop"}]
            payload = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload.encode())))
            self.end_headers()
            self.wfile.write(payload.encode())

        def log_message(self, *_):
            pass

    with TemporaryDirectory() as temporary, HTTPServer(("127.0.0.1", 0), Stub) as server:
        root = Path(temporary)
        work = root / "workspace"
        work.mkdir()
        inside = work / "inside.txt"
        outside = root / "outside.txt"
        inside_write = work / "allowed.txt"
        outside_write = root / "blocked.txt"
        inside.write_text("inside", encoding="utf-8")
        outside.write_text("outside", encoding="utf-8")
        config = root / ".claude"
        config.mkdir()
        (config / "settings.json").write_text(json.dumps(STAGED_ENGINE_SETTINGS), encoding="utf-8")
        thread = Thread(target=server.serve_forever)
        thread.start()
        try:
            result = subprocess.run(
                ["node", "--input-type=module", "-e", RUN_SDK, sdk.as_uri(), str(work)],
                env={"PATH": os.environ["PATH"], "HOME": str(root), "CLAUDE_CONFIG_DIR": str(config),
                     "ANTHROPIC_API_KEY": "dummy", "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                     "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"},
                capture_output=True, text=True, timeout=60,
            )
        finally:
            server.shutdown()
            thread.join()
        inside_written = inside_write.is_file() and inside_write.read_text(encoding="utf-8") == "control"
        outside_written = outside_write.exists()
    actual = {"initialization": [json.loads(line) for line in result.stdout.splitlines()],
              "outbound_tools": requests, "tool_errors": results,
              "inside_file_written": inside_written, "outside_file_written": outside_written}
    print(json.dumps(actual, indent=2), flush=True)
    initialized = actual["initialization"]
    expected = sorted(ENGINE_TOOLS)
    connected_mcp = {"name": "mj-agents", "status": "connected", "source": "sdk"}
    if (result.returncode or len(initialized) != 1 or initialized[0]["tools"] != expected
            or initialized[0]["permissionMode"] != "bypassPermissions"
            or connected_mcp not in initialized[0].get("mcpServers", [])
            or not requests or any(tools != expected for tools in requests)):
        raise SystemExit(f"Unexpected tool schema from pinned Claude SDK. {result.stderr[-1200:]}")
    expected_errors = {"toolu_read_inside": False, "toolu_write_inside": False,
                       "toolu_read_outside": True, "toolu_write_outside": True}
    if results != expected_errors or not inside_written or outside_written:
        raise SystemExit("Unexpected result for an inside or outside file tool")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sdk", type=Path, nargs="?", help="Path to the pinned installation's sdk.mjs")
    parser.add_argument("--settings", action="store_true", help="Print the engine-only settings.json")
    args = parser.parse_args()
    if args.settings:
        print(json.dumps(ENGINE_SETTINGS, indent=2))
        return
    if args.sdk is None:
        parser.error("provide the path to sdk.mjs")
    # Run in a fresh network namespace with loopback and no provider route.
    # The child also receives a disposable HOME and dummy credentials.
    interfaces = {line.split(":", 1)[0].strip() for line in Path("/proc/net/dev").read_text().splitlines()[2:]}
    if interfaces == {"lo"}:
        probe(args.sdk.resolve(strict=True))
    else:
        subprocess.run(
            ["bwrap", "--unshare-net", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
             "--tmpfs", "/tmp", sys.executable, "-B", "-m", __spec__.name, str(args.sdk.resolve(strict=True))],
            check=True,
        )


if __name__ == "__main__":
    main()
