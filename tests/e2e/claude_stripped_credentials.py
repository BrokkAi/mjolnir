#!/usr/bin/env python3
"""Pin check: pinned Claude Code and claude-agent-acp with a refresh-token-less login.

The daemon is the only refresher of a Claude grant (#1490): every credentials
file handed to a session carries the access token but no `refreshToken` key,
and the daemon replaces the file when it refreshes the host grant. That design
depends on three behaviours of the pinned harness, which this script verifies
against the real pinned binaries so a pin bump that changes one fails here:

  1. With no `refreshToken`, a login that expires in under five minutes causes
     no token-endpoint attempt and no rewrite of the file. A control run with a
     `refreshToken` must attempt the endpoint, or the observation proves nothing.
  2. A server 401 ends the turn with an error but not the process, writes
     nothing, and the next turn recovers once the file is replaced.
  3. A file replaced by atomic rename while the process runs is used by the next
     API request, directly and through a long-lived claude-agent-acp session.

On macOS the same checks also cover the Keychain (#1491): Claude Code looks a
login up in the Keychain item named for its CLAUDE_CONFIG_DIR before reading
`.credentials.json`, and mj stages every macOS session's login as that file.
The checks pass only if the staged file, not a Keychain item, is what Claude
Code sends.

No credentials or paid calls: tokens are synthetic, the API is a local fake, and
the whole run happens with only loopback reachable (a private network namespace
from `unshare -rn` on Linux, a `sandbox-exec` profile on macOS), so nothing can
reach a real endpoint. Opt-in, like the other tests/e2e scripts:

    python3 tests/e2e/claude_stripped_credentials.py [--bundle <managed claude install>]

The bundle defaults to the managed harness cache entry for the pin in
mj-core/src/harness_runtime.rs; install it first (any mj session or doctor run
with the Claude harness does that). The script refuses a bundle whose package
versions differ from mj-core/assets/harnesses/claude/package.json.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import queue
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = Path(__file__).resolve().parents[2]
TOKEN_HOST = "platform.claude.com"  # TOKEN_URL in the pinned Claude Code bundle


# Everything but loopback is refused. Unix sockets stay open for the system
# services Claude Code talks to locally (the Keychain goes through Mach, not these).
MACOS_LOOPBACK_ONLY = """(version 1)
(allow default)
(deny network-outbound)
(allow network-outbound (remote ip "localhost:*"))
(allow network-outbound (remote unix-socket))
"""


def enter_private_network():
    """Re-execute where only loopback is reachable."""
    if os.environ.get("CLAUDE_STRIPPED_NETNS") != "1":
        os.environ["CLAUDE_STRIPPED_NETNS"] = "1"
        if sys.platform == "linux" and shutil.which("unshare"):
            os.execvp("unshare", ["unshare", "-rn", sys.executable, *sys.argv])
        if sys.platform == "darwin":
            os.execv("/usr/bin/sandbox-exec", ["sandbox-exec", "-p", MACOS_LOOPBACK_ONLY, sys.executable, *sys.argv])
        sys.exit("this test needs Linux with unshare(1) or macOS with sandbox-exec for network isolation")
    if sys.platform == "linux":
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        flags = struct.unpack("16sH14x", fcntl.ioctl(sock, 0x8913, struct.pack("16sH14x", b"lo", 0)))[1]
        fcntl.ioctl(sock, 0x8914, struct.pack("16sH14x", b"lo", flags | 1))
    # Nothing outside loopback may be reachable; fail loudly if isolation is not real.
    probe = socket.socket()
    probe.settimeout(2)
    try:
        probe.connect(("192.0.2.1", 9))
        sys.exit("network namespace is not isolated")
    except OSError:
        pass
    probe = socket.socket()
    probe.settimeout(2)
    try:
        probe.connect(("1.1.1.1", 443))
        sys.exit("network is not isolated: reached 1.1.1.1:443")
    except OSError:
        pass


def pinned_bundle(explicit):
    expected = json.loads((REPO / "mj-core/assets/harnesses/claude/package.json").read_text())["dependencies"]
    if explicit:
        root = Path(explicit).resolve()
    else:
        install_id = re.search(r'"(claude-agent-acp-[\d.]+_claude-[\d.]+_pkg\d+)"',
                               (REPO / "mj-core/src/harness_runtime.rs").read_text()).group(1)
        cache = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
        root = cache / "mjolnir/harnesses/claude" / install_id
    modules = root / "node_modules"
    for name, version in expected.items():
        have = json.loads((modules / name / "package.json").read_text())["version"]
        assert have == version, f"{name} is {have} in {root}, pinned {version}"
    return modules / ".bin/claude", modules / ".bin/claude-agent-acp"


class Lab:
    """Fake Anthropic API plus a proxy that records and refuses every CONNECT."""

    def __init__(self):
        self.requests = []
        self.accepted = set()
        self.lock = threading.Lock()
        lab = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def handle_any(self):
                length = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(length) if length else b""
                auth = self.headers.get("authorization", "")
                ok = auth.removeprefix("Bearer ") in lab.accepted
                with lab.lock:
                    lab.requests.append({"kind": "api", "path": self.path, "authorization": auth,
                                         "status": 200 if ok else 401})
                if not ok:
                    return self.reply(401, {"type": "error", "error": {
                        "type": "authentication_error", "message": "OAuth token has expired (synthetic)"}})
                if not self.path.startswith("/v1/messages"):
                    return self.reply(200, {})
                request = json.loads(body or b"{}")
                message = {"id": "msg_1", "type": "message", "role": "assistant",
                           "model": request.get("model", "m"), "stop_reason": None, "stop_sequence": None,
                           "usage": {"input_tokens": 1, "output_tokens": 1}}
                if not request.get("stream"):
                    return self.reply(200, {**message, "content": [{"type": "text", "text": "pong"}],
                                            "stop_reason": "end_turn"})

                def event(kind, data):
                    return f"event: {kind}\ndata: {json.dumps({'type': kind, **data})}\n\n"
                stream = (event("message_start", {"message": {**message, "content": []}})
                          + event("content_block_start", {"index": 0, "content_block": {"type": "text", "text": ""}})
                          + event("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": "pong"}})
                          + event("content_block_stop", {"index": 0})
                          + event("message_delta", {"delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                                    "usage": {"output_tokens": 1}})
                          + event("message_stop", {})).encode()
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("content-length", str(len(stream)))
                self.end_headers()
                self.wfile.write(stream)

            do_GET = do_POST = do_PUT = do_DELETE = handle_any

            def reply(self, status, obj):
                data = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.proxy = socket.socket()
        self.proxy.bind(("127.0.0.1", 0))
        self.proxy.listen(50)
        threading.Thread(target=self.refuse_connections, daemon=True).start()

    def refuse_connections(self):
        while True:
            conn, _ = self.proxy.accept()
            try:
                line = conn.recv(4096).decode("latin1").split("\r\n")[0]
                with self.lock:
                    self.requests.append({"kind": "blocked", "line": line})
                conn.sendall(b"HTTP/1.1 502 Blocked\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            except OSError:
                pass
            finally:
                conn.close()

    def token_endpoint_attempts(self):
        with self.lock:
            return [r["line"] for r in self.requests if r["kind"] == "blocked" and TOKEN_HOST in r["line"]]

    def api_auth_since(self, mark):
        with self.lock:
            return [r["authorization"] for r in self.requests[mark:]
                    if r["kind"] == "api" and r["path"].startswith("/v1/messages")]

    def mark(self):
        with self.lock:
            return len(self.requests)

    def environment(self, home):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CLAUDE", "ANTHROPIC", "MJ_", "CODEX"))}
        proxy = f"http://127.0.0.1:{self.proxy.getsockname()[1]}"
        env.update({
            "HOME": str(home), "CLAUDE_CONFIG_DIR": str(home / ".claude"),
            "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{self.httpd.server_address[1]}",
            "HTTPS_PROXY": proxy, "HTTP_PROXY": proxy, "https_proxy": proxy, "http_proxy": proxy,
            "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_AUTOUPDATER": "1",
        })
        return env


def install_credentials(home, access, expires_in, refresh=None):
    """Write like mj's install: temp file in the same directory, then rename."""
    oauth = {"accessToken": access, "expiresAt": int((time.time() + expires_in) * 1000),
             "scopes": ["user:inference", "user:profile", "user:sessions:claude_code"],
             "subscriptionType": "max"}
    if refresh is not None:
        oauth["refreshToken"] = refresh
    target = home / ".claude/.credentials.json"
    temporary = target.with_name(".credentials.json.tmp")
    temporary.write_text(json.dumps({"claudeAiOauth": oauth}))
    temporary.chmod(0o600)
    os.rename(temporary, target)


