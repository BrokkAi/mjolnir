#!/usr/bin/env python3
"""Minimal newline-delimited JSON-RPC client for the site_context server.

usage: probe_client.py [--site SITE ...] -- COMMAND [ARG ...]
Starts COMMAND (the wrapper locally, or `ssh ...` for the real path), sends initialize,
notifications/initialized and tools/list, then one tools/call per --site, printing every
response. Exit status is the server's, so a wrapper refusal (64, 65) is visible.
"""
import json
import subprocess
import sys


def main(argv):
    sites, command = [], []
    it = iter(argv[1:])
    for arg in it:
        if arg == "--site":
            sites.append(next(it))
        elif arg == "--":
            command = list(it)
        else:
            sys.exit(__doc__)
    if not command:
        sys.exit(__doc__)
    proc = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    next_id = 0

    def rpc(method, params):
        nonlocal next_id
        next_id += 1
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params}) + "\n")
        proc.stdin.flush()
        line = proc.stdout.readline()
        if not line:
            raise SystemExit(f"server closed the pipe during {method}; exit {proc.wait()}")
        return json.loads(line)

    print("initialize ->", json.dumps(rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                                          "clientInfo": {"name": "probe_client", "version": "1"}})))
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    proc.stdin.flush()
    print("tools/list ->", json.dumps(rpc("tools/list", {})))
    for site in sites:
        response = rpc("tools/call", {"name": "site_context", "arguments": {"site": site}})
        print(f"\n=== site_context({site!r})")
        for item in response["result"]["content"]:
            print(item["text"])
    proc.stdin.close()
    sys.exit(proc.wait())


if __name__ == "__main__":
    main(sys.argv)
