#!/usr/bin/env python3
"""Record, from inside an engine container, whether non-provider hosts are reachable.

The engine's `seal_probe` hook runs this before any prompt. mj's Podman targets have no
network option, so provider-only egress can't be enforced for the pilot (study plan, Lean
pilot: egress is an unenforced gap the readout states). This probe therefore measures it
rather than enforcing it: it appends one JSON line per engine container to the evidence log,
and exits 0. It exits non-zero only if it can't run the check or write the record, so a
missing measurement fails visibly. The audit separately voids any engine session that made a
network request.

Usage: record_egress.py --out <evidence.jsonl> <container>
"""
import argparse
from datetime import datetime, timezone
import fcntl
import json
from pathlib import Path
import subprocess

# Hosts an engine has no reason to reach. Reaching any of them means a session could fetch
# real history; github.com would hold every repo's later commits.
HOSTS = (("github.com", 443), ("gitlab.com", 443), ("crates.io", 443))

CHECK = """
import json, socket, sys
out = {}
for host, port in json.loads(sys.argv[1]):
    try:
        socket.create_connection((host, port), timeout=5).close()
        out[host] = True
    except OSError as error:
        out[host] = f"unreachable: {error.__class__.__name__}"
print(json.dumps(out))
"""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("container")
    args = parser.parse_args()
    proc = subprocess.run(["podman", "exec", args.container, "python3", "-c", CHECK, json.dumps(HOSTS)],
                          capture_output=True, timeout=120)
    if proc.returncode:
        raise SystemExit(f"egress check failed in {args.container}: {proc.stderr.decode(errors='replace').strip()}")
    reachable = json.loads(proc.stdout)
    record = {"time": datetime.now(timezone.utc).isoformat(), "container": args.container,
              "reachable": reachable, "enforced": False}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("a") as log:
        fcntl.flock(log, fcntl.LOCK_EX)
        log.write(json.dumps(record, sort_keys=True) + "\n")
        log.flush()


if __name__ == "__main__":
    main()