def credentials_bytes(home):
    path = home / ".claude/.credentials.json"
    return path.stat().st_mtime_ns, path.read_bytes()


CHILDREN = []


def terminate_children():
    """Kill and reap every started process group; callers delete files only afterwards."""
    while CHILDREN:
        child = CHILDREN.pop()
        try:
            os.killpg(child.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.process.wait()


class Jsonl:
    """A child speaking newline-delimited JSON on stdio, in its own process group."""

    def __init__(self, command, cwd, env):
        self.process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                        start_new_session=True)
        CHILDREN.append(self)
        self.lines = queue.Queue()
        self.stderr = []
        threading.Thread(target=self.read_stdout, daemon=True).start()
        threading.Thread(target=lambda: self.stderr.extend(self.process.stderr), daemon=True).start()

    def read_stdout(self):
        for line in self.process.stdout:
            if line.lstrip().startswith("{"):
                self.lines.put(json.loads(line))

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def next(self, accept, timeout=90):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.lines.get(timeout=1)
            except queue.Empty:
                assert self.process.poll() is None, f"child exited: {''.join(self.stderr)[-800:]}"
                continue
            if accept(message):
                return message
        raise AssertionError(f"no matching message; stderr: {''.join(self.stderr)[-800:]}")

    def close(self):
        try:
            self.process.stdin.close()
            self.process.wait(10)
        except (OSError, subprocess.TimeoutExpired):
            self.process.kill()
            self.process.wait()


