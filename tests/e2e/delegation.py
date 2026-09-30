#!/usr/bin/env python3
"""Delegation through real workers with web access disabled, in a named instance."""

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
            '        if "second turn" in text:\n            os.environ["MJ_FAKE_ACP_PROMPT_DELAY_MS"] = "60000"\n        if wait_for_prompt_cancel():',
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
        first = tool(
            parent, "wait_agents", {"child_session_ids": [child], "timeout_seconds": 30}
        )
        assert first["status"] == "complete", first
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
        parked = tool(
            parent, "wait_agents", {"child_session_ids": [child], "timeout_seconds": 30}
        )
        parked_seconds = time.monotonic() - asked
        assert parked["status"] == "complete", parked
        assert parked["agents"][0]["output"] == "first report without a dashboard", parked
        assert parked_seconds < 5, f"wait on a parked child took {parked_seconds}"
        # A wait that names no children finds nothing running and answers at
        # once, listing the finished child.
        idless = tool(parent, "wait_agents", {"timeout_seconds": 30})
        assert idless["status"] == "complete", idless
        assert [agent["child_session_id"] for agent in idless["agents"]] == [child], idless
        # Submit the next wait while send_input is still starting the parked
        # child. It must not answer with the old report or return early, and
        # the daemon must not re-park the child under the input. Then replace
        # only this instance's daemon: workers and their accepted input
        # survive, and the replacement rebuilds delegation.
        with concurrent.futures.ThreadPoolExecutor() as pool:
            tool(
                parent,
                "send_input",
                {"child_session_id": child, "message": "second turn"},
            )
            waiting = pool.submit(
                tool,
                parent,
                "wait_agents",
                {"child_session_ids": [child], "timeout_seconds": 60},
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
            tool(
                child,
                "handback",
                {"message": "second report across daemon replacement"},
            )
            interrupted = tool(parent, "interrupt_agent", {"child_session_id": child})
            assert interrupted["interrupted"], (
                "the child turn must survive daemon replacement"
            )
            second = waiting.result(timeout=40)
            assert second["status"] == "complete", second
            assert second["agents"][0]["output"] == (
                "second report across daemon replacement"
            ), second
        tool(parent, "list_agents")
        wait_parked(child)
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
