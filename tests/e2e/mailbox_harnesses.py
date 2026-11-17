#!/usr/bin/env python3
"""Exercise mid-turn mailboxes with real DSH/OpenCode and a recorded model.

No credentials or paid calls. Runs real workers with isolated named-instance
directories; only the external model's responses are recorded fixtures.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import threading
import time
import uuid


def eventually(label, predicate, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out: {label}")


class RecordingModel(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.server.lock:
            self.server.requests.append(request)
            (self.server.root / "requests.json").write_text(json.dumps(self.server.requests))
            primary = bool(request.get("tools"))
            number = sum(bool(item.get("tools")) for item in self.server.requests)
        tools = request.get("tools", [])
        if primary and number == 1:
            self.server.entered.set()
            assert self.server.release.wait(120), "test did not release the first model call"
            tool = next(tool["function"] for tool in tools
                        if tool["function"]["name"] in ("shell", "bash", "execute_command", "run_command"))
            arguments = {"command": "printf mailbox-tool-finished"}
            if tool["name"] == "run_command":
                arguments = {"command": "printf", "args": ["mailbox-tool-finished"]}
            if "description" in tool["parameters"].get("properties", {}):
                arguments["description"] = "Mailbox test"
            delta = {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "mailbox-shell", "type": "function",
                "function": {"name": tool["name"], "arguments": json.dumps(arguments)},
            }]}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": "MID_TURN_MAILBOX_OK"}
            finish = "stop"
        if request.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            for item in (
                {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                 "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}},
            ):
                item.update(id="recorded-mailbox", object="chat.completion.chunk", created=1,
                            model=request.get("model", "mailbox"))
                self.wfile.write(("data: " + json.dumps(item) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            body = json.dumps({"id": "recorded-mailbox", "object": "chat.completion",
                               "created": 1, "model": request.get("model", "mailbox"),
                               "choices": [{"index": 0, "message": delta, "finish_reason": finish}],
                               "usage": {"prompt_tokens": 100, "completion_tokens": 10}}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)


class Relay:
    def __init__(self, path):
        self.path = path
        self.protocol = 1

    def request(self, method, params=None):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(10)
            connection.connect(str(self.path))
            request = {"request_id": uuid.uuid4().hex, "protocol_version": self.protocol,
                       "request": {"method": method}}
            if params is not None:
                request["request"]["params"] = params
            connection.sendall(json.dumps(request).encode() + b"\n")
            with connection.makefile("rb") as reader:
                response = json.loads(reader.readline())
        self.protocol = response["protocol_version"]
        assert response["result"] == "ok", response
        return response["payload"]

    def submit(self, command_id, method, params):
        return self.request("submit", {"command_id": command_id,
                                       "command": {"type": method, "data": params}})


def run(args, harness):
    root = args.root.resolve() / harness
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    worker_root = root / "worker"
    project = root / "project"
    home = root / "home"
    config = root / "config"
    data = root / "data"
    for directory in (worker_root, project, home, config, data):
        directory.mkdir()
    subprocess.run(["git", "init", "-q", str(project)], check=True)
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "SHELL", "USER", "LOGNAME")
           if key in os.environ}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(config), XDG_DATA_HOME=str(data),
               XDG_STATE_HOME=str(root / "state"), XDG_CACHE_HOME=str(args.cache.resolve()),
               MJ_INSTANCE=args.instance, MJ_CONFIG_DIR=str(root / "mj-config"),
               MJ_DATA_DIR=str(root / "mj-data"), MJOLNIR_NO_UPDATE_CHECK="1")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RecordingModel)
    server.root, server.requests, server.lock = root, [], threading.Lock()
    server.entered, server.release = threading.Event(), threading.Event()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}/v1"
    if harness == "open-code":
        profile = config / "opencode"
        profile.mkdir()
        (profile / "opencode.json").write_text(json.dumps({
            "model": "mailbox/fixture", "snapshots": False,
            "providers": {"mailbox": {
                "name": "Recorded mailbox model", "env": ["MAILBOX_FIXTURE_API_KEY"],
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": base},
                "models": {"fixture": {"name": "Recorded fixture"}},
            }},
        }))
        env["MAILBOX_FIXTURE_API_KEY"] = "not-a-real-credential"
        env["XDG_DATA_HOME"] = str(profile / ".data")
        bridge, bridge_args = str(args.opencode.resolve()), ["acp"]
        marker = "opencode.json"
    else:
        profile = root / "dsh"
        profile.mkdir()
        (profile / "auth.json").write_text("{}")
        env.update(DSH_ACP_DATA_DIR=str(profile), DEEPSEEK_API_KEY="not-a-real-credential",
                   DEEPSEEK_BASE_URL=base, DSH_ACP_REASONING_EFFORT="none")
        bridge, bridge_args, marker = str(args.dsh.resolve()), [], "auth.json"
    launch = {
        "session_id": "mailbox-fixture", "harness": harness,
        "harness_home": str(profile), "authentication_marker": marker,
        "bridge_command": bridge, "bridge_args": bridge_args,
        "environment": env, "target_environment": env,
        "cwd": str(project), "execution_policy": "unconstrained",
        "initial_commands": [{"command_id": "first-prompt", "command": {
            "type": "prompt", "data": {"prompt": [{"type": "text", "text": "Run the recorded tool."}]},
        }}],
    }
    launch_path = worker_root / "launch.json"
    launch_path.write_text(json.dumps(launch))
    log = (root / "worker.log").open("w")
    worker = subprocess.Popen([str(args.worker.resolve()), "worker", "run", "--root",
                               str(worker_root), "--config", str(launch_path)],
                              env=env, stdout=log, stderr=log, start_new_session=True)
    def first_call():
        assert worker.poll() is None, f"worker exited; see {worker_root / 'worker.log'}"
        return server.entered.is_set()

    try:
        eventually("first model call", first_call)
        relay = Relay(worker_root / "control.sock")
        relay.request("hello", {"controller_version": "mailbox-fixture",
                                 "supported": {"min": 1, "max": 4294967295}})
        token = "MAILBOX_1473_" + "x" * (96 * 1024)
        event = {"key": "ready-oracle", "source": "mjolnir", "wake": True,
                 "created_at_ms": int(time.time() * 1000),
                 "body": {"type": "parent_message", "text": token}}
        relay.submit("deliver-oracle", "deliver_mailbox_event", {"event": event})
        before = relay.request("status")["data"]
        assert before["active_prompt"]["command_id"] == "first-prompt", before
        server.release.set()
        eventually("second primary model request", lambda: len([
            request for request in server.requests if request.get("tools")]) >= 2)
        primary = [request for request in server.requests if request.get("tools")]
        serialized = json.dumps(primary[1]["messages"])
        assert serialized.count(token) == 1, f"mailbox missing or duplicated; see {root}"
        assert "mailbox-tool-finished" in serialized, f"tool result missing; see {root}"
        eventually("original prompt completes", lambda:
                   relay.request("status")["data"]["execution"] == "idle")
        state = relay.request("status")["data"]
        assert not state.get("queued_prompts"), state
        assert len([request for request in server.requests if request.get("tools")]) == 2
        print(f"{harness}: mailbox reached request 2 of the original turn exactly once", flush=True)
        (root / "result.json").write_text(json.dumps({"harness": harness, "passed": True,
                                                     "primary_requests": 2, "mailbox_bytes": len(token)}))
    finally:
        server.release.set()
        if worker.poll() is None:
            os.killpg(worker.pid, signal.SIGTERM)
            try:
                worker.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(worker.pid, signal.SIGKILL)
                worker.wait()
        log.close()
        server.shutdown()
        server.server_close()
        thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--opencode", type=Path)
    parser.add_argument("--dsh", type=Path)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default" or not args.instance:
        parser.error("a named isolated instance is required")
    if not (args.opencode or args.dsh):
        parser.error("supply at least one real harness binary")
    if args.opencode:
        run(args, "open-code")
    if args.dsh:
        run(args, "dsh")


if __name__ == "__main__":
    main()
