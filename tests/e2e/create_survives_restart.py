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
With --failed-launch-retry the same hook fails the first launch and
holds its accepted retry across the restart.
"""
import argparse
import json
import os
import shlex
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


def durable_rows(lab, sql, parameters=()):
    database = sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True)
    try:
        return database.execute(sql, parameters).fetchall()
    finally:
        database.close()


def failed_launch_retry(lab, cli, record, run, command, environment, instance):
    reached = lab.hooks / "create_after_target_provisioned.reached"
    continuation = lab.hooks / "create_after_target_provisioned.continue"
    (lab.hooks / "create_after_target_provisioned.fail").touch()
    old_prompt = "this startup prompt failed before the worker existed"
    original_prompt = lab.runtime_root / "ORIGINAL_PROMPT_FILE"
    original_prompt.write_text(old_prompt)
    draft = "an unsent terminal draft"
    session = json.loads(cli("new", "--workspace", "Create restart", "--profile", "fake",
                             "--target", "localhost", "--project-directory", str(lab.project),
                             "--title", "Retry this launch", "--subagents", "none",
                             "--json", "--prompt-file", str(original_prompt)))["session_id"]
    wait_for(reached.exists, "the first attempt to reach its target")
    lab.daemon_request({"action": "persist_terminal_draft", "arguments": {
        "client_id": "retry-lab", "session_id": session, "owner_pid": os.getpid(),
        "revision": 1, "draft": {"text": draft, "inherited_input": None},
    }})
    continuation.touch()

    def failed():
        row = record(session)
        return row if row["state"] == "error" else None

    before = wait_for(failed, "the launch to fail with Retry available")
    assert "one-shot launch failure" in before["error"], before
    startup = wait_for(lambda: durable_rows(
        lab, "SELECT command_id,phase,error FROM startup_steps WHERE session_id=? AND phase='failed'",
        (session,)), "the startup prompt to settle as failed")
    assert len(startup) == 1, startup
    before_settings = durable_rows(
        lab, "SELECT c.workspace_id,s.project_directory,s.last_profile,s.target_template_id FROM sessions s JOIN session_contexts c USING(session_id) WHERE session_id=?",
        (session,))
    reached.unlink()
    continuation.unlink()

    refused = run("resume", "--session", session, "--profile", "another-profile", "--json")
    assert refused.returncode != 0 and "stored launch request" in refused.stderr, refused.stderr
    assert not durable_rows(lab, "SELECT operation_id FROM session_create_intents WHERE session_id=?",
                            (session,))
    accepted = run("resume", "--session", session, "--json")
    assert accepted.returncode == 0, (accepted.stdout, accepted.stderr)
    assert json.loads(accepted.stdout)["session_id"] == session, accepted.stdout
    assert "Your first prompt is still unsent. In the dashboard, type or paste it again." in accepted.stderr
    printed_command, = [part for part in accepted.stderr.split("`")
                        if part.startswith("mj ") and " --prompt-file " in part]
    assert printed_command == (f"mj --instance={instance} message --session {session}"
                               f" --prompt-file {original_prompt.name}"), accepted.stderr
    assert len(durable_rows(lab, "SELECT operation_id FROM session_create_intents WHERE session_id=?",
                            (session,))) == 1
    wait_for(reached.exists, "the accepted retry to reach its target")
    assert record(session)["state"] != "running"

    restart = subprocess.Popen(command("daemon", "restart"), env=environment(),
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    # A stopping daemon refuses ordinary IPC. Keep the old attempt held
    # until we observe that refusal and require the new daemon to reach again.
    attempt = reached.read_text()

    def stopping():
        try:
            lab.daemon_request({"action": "session_record", "arguments": {"session_id": session}})
        except reliability_lab.ScenarioFailure as error:
            if "daemon is shutting down" in str(error):
                return True
            raise
        except (ConnectionError, FileNotFoundError):
            return True
        return False

    try:
        wait_for(stopping, "old daemon to stop serving while the retry is held")
        reached.unlink()
    finally:
        continuation.touch()
    stdout, stderr = restart.communicate(timeout=WAIT)
    assert restart.returncode == 0, (stdout, stderr)

    ready = wait_for(lambda: (row if (row := record(session))["state"] == "running" else None),
                     "the same session to run after daemon restart")
    assert not ready.get("has_error"), ready
    assert reached.read_text() != attempt, "the restarted daemon never provisioned the retry"
    assert ready["title"] == "Retry this launch", ready
    assert durable_rows(
        lab, "SELECT c.workspace_id,s.project_directory,s.last_profile,s.target_template_id FROM sessions s JOIN session_contexts c USING(session_id) WHERE session_id=?",
        (session,)) == before_settings
    assert not durable_rows(lab, "SELECT command_id FROM startup_steps WHERE session_id=?", (session,))
    waited = json.loads(cli("wait", "--session", session, "--json", "--timeout", str(WAIT)))
    assert waited["outcome"] == "finished", waited
    assert not durable_rows(lab, "SELECT operation_id FROM session_create_intents WHERE session_id=?",
                            (session,))
    assert durable_rows(lab, "SELECT source,text FROM detached_drafts WHERE session_id=?",
                        (session,)) == [("retry-lab", draft)]

    def harness_prompts():
        harness = [json.loads(line) for line in (lab.runtime_root / "fake-acp.log").read_text().splitlines()]
        return [message for message in harness if message.get("method") == "session/prompt"]

    assert not harness_prompts()
    shell_env = environment()
    del shell_env["MJ_INSTANCE"]
    resend = [str(lab.hel), *shlex.split(printed_command)[1:]]
    sent = subprocess.run(resend, cwd=lab.runtime_root, env=shell_env,
                          capture_output=True, text=True, timeout=WAIT)
    assert sent.returncode == 0, (sent.stdout, sent.stderr)
    prompts = wait_for(harness_prompts, "the explicitly resent first prompt to reach the harness")
    assert len(prompts) == 1 and old_prompt in json.dumps(prompts[0]), prompts

    # A new failed row gives Resume and Destroy the same starting state.
    # Destroy can preempt an owned Create, but it must also retire its intent
    # so a late retry cannot recreate the destroyed session.
    reached.unlink()
    (lab.hooks / "create_after_target_provisioned.fail").touch()
    doomed = json.loads(cli("new", "--workspace", "Create restart", "--profile", "fake",
                            "--target", "localhost", "--project-directory", str(lab.project),
                            "--subagents", "none", "--json"))["session_id"]
    wait_for(lambda: record(doomed)["state"] == "error", "a second session's failed launch")
    failure_generation = durable_rows(lab, "SELECT failure_generation FROM sessions WHERE session_id=?",
                                      (doomed,))[0][0]
    reached.unlink()
    continuation.unlink()
    (lab.hooks / "create_after_target_provisioned.fail").touch()
    without_prompt = run("resume", "--session", doomed, "--json")
    assert without_prompt.returncode == 0, (without_prompt.stdout, without_prompt.stderr)
    assert "Prompts that failed with the previous launch are not resent." in without_prompt.stderr
    wait_for(reached.exists, "the next failed attempt to reach its target")
    continuation.touch()
    wait_for(lambda: durable_rows(
        lab, "SELECT failure_generation FROM sessions WHERE session_id=? AND state='error' AND failure_generation > ?",
        (doomed, failure_generation)), "the retry to record another launch error")
    # Leave the hook open so Destroy can cancel a winning retry without
    # waiting on a fixture that cannot observe provisioning cancellation.
    retry = subprocess.Popen(command("resume", "--session", doomed, "--json"), env=environment(),
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    cli("destroy", "--session", doomed, "--json")
    stdout, stderr = retry.communicate(timeout=WAIT)
    if retry.returncode == 0:
        assert json.loads(stdout)["session_id"] == doomed, stdout
    else:
        assert stderr.strip(), (stdout, stderr)

    def destroyed():
        rows = durable_rows(lab, "SELECT archived FROM sessions WHERE session_id=?", (doomed,))
        return (not rows or rows == [(1,)]) and not durable_rows(
            lab, "SELECT operation_id FROM session_create_intents WHERE session_id=?", (doomed,))

    wait_for(destroyed, "Destroy to settle the session and any competing create intent")
    print(json.dumps({"session": session, "race_session": doomed, "artifacts": str(lab.root)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--failed-launch-retry", action="store_true")
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

        def command(*arguments):
            return [str(args.mj.resolve()), "--instance", args.instance, *arguments]

        def run(*arguments):
            return subprocess.run(command(*arguments), env=environment(),
                                  capture_output=True, text=True, timeout=WAIT)

        def cli(*arguments):
            result = run(*arguments)
            assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result.stdout

        def record(session):
            return json.loads(cli("sessions", "--session", session, "--json"))

        cli("workspaces", "create", "Create restart")
        if args.failed_launch_retry:
            failed_launch_retry(lab, cli, record, run, command, environment, args.instance)
            return
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
        if (lab.data / "daemon.json").exists():
            (lab.hooks / "create_after_target_provisioned.continue").touch()
            try:
                cli("daemon", "stop")
            except (AssertionError, subprocess.TimeoutExpired) as error:
                print(f"Graceful daemon stop failed: {error}")
        # Keep the daemon logs with the artifacts.
        if (lab.data / "daemon.log").exists():
            (lab.root / "daemon.log").write_bytes((lab.data / "daemon.log").read_bytes())
        for log in (lab.data / "logs").glob("*.log"):
            (lab.root / log.name).write_bytes(log.read_bytes())
        lab.cleanup_owned()


if __name__ == "__main__":
    main()