class ClaudeCode(Jsonl):
    def __init__(self, claude, lab, home):
        super().__init__([str(claude), "-p", "--input-format", "stream-json", "--output-format", "stream-json",
                          "--verbose", "--model", "claude-sonnet-4-5", "--no-session-persistence"],
                         home, lab.environment(home))
        self.turns = 0

    def turn(self):
        self.turns += 1
        self.send({"type": "user", "message": {"role": "user", "content": f"ping {self.turns}"}})
        return self.next(lambda m: m.get("type") == "result")


class Acp(Jsonl):
    def __init__(self, acp, claude, lab, home):
        # Managed launches select the separately pinned Claude Code over the SDK's bundled one
        # (mj-core/src/harness_install.rs, CLAUDE_CODE_EXECUTABLE).
        env = {**lab.environment(home), "CLAUDE_CODE_EXECUTABLE": str(claude)}
        super().__init__(["node", str(acp.resolve())], home, env)
        self.ids = 0
        self.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        self.session = self.request("session/new", {"cwd": str(home), "mcpServers": []})["result"]["sessionId"]

    def request(self, method, params):
        self.ids += 1
        mine = self.ids
        self.send({"jsonrpc": "2.0", "id": mine, "method": method, "params": params})
        while True:
            message = self.next(lambda m: "id" in m)
            if "method" in message:  # agent-to-client request: decline
                self.send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "n/a"}})
            elif message["id"] == mine:
                assert "error" not in message, message
                return message

    def turn(self):
        return self.request("session/prompt", {"sessionId": self.session,
                                               "prompt": [{"type": "text", "text": "ping"}]})["result"]


def with_home(body):
    home = Path(tempfile.mkdtemp(prefix="stripped-creds-"))
    (home / ".claude").mkdir()
    try:
        body(Lab(), home)
    finally:
        terminate_children()
        shutil.rmtree(home, ignore_errors=True)


