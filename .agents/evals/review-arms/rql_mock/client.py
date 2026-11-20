"""Connect a writer's MCP stdio to its SSH endpoint or check the connection."""

import argparse
import json
import os
import re
import subprocess
import sys


# The overlay gives MCP startup this same timeout. The explicit preflight also
# needs a deadline because it runs outside Codex.
STARTUP_SECONDS = 60


def connect_args(session, key):
    return [
        "ssh", "-F", os.environ["RQL_SSH_CONFIG"],
        "-o", f'UserKnownHostsFile="{os.environ["RQL_SSH_KNOWN_HOSTS"]}"',
        "-i", key, "--", os.environ["RQL_SSH_TARGET"], session,
    ]


def preflight(command):
    messages = [
        {"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "rql-preflight", "version": "1"},
        }},
        {"method": "notifications/initialized"},
        {"id": 2, "method": "tools/list"},
        {"id": 3, "method": "ping"},
    ]
    request = "".join(json.dumps({"jsonrpc": "2.0", **message}) + "\n" for message in messages)
    completed = subprocess.run(
        command, input=request, capture_output=True, encoding="utf-8", timeout=STARTUP_SECONDS,
    )
    if completed.returncode:
        raise ValueError(completed.stderr.strip() or f"SSH exited {completed.returncode}")
    replies = {response["id"]: response["result"]
               for response in map(json.loads, completed.stdout.splitlines())}
    tools = [tool["name"] for tool in replies[2]["tools"]]
    if replies[1]["serverInfo"]["name"] != "rql_mock" or tools != ["rql_query"] or replies[3] != {}:
        raise ValueError("unexpected MCP handshake")
    return tools


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--preflight", action="store_true")
    try:
        options = parser.parse_args()
        session = os.environ.get("MJ_SESSION_ID", "")
        if not re.fullmatch(r"[0-9a-f]{32}", session):
            raise ValueError("expected a Mjolnir session ID")
        key = os.environ["RQL_SSH_KEY"]
        if not os.access(key, os.R_OK):
            raise ValueError("writer key has not been published")
        command = connect_args(session, key)
        if options.preflight:
            print(json.dumps({"session_id": session, "tools": preflight(command)}))
        else:
            os.execvp(command[0], command)
    except (OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as error:
        sys.exit(f"rql client: {error}")


if __name__ == "__main__":
    main()
