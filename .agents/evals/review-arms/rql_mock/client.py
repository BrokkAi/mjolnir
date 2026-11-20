"""Serve an empty MCP catalog locally or connect an assigned writer over SSH."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from protocol import serve


# The overlay gives MCP startup this same timeout. The explicit preflight also
# needs a deadline because it runs outside Codex.
STARTUP_SECONDS = 60


def assigned_arm(session):
    assignment_dir = os.environ.get("RQL_SESSION_ASSIGNMENT_DIR")
    if not session or not assignment_dir:
        return None
    if not re.fullmatch(r"[0-9a-f]{32}", session):
        raise ValueError("expected a Mjolnir session ID")
    try:
        path = Path(assignment_dir) / f"{session}.json"
        assignment = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    arm = assignment["arm"]
    if assignment["session_id"] != session or arm not in {"none", "R", "R-loc"}:
        raise ValueError("invalid RQL assignment for this session")
    return arm


def connect_args(session, key):
    return [
        "ssh", "-F", os.environ["RQL_SSH_CONFIG"],
        "-o", f'UserKnownHostsFile="{os.environ["RQL_SSH_KNOWN_HOSTS"]}"',
        "-i", key, "--", os.environ["RQL_SSH_TARGET"], session,
    ]


def preflight(command, expected_tools):
    messages = [
        {"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "rql-preflight", "version": "1"},
        }},
        {"method": "notifications/initialized"},
        {"id": 3, "method": "ping"},
        {"id": 2, "method": "tools/list"},
    ]
    request = "".join(json.dumps({"jsonrpc": "2.0", **message}) + "\n" for message in messages)
    completed = subprocess.run(
        command, input=request, capture_output=True, encoding="utf-8", timeout=STARTUP_SECONDS,
    )
    if completed.returncode:
        raise ValueError(completed.stderr.strip() or f"MCP process exited {completed.returncode}")
    replies = {response["id"]: response["result"]
               for response in map(json.loads, completed.stdout.splitlines())}
    tools = [tool["name"] for tool in replies[2]["tools"]]
    if replies[1]["serverInfo"]["name"] != "rql_mock" or tools != expected_tools or replies[3] != {}:
        raise ValueError("unexpected MCP handshake")
    return tools


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--preflight", action="store_true")
    try:
        options = parser.parse_args()
        session = os.environ.get("MJ_SESSION_ID", "")
        arm = assigned_arm(session)
        has_tool = arm in {"R", "R-loc"}
        if options.preflight:
            tools = preflight([sys.executable, "-B", __file__], ["rql_query"] if has_tool else [])
            print(json.dumps({"session_id": session, "tools": tools}))
        elif not has_tool:
            # A launch can publish its record after Codex starts. Reconnect
            # replaces this stopped client with one that reads the record.
            serve([], close_after_list=bool(session) and arm is None)
        else:
            key = os.environ["RQL_SSH_KEY"]
            if not os.access(key, os.R_OK):
                raise ValueError("writer key has not been published")
            command = connect_args(session, key)
            os.execvp(command[0], command)
    except (OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as error:
        sys.exit(f"rql client: {error}")


if __name__ == "__main__":
    main()
