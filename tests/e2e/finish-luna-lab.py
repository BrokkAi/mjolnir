#!/usr/bin/env python3
"""Stop a Luna lab, check its database, and remove its runtime.

Run it with the artifact directory prepare-luna-lab.py printed, after the
TUIs have quit and the lab's tmux server is gone. It uses the same ownership
rule as the automated labs, so it stops workers that re-exec with a cleared
environment as well as processes that still carry the lab's environment.
The runtime is removed only when the database is intact and no owned process
survives; otherwise it stays for diagnosis.
"""

from __future__ import annotations

import argparse
import contextlib
import pathlib
import subprocess
import sys

from reliability_lab import Lab, ScenarioFailure


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("artifacts", type=pathlib.Path, help="the lab's artifact directory ($MJ_LUNA_ARTIFACTS)")
    args = parser.parse_args()

    lab = Lab.reopen(args.artifacts.resolve())
    # A daemon that does not stop in time is stopped with the other owned
    # processes below.
    with contextlib.suppress(subprocess.TimeoutExpired):
        lab.command("daemon", "stop", check=False)
    lab.cleanup_owned()
    leaks = lab.leak_report()
    (lab.root / "leaks.txt").write_text(f"{leaks}\n" if leaks else "")

    failures: list[str] = []
    if (lab.data / "mj.sqlite3").exists():
        try:
            lab.integrity()
        except ScenarioFailure as error:
            failures.append(str(error))
    else:
        (lab.root / "integrity.txt").write_text("no database: the lab never started a daemon\n")
    if leaks:
        failures.append(leaks)
    if failures:
        for failure in failures:
            print(f"luna lab not finished: {failure}", file=sys.stderr)
        print(f"runtime kept at {lab.runtime_root}", file=sys.stderr)
        return 1

    lab.remove_runtime()
    print(f"luna lab finished: database intact, no owned processes, removed {lab.runtime_root}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