def refresh_window(claude):
    """Section 1: no refreshToken, expiring soon: no refresh attempt, no write."""
    def stripped(lab, home):
        lab.accepted = {"SYNTH-A"}
        install_credentials(home, "SYNTH-A", 120)
        before = credentials_bytes(home)
        cc = ClaudeCode(claude, lab, home)
        for _ in range(2):
            assert cc.turn()["is_error"] is False
        cc.close()
        assert lab.token_endpoint_attempts() == [], lab.token_endpoint_attempts()
        assert credentials_bytes(home) == before, "Claude Code rewrote a refresh-token-less file"

    def control(lab, home):
        # The same login with a refreshToken must reach for the token endpoint; otherwise the
        # absence above would only show the observation cannot see refreshes.
        lab.accepted = {"SYNTH-A"}
        install_credentials(home, "SYNTH-A", 120, refresh="SYNTH-REFRESH")
        cc = ClaudeCode(claude, lab, home)
        cc.turn()
        cc.close()
        assert lab.token_endpoint_attempts(), "control: a refreshable login never tried to refresh"

    with_home(stripped)
    with_home(control)


def rejection_and_replacement(claude):
    """Sections 2 and 3 on Claude Code itself."""
    def body(lab, home):
        lab.accepted = {"SYNTH-A", "SYNTH-B", "SYNTH-C"}
        install_credentials(home, "SYNTH-A", 3600)
        cc = ClaudeCode(claude, lab, home)
        assert cc.turn()["is_error"] is False
        # Replacement is used by the very next request, with 55 minutes of validity left on the old one.
        install_credentials(home, "SYNTH-B", 7200)
        mark = lab.mark()
        assert cc.turn()["is_error"] is False
        assert lab.api_auth_since(mark) == ["Bearer SYNTH-B"], lab.api_auth_since(mark)
        before_rejection = credentials_bytes(home)
        # The server stops accepting the token: the turn errors, the process lives, nothing is written.
        lab.accepted = {"SYNTH-C"}
        failed = cc.turn()
        assert failed["is_error"] and failed.get("api_error_status") == 401, failed
        assert cc.process.poll() is None
        assert lab.token_endpoint_attempts() == []
        assert credentials_bytes(home) == before_rejection, "a 401 made Claude Code rewrite the file"
        install_credentials(home, "SYNTH-C", 7200)
        mark = lab.mark()
        assert cc.turn()["is_error"] is False
        assert lab.api_auth_since(mark) == ["Bearer SYNTH-C"]
        cc.close()
    with_home(body)


def through_the_bridge(acp, claude):
    """Section 3 through claude-agent-acp, one long-lived session, several prompts."""
    def body(lab, home):
        lab.accepted = {"SYNTH-A", "SYNTH-B", "SYNTH-C"}
        install_credentials(home, "SYNTH-A", 3600)
        bridge = Acp(acp, claude, lab, home)
        mark = lab.mark()
        assert bridge.turn()["stopReason"] == "end_turn"
        assert set(lab.api_auth_since(mark)) == {"Bearer SYNTH-A"}
        for token in ("SYNTH-B", "SYNTH-C"):
            install_credentials(home, token, 7200)
            mark = lab.mark()
            assert bridge.turn()["stopReason"] == "end_turn"
            used = lab.api_auth_since(mark)
            assert used and set(used) == {f"Bearer {token}"}, (token, used)
        # Expiring soon and refresh-token-less through the bridge: no refresh, no rewrite.
        install_credentials(home, "SYNTH-C", 90)
        before = credentials_bytes(home)
        assert bridge.turn()["stopReason"] == "end_turn"
        assert lab.token_endpoint_attempts() == []
        assert credentials_bytes(home) == before
        bridge.close()
    with_home(body)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bundle", help="managed claude harness install directory (default: the pin in the cache)")
    args = parser.parse_args()
    enter_private_network()
    claude, acp = pinned_bundle(args.bundle)
    refresh_window(claude)
    rejection_and_replacement(claude)
    through_the_bridge(acp, claude)
    print("ok: pinned Claude Code and claude-agent-acp honour refresh-token-less credentials")


if __name__ == "__main__":
    main()
