#!/usr/bin/env python3
"""A session being created when the daemon restarts is finished by the next daemon.

Build mj and mj-worker with --features test-hooks. The run uses a private named
instance; no host instance is touched. The daemon holds the create at the
`create_after_target_provisioned` hook, after the session's target exists and
before its worker starts, which makes the restart land in the middle of the
create every time. The test then checks that the restart does not wait for the
create, that the next daemon retires the partial attempt and provisions again,
and that the session ends up running with no error. A `mj wait` started before
the restart follows the session's turns, not the create wait; it only shows
that the session settles without error across the restart. The dashboard's
create wait is covered by `create_wait_tests` in mj-cli.
"""
import argparse
import json
import sqlite3
from pathlib import Path
import subprocess
import time

import reliability_lab

WAIT = 120


def wait_for(predicate, description):
    deadline = time.monotonic() + WAIT
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for {description}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named instance")
    lab = reliability_lab.Lab(args.mj, "create-survives-restart", 1548)
    try:
        lab.prepare()
        with (lab.config / "config.toml").open("a") as config:
            config.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")
        lab.hook_name = "create_after_target_provisioned"

        def environment():
            return {**lab.environment(), "MJ_INSTANCE": args.instance,
                    "MJ_WORKER_BINARY": str(args.worker.resolve())}

        def cli(*arguments):
            result = subprocess.run([str(args.mj.resolve()), "--instance", args.instance, *arguments],
                                    env=environment(), capture_output=True, text=True, timeout=WAIT)
            assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result.stdout

        def record(session):
            return json.loads(cli("sessions", "--session", session, "--json"))

        cli("workspaces", "create", "Create restart")
        session = json.loads(cli("new", "--workspace", "Create restart", "--profile", "fake",
                                 "--target", "localhost", "--project-directory", str(lab.project),
                                 "--subagents", "none", "--json"))["session_id"]
        wait_for((lab.hooks / "create_after_target_provisioned.reached").exists,
                 "the create to reach its target")
        before = record(session)
        assert before["state"] != "running", before

        # `mj wait` uses the turn-wait API, not WaitCreateSession. Started
        # before the restart, it shows only that the session settles without
        # error across the restart.
        waiter = subprocess.Popen([str(args.mj.resolve()), "--instance", args.instance,
                                   "wait", "--session", session, "--json", "--timeout", str(WAIT)],
                                  env=environment(), stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True)
        started = time.monotonic()
        restart = subprocess.Popen([str(args.mj.resolve()), "--instance", args.instance,
                                    "daemon", "restart"], env=environment(),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        # The old daemon's create is blocked in the hook; release it only
        # after the restart has begun, so the shutdown interrupts it.
        time.sleep(1.0)
        (lab.hooks / "create_after_target_provisioned.continue").touch()
        stdout, stderr = restart.communicate(timeout=WAIT)
        assert restart.returncode == 0, (stdout, stderr)
        restart_seconds = time.monotonic() - started
        # The hook holds the create for the 1.0 s above. A restart that waited
        # for the create would also wait for its clone and worker install, which
        # take far longer than the few seconds of daemon exit and start-up
        # allowed here on top of the hold.
        assert restart_seconds < 1.0 + 5.0, f"the restart waited for the create: {restart_seconds:.1f}s"

        def running():
            current = record(session)
            assert not current.get("has_error"), current
            return current["state"] == "running"
        wait_for(running, "the recovered session to run")
        waiter_out, waiter_err = waiter.communicate(timeout=WAIT)
        # Exit 0 is `finished`: the session settled without error.
        assert waiter.returncode == 0, (waiter.returncode, waiter_out, waiter_err)
        assert json.loads(waiter_out)["outcome"] == "finished", waiter_out

        database = sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True)
        intents = database.execute("SELECT count(*) FROM session_create_intents").fetchone()[0]
        database.close()
        assert intents == 0, "the create intent outlived the create"
        print(json.dumps({"session": session, "restart_seconds": restart_seconds,
                          "artifacts": str(lab.root)}))
    finally:
        # Keep the daemon logs with the artifacts; the runtime root is removed.
        for log in (lab.data / "logs").glob("*.log"):
            (lab.root / log.name).write_bytes(log.read_bytes())
        lab.cleanup_owned()


if __name__ == "__main__":
    main()
