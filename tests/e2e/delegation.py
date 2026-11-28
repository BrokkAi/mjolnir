#!/usr/bin/env python3
"""Delegation without web access, then durable accounting, in a named instance."""

import argparse
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import mmap
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
import zipfile

import reliability_lab

# Every polling wait below detects a hang, not a slow host. The test host's
# shared ZFS pool can hold an ordinary write for several seconds while other
# builds flush, which stalls the daemon's database and a worker's journal, so
# a deadline sized for a quiet machine fails a correct run.
WAIT_SECONDS = 120

# The scenario acts on these turns while they run: it hands a report back,
# changes the parent's settings, or replaces the daemon. The fake agent holds
# each one until the scenario releases or cancels it. A timer instead ends a
# turn too early on a slow host, or leaves too little time for what follows.
HELD_PROMPTS = (
    "Return a short fixture report.",
    "Exhaust fixture quota ",
    "Continue the unfinished work already requested",
    "Hold the parent turn for queued settings.",
    "Run after the pending settings.",
    "second turn",
    "Return the legacy attachment evidence.",
    "Return a judgment report.",
    "Report the session override.",
    "Hold OpenAI Codex turn for mailbox delivery.",
    "Hold DeepSeek Codex turn for deferred mailbox delivery.",
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--mailbox-regression-only", action="store_true")
    parser.add_argument("--unanswered-wake-only", action="store_true")
    parser.add_argument("--restart-regression-only", action="store_true")
    parser.add_argument("--missing-child-cwd-only", action="store_true")
    parser.add_argument("--create-restart-only", action="store_true")
    quota_modes = parser.add_mutually_exclusive_group()
    quota_modes.add_argument("--child-quota-only", action="store_true")
    quota_modes.add_argument("--top-level-quota-only", action="store_true")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named test instance")
    if args.top_level_quota_only:
        # The worker consumes this endpoint variable only in test-hooks builds.
        # Check the executable before starting anything that could call GLiDE.
        try:
            with args.worker.open("rb") as executable, mmap.mmap(
                executable.fileno(), 0, access=mmap.ACCESS_READ
            ) as binary:
                supports_glide_override = binary.find(b"MJ_TEST_GLIDE_ENDPOINT") != -1
        except (OSError, ValueError) as error:
            parser.error(f"cannot inspect the worker executable: {error}")
        if not supports_glide_override:
            parser.error("--top-level-quota-only requires mj-worker built with --features test-hooks")
    lab = reliability_lab.Lab(args.mj, "delegation", 1171)
    quota_only = args.child_quota_only or args.top_level_quota_only
    catalog_proxy = None
    glide_proxy = None
    cli = None
    try:
        lab.prepare(fake_acp_prompt_delay_ms=10000)
        bridge = lab.runtime_root / "fake_acp.py"
        script = bridge.read_text().replace(
            'session_id = "reliability-native"',
            'session_id = __import__("uuid").uuid4().hex',
        )
        # Hard-won: cdb6971bc: the fixture kept child role text in the first user prompt.
        initialize_anchor = '            "protocolVersion": 1,\n'
        assert initialize_anchor in script
        script = script.replace(
            initialize_anchor,
            initialize_anchor + '            "agentCapabilities": {"sessionCapabilities": {"systemPrompt": {}}},\n',
            1,
        )
        open_anchor = '    elif method in ("session/new", "session/load"):\n'
        assert open_anchor in script
        script = script.replace(
            open_anchor,
            open_anchor
            + '        with open(os.path.join(os.environ["CODEX_HOME"], "fixture-system-prompt.jsonl"), "a") as marker:\n'
            + '            marker.write(json.dumps(message.get("params", {}).get("systemPrompt")) + "\\n")\n',
            1,
        )
        options = [
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "tiny",
                "options": [{"value": "tiny", "name": "Tiny fixture"}, {"value": "subtle-fixture", "name": "Subtle fixture"}],
            }
        ]
        script = script.replace(
            '    elif method in ("session/new", "session/load"):\n',
            '    elif method in ("session/new", "session/load"):\n        session_id = message.get("params", {}).get("sessionId", session_id)\n',
        )
        script = script.replace(
            '            "sessionId": session_id,\n            "modes":',
            '            "sessionId": session_id,\n            "configOptions": '
            + repr(options)
            + ',\n            "modes":',
        )
        script = script.replace(
            '    elif method == "session/set_mode":',
            '    elif method == "session/set_config_option":\n        result = {"configOptions": '
            + repr(options)
            + '}\n    elif method == "session/set_mode":',
        )
        options.append({
            "id": "effort", "name": "Reasoning effort", "category": "thought_level",
            "type": "select", "currentValue": "high",
            "options": [{"value": "high", "name": "High fixture"}, {"value": "low", "name": "Low fixture"}],
        })
        # Rebuild the inserted options with effort as well, so attribution is
        # checked against the harness's actual advertised turn configuration.
        script = script.replace(repr(options[:1]), repr(options))
        script = script.replace(
            '    if method == "session/prompt":\n        report_execution("idle")',
            '    if method == "session/prompt":\n'
            '        result["usage"] = {"totalTokens": 120, "inputTokens": 100, '
            '"outputTokens": 20, "_meta": {"mjolnir.dev/usage-scope": "turn"}}\n'
            '        report_execution("idle")',
        )
        # Keep advertised current values in sync with the daemon's resolved choice.
        script = script.replace(repr(options), "config_options")
        script = script.replace('for line in sys.stdin:', 'config_options = ' + repr(options) + '\nfor line in sys.stdin:')
        script = script.replace(
            '        result = {"configOptions": config_options}',
            '        params = message["params"]\n'
            '        for option in config_options:\n'
            '            if option["id"] == params["configId"]:\n'
            '                option["currentValue"] = params["value"]\n'
            '        result = {"configOptions": config_options}',
        )
        config = lab.config / "config.toml"
        config.write_text(config.read_text() + '\n[github.watch]\nenabled = false\n[subagents.straightforward]\nmodel = "tiny"\neffort = "high"\n[subagents.subtle]\nmodel = "subtle-fixture"\neffort = "low"\n')
        # This fixture never owns a native goal. Report that alongside every
        # execution update, including after loading a parked conversation.
        script = script.replace(
            "def report_execution(status, goal_cleared=False):",
            "def report_execution(status, goal_cleared=True):",
        )
        script = script.replace(
            '        report_execution("running")',
            '        report_execution("running")\n'
            '        with open(os.path.join(os.environ["CODEX_HOME"], "fixture-prompt.jsonl"), "a") as marker:\n'
            '            marker.write(json.dumps(blocks) + "\\n")\n'
            '        with open(os.path.join(os.environ["CODEX_HOME"], "fixture-prompt.txt"), "a") as marker:\n'
            '            marker.write(text + "\\n")',
        )
        if args.unanswered_wake_only:
            script = script.replace(
                '            marker.write(text + "\\n")',
                '            marker.write(text + "\\n")\n'
                '        if "Message from your parent agent:" in text and "unanswered wake fixture" in text:\n'
                '            report_execution("idle")\n'
                '            send({"jsonrpc":"2.0", "id":ident, "result":{"stopReason":"end_turn"}})\n'
                '            continue',
                1,
            )
        # A held prompt ends with end_turn once `release` writes its file into
        # the session's private CODEX_HOME, or as cancelled on session/cancel.
        held = (
            "HELD_PROMPTS = " + repr(HELD_PROMPTS) + "\n\n"
            "def hold_until_released():\n"
            '    release = os.path.join(os.environ["CODEX_HOME"], "fixture-release")\n'
            "    deadline = time.monotonic() + 900\n"
            "    while time.monotonic() < deadline:\n"
            "        if os.path.exists(release):\n"
            "            os.unlink(release)\n"
            "            return False\n"
            "        ready, _, _ = select.select([sys.stdin], [], [], 0.05)\n"
            "        if ready:\n"
            "            cancellation = json.loads(sys.stdin.readline())\n"
            "            log(cancellation)\n"
            '            if cancellation.get("method") != "session/cancel":\n'
            '                raise RuntimeError("expected session/cancel while prompt was held")\n'
            "            return True\n"
            '    raise RuntimeError("held prompt was never released")\n\n'
            "def wait_for_prompt_cancel():\n"
            '    if any(marker in globals().get("text", "") for marker in HELD_PROMPTS):\n'
            "        return hold_until_released()\n"
        )
        assert script.count("def wait_for_prompt_cancel():\n") == 1
        script = script.replace("def wait_for_prompt_cancel():\n", held)
        model_gate = lab.runtime_root / "hold-startup-model"
        script = script.replace(
            '        params = message["params"]\n',
            '        params = message["params"]\n'
            f'        gate = {str(model_gate)!r}\n'
            '        if params["configId"] == "model" and os.path.exists(gate):\n'
            '            open(os.path.join(os.environ["CODEX_HOME"], "fixture-model-pending"), "w").close()\n'
            '            deadline = time.monotonic() + 120\n'
            '            while os.path.exists(gate):\n'
            '                if time.monotonic() >= deadline:\n'
            '                    raise RuntimeError("startup model response was not released")\n'
            '                time.sleep(0.05)\n',
        )
        if quota_only:
            reset = datetime.now(timezone.utc).replace(microsecond=0) + timedelta(days=1)
            script = script.replace(
                '            marker.write(text + "\\n")',
                '            marker.write(text + "\\n")\n'
                '        if "Exhaust fixture quota " in text or (\n'
                '            "Continue the unfinished work already requested" in text\n'
                '            and not os.environ.get("MJ_FIXTURE_ACCOUNT")\n'
                '        ):\n'
                '            if "Exhaust fixture quota " in text:\n'
                '                hold_until_released()\n'
                '            report_execution("idle")\n'
                '            send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32603,\n'
                '                  "message": "Internal error", "data": {"message": "Fixture quota exhausted",\n'
                '                  "codexErrorInfo": "usageLimitExceeded",\n'
                f'                  "resetAt": {int(reset.timestamp())!r}}}' + '}})\n'
                '            continue',
                1,
            )
            script = script.replace(
                '        session_id = message.get("params", {}).get("sessionId", session_id)',
                '        session_id = message.get("params", {}).get("sessionId", session_id)\n'
                '        with open(os.path.join(os.environ["CODEX_HOME"], "fixture-open.jsonl"), "a") as marker:\n'
                '            marker.write(json.dumps([method, session_id, os.environ.get("MJ_FIXTURE_ACCOUNT", "original")]) + "\\n")',
            )
        if quota_only:
            # The daemon reads quota through codex app-server. Serve both
            # accounts from the same fixture as the typed ACP prompt error.
            script = script.replace(
                '    elif method == "session/prompt":\n',
                '    elif method == "account/read":\n'
                '        result = {"account": {"type": "chatgpt", "planType": "plus"}}\n'
                '    elif method == "account/rateLimits/read":\n'
                '        result = {"rateLimits": {"secondary": {\n'
                f'            "usedPercent": 100 if {args.top_level_quota_only!r} and not os.environ.get("MJ_FIXTURE_ACCOUNT") else 20,\n'
                f'            "windowDurationMins": 10080, "resetsAt": {int(reset.timestamp())},\n'
                '        }}}\n'
                '    elif method == "session/prompt":\n',
                1,
            ).replace(
                '        report_execution("running")\n',
                '        log({"fixture_prompt": text, "account": os.environ.get("MJ_FIXTURE_ACCOUNT", "original")})\n'
                '        report_execution("running")\n',
                1,
            )
            (lab.runtime_root / "bin" / "codex").symlink_to(bridge)
        compile(script, str(bridge), "exec")
        bridge.write_text(script)
        if args.create_restart_only:
            # Holds a child launch before its worker starts, so the daemon
            # restart lands in the middle of the create.
            lab.hook_name = "subagent_create_before_worker_start"
        env = lab.environment()
        env.update(
            MJ_INSTANCE=args.instance, MJ_WORKER_BINARY=str(args.worker.resolve())
        )
        env.pop("MJ_SESSION_ID", None)

        # Keep the default fixture explicitly OpenAI-backed for its subagents.
        (lab.profile / "config.toml").write_text('model_provider = "openai"\n')
        if quota_only or args.missing_child_cwd_only or args.unanswered_wake_only:
            (lab.profile / "auth.json").write_text(json.dumps({
                "auth_mode": "chatgpt",
                "tokens": {"access_token": "fixture-token"},
            }))
        config_path = lab.config / "config.toml"

        def add_replacement_profile():
            replacement_home = lab.runtime_root / "profile-replacement"
            replacement_home.mkdir()
            for source_file in lab.profile.iterdir():
                if source_file.is_file():
                    (replacement_home / source_file.name).write_bytes(source_file.read_bytes())
            config_text = config_path.read_text()
            profile = config_text.split("[profiles.fake]\n", 1)[1].split("\n[", 1)[0]
            replacement = profile.replace(
                json.dumps(str(lab.profile)), json.dumps(str(replacement_home))
            ).replace("environment = {", 'environment = { MJ_FIXTURE_ACCOUNT = "replacement",')
            config_path.write_text(config_text + "\n[profiles.replacement]\n" + replacement)

        if args.top_level_quota_only:
            add_replacement_profile()
            glide_requests = []

            class FailingGlideHandler(BaseHTTPRequestHandler):
                def do_POST(self):
                    self.rfile.read(int(self.headers["Content-Length"]))
                    glide_requests.append(self.path)
                    body = b'{"error":"upstream_failure"}'
                    self.send_response(502)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def log_message(self, _format, *_args):
                    pass

            glide_proxy = ThreadingHTTPServer(("127.0.0.1", 0), FailingGlideHandler)
            threading.Thread(target=glide_proxy.serve_forever, daemon=True).start()
            env["MJ_TEST_GLIDE_ENDPOINT"] = f"http://127.0.0.1:{glide_proxy.server_port}"

        if args.mailbox_regression_only:
            class CatalogProxyHandler(BaseHTTPRequestHandler):
                def do_GET(self):
                    body = json.dumps({
                        "object": "list",
                        "data": [{"id": "tiny"}, {"id": "deepseek-fixture"}],
                    }).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def log_message(self, _format, *_args):
                    pass

            # Keep provider classification on DeepSeek's real host while
            # serving its catalog locally; the test does not depend on its API.
            catalog_proxy = ThreadingHTTPServer(("127.0.0.1", 0), CatalogProxyHandler)
            threading.Thread(target=catalog_proxy.serve_forever, daemon=True).start()
            proxy_url = f"http://127.0.0.1:{catalog_proxy.server_port}"
            env.update(HTTP_PROXY=proxy_url, http_proxy=proxy_url)
            env.update(NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")

            deepseek_home = lab.runtime_root / "profile-deepseek"
            deepseek_home.mkdir()
            (deepseek_home / "config.toml").write_text(
                'model_provider = "deepseek-fixture"\n'
                '[model_providers.deepseek-fixture]\n'
                'name = "DeepSeek fixture"\n'
                'base_url = "http://api.deepseek.com"\n'
                'wire_api = "responses"\n'
                'env_key = "DEEPSEEK_API_KEY"\n'
            )
            fixture_bin = lab.runtime_root / "bin"
            profile_environment = {
                "MJ_FAKE_ACP_LOG": str(lab.runtime_root / "fake-acp.log"),
                "MJ_FAKE_ACP_DELAY_MS": "0",
                "MJ_FAKE_ACP_PROMPT_DELAY_MS": "10000",
                "MJ_FAKE_ACP_SWALLOW_PROMPT": "0",
                "MJ_FAKE_ACP_SWALLOW_BANNER": "",
                "PATH": str(fixture_bin),
                "XDG_CACHE_HOME": str(lab.runtime_root / "cache"),
                "DEEPSEEK_API_KEY": "fixture-api-key",
            }
            env_toml = ", ".join(
                f"{key} = {json.dumps(value)}" for key, value in profile_environment.items()
            )
            config_path.write_text(
                config_path.read_text()
                + "\n[profiles.fake-deepseek]\n"
                + 'kind = "codex"\n'
                + f"home = {json.dumps(str(deepseek_home))}\n"
                + f"environment = {{ {env_toml} }}\n"
            )

        def cli(*command, check=True):
            result = subprocess.run(
                [str(args.mj.resolve()), "--instance", args.instance, *command],
                env=env,
                capture_output=True,
                text=True,
                timeout=WAIT_SECONDS,
            )
            with (lab.root / "cli.log").open("a") as log:
                log.write(
                    json.dumps(
                        {
                            "command": command,
                            "status": result.returncode,
                            "stdout": result.stdout,
                            "stderr": result.stderr,
                        }
                    )
                    + "\n"
                )
            if check and result.returncode:
                raise RuntimeError(f"{command}: {result.stderr}")
            return result.stdout

        cli("workspaces", "create", "Delegation test")
        created = json.loads(
            cli(
                "new",
                "--workspace",
                "Delegation test",
                "--profile",
                "fake",
                "--target",
                "localhost",
                "--project-directory",
                str(lab.project),
                "--subagents",
                "mjolnir",
                "--json",
            )
        )
        parent = created["session_id"]

        def endpoint(session):
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline:
                paths = [
                    p for p in lab.data.rglob("subagents.sock") if session in p.parts
                ]
                if paths:
                    return paths[0]
                time.sleep(0.05)
            raise RuntimeError(f"no worker tool socket for {session}")

        def tool(session, action, params=None, *, raw_message=False, expect_error=False):
            request = {
                "request_id": uuid.uuid4().hex,
                "created_at_ms": int(time.time() * 1000),
                "action": {"action": action},
            }
            if params is not None:
                request["action"]["params"] = params
            started = time.monotonic()
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(WAIT_SECONDS)
                connection.connect(str(endpoint(session)))
                connection.sendall(json.dumps(request).encode() + b"\n")
                response_line = connection.makefile("rb").readline()
                if not response_line:
                    lab.capture_process_tree()
                    lab.preserve_runtime()
                    raise RuntimeError(
                        f"session tool socket closed without replying: {session}, {action}"
                    )
                reply = json.loads(response_line)
            with (lab.root / "tools.jsonl").open("a") as log:
                log.write(
                    json.dumps(
                        {
                            "session": session,
                            "request": request,
                            "reply": reply,
                            "seconds": time.monotonic() - started,
                        }
                    )
                    + "\n"
                )
            result = reply.get("result")
            assert result is not None, reply
            assert result["is_error"] == expect_error, result
            return result["message"] if raw_message else json.loads(result["message"])

        def worker_status(session):
            # Ask the worker that owns the turn, independently of projection lag.
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(WAIT_SECONDS)
                connection.connect(str(endpoint(session).with_name("control.sock")))
                with connection.makefile("rb") as reader:
                    hello = {
                        "request_id": uuid.uuid4().hex, "protocol_version": 1,
                        "request": {"method": "hello", "params": {
                            "controller_version": "delegation-e2e",
                            "supported": {"min": 1, "max": 4294967295},
                        }},
                    }
                    connection.sendall(json.dumps(hello).encode() + b"\n")
                    response = json.loads(reader.readline())
                    assert response["result"] == "ok", response
                    request = {
                        "request_id": uuid.uuid4().hex,
                        "protocol_version": response["protocol_version"],
                        "request": {"method": "status"},
                    }
                    connection.sendall(json.dumps(request).encode() + b"\n")
                    response = json.loads(reader.readline())
                    assert response["result"] == "ok", response
                    assert response["payload"]["type"] == "status", response
                    return response["payload"]["data"]

        def wait_worker_ready(session):
            deadline = time.monotonic() + WAIT_SECONDS
            last_error = None
            while time.monotonic() < deadline:
                try:
                    return worker_status(session)
                except (OSError, TimeoutError, json.JSONDecodeError, AssertionError) as error:
                    last_error = error
                    time.sleep(0.05)
            raise RuntimeError(f"worker control socket did not become ready: {session}: {last_error}")

        def wait_recorded_parent_turn():
            # The fake agent has already observed the user message. A missing
            # selection row is still unknown, rather than an empty work queue.
            deadline = time.monotonic() + WAIT_SECONDS
            command = None
            while time.monotonic() < deadline:
                if command is None:
                    state = worker_status(parent)
                    turn = state.get("active_prompt") or state.get("turn_completion")
                    if turn:
                        command = turn["command_id"]
                if command is not None:
                    with sqlite3.connect(
                        f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True
                    ) as database:
                        recorded = database.execute(
                            "SELECT 1 FROM session_turn_usage "
                            "WHERE session_id=? AND command_id=?", (parent, command),
                        ).fetchone()
                    if recorded:
                        return
                time.sleep(0.1)
            raise RuntimeError(f"parent turn usage was not recorded: {command}")

        def run_mailbox_hook(session):
            # Exercise the same worker boundary that the staged Codex
            # PostToolUse handler calls, and verify the message is in its
            # response before the child's current turn finishes.
            control_socket = endpoint(session).with_name("control.sock")
            result = subprocess.run(
                [
                    str(args.worker.resolve()),
                    "worker",
                    "mailbox-hook",
                    "--socket",
                    str(control_socket),
                    "--event",
                    "PostToolUse",
                ],
                env=env,
                input="{}\n",
                capture_output=True,
                text=True,
                timeout=WAIT_SECONDS,
            )
            if result.returncode:
                raise RuntimeError(
                    f"mailbox hook failed: {result.stderr or result.stdout}"
                )
            return json.loads(result.stdout)

        endpoint(parent)
        deadline = time.monotonic() + WAIT_SECONDS
        while time.monotonic() < deadline:
            sessions = json.loads(cli("sessions", "--json"))
            rows = (
                sessions if isinstance(sessions, list) else sessions.get("sessions", [])
            )
            record = next(
                (row for row in rows if row.get("id", row.get("session_id")) == parent),
                {},
            )
            if str(record.get("state", "")).lower() == "running":
                break
            time.sleep(0.25)
        else:
            raise RuntimeError(f"parent did not finish provisioning: {sessions}")
        wait_worker_ready(parent)
        config = config_path
        if not quota_only:
            config.write_text(
                config.read_text().replace(
                    "[phone]\nenabled = true", "[phone]\nenabled = false"
                )
            )
            cli("daemon", "restart")
            assert "disabled" in cli("daemon", "status").lower()
        if not args.top_level_quota_only:
            tool(parent, "list_agents")

        def listed_session_ids(*options):
            response = json.loads(cli("sessions", *options, "--json"))
            rows = response if isinstance(response, list) else response["sessions"]
            return {row.get("id", row.get("session_id")) for row in rows}

        def wait_prompt(session, text):
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline:
                # The fake ACP agent appends each prompt to a file in its
                # session-private CODEX_HOME, which lies under the session id.
                for marker in lab.runtime_root.rglob("fixture-prompt.txt"):
                    if session in marker.parts and text in marker.read_text():
                        return marker
                time.sleep(0.05)
            raise RuntimeError(f"worker did not begin prompt: {text}")

        def release(session, text):
            # End a held turn as the agent would on its own, and wait until the
            # fake agent has taken the release, so it cannot end a later turn.
            assert any(marker in text for marker in HELD_PROMPTS), text
            marker = wait_prompt(session, text)
            staged = marker.with_name("fixture-release.tmp")
            staged.write_text(text)
            staged.replace(marker.with_name("fixture-release"))
            deadline = time.monotonic() + WAIT_SECONDS
            while marker.with_name("fixture-release").exists():
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"held prompt did not take its release: {text}")
                time.sleep(0.05)

        def wait_report(session, finished_child, text):
            wait_prompt(session, text)
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline:
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                    rows = database.execute(
                        "SELECT event_json FROM mailbox_outbox WHERE target_session_id=? AND event_key LIKE 'subagent-finished-%' AND accepted_at IS NOT NULL",
                        (session,),
                    ).fetchall()
                if any(
                    (body := json.loads(row[0])["body"])["type"] == "subagent_finished"
                    and body["report"]["child_session_id"] == finished_child
                    and body["report"]["output"] == text
                    for row in rows
                ):
                    return
                time.sleep(0.1)
            raise RuntimeError(f"report was not acknowledged in the outbox: {session}, {finished_child}, {rows}")

        def wait_parked(session):
            # At least the park lifecycle's own 120-second deadline. Cleanup
            # kills the worker, so a shorter wait aborts a still-valid park.
            deadline = time.monotonic() + max(WAIT_SECONDS, 120)
            while time.monotonic() < deadline:
                with sqlite3.connect(
                    f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True
                ) as database:
                    row = database.execute(
                        "SELECT state, last_error FROM sessions WHERE session_id = ?", (session,)
                    ).fetchone()
                if row and row[0].lower() == "parked":
                    return
                if row and row[0].lower() == "error":
                    raise RuntimeError(f"child parking failed for {session}: {row}")
                time.sleep(0.1)
            raise RuntimeError(f"child completion did not park {session}: {row}")

        def mailbox_acceptance(session, text):
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                rows = database.execute(
                    "SELECT event_json, accepted_at FROM mailbox_outbox WHERE target_session_id=?",
                    (session,),
                ).fetchall()
            for event_json, accepted_at in rows:
                if text in event_json:
                    return True, accepted_at
            return False, None

        def wait_mailbox_acceptance(session, text):
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline:
                found, accepted_at = mailbox_acceptance(session, text)
                if found and accepted_at is not None:
                    return
                time.sleep(0.05)
            raise RuntimeError(f"mailbox event was not accepted: {session}, {text}")

        # Hard-won: #1705: an empty mailbox wake hid the earlier child report.
        if args.unanswered_wake_only:
            prompt = "Return a short fixture report."
            child = tool(parent, "spawn", {
                "task_name": "unanswered-wake-child", "task_type": "straightforward",
                "instructions": prompt,
            })["child_session_id"]
            wait_prompt(child, prompt)
            requests = []
            for suffix in ("one", "two"):
                text = f"unanswered wake fixture {suffix}"
                requests.append(tool(parent, "send_message", {
                    "child_session_id": child, "message": text,
                })["request_id"])
                wait_mailbox_acceptance(child, text)
            release(child, prompt)
            deadline = time.monotonic() + WAIT_SECONDS
            report = None
            while time.monotonic() < deadline:
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                    rows = database.execute(
                        "SELECT event_json FROM mailbox_outbox WHERE target_session_id=? AND event_key LIKE 'subagent-finished-%'",
                        (parent,),
                    ).fetchall()
                for (raw,) in rows:
                    candidate = json.loads(raw)["body"]["report"]
                    if candidate["child_session_id"] == child and (candidate.get("failure") or {}).get("kind") == "mailbox_unanswered":
                        report = candidate
                        break
                if report:
                    break
                time.sleep(0.1)
            assert report, f"no unanswered wake notice: {rows}"
            assert report["state"] == "completed", report
            assert "reliability reply:" in report["output"], report
            warning = report["failure"]["summary"]
            assert all(request in warning for request in requests), report
            assert "may not have acted" in warning, report
            cli("daemon", "restart")
            listed = tool(parent, "list_agents")
            entry = next(agent for agent in listed["agents"] if agent["child_session_id"] == child)
            assert entry["state"] == "completed", entry
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                rows = database.execute(
                    "SELECT event_json FROM mailbox_outbox WHERE target_session_id=? AND event_key LIKE 'subagent-finished-%'",
                    (parent,),
                ).fetchall()
            assert any(json.loads(raw)["body"]["report"] == report for (raw,) in rows), rows
            return

        # Hard-won: #1664: a removed worktree made parked-child wakes look retryable.
        if args.missing_child_cwd_only:
            worktree = lab.runtime_root / "child worktree"
            lab.git_output(["worktree", "add", "--detach", str(worktree)], lab.project)
            prompt = "Return a short fixture report."
            child = tool(parent, "spawn", {
                "task_name": "child-with-worktree",
                "instructions": prompt,
                "working_directory": str(worktree),
            })["child_session_id"]
            wait_prompt(child, prompt)
            worker_root = endpoint(child).parent
            report = "child finished in its worktree"
            tool(child, "handback", {"message": report})
            release(child, prompt)
            wait_report(parent, child, report)
            wait_parked(child)
            lab.git_output(["worktree", "remove", str(worktree)], lab.project)

            message = {"child_session_id": child, "message": "Wake after worktree removal."}
            result = tool(parent, "send_message", message, expect_error=True)
            assert str(worktree) in result["error"], result
            assert "Use close to close this child" in result["error"], result
            assert "retry" not in result["error"] and "Broken pipe" not in result["error"], result

            startup = json.loads((worker_root / "worker-startup.json").read_text())
            assert startup["failure"]["step"] == "working-directory", startup
            assert "bind-socket" not in {step["step"] for step in startup["steps"]}, startup
            exit_record = json.loads((worker_root / "worker-exit.json").read_text())
            failure = exit_record["harness_startup_failure"]
            assert failure["kind"] == "working_directory_unavailable", exit_record
            assert str(worktree) in failure["message"], exit_record

            listed = tool(parent, "list_agents")["agents"][0]
            assert listed["state"] == "error", listed
            assert str(worktree) in listed["error"] and "Use close to close this child" in listed["next_action"], listed
            assert not listed.get("parked") and not listed.get("promptable"), listed

            result = tool(parent, "send_message", message, expect_error=True)
            assert str(worktree) in result["error"] and "Use close to close this child" in result["error"], result
            assert startup == json.loads((worker_root / "worker-startup.json").read_text())
            assert tool(parent, "close_agent", {"child_session_id": child})["closed"]
            print(json.dumps({"instance": args.instance, "artifacts": str(lab.root),
                              "result": "passed"}), flush=True)
            return

        # Hard-won: #1492: exercise both provider delivery modes through real
        # workers without traversing the independent child-startup regression.
        if args.mailbox_regression_only:
            config.write_text(config.read_text().replace(
                "[phone]\nenabled = false", "[phone]\nenabled = true"
            ))
            cli("daemon", "restart")

            def wait_running(session):
                deadline = time.monotonic() + WAIT_SECONDS
                while time.monotonic() < deadline:
                    sessions = json.loads(cli("sessions", "--json"))
                    rows = sessions if isinstance(sessions, list) else sessions.get("sessions", [])
                    record = next(
                        (row for row in rows if row.get("id", row.get("session_id")) == session),
                        {},
                    )
                    if str(record.get("state", "")).lower() == "running":
                        wait_worker_ready(session)
                        return
                    time.sleep(0.25)
                raise RuntimeError(f"session did not finish provisioning: {session}, {record}")

            def wait_fixture_prompt(session, text):
                deadline = time.monotonic() + WAIT_SECONDS
                while time.monotonic() < deadline:
                    for marker in lab.runtime_root.rglob("fixture-prompt.txt"):
                        if session in marker.parts and text in marker.read_text():
                            return marker
                    time.sleep(0.05)
                raise RuntimeError(f"worker did not begin prompt: {session}, {text}")

            for profile_id, provider_mode, prompt in [
                ("fake", "tool_boundary", "Hold OpenAI Codex turn for mailbox delivery."),
                ("fake-deepseek", "turn_boundary", "Hold DeepSeek Codex turn for deferred mailbox delivery."),
            ]:
                if prompt not in HELD_PROMPTS:
                    raise AssertionError(f"fixture prompt is not held: {prompt}")
                recipient = json.loads(cli(
                    "new", "--workspace", "Delegation test", "--profile", profile_id,
                    "--target", "localhost", "--project-directory", str(lab.project), "--json",
                ))["session_id"]
                wait_running(recipient)
                prompt_command = lab.daemon_request({
                    "action": "submit_session_command",
                    "arguments": {
                        "session_id": recipient,
                        "command_id": f"hold-{provider_mode}-mailbox-turn",
                        "command": {"type": "prompt", "data": {
                            "prompt": [{"type": "text", "text": prompt}],
                        }},
                    },
                }, timeout=WAIT_SECONDS)
                assert prompt_command["reply"] == "ordinal", prompt_command
                wait_fixture_prompt(recipient, prompt)
                active = worker_status(recipient)["active_prompt"]
                assert active is not None
                message_text = f"{profile_id} mailbox delivery at {provider_mode}."
                response = tool(parent, "send_message", {
                    "child_session_id": recipient, "message": message_text,
                })
                assert response["status"] == "queued", response
                assert response["mailbox_delivery"] == provider_mode, response
                if provider_mode == "turn_boundary":
                    assert "deferred until" in response["note"], response
                else:
                    assert "note" not in response, response
                wait_mailbox_acceptance(recipient, message_text)
                hook = run_mailbox_hook(recipient)
                assert worker_status(recipient)["active_prompt"] == active
                if provider_mode == "tool_boundary":
                    assert message_text in hook.get("hookSpecificOutput", {}).get("additionalContext", ""), hook
                else:
                    assert hook == {}, hook
                marker = wait_fixture_prompt(recipient, prompt)
                release_marker = marker.with_name("fixture-release.tmp")
                release_marker.write_text(prompt)
                release_marker.replace(marker.with_name("fixture-release"))
                deadline = time.monotonic() + WAIT_SECONDS
                while marker.with_name("fixture-release").exists():
                    if time.monotonic() >= deadline:
                        raise RuntimeError(f"held prompt did not take its release: {prompt}")
                    time.sleep(0.05)
                if provider_mode == "turn_boundary":
                    wait_fixture_prompt(recipient, message_text)
            print(json.dumps({
                "instance": args.instance,
                "artifacts": str(lab.root),
                "result": "passed",
                "mailbox_delivery": ["tool_boundary", "turn_boundary"],
            }), flush=True)
            return

        # A child being created when the daemon restarts is finished by the next
        # daemon: no failure, and the parent's spawn is not asked to repeat.
        if args.create_restart_only:
            spawned = tool(parent, "spawn", {
                "task_name": "create-restart-child",
                "task_type": "straightforward",
                "instructions": "Return a short fixture report.",
            })
            child = spawned["child_session_id"]
            hook = lab.hooks / "subagent_create_before_worker_start"
            deadline = time.monotonic() + WAIT_SECONDS
            while not hook.with_suffix(".reached").exists():
                if time.monotonic() >= deadline:
                    raise RuntimeError("the child's create never reached its hook")
                time.sleep(0.05)
            restart = subprocess.Popen(
                [str(args.mj.resolve()), "--instance", args.instance, "daemon", "restart"],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            time.sleep(1.0)
            hook.with_suffix(".continue").touch()
            stdout, stderr = restart.communicate(timeout=WAIT_SECONDS)
            assert restart.returncode == 0, (stdout, stderr)
            deadline = time.monotonic() + WAIT_SECONDS
            while True:
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                    row = database.execute(
                        "SELECT state, last_error FROM sessions WHERE session_id=?", (child,)
                    ).fetchone()
                    intents = database.execute(
                        "SELECT count(*) FROM session_create_intents WHERE session_id=?", (child,)
                    ).fetchone()[0]
                assert row and row[0].lower() != "error", row
                if row[0].lower() == "running" and not intents:
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"the recovered child never ran: {row}, intents={intents}")
                time.sleep(0.1)
            print(json.dumps({"instance": args.instance, "artifacts": str(lab.root),
                              "result": "passed", "child": child}), flush=True)
            return

        # Hard-won: #1535: a Restart re-sent a finished sub-agent's assignment and froze its view.
        if args.restart_regression_only:
            config.write_text(config.read_text().replace(
                "[phone]\nenabled = false", "[phone]\nenabled = true"
            ))
            cli("daemon", "restart")
            assignment = "Return a short fixture report."
            spawned = tool(parent, "spawn", {
                "task_name": "fixture-child",
                "task_type": "straightforward",
                "instructions": assignment,
            })
            child = spawned["child_session_id"]
            wait_prompt(child, assignment)
            # The automatic completion check may refuse the first handback once;
            # the same report is then delivered without another check.
            for attempt in range(2):
                try:
                    tool(child, "handback", {"message": "finished before the restart"})
                    break
                except json.JSONDecodeError:
                    if attempt:
                        raise
            release(child, assignment)
            wait_report(parent, child, "finished before the restart")

            def child_state():
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                    row = database.execute(
                        "SELECT state, last_error FROM sessions WHERE session_id=?", (child,)
                    ).fetchone()
                return row

            def wait_child_state(state):
                deadline = time.monotonic() + WAIT_SECONDS
                row = None
                while time.monotonic() < deadline:
                    row = child_state()
                    if row and row[0].lower() == state:
                        return
                    if row and row[0].lower() == "error":
                        raise RuntimeError(f"child entered an error state: {row}")
                    time.sleep(0.1)
                raise RuntimeError(f"child never reached {state}: {row}")

            wait_child_state("parked")
            info = json.loads(cli("api-info", "--json"))
            api_url = info["base_url"]
            token = cli("api-token").strip()

            def api(method, path):
                request = urllib.request.Request(
                    api_url + path, method=method,
                    headers={"Authorization": "Bearer " + token},
                )
                try:
                    with urllib.request.urlopen(request, timeout=WAIT_SECONDS) as response:
                        payload = response.read()
                        return response.status, json.loads(payload) if payload else None
                except urllib.error.HTTPError as error:
                    return error.code, error.read().decode("utf-8", "replace")

            def journal_spawn_commands():
                found = []
                for journal in lab.data.rglob("active.jsonl"):
                    if child not in journal.parts:
                        continue
                    for line in journal.read_text().splitlines():
                        if "command_queued" in line:
                            found += re.findall(r"subagent-spawn-[0-9a-f]+-[a-z-]+", line)[:1]
                return found

            # The first launch queued the brief once.
            assert len(journal_spawn_commands()) >= 1, "the first launch never queued the brief"
            status, body = api("POST", f"/sessions/{child}/restart")
            assert status in (200, 202, 204), (status, body)
            # Wait for the restart to settle either way, so a re-sent brief is
            # reported as such and not only as the failure it later causes.
            deadline = time.monotonic() + WAIT_SECONDS
            while (child_state() or ("",))[0].lower() not in ("running", "error"):
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"restart did not settle: {child_state()}")
                time.sleep(0.1)
            time.sleep(5)
            resent = journal_spawn_commands()
            assert not resent, f"the restart re-sent the child's initial input: {resent}"
            wait_child_state("running")
            wait_worker_ready(child)
            status, view = api("GET", f"/sessions/{child}")
            assert status == 200 and not view.get("has_error"), (status, view)
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                items = database.execute(
                    "SELECT count(*) FROM materialized_transcript_items "
                    "WHERE session_id=? AND stable_id LIKE 'user:subagent-spawn-%-prompt'",
                    (child,),
                ).fetchone()[0]
            assert items == 1, items
            for log in (lab.data / "logs").rglob("*.log"):
                assert "changed immutable identity" not in log.read_text(errors="replace"), log
            print(json.dumps({
                "instance": args.instance,
                "artifacts": str(lab.root),
                "result": "passed",
                "child": child,
            }), flush=True)
            return

        def wait_for(description, observe):
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline:
                if result := observe():
                    return result
                time.sleep(0.1)
            raise RuntimeError(f"timed out waiting for {description}")

        def stored(session):
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as db:
                return db.execute(
                    "SELECT state, last_profile, native_session_id FROM sessions WHERE session_id=?",
                    (session,),
                ).fetchone()

        def metadata():
            reply = reliability_lab.daemon_request(lab.data, {
                "action": "runtime_changes",
                "arguments": {"cursor": None, "wait": False, "subagent_parents": []},
            })
            return reply["value"]["Snapshot"]["projection"]["metadata"]

        continuation = (
            "Continue the unfinished work already requested by the user, following their latest instructions. "
            "This message supplies no new approval or missing information. "
            "If a genuine decision, required approval, or external action remains necessary, explain it and stop."
        )

        # Hard-won: 2026-11-22 live GLiDE 502 outage blocked top-level typed QuotaLimit automove
        if args.top_level_quota_only:
            def published_quota():
                reports = metadata()["quotas"]["reports"]
                return all(
                    any(
                        window["duration_seconds"] == 7 * 24 * 60 * 60
                        and window["remaining_percent"] == remaining
                        and window["resets_at_epoch_seconds"] == int(reset.timestamp())
                        for window in reports.get(profile, {}).get("windows", [])
                    )
                    for profile, remaining in [("fake", 0), ("replacement", 80)]
                )

            def completed_move():
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as db:
                    row = db.execute(
                        "SELECT operation_json FROM session_moves WHERE session_id=?", (parent,)
                    ).fetchone()
                return (move if row and (move := json.loads(row[0]))["phase"] == "completed" else None)

            wait_for("both weekly quota readings", published_quota)
            cli("set-config", "--session", parent, "--key", "model", "--value", "tiny")
            before = wait_for("the worker to accept the selected model", lambda: (
                status if (status := worker_status(parent))["config"].get("model") == "tiny" else None
            ))
            _, profile, native = stored(parent)
            assert profile == "fake" and native, stored(parent)
            prompt = "Exhaust fixture quota in a top-level session."
            cli("message", "--session", parent, "--command-id", "top-level-quota", "--json", prompt)
            release(parent, prompt)
            wait_prompt(parent, continuation)
            moving = wait_for("the completed in-place quota Move", completed_move)
            assert moving["in_place"] and moving["queue_admission_finished"], moving
            assert stored(parent) == ("running", "replacement", native), stored(parent)
            assert not glide_requests, glide_requests

            during = worker_status(parent)
            assert during["store_id"] == before["store_id"], during
            command = during["active_prompt"]["command_id"]
            assert command.startswith("quota-retry-"), command
            release(parent, continuation)

            def completed_continuation():
                status = worker_status(parent)
                assessment = status.get("assessment", {})
                if assessment.get("turn_id") == command and assessment["reason"] == "request_failed":
                    assert status["continuation"]["completed_command_id"] == command, status
                    assert not status.get("active_prompt"), status
                    assert assessment["completion"]["stop_reason"] == "EndTurn", assessment
                    return assessment

            wait_for("the completed continuation to receive GLiDE's 502", completed_continuation)
            assert glide_requests == ["/v1/glide-turn-verdict"], glide_requests
            prompt_log = [
                (event["account"], event["fixture_prompt"])
                for line in (lab.runtime_root / "fake-acp.log").read_text().splitlines()
                if "fixture_prompt" in (event := json.loads(line))
            ]
            assert prompt_log == [("original", prompt), ("replacement", continuation)], prompt_log
            print(json.dumps({"instance": args.instance, "artifacts": str(lab.root),
                              "result": "passed", "continuations": 1,
                              "typed_stop_glide_requests": 0, "continuation_glide_requests": 1}),
                  flush=True)
            return

        if args.child_quota_only:
            def assigned_turn(child):
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as db:
                    row = db.execute(
                        "SELECT accepted_ordinal FROM startup_steps WHERE session_id=? "
                        "AND command_id LIKE 'subagent-spawn-%' AND accepted_ordinal IS NOT NULL",
                        (child,),
                    ).fetchone()
                    return row[0] if row else None

            def reports(child):
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as db:
                    rows = db.execute(
                        "SELECT event_json FROM mailbox_outbox WHERE target_session_id=? "
                        "AND event_key LIKE 'subagent-finished-%'",
                        (parent,),
                    ).fetchall()
                return [
                    body["report"] for (event,) in rows
                    if (body := json.loads(event)["body"])["type"] == "subagent_finished"
                    and body["report"]["child_session_id"] == child
                ]

            def spawn_quota_child(title):
                prompt = f"Exhaust fixture quota {title}."
                child = tool(parent, "spawn", {
                    "task_name": title,
                    "task_type": "straightforward",
                    "instructions": prompt,
                })["child_session_id"]
                marker = wait_prompt(child, prompt)
                wait_for(f"{title} to publish its native session", lambda: (
                    (record := stored(child))[0] == "running" and record[2]
                ))
                return child, prompt, marker

            miss, miss_prompt, _ = spawn_quota_child("with no other profile")
            recovering, recover_prompt, marker = spawn_quota_child("with a replacement")
            handed_back, handed_back_prompt, _ = spawn_quota_child("after handing back")
            _, profile, native = stored(recovering)
            assert profile == "fake" and native, stored(recovering)

            release(miss, miss_prompt)
            refusal = wait_for("the quota miss report", lambda: next(
                (report for report in reports(miss) if report.get("finished")), None
            ))
            assert refusal["state"] == "failed", refusal
            assert reset.astimezone().strftime("%Y-%m-%d %H:%M:%S") in refusal["output"], refusal
            assert "Start a new sub-agent" in refusal["output"], refusal
            wait_report(parent, miss, refusal["output"])

            add_replacement_profile()

            def published_replacement():
                metadata_snapshot = metadata()
                ready = [
                    profile for profile in metadata_snapshot["profile_capabilities"]["profiles"].values()
                    if profile["choices"]["status"] == "ready"
                    and profile["choices"]["value"].get("model") == "tiny"
                ]
                return len(ready) >= 2

            wait_for("the replacement profile to offer the child's model", published_replacement)
            report = "The child handed back its assignment before the quota stop."
            handback = tool(handed_back, "handback", {"message": report}, raw_message=True)
            if handback.startswith("Not delivered yet:"):
                tool(handed_back, "handback", {"message": report})
            release(handed_back, handed_back_prompt)
            wait_report(parent, handed_back, report)
            assert stored(handed_back)[1] == "fake", stored(handed_back)

            def loaded_replacement():
                log = marker.with_name("fixture-open.jsonl")
                opens = log.read_text().splitlines() if log.exists() else []
                return ["session/load", native, "replacement"] in [json.loads(line) for line in opens]

            result = "Continued on the replacement fixture profile."
            turn = wait_for("the original assignment receipt", lambda: assigned_turn(recovering))
            with subprocess.Popen(
                [str(args.mj.resolve()), "--instance", args.instance, "wait", "--session",
                 recovering, "--turn", str(turn), "--json", "--timeout", str(WAIT_SECONDS)],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            ) as wait:
                try:
                    release(recovering, recover_prompt)
                    wait_for("the original native conversation to load on the replacement", loaded_replacement)
                    wait_prompt(recovering, continuation)
                    assert wait.poll() is None, wait.communicate()
                    agents = tool(parent, "list_agents")["agents"]
                    listed = next(agent for agent in agents if agent["child_session_id"] == recovering)
                    assert listed["profile_id"] == "replacement", listed
                    handback = tool(recovering, "handback", {"message": result}, raw_message=True)
                    if handback.startswith("Not delivered yet:"):
                        tool(recovering, "handback", {"message": result})
                    release(recovering, continuation)
                    stdout, stderr = wait.communicate(timeout=WAIT_SECONDS)
                    assert wait.returncode == 0, (stdout, stderr)
                    assert json.loads(stdout)["outcome"] == "finished", stdout
                finally:
                    if wait.poll() is None:
                        wait.terminate()
            wait_report(parent, recovering, result)
            assert stored(recovering)[1:] == ("replacement", native), stored(recovering)
            assert all(report["state"] != "failed" for report in reports(recovering)), reports(recovering)
            print(json.dumps({"instance": args.instance, "artifacts": str(lab.root),
                              "result": "passed", "quota_children": 3}), flush=True)
            return

        model_gate.touch()
        spawned = tool(
            parent,
            "spawn",
            {
                "task_name": "fixture-child",
                "task_type": "straightforward",
                "instructions": "Return a short fixture report.",
            },
        )
        child = spawned["child_session_id"]

        # Hard-won: #1394: a parent message started the child between model and effort, before its brief.
        deadline = time.monotonic() + WAIT_SECONDS
        while not any(
            child in marker.parts
            for marker in lab.runtime_root.rglob("fixture-model-pending")
        ):
            if time.monotonic() >= deadline:
                raise RuntimeError("child never reached the held startup configuration")
            time.sleep(0.05)
        wait_worker_ready(child)
        early_message = tool(parent, "send_message", {
            "child_session_id": child, "message": "Follow-up after the original brief.",
        })
        with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            key, payload = database.execute(
                "SELECT event_key, event_json FROM mailbox_outbox WHERE target_session_id=?",
                (child,),
            ).fetchone()
            initial_prompt_id = database.execute(
                "SELECT command_id FROM startup_steps WHERE session_id=? "
                "AND json_type(step_json, '$.ApiPrompt')='object' ORDER BY sequence LIMIT 1",
                (child,),
            ).fetchone()[0]
        # Force a delivery attempt through the normal serialized actor instead
        # of relying on the scheduling of the background outbox sweep.
        lab.daemon_request({
            "action": "submit_session_command",
            "arguments": {"session_id": child, "command_id": "early-child-mailbox-regression",
                          "command": {"type": "deliver_mailbox_event", "data": {"event": json.loads(payload)}}},
        }, timeout=WAIT_SECONDS)
        held_status = worker_status(child)
        assert held_status["active_prompt"] is None, held_status
        assert any(
            queued["command_id"] == initial_prompt_id
            for queued in held_status["queued_prompts"]
        ), "the worker FIFO does not contain the original brief before accepting messages"
        model_gate.unlink()
        prompt_marker = wait_prompt(child, "Return a short fixture report.")
        first_prompt = prompt_marker.read_text()
        assert "You are a Mjolnir sub-agent" not in first_prompt, first_prompt
        assert "Your report directory is" in first_prompt, first_prompt
        opening = json.loads(prompt_marker.with_name("fixture-system-prompt.jsonl").read_text().splitlines()[0])
        assert opening["mode"] == "append", opening
        standing = "\n".join(block.get("text", "") for block in opening["content"])
        assert "You are a Mjolnir sub-agent" in standing, standing
        assert "Your parent reads your report" in standing, standing
        assert "Return a short fixture report." not in standing, standing
        assert worker_status(child)["config"]["effort"] == "high"
        deadline = time.monotonic() + WAIT_SECONDS
        while True:
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                accepted = database.execute(
                    "SELECT accepted_at FROM mailbox_outbox WHERE event_key=?", (key,),
                ).fetchone()[0]
            if accepted is not None:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("parent message stayed queued after the initial prompt")
            time.sleep(0.05)
        hook = run_mailbox_hook(child)
        # Pending mail may join the original prompt's hidden context or arrive
        # at its next tool boundary. In either case the brief starts the turn.
        first_blocks = prompt_marker.with_name("fixture-prompt.jsonl").read_text().splitlines()[0]
        delivered = first_blocks + hook.get("hookSpecificOutput", {}).get("additionalContext", "")
        assert delivered.count("Follow-up after the original brief.") == 1, delivered
        hook = run_mailbox_hook(child)
        assert "Follow-up after the original brief." not in hook.get("hookSpecificOutput", {}).get("additionalContext", ""), hook
        assert early_message["status"] == "queued", early_message
        # Hard-won: 8ba3d377e: queued settings prevented steering a parent's user input.
        # Drive the real daemon and worker; the child must retain its own turn.
        def submit_parent(command_id, command):
            # This suite deliberately disables HTTP; use the daemon's normal
            # session-command transport, which forwards the same published command.
            return lab.daemon_request({
                "action": "submit_session_command",
                "arguments": {"session_id": parent, "command_id": command_id, "command": command},
            }, timeout=WAIT_SECONDS)

        submit_parent("hold-parent-settings", {"type": "prompt", "data": {
            "prompt": [{"type": "text", "text": "Hold the parent turn for queued settings."}],
        }})
        wait_prompt(parent, "Hold the parent turn for queued settings.")
        child_before = worker_status(child)["active_prompt"]
        assert child_before is not None
        submit_parent("parent-model-settings", {"type": "set_config", "data": {"key": "model", "value": "subtle-fixture"}})
        submit_parent("parent-effort-settings", {"type": "set_config", "data": {"key": "effort", "value": "low"}})
        submit_parent("parent-after-settings", {"type": "prompt", "data": {
            "prompt": [{"type": "text", "text": "Run after the pending settings."}],
        }})
        controls = worker_status(parent)["turn_controls"]
        assert controls["primary"]["kind"] == "apply_queued", controls
        assert controls["interrupt"]["type"] == "cancel_turn_for", controls
        command = controls["primary"]["command"]
        applied = lab.daemon_request({
            "action": "submit_session_command",
            "arguments": {"session_id": parent, "command_id": "apply-queued-settings-regression", "command": command},
        }, timeout=WAIT_SECONDS)
        assert applied["reply"] == "ordinal", applied
        wait_prompt(parent, "Run after the pending settings.")
        parent_after = worker_status(parent)
        child_after = worker_status(child)
        assert child_after["active_prompt"] == child_before, (child_before, child_after)
        assert parent_after["config"]["model"] == "subtle-fixture", parent_after
        assert parent_after["config"]["effort"] == "low", parent_after
        assert not parent_after["queued_prompts"], parent_after
        # The same stale control may not stop this replacement parent turn.
        try:
            lab.daemon_request({
                "action": "submit_session_command",
                "arguments": {"session_id": parent, "command_id": "stale-apply-settings-regression", "command": command},
            }, timeout=WAIT_SECONDS)
        except reliability_lab.ScenarioFailure as error:
            assert "changed" in str(error).lower(), error
        else:
            raise AssertionError("stale Apply queued was accepted for a later turn")
        assert worker_status(parent)["active_prompt"] == parent_after["active_prompt"]
        # Restore the original idle-parent setup before the existing wait tests.
        # A wait attached to this fixture turn would correctly be interrupted
        # when the fixture finishes, rather than consuming the child's report.
        submit_parent("settle-parent-settings-case", parent_after["turn_controls"]["interrupt"])
        deadline = time.monotonic() + WAIT_SECONDS
        while worker_status(parent)["active_prompt"] is not None:
            if time.monotonic() >= deadline:
                raise RuntimeError("parent settings fixture did not settle after interruption")
            time.sleep(0.05)
        assert worker_status(child)["active_prompt"] == child_before
        # Handback travels through the child's durable queue while its turn runs.
        tool(child, "handback", {"message": "first report without a dashboard"})
        release(child, "Return a short fixture report.")
        wait_report(parent, child, "first report without a dashboard")
        # Older workers receive immediate guidance without collecting reports.
        for action in ("wait_agents", "await_children"):
            legacy_reply = tool(parent, action)
            assert legacy_reply["status"] == "reports_arrive_as_messages", legacy_reply
            assert "end your turn" in legacy_reply["next_action"], legacy_reply

        wait_parked(child)
        # Accepted child work and reports survive a daemon replacement.
        tool(
            parent,
            "send_message",
            {"child_session_id": child, "message": "second turn"},
        )
        wait_prompt(child, "second turn")
        time.sleep(1)
        started = time.monotonic()
        cli("daemon", "restart")
        elapsed = time.monotonic() - started
        assert elapsed < 30, (
            f"daemon replacement waited for a worker turn: {elapsed}"
        )
        message = tool(
            parent,
            "send_message",
            {"child_session_id": child, "message": "after daemon replacement"},
        )
        assert message["via"] == "mailbox", message
        assert message["status"] == "queued", (
            "a child message is queued without cancelling its turn"
        )
        assert message["mailbox_delivery"] == "tool_boundary", message
        assert "note" not in message, message
        active_before_hook = worker_status(child)["active_prompt"]
        assert active_before_hook is not None
        wait_mailbox_acceptance(child, "after daemon replacement")
        hook = run_mailbox_hook(child)
        additional_context = hook.get("hookSpecificOutput", {}).get(
            "additionalContext", ""
        )
        assert "after daemon replacement" in additional_context, hook
        assert worker_status(child)["active_prompt"] == active_before_hook, (
            "OpenAI-backed Codex receives the message at a tool boundary without ending its turn"
        )
        deadline = time.monotonic() + WAIT_SECONDS
        while time.monotonic() < deadline:
            listed = tool(parent, "list_agents")
            deliveries = listed["agents"][0].get("message_deliveries", [])
            if any(
                delivery["request_id"] == message["request_id"]
                and delivery["status"] == "delivered"
                for delivery in deliveries
            ):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f"child mailbox message was not delivered: {listed}")
        assert listed["agents"][0]["state"] == "running", (
            "send_message must not cancel the child's current turn"
        )
        tool(
            child,
            "handback",
            {"message": "second report across daemon replacement"},
        )
        release(child, "second turn")
        wait_report(parent, child, "second report across daemon replacement")
        tool(parent, "list_agents")
        wait_parked(child)

        # Delegation above works with the viewer disabled. Usage is an HTTP
        # command, so enable its API for the accounting checks below.
        config.write_text(config.read_text().replace("[phone]\nenabled = false", "[phone]\nenabled = true"))
        cli("daemon", "restart")
        deadline = time.monotonic() + WAIT_SECONDS
        while time.monotonic() < deadline:
            all_session_ids = listed_session_ids("--all")
            if child in all_session_ids:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f"mj sessions --all did not list child {child}")
        assert child not in listed_session_ids(), "mj sessions hides sub-agents by default"

        usage = json.loads(cli("usage", "--parent", parent, "--json"))
        child_usage = json.loads(cli("usage", "--session", child, "--json"))
        assert child_usage["coverage"]["full_turn_reports"] >= 2, child_usage
        assert child_usage["totals"]["total_tokens"]["tokens"] >= 240, child_usage
        assert any(group["model"] == "tiny" and group["effort"] == "high"
                   for group in usage["by_model"]), usage

        # The old worker wire shape remains supported for requests accepted
        # before an upgrade, including exact source ranges and parent context.
        (lab.project / "legacy.rs").write_text("excluded\nlegacy source evidence\nexcluded\n")
        deadline = time.monotonic() + WAIT_SECONDS
        while time.monotonic() < deadline:
            sessions = json.loads(cli("sessions", "--json"))
            rows = sessions if isinstance(sessions, list) else sessions.get("sessions", [])
            record = next((row for row in rows if row.get("id") == parent), {})
            if record.get("is_idle"):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f"parent did not finish its report wake turn: {record}")
        cli("put-file", "--session", parent, "--path", "legacy.rs", str(lab.project / "legacy.rs"))

        # Another session reads what this one cites: a workspace file, and a
        # sub-agent's report directory as a zip, each copied under its own
        # target path; a path outside what a session shares is refused (#1327).
        fetched_root = lab.root / "fetched"

        def fetch(path, *options):
            return subprocess.run(
                [str(args.mj.resolve()), "--instance", args.instance, "fetch",
                 "--session", parent, "--path", path, *options],
                env={**env, "TMPDIR": str(fetched_root)},
                capture_output=True,
                text=True,
                timeout=WAIT_SECONDS,
            )

        legacy_text = "excluded\nlegacy source evidence\nexcluded\n"
        fetched = fetch("legacy.rs")
        assert fetched.returncode == 0, fetched.stderr
        local = Path(fetched.stdout.strip())
        # The copy sits under the file's own path on the target, which here is
        # this host, so that path names the session's file too.
        source = Path("/") / local.relative_to(fetched_root / "mjolnir" / parent)
        assert source.name == "legacy.rs" and source.read_text() == legacy_text, source
        assert local.read_text() == legacy_text
        streamed = fetch("legacy.rs", "--stdout")
        assert streamed.returncode == 0 and streamed.stdout == legacy_text, streamed
        report_dir = Path(spawned["report_dir"])
        (report_dir / "probes").mkdir(parents=True, exist_ok=True)
        (report_dir / "probes" / "probe.log").write_bytes(b"probe\n" * 20000)
        fetched = fetch(str(report_dir), "--json")
        assert fetched.returncode == 0, fetched.stderr
        reports = json.loads(fetched.stdout)
        assert reports["kind"] == "directory" and reports["path"].endswith(".zip"), reports
        with zipfile.ZipFile(reports["path"]) as archive:
            probe = archive.read(f"{report_dir.name}/probes/probe.log")
        assert probe == b"probe\n" * 20000
        listed = fetch(str(report_dir / "probes"), "--ls", "--json")
        assert listed.returncode == 0, listed.stderr
        entries = json.loads(listed.stdout)["entries"]
        assert [(entry["name"], entry["kind"], entry["size"]) for entry in entries] == [
            ("probe.log", "file", 120000)
        ], entries
        assert entries[0]["modified"].endswith("Z"), entries
        refused = fetch("/etc/passwd")
        assert refused.returncode != 0, refused.stdout
        assert "outside the directories a session shares" in refused.stderr, refused.stderr

        # Fetch is available through the real MCP server with delegation off
        # or native. The reply stays small for payloads larger than the old cap.
        binary = bytes(range(256)) * (17 * 1024 * 1024 // 256)
        large_source = source.with_name("fetch binary λ.dat")
        large_source.write_bytes(binary)
        for policy in ["none", "native"]:
            receiver = json.loads(cli(
                "new", "--workspace", "Delegation test", "--profile", "fake",
                "--target", "localhost", "--project-directory", str(lab.project),
                "--subagents", policy, "--json",
            ))["session_id"]
            receiver_socket = endpoint(receiver)

            def mcp_fetch(path, ls=False):
                requests = [
                    {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                        "name": "fetch", "arguments": {
                            "session_id": parent, "path": path, "ls": ls,
                        },
                    }},
                ]
                response = subprocess.run(
                    [str(args.worker.resolve()), "worker", "subagent-mcp",
                     "--socket", str(receiver_socket), "--role", "message_only"],
                    env=env, input="".join(json.dumps(r) + "\n" for r in requests),
                    capture_output=True, text=True, timeout=WAIT_SECONDS,
                )
                assert response.returncode == 0, response.stderr
                replies = {r["id"]: r for r in map(json.loads, response.stdout.splitlines())}
                assert {t["name"] for t in replies[1]["result"]["tools"]} == {"send_message", "fetch"}
                assert "error" not in replies[2], replies[2]
                return replies[2]["result"]

            copied = mcp_fetch(large_source.name)
            assert not copied["isError"], copied
            saved = copied["structuredContent"]
            assert saved["bytes"] == len(binary) and saved["kind"] == "file", saved
            assert Path(saved["path"]).read_bytes() == binary
            assert Path(saved["path"]) != large_source
            assert len(json.dumps(copied)) < 2048
            Path(saved["path"]).unlink()
            archived = mcp_fetch(str(report_dir / "probes"))
            assert not archived["isError"], archived
            saved_zip = archived["structuredContent"]
            assert saved_zip["kind"] == "directory" and saved_zip["path"].endswith(".zip")
            with zipfile.ZipFile(saved_zip["path"]) as archive:
                assert archive.read("probes/probe.log") == b"probe\n" * 20000
            Path(saved_zip["path"]).unlink()
            listing = mcp_fetch(str(report_dir / "probes"), ls=True)
            assert not listing["isError"], listing
            assert listing["structuredContent"]["entries"] == entries
            refused = mcp_fetch("/etc/passwd")
            assert refused["isError"] and "outside the directories a session shares" in json.dumps(refused)
            cli("destroy", "--session", receiver)
        large_source.unlink()

        legacy = tool(parent, "spawn", {
            "task_name": "legacy-accepted",
            "instructions": "Return the legacy attachment evidence.",
            "context": "retained parent context",
            "files": [{"file": "legacy.rs", "ranges": [{"start": 2, "end": 2}]}],
        })["child_session_id"]
        wait_prompt(legacy, "legacy source evidence")
        wait_prompt(legacy, "retained parent context")
        tool(legacy, "handback", {"message": "legacy content retained"})
        release(legacy, "Return the legacy attachment evidence.")
        wait_report(parent, legacy, "legacy content retained")
        wait_parked(legacy)

        # Both global task types are resolved into the frozen child selection.
        subtle = tool(parent, "spawn", {"task_name": "subtle-child", "instructions": "Return a judgment report.", "task_type": "subtle"})["child_session_id"]
        wait_prompt(subtle, "Return a judgment report.")
        with sqlite3.connect(lab.data / "mj.sqlite3") as database:
            for session, model, effort in [(child, "tiny", "high"), (subtle, "subtle-fixture", "low")]:
                row = database.execute("SELECT json_extract(record_json, '$.model'), json_extract(record_json, '$.effort') FROM subagent_sessions WHERE child_session_id=?", (session,)).fetchone()
                assert row == (model, effort), (session, row)
        tool(subtle, "handback", {"message": "subtle configured selection verified"})
        release(subtle, "Return a judgment report.")
        wait_report(parent, subtle, "subtle configured selection verified")
        wait_parked(subtle)

        # Simulate a daemon exiting after it records failed startup but before
        # confirming teardown. Recovery must stop and settle it, not start it.
        cli("daemon", "stop")
        with sqlite3.connect(lab.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='startup-cleanup', last_error='fixture startup failure' WHERE session_id=?", (child,))
        cli("daemon", "restart")
        deadline = time.monotonic() + WAIT_SECONDS
        while time.monotonic() < deadline:
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                row = database.execute("SELECT state, archived FROM sessions WHERE session_id=?", (child,)).fetchone()
            if row == ("error", 1):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f"startup teardown did not settle after restart: {row}")
        assert child_usage == json.loads(cli("usage", "--session", child, "--json"))

        # Hard-won: d3312d34: a failed child's report turn can remain unfinished during destroy.
        # Hard-won: #1302: startup cleanup can settle after initial report reconciliation.
        wait_report(parent, child, "fixture startup failure")
        # From the user's shell, `mj message` is the user's own prompt, and a
        # retry with the same command id names the same turn, not a second one.
        message_text = "check the last change from the user"
        send_message = ("message", "--session", parent, "--command-id",
                        "user-message-check", "--json", message_text)
        message = json.loads(cli(*send_message))
        assert message == {"session_id": parent, "via": "turn",
                           "turn_id": message["turn_id"]}, message
        marker = wait_prompt(parent, message_text)
        assert json.loads(cli(*send_message)) == message
        wait_recorded_parent_turn()
        received = marker.read_text()
        assert received.count(message_text) == 1, received
        assert "Message from the user:" not in received, received

        retained = json.loads(cli("usage", "--parent", parent, "--json"))
        for session in [child, legacy, subtle, parent]:
            cli("destroy", "--session", session)
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline:
                with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                    exists = database.execute("SELECT 1 FROM sessions WHERE session_id=?", (session,)).fetchone()
                if not exists:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"destroy did not remove {session}")
        after = json.loads(cli("usage", "--parent", parent, "--json"))
        for key in ["totals", "coverage", "by_model"]:
            assert after[key] == retained[key], (key, retained, after)
        assert {s["session_id"] for s in after["sessions"]} == {parent, child, legacy, subtle}, after
        assert all(not s["operational_session_present"] for s in after["sessions"]), after
        cli("daemon", "restart")
        assert after == json.loads(cli("usage", "--parent", parent, "--json"))
        # One task uses the session override while the other falls back to global config.
        cli("daemon", "stop")
        config.write_text(config.read_text().split("\n[subagents.straightforward]")[0]
                          + '\n[subagents.straightforward]\nmodel = "tiny"\neffort = "high"\n')
        cli("daemon", "restart")
        overridden = json.loads(cli("new", "--workspace", "Delegation test", "--profile", "fake",
            "--target", "localhost", "--project-directory", str(lab.project),
            "--subagent-subtle", "tiny", "--subagent-subtle-effort", "low", "--json"))["session_id"]
        for task_type in ["straightforward", "subtle"]:
            override_child = tool(overridden, "spawn", {"task_name": f"override-{task_type}",
                "instructions": "Report the session override.", "task_type": task_type})["child_session_id"]
            wait_prompt(override_child, "Report the session override.")
            with sqlite3.connect(lab.data / "mj.sqlite3") as database:
                row = database.execute("SELECT json_extract(record_json, '$.model'), json_extract(record_json, '$.effort') FROM subagent_sessions WHERE child_session_id=?", (override_child,)).fetchone()
                expected = ("tiny", "high") if task_type == "straightforward" else ("tiny", "low")
                assert row == expected, (task_type, expected, row)
            tool(override_child, "handback", {"message": f"session override verified for {task_type}"})
            release(override_child, "Report the session override.")
            wait_report(overridden, override_child, f"session override verified for {task_type}")
            wait_parked(override_child)
            cli("destroy", "--session", override_child)
        cli("destroy", "--session", overridden)
        print(
            json.dumps(
                {
                    "instance": args.instance,
                    "parent": parent,
                    "child": child,
                    "handoff_seconds": elapsed,
                    "artifacts": str(lab.root),
                    "result": "passed",
                }
            ),
            flush=True,
        )
    finally:
        try:
            if args.unanswered_wake_only and cli is not None:
                metadata = lab.data / "daemon.json"
                pid = json.loads(metadata.read_text())["pid"] if metadata.exists() else None
                cli("daemon", "stop", check=False)
                if pid is not None:
                    deadline = time.monotonic() + WAIT_SECONDS
                    while reliability_lab.process_is_running(pid) and time.monotonic() < deadline:
                        time.sleep(0.05)
                    assert not reliability_lab.process_is_running(pid), f"test daemon {pid} survived stop"
            elif (quota_only or args.missing_child_cwd_only) and cli is not None:
                cli("daemon", "stop", check=False)
        finally:
            for server in (catalog_proxy, glide_proxy):
                if server is not None:
                    server.shutdown()
                    server.server_close()
            lab.cleanup_owned()
        if quota_only:
            assert not (leak := lab.leak_report()), leak


if __name__ == "__main__":
    main()
