#!/usr/bin/env python3
"""Delegation without web access, then durable accounting, in a named instance."""

import argparse
import concurrent.futures
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import time
import uuid

import reliability_lab


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named test instance")
    lab = reliability_lab.Lab(args.mj, "delegation", 1171)
    try:
        lab.prepare(fake_acp_prompt_delay_ms=10000)
        bridge = lab.runtime_root / "fake_acp.py"
        script = bridge.read_text().replace(
            'session_id = "reliability-native"',
            'session_id = __import__("uuid").uuid4().hex',
        )
        options = [
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "tiny",
                "options": [{"value": "tiny", "name": "Tiny fixture"}],
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
            "options": [{"value": "high", "name": "High fixture"}],
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
        # This fixture never owns a native goal. Report that alongside every
        # execution update, including after loading a parked conversation.
        script = script.replace(
            "def report_execution(status, goal_cleared=False):",
            "def report_execution(status, goal_cleared=True):",
        )
        script = script.replace(
            '        report_execution("running")',
            '        report_execution("running")\n        with open(os.path.join(os.environ["CODEX_HOME"], "fixture-prompt.txt"), "a") as marker:\n            marker.write(text + "\\n")',
        )
        script = script.replace(
            "        if wait_for_prompt_cancel():",
            '        if "second turn" in text:\n            os.environ["MJ_FAKE_ACP_PROMPT_DELAY_MS"] = "15000"\n        if wait_for_prompt_cancel():',
        )
        compile(script, str(bridge), "exec")
        bridge.write_text(script)
        env = lab.environment()
        env.update(
            MJ_INSTANCE=args.instance, MJ_WORKER_BINARY=str(args.worker.resolve())
        )

        def cli(*command):
            result = subprocess.run(
                [str(args.mj.resolve()), "--instance", args.instance, *command],
                env=env,
                capture_output=True,
                text=True,
                timeout=90,
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
            if result.returncode:
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
                "single-model",
                "--subagent-model",
                "tiny",
                "--subagent-effort",
                "high",
                "--json",
            )
        )
        parent = created["session_id"]

        def endpoint(session):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                paths = [
                    p for p in lab.data.rglob("subagents.sock") if session in p.parts
                ]
                if paths:
                    return paths[0]
                time.sleep(0.05)
            raise RuntimeError(f"no worker tool socket for {session}")

        def tool(session, action, params=None):
            request = {
                "request_id": uuid.uuid4().hex,
                "created_at_ms": int(time.time() * 1000),
                "action": {"action": action},
            }
            if params is not None:
                request["action"]["params"] = params
            started = time.monotonic()
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(60)
                connection.connect(str(endpoint(session)))
                connection.sendall(json.dumps(request).encode() + b"\n")
                reply = json.loads(connection.makefile("rb").readline())
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
            assert not result["is_error"], result
            return json.loads(result["message"])

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
                timeout=20,
            )
            if result.returncode:
                raise RuntimeError(
                    f"mailbox hook failed: {result.stderr or result.stdout}"
                )
            return json.loads(result.stdout)

        endpoint(parent)
        deadline = time.monotonic() + 60
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
        config = lab.config / "config.toml"
        config.write_text(
            config.read_text().replace(
                "[phone]\nenabled = true", "[phone]\nenabled = false"
            )
        )
        cli("daemon", "restart")
        assert "disabled" in cli("daemon", "status").lower()
        tool(parent, "list_agents")
        spawned = tool(
            parent,
            "spawn",
            {
                "task_name": "fixture-child",
                "instructions": "Return a short fixture report.",
            },
        )
        child = spawned["child_session_id"]

        def wait_prompt(session, text):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                # The fake ACP agent appends each prompt to a file in its
                # session-private CODEX_HOME, which lies under the session id.
                for marker in lab.runtime_root.rglob("fixture-prompt.txt"):
                    if session in marker.parts and text in marker.read_text():
                        return
                time.sleep(0.05)
            raise RuntimeError(f"worker did not begin prompt: {text}")

        wait_prompt(child, "Return a short fixture report.")
        # Handback travels through the child's durable queue while its turn runs.
        tool(child, "handback", {"message": "first report without a dashboard"})
        first = tool(parent, "wait_agents")
        assert first["status"] == "reported", first
        assert first["agents"][0]["output"] == "first report without a dashboard", first

        def wait_parked(session):
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                with sqlite3.connect(
                    f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True
                ) as database:
                    row = database.execute(
                        "SELECT state FROM sessions WHERE session_id = ?", (session,)
                    ).fetchone()
                if row and row[0].lower() == "parked":
                    return
                time.sleep(0.1)
            raise RuntimeError(f"child completion did not park {session}: {row}")

        wait_parked(child)
        # A wait on a parked child whose report is already delivered answers
        # at once, not after its timeout.
        asked = time.monotonic()
        parked = tool(parent, "wait_agents")
        parked_seconds = time.monotonic() - asked
        assert parked["status"] == "nothing_to_wait_for", parked
        assert parked["agents"][0]["output"] is None, parked
        assert parked_seconds < 5, f"wait on a parked child took {parked_seconds}"
        # Wait always watches the parent's children and answers immediately
        # when no new report or unfinished child remains.
        idless = tool(parent, "wait_agents")
        assert idless["status"] == "nothing_to_wait_for", idless
        assert [agent["child_session_id"] for agent in idless["agents"]] == [child], idless
        assert idless["agents"][0]["output"] is None, idless
        # Submit the next wait while send_message is still starting the parked
        # child. It must not answer with the old report or return early, and
        # the daemon must not re-park the child under the input. Then replace
        # only this instance's daemon: workers and their accepted input
        # survive, and the replacement rebuilds delegation.
        with concurrent.futures.ThreadPoolExecutor() as pool:
            tool(
                parent,
                "send_message",
                {"child_session_id": child, "message": "second turn"},
            )
            waiting = pool.submit(
                tool,
                parent,
                "wait_agents",
            )
            wait_prompt(child, "second turn")
            assert not waiting.done(), "wait answered before the second report"
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
            hook = run_mailbox_hook(child)
            additional_context = hook.get("hookSpecificOutput", {}).get(
                "additionalContext", ""
            )
            assert "after daemon replacement" in additional_context, hook
            deadline = time.monotonic() + 15
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
            second = waiting.result(timeout=40)
            assert second["status"] == "reported", second
            assert second["agents"][0]["output"] == (
                "second report across daemon replacement"
            ), second
        tool(parent, "list_agents")
        wait_parked(child)
        # Delegation above works with the viewer disabled. Usage is an HTTP
        # command, so enable its API for the accounting checks below.
        config.write_text(config.read_text().replace("[phone]\nenabled = false", "[phone]\nenabled = true"))
        cli("daemon", "restart")
        usage = json.loads(cli("usage", "--parent", parent, "--json"))
        child_usage = json.loads(cli("usage", "--session", child, "--json"))
        assert child_usage["coverage"]["full_turn_reports"] >= 2, child_usage
        assert child_usage["totals"]["total_tokens"]["tokens"] >= 240, child_usage
        assert any(group["model"] == "tiny" and group["effort"] == "high"
                   for group in usage["by_model"]), usage

        # The old worker wire shape remains supported for requests accepted
        # before an upgrade, including exact source ranges and parent context.
        (lab.project / "legacy.rs").write_text("excluded\nlegacy source evidence\nexcluded\n")
        cli("put-file", "--session", parent, "--path", "legacy.rs", str(lab.project / "legacy.rs"))
        legacy = tool(parent, "spawn", {
            "task_name": "legacy-accepted",
            "instructions": "Return the legacy attachment evidence.",
            "context": "retained parent context",
            "files": [{"file": "legacy.rs", "ranges": [{"start": 2, "end": 2}]}],
        })["child_session_id"]
        wait_prompt(legacy, "legacy source evidence")
        wait_prompt(legacy, "retained parent context")
        tool(legacy, "handback", {"message": "legacy content retained"})
        tool(parent, "wait_agents")
        wait_parked(legacy)

        # Simulate a daemon exiting after it records failed startup but before
        # confirming teardown. Recovery must stop and settle it, not start it.
        cli("daemon", "stop")
        with sqlite3.connect(lab.data / "mj.sqlite3") as database:
            database.execute("UPDATE sessions SET state='startup-cleanup', last_error='fixture startup failure' WHERE session_id=?", (child,))
        cli("daemon", "restart")
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                row = database.execute("SELECT state, archived FROM sessions WHERE session_id=?", (child,)).fetchone()
            if row == ("error", 1):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError(f"startup teardown did not settle after restart: {row}")
        assert child_usage == json.loads(cli("usage", "--session", child, "--json"))

        # Hard-won: d3312d34: a failed child queues a parent prompt that can
        # otherwise become an unfinished turn while the parent is destroyed.
        startup_failure = tool(parent, "wait_agents")
        assert startup_failure["status"] == "reported", startup_failure
        assert any(
            agent["child_session_id"] == child
            and agent["output"] == "fixture startup failure"
            for agent in startup_failure["agents"]
        ), startup_failure
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                unfinished = database.execute(
                    "SELECT COUNT(*) FROM session_turn_selections s "
                    "WHERE session_id=? AND NOT EXISTS("
                    "SELECT 1 FROM session_turn_usage u "
                    "WHERE u.session_id=s.session_id AND u.command_id=s.command_id)",
                    (parent,),
                ).fetchone()[0]
            if unfinished == 0:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("parent wait prompt did not finish before accounting checks")

        retained = json.loads(cli("usage", "--parent", parent, "--json"))
        for session in [child, legacy, parent]:
            cli("destroy", "--session", session)
            deadline = time.monotonic() + 30
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
        assert {s["session_id"] for s in after["sessions"]} == {parent, child, legacy}, after
        assert all(not s["operational_session_present"] for s in after["sessions"]), after
        cli("daemon", "restart")
        assert after == json.loads(cli("usage", "--parent", parent, "--json"))
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
        lab.cleanup_owned()


if __name__ == "__main__":
    main()
