"""Exercise the pilot scheduler and output contract without a daemon."""
from contextlib import redirect_stderr, redirect_stdout
from decimal import Decimal
from io import StringIO
import hashlib
from io import BytesIO
import json
from pathlib import Path
import re
import subprocess
import tarfile
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
import unittest.mock

import pilot_run as pilot
import audit
import brief_driver
import pilot_cache_probe
import pilot_review


def manifest_fixture():
    manifest = {"version": 1, "seed": 20261120, "plan_commit": "c" * 40, "inputs": {},
                "models": {"writer": "gpt-6.1-sol/high", "reviewer": "gpt-6.1-terra/high",
                           "judge": "sonnet/high", "engine": "sonnet/high"},
                "page_rows": 50, "diffs": [], "jobs": []}
    for index, (repo, name) in enumerate([("mjolnir", "first"), ("bifrost-dev", "second")]):
        key = f"{repo}/{name}"
        reps = {"G": 1 + index, "R": 1 + index, "R-loc": 1}
        manifest["diffs"].append({"key": key, "repo": repo, "id": name, "intro": "a" * 40,
                                  "base": "b" * 40, "label_line": index + 1, "replicates": reps,
                                  "agreement_sample": False})
        for arm, count in {"N": 1, **reps}.items():
            for rep in range(1, count + 1):
                for slot in ([1, 2] if arm == "N" else [1, 2, None]):
                    # Reviewers deliberately precede the writer on disk.
                    suffix = "writer" if slot is None else f"reviewer-{slot}"
                    manifest["jobs"].append({"job": f"{key}/{arm}/r{rep}/{suffix}", "diff": key,
                                             "arm": arm, "replicate": rep, "slot": slot,
                                             "kind": "writer" if slot is None else "reviewer"})
    return json.dumps(manifest).encode()


class FakeQuota:
    def __init__(self, remaining):
        self.remaining = Decimal(remaining)
        self.checks = []

    def __call__(self, *, floor, weekly, strict):
        profile, threshold = floor.split("=")
        self.checks.append((profile, threshold, weekly, strict))
        if self.remaining <= Decimal(threshold):
            return "below reserved budget"
        return None


class FakeSessions:
    def __init__(self):
        self.done = set()
        self.calls = []
        self.interrupt = None

    def complete(self, job):
        return job["job"] in self.done

    def execute(self, job):
        key = job["job"]
        self.calls.append(key)
        if self.interrupt == key:
            return "interrupted"
        self.done.add(key)
        return "done"


class PilotTest(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.manifest = pilot.load_manifest(manifest_fixture())
        self.groups = pilot.select_jobs(self.manifest, arms=["N", "G"])

    # Hard-won: 49e16b8: reused engines could spend below quota and the writer could still finish.
    def test_reused_engine_quota_hold_interrupts_writer_and_supersedes_attempt(self):
        self.assert_writer_interrupted_on_engine_quota()

    # Hard-won: 8c9d945: engine rate limit scored as engine_error.
    def test_provider_refusal_interrupts_writer_instead_of_scoring_it(self):
        for refusal in ({"outcome": "error", "stop_reason": "QuotaLimit"},
                        {"outcome": "error", "diagnostic": {"http_status": 429}},
                        {"outcome": "error", "diagnostic": {"message": '{"type":"rate_limit_error"}'}}):
            with self.subTest(refusal=refusal), TemporaryDirectory() as directory:
                self.assert_writer_interrupted_on_engine_quota(refusal, Path(directory))
        with self.subTest(remaining_5h=20), TemporaryDirectory() as directory:
            self.assert_writer_interrupted_on_engine_quota(root=Path(directory), short_window=True)

    def assert_writer_interrupted_on_engine_quota(self, refusal=None, root=None, short_window=False):
        from rql_mock import engine as endpoint
        from rql_mock.engine.test_engine import CATALOGUE, COMMIT, MENU, QUERY, SOURCE, VERSION, Engine, FakeAcpRunner

        root = root or self.root
        reused = FakeAcpRunner()
        engine = Engine(root / "cache", (CATALOGUE, MENU),
                        lambda commit: (SOURCE.get, {"tree": "frozen", "orphan": COMMIT}),
                        lambda: reused, VERSION)
        short = {"duration_seconds": 18000, "remaining_percent": 21}
        if short_window:
            def quota(operation, *args):
                self.assertEqual((operation, *args), ("quota", "--profile", "claude-test"))
                windows = [{"remaining_percent": 80, "duration_seconds": 604800}]
                return {"profiles": [{"report": {"windows": [*windows, short]}}]}

            self.addCleanup(setattr, pilot.driver, "mj", pilot.driver.mj)
            pilot.driver.mj = quota
            reused.pilot_claude_hold = pilot.driver.pilot_claude_hold
        self.assertEqual(engine.answer(COMMIT, QUERY)["status"], "ok")
        reused.turn_refusal = refusal
        reused.quota_hold = None if refusal or short_window else "weekly quota below floor"
        short["remaining_percent"] = 20
        turns = len(reused.turns)
        query = '(function :name "different")'

        runner = pilot.Pilot.__new__(pilot.Pilot)
        runner.args = SimpleNamespace(run_root=root)
        runner.engine_identity = engine.identity
        self.addCleanup(setattr, endpoint, "load_engine", endpoint.load_engine)
        endpoint.load_engine = lambda: engine
        runner.diffs = {"mjolnir/first": {}}
        runner.writer_model = ("gpt-6.1-sol", "high")
        runner.gate, runner.attested = None, set()
        job = {"job": "mjolnir/first/R/r1/writer", "diff": "mjolnir/first",
               "arm": "R", "replicate": 1, "kind": "writer"}
        path = pilot.job_path(root, job)
        runner.render = lambda job: {"path": path, "bug": {"id": "first"}, "inputs": {"rql_tools": ["rql_query"]}}
        answers = []

        def write(bug_id, gate, *, pilot):
            answer = engine.answer(COMMIT, query)
            answers.append(answer)
            sid = f"{pilot['tag'].split('/')[-1]:0>32}"
            output = b"Writer brief.\n"
            (path / "brief-candidate.md").write_bytes(output)
            (path / "accepted.json").write_text(json.dumps({"brief_sha256": hashlib.sha256(output).hexdigest()}))
            (path / "mcp-reconnect-result.json").write_text("{}")
            (path / f"queries-{sid}.jsonl").write_text(
                json.dumps({"session": sid, "delivered": True, "status": answer["status"]}) + "\n")
            # The launch and teardown IPC are faked here. Use the real writer publication decision.
            return brief_driver.finish_pilot_writer(pilot, sid, bug_id, "fixture")

        self.addCleanup(setattr, brief_driver, "run", brief_driver.run)
        self.addCleanup(setattr, brief_driver.driver, "destroy_captured_pilot",
                        brief_driver.driver.destroy_captured_pilot)
        brief_driver.run = write
        brief_driver.driver.destroy_captured_pilot = lambda *args, **kwargs: "done"
        state = runner.execute(job)
        self.assertIn("interrupted: RQL engine quota", state)
        self.assertEqual(answers[0]["status"], "quota_hold")
        if short_window:
            self.assertIn("20% 5-hour remaining", answers[0]["message"])
        if refusal:
            self.assertEqual(answers[0]["provenance"]["turns"][-1]["outcome"], refusal["outcome"])
        self.assertEqual(len(reused.turns), turns)
        self.assertIsNone(runner.complete(job))
        self.assertFalse((path / "done.json").exists())
        self.assertTrue((path / "failed.json").is_file())
        if short_window:
            self.assertIn("20% 5-hour remaining", runner.execute(job))
            short["remaining_percent"] = None
            self.assertIn("no usable 5-hour quota report", runner.execute(job))
            self.assertEqual(len(answers), 1)
            self.assertFalse(list(path.parent.glob(f"{path.name}.superseded-*")))
        reused.quota_hold = reused.turn_refusal = None
        short["remaining_percent"] = 21
        with unittest.mock.patch.object(pilot.driver, "pilot_claude_hold", reused.pilot_claude_hold):
            self.assertEqual(runner.execute(job), "done")
        self.assertEqual(answers[1]["status"], "ok")
        self.assertEqual(len(reused.turns), turns + 1)
        self.assertTrue((path / "done.json").is_file())
        self.assertEqual(len(list(path.parent.glob(f"{path.name}.superseded-*"))), 1)

    # Hard-won: 8c9d945: provisioning was treated as a missing container by the assignment hook.
    def test_tool_assignment_waits_for_the_writer_and_reviewer_startup_gate(self):
        import threading

        class StopBeforePrompt(RuntimeError):
            pass

        state = {"lifecycle": "live", "is_idle": True, "no_later_history": True,
                 "at": "intro", "base": "base", "profile_id": "profile"}
        events = []

        def wait(operation, *args, **kwargs):
            self.assertEqual(operation, "wait")
            events.append("ready")
            return {"outcome": "finished", "session": state}

        def place(sid):
            self.assertEqual(events[-1], "ready")
            return {"image": "test-image"}

        def publish(sid):
            self.assertTrue((path / "settings.json").is_file())
            events.append("hook")

        def before_prompt(*args):
            self.assertEqual(events, ["launch", "ready", "hook"])
            raise StopBeforePrompt

        with (unittest.mock.patch.object(brief_driver.driver, "MJ_INSTANCE_ARGS", ("--instance", "fixture")),
              unittest.mock.patch.object(brief_driver, "sealed_launch", lambda *a, **k: (events.append("launch"), "sid")[1]),
              unittest.mock.patch.object(brief_driver, "mj", wait),
              unittest.mock.patch.object(brief_driver, "configure_pilot_model_ready", lambda *a: {}),
              unittest.mock.patch.object(brief_driver.driver, "container", place),
              unittest.mock.patch.object(brief_driver.driver, "inventory", lambda *a: {
                  "remotes": [], "alternates": False, "status": ""}),
              unittest.mock.patch.object(brief_driver, "check_pilot_tool", before_prompt)):
            for kind in ("writer", "reviewer"):
                events.clear()
                path = self.root / kind
                params = {"path": path, "tag": kind, "profile": "profile", "model": ("model", "high"),
                          "bug": {"intro_commit": "intro", "base_commit": "base"},
                          "inputs": {"image_digest": "sha256:test-image"}, "attest_cache": False,
                          "publish_tool_assignment": publish}
                with self.subTest(kind=kind), self.assertRaises(StopBeforePrompt):
                    if kind == "writer":
                        brief_driver.run("bug", threading.Semaphore(), pilot=params)
                    else:
                        pilot_review.run(params)

    # Hard-won: 8c9d945: a lost prompt acknowledgement launched and charged a replacement.
    def test_prompt_timeout_reconciles_same_launch_without_resending(self):
        runner = pilot.Pilot.__new__(pilot.Pilot)
        runner.args = SimpleNamespace(run_root=self.root)
        runner.writer_model, runner.gate, runner.attested = ("sol", "high"), None, set()
        job = {"job": "mjolnir/first/G/r1/writer", "diff": "mjolnir/first",
               "arm": "G", "replicate": 1, "kind": "writer"}
        path = pilot.job_path(self.root, job)
        runner.render = lambda job: {"path": path, "bug": {"id": "first"}, "inputs": {}}
        launches, sent = [], []

        def fake_message(operation, *args, **kwargs):
            self.assertEqual(operation, "message")
            sent.append(args[args.index("--command-id") + 1])
            raise subprocess.TimeoutExpired("mj message", 300)

        def write(bug_id, gate, *, pilot):
            launch = path / "launch.json"
            if not launch.exists():
                launches.append("original")
                brief_driver.save(launch, {"session_id": "original"})
            brief_driver.send(path, "original", "prompt", "writer-command", "Write the brief.")
            brief_driver.save(path / "done.json", {})
            return "done"

        def observe(sid):
            self.assertEqual(sid, "original")
            return {"lifecycle": "live", "is_idle": True,
                    "last_turn_outcome": {"command_id": "writer-command", "outcome": {"kind": "completed"}}}

        with (unittest.mock.patch.object(brief_driver, "run", write),
              unittest.mock.patch.object(brief_driver, "mj", fake_message),
              unittest.mock.patch.object(brief_driver, "session", observe)):
            self.assertIn("interrupted", runner.execute(job))
            self.assertTrue((path / "prompt-intent.json").is_file())
            self.assertFalse((path / "failed.json").exists())
            self.assertEqual(runner.execute(job), "done")
        self.assertEqual(launches, ["original"])
        self.assertEqual(sent, ["writer-command"])
        self.assertFalse(list(path.parent.glob(f"{path.name}.superseded-*")))

    # Hard-won: dfb55808: a rejected brief lost its delivered engine quota hold.
    def test_invalid_brief_preserves_quota_hold_before_scheduler_retry(self):
        for outcome in ("missing", "lint"):
            with self.subTest(outcome=outcome), TemporaryDirectory() as directory:
                root = Path(directory)
                runner = pilot.Pilot.__new__(pilot.Pilot)
                runner.args = SimpleNamespace(run_root=root)
                runner.writer_model, runner.gate, runner.attested = ("sol", "high"), None, set()
                job = {"job": "mjolnir/first/R/r1/writer", "diff": "mjolnir/first",
                       "arm": "R", "replicate": 1, "kind": "writer"}
                runner.diffs, runner.engine_identity = {job["diff"]: {}}, None
                path = pilot.job_path(root, job)
                for name, data in (("launch", {"session_id": "fixture"}), ("launch-intent", {"profile": "fixture"}),
                                   ("settings", {}), ("inventory-check", {"problems": []})):
                    pilot.driver.save(path / f"{name}.json", data)
                runner.render = lambda job: {"path": path, "bug": {"id": "first"}, "profile": "fixture",
                                            "prompt": "Write the brief.", "inputs": {"rql_tools": ["rql_query"]}}
                writes = []

                def write(*args, **kwargs):
                    writes.append(outcome)
                    (path / "queries-fixture.jsonl").write_text(
                        '{"session":"fixture","status":"quota_hold","delivered":true}\n')
                    if outcome == "missing":
                        raise brief_driver.WriterBriefMissing("no brief.md")
                    return "Rejected brief.", ["missing source anchor"], brief_driver.FIX_ROUNDS

                with (unittest.mock.patch.object(pilot, "publish_tool_assignment", lambda *a, **k: None),
                      unittest.mock.patch.object(brief_driver, "check_pilot_tool", lambda *a: True),
                      unittest.mock.patch.object(brief_driver, "write_brief", write),
                      unittest.mock.patch.object(brief_driver, "pilot_audit", lambda *a: {"passed": True}),
                      unittest.mock.patch.object(brief_driver.driver, "transcript", lambda *a: []),
                      unittest.mock.patch.object(brief_driver, "mj", lambda *a: {}),
                      unittest.mock.patch.object(brief_driver, "session", lambda *a: {"profile_id": "fixture"}),
                      redirect_stdout(StringIO()) as status):
                    self.assertEqual(pilot.run_matrix(root, {job["diff"]: [job]}, "fixture",
                                                      lambda job: None, runner.execute, FakeQuota("90")), 1)
                self.assertEqual(writes, [outcome])
                self.assertIn("interrupted: RQL engine quota", status.getvalue())
                failure = pilot.read_json(path / "failed.json")
                self.assertEqual(failure["status"], "quota_hold")
                self.assertIn("brief.md" if outcome == "missing" else "lint", failure["terminal_reason"])

    # Hard-won: dfb55808: a requested permission kept the unbounded writer wait open.
    def test_pending_writer_input_ends_the_unbounded_wait_and_retains_the_question(self):
        runner = pilot.Pilot.__new__(pilot.Pilot)
        runner.args = SimpleNamespace(run_root=self.root)
        runner.writer_model, runner.gate, runner.attested = ("sol", "high"), None, set()
        job = {"job": "mjolnir/first/G/r1/writer", "diff": "mjolnir/first",
               "arm": "G", "replicate": 1, "kind": "writer"}
        path = pilot.job_path(self.root, job)
        runner.render = lambda job: {"path": path, "bug": {"id": "first"}, "inputs": {"rql_tools": []}}
        question = {"id": "tool-permission-1", "message": "May I read the source file?", "fields": []}

        def write(bug_id, gate, *, pilot):
            brief_driver.save(path / "launch.json", {"session_id": "original"})
            brief_driver.send(path, "original", "prompt", "writer-command", "Write the brief.", limit=None)

        with (unittest.mock.patch.object(brief_driver, "run", write),
              unittest.mock.patch.object(brief_driver, "mj", lambda *a: {
                  "session_id": "original", "via": "turn", "turn_id": 1}),
              unittest.mock.patch.object(brief_driver, "session", lambda sid: {
                  "lifecycle": "live", "is_idle": False, "pending_elicitations": [question]}),
              unittest.mock.patch.object(brief_driver.time, "sleep", side_effect=AssertionError("wait slept"))):
            self.assertIn("writer needs input. May I read the source file?", runner.execute(job))
        failure = pilot.read_json(path / "failed.json")
        self.assertEqual(failure["retained_session"], "original")
        self.assertEqual(failure["pending_elicitations"], [question])
        self.assertFalse((path / "done.json").exists())

    # Hard-won: 2ac06cf: input-required writer replaced
    def test_pending_writer_resume_waits_and_collects_the_original_turn(self):
        runner = pilot.Pilot.__new__(pilot.Pilot)
        runner.args = SimpleNamespace(run_root=self.root)
        runner.writer_model, runner.gate, runner.attested = ("sol", "high"), None, set()
        job = {"job": "mjolnir/first/R/r1/writer", "diff": "mjolnir/first",
               "arm": "R", "replicate": 1, "kind": "writer"}
        path = pilot.job_path(self.root, job)
        runner.render = lambda job: {"path": path, "bug": {"id": "first"}, "inputs": {"rql_tools": ["rql_query"]}}
        question = {"id": "permission-1", "message": "May I read the source file?", "fields": []}
        status = {"lifecycle": "live", "is_idle": False, "pending_elicitations": [question]}
        launches, submissions = [], []

        def write(bug_id, gate, *, pilot):
            if not (path / "launch.json").exists():
                launches.append("original")
                brief_driver.save(path / "launch.json", {"session_id": "original"})
                brief_driver.save(path / "tool-assignment.json", {"session_id": "original", "arm": "R"})
                brief_driver.save(path / "prompt-completion.json", {"command_id": "initial-command"})
            _, problems, rounds = brief_driver.write_brief(
                path, "original", "writer-command", "Write the brief.", {}, limit=None)
            self.assertFalse(problems)
            brief_driver.save(path / "done.json", {"session_id": "original", "fix_rounds": rounds})
            return "done"

        def message(operation, *args):
            self.assertEqual(operation, "message")
            submissions.append(args[args.index("--command-id") + 1])
            return {"session_id": "original", "via": "turn", "turn_id": 1}

        with (unittest.mock.patch.object(brief_driver, "run", write),
              unittest.mock.patch.object(brief_driver, "mj", message),
              unittest.mock.patch.object(brief_driver, "session", lambda sid: status),
              unittest.mock.patch.object(brief_driver, "fetch_brief", lambda sid: "fixed" if status["is_idle"] else "rough"),
              unittest.mock.patch.object(brief_driver, "lint", lambda text, bug: [] if text == "fixed" else ["rough"]),
              unittest.mock.patch.object(pilot.subprocess, "run", side_effect=AssertionError("assignment revoked"))):
            self.assertIn("original writer needs input. May I read the source file?", runner.execute(job))
            # Simulate the receipt written before command IDs were added to failures.
            failure = pilot.read_json(path / "failed.json")
            del failure["command_id"]
            pilot.driver.save(path / "failed.json", failure)
            self.assertIn("original writer needs input. May I read the source file?", runner.execute(job))
            self.assertFalse((path / "done.json").exists())
            status.update(is_idle=True, pending_elicitations=[], last_turn_outcome={
                "command_id": "brief-fix1-writer-command", "outcome": {"kind": "completed"},
            })
            self.assertEqual(runner.execute(job), "done")
        self.assertEqual(launches, ["original"])
        self.assertEqual(submissions, ["brief-fix1-writer-command"])
        self.assertEqual(pilot.read_json(path / "done.json"), {"session_id": "original", "fix_rounds": 1})
        self.assertEqual(pilot.read_json(path / "lint-0.json"), ["rough"])
        self.assertEqual(pilot.read_json(path / "lint-1.json"), [])
        self.assertFalse((path / "failed.json").exists())
        self.assertFalse(list(path.parent.glob(f"{path.name}.superseded-*")))

    # Hard-won: 546ebd4: resumed writer failure skipped its audit
    def test_terminal_failure_after_writer_input_captures_and_audits_before_retry(self):
        runner = pilot.Pilot.__new__(pilot.Pilot)
        runner.args = SimpleNamespace(run_root=self.root)
        job = {"job": "mjolnir/first/G/r1/writer", "diff": "mjolnir/first",
               "arm": "G", "replicate": 1, "kind": "writer"}
        path = pilot.job_path(self.root, job)
        runner.render = lambda job: {"path": path, "bug": {"id": "first"}, "inputs": {"rql_tools": []}}
        sid = "original"
        prompt = "Write a brief."
        for name, value in {
            "launch": {"session_id": sid},
            "prompt-intent": {"session_id": sid, "command_id": "original-command",
                              "prompt_sha256": pilot.digest(prompt.encode())},
            "prompt-request": {"session_id": sid, "via": "turn", "turn_id": 1},
            "failed": {"retained_session": sid, "command_id": "original-command",
                       "status": "input_required", "reason": "writer needs input"},
            "pre-inventory": {"commits": ["a" * 40], "head_tree": "b" * 40, "base_tree": "c" * 40},
        }.items():
            pilot.driver.save(path / f"{name}.json", value)
        (path / "prompt.txt").write_text(prompt)
        turn = {"command_id": "original-command", "outcome": {"kind": "failed"}}
        transcript = [{"session_id": sid, "items": [
            {"seq": 1, "role": "tool", "body": {"call": {"title": "ls /run/hel/git-cache"}}},
        ]}]

        def usage(operation, *args):
            self.assertEqual((operation, *args), ("usage", "--session", sid))
            return {"session_id": sid}

        with (unittest.mock.patch.object(brief_driver, "session", lambda sid: {
                  "lifecycle": "live", "is_idle": True, "last_turn_outcome": turn}),
              unittest.mock.patch.object(brief_driver, "mj", usage),
              unittest.mock.patch.object(brief_driver.driver, "transcript", lambda sid: transcript),
              unittest.mock.patch.object(brief_driver, "run", side_effect=AssertionError("writer replaced"))):
            self.assertEqual(runner.execute(job), "retained-failure")
        self.assertEqual(pilot.read_json(path / "transcript.json"), transcript)
        self.assertEqual(pilot.read_json(path / "usage.json"), {"session_id": sid})
        audit = pilot.read_json(path / "audit.json")
        self.assertFalse(audit["passed"])
        self.assertTrue(audit["hits"])
        failure = pilot.read_json(path / "failed.json")
        self.assertEqual(failure["void_reason"], audit["hits"])
        self.assertIn("did not complete", failure["reason"])

    # Hard-won: 49e16b8: startup refusal retained a session and charged a replacement.
    def test_config_startup_refusal_retries_same_session(self):
        calls, waits = [], []
        def configure(sid, model, effort):
            calls.append(sid)
            if len(calls) == 1:
                raise RuntimeError("session initialization is still running")
            return {"model": model, "effort": effort}
        with unittest.mock.patch.object(brief_driver, "configure_pilot_model", configure), \
                unittest.mock.patch("brief_driver.time.sleep"), \
                unittest.mock.patch.object(brief_driver.driver, "wait_idle", lambda sid: waits.append(sid)):
            result = brief_driver.configure_pilot_model_ready("same-session", "model", "high")
        self.assertEqual(result, {"model": "model", "effort": "high"})
        self.assertEqual(calls, ["same-session", "same-session"])
        self.assertEqual(waits, ["same-session"])

    def run_fake(self, groups, sessions, quota):
        with redirect_stdout(StringIO()):
            return pilot.run_matrix(self.root, groups, "chosen", sessions.complete, sessions.execute, quota)

    def test_weekly_admission_reserves_whole_diff_and_holds_at_floor(self):
        first = {"mjolnir/first": self.groups["mjolnir/first"]}
        sessions = FakeSessions()
        at_floor = FakeQuota("26.23")
        self.assertEqual(self.run_fake(first, sessions, at_floor), 1)
        self.assertEqual(at_floor.checks, [("chosen", "26.23", True, True)])
        self.assertEqual(sessions.calls, [])
        self.assertFalse((self.root / "active-diff.json").exists())
        self.assertEqual(self.run_fake(first, sessions, FakeQuota("26.24")), 0)
        self.assertEqual(len(sessions.calls), 5)

    def test_interrupted_diff_resumes_first_and_charges_only_missing_jobs(self):
        first, second = self.groups
        interrupted = FakeSessions()
        interrupted.interrupt = f"{second}/G/r2/writer"
        self.assertEqual(self.run_fake({second: self.groups[second]}, interrupted, FakeQuota("90")), 1)
        self.assertEqual(interrupted.calls, [f"{second}/G/r1/writer", interrupted.interrupt])
        resumed = FakeSessions()
        resumed.done = set(interrupted.done)
        quota = FakeQuota("90")
        self.assertEqual(self.run_fake(self.groups, resumed, quota), 0)
        self.assertEqual(resumed.calls[0], interrupted.interrupt)
        self.assertTrue(all(key.startswith(second) for key in resumed.calls[:7]))
        self.assertTrue(all(key.startswith(first) for key in resumed.calls[7:]))
        self.assertEqual(quota.checks, [("chosen", "26.63", True, True), ("chosen", "26.23", True, True)])
        self.assertFalse((self.root / "active-diff.json").exists())

    def test_filter_cannot_bypass_admitted_diff(self):
        first, second = self.groups
        sessions = FakeSessions()
        sessions.interrupt = f"{second}/G/r1/writer"
        self.assertEqual(self.run_fake({second: self.groups[second]}, sessions, FakeQuota("90")), 1)
        with self.assertRaisesRegex(RuntimeError, "was interrupted"):
            self.run_fake({first: self.groups[first]}, sessions, FakeQuota("90"))
        self.assertEqual(sessions.calls, [sessions.interrupt])

    def test_failed_attempt_relaunches_before_any_other_job(self):
        groups = {"mjolnir/first": self.groups["mjolnir/first"]}
        for cause in ("rql-preflight-failed", "missing-brief"):
            with self.subTest(cause=cause):
                sessions = FakeSessions()
                failed_writer = f"mjolnir/first/G/r1/writer"

                def execute(job):
                    if not sessions.calls:
                        sessions.calls.append(job["job"])
                        return "retained-failure"
                    return sessions.execute(job)

                quota = FakeQuota("90")
                with redirect_stdout(StringIO()):
                    self.assertEqual(pilot.run_matrix(self.root, groups, "chosen", sessions.complete, execute, quota), 0)
                self.assertEqual(sessions.calls[:2], [failed_writer, failed_writer])
                self.assertEqual(quota.checks, [("chosen", "26.23", True, True)] * 2)

    def test_second_capture_failure_reports_both_sessions_and_stops_before_next_diff(self):
        calls = []
        receipts = []

        def execute(job):
            path = pilot.job_path(self.root, job)
            if calls:
                pilot.archive(path)
            calls.append(job["job"])
            sid = f"retained-{len(calls)}"
            receipts.append(sid)
            return pilot.driver.retain_pilot_session(path, sid, f"capture failure {len(calls)}")

        out = StringIO()
        with redirect_stdout(out):
            self.assertEqual(pilot.run_matrix(self.root, self.groups, "chosen", lambda job: False,
                                             execute, FakeQuota("90")), 1)
        self.assertEqual(calls, ["mjolnir/first/G/r1/writer"] * 2)
        self.assertEqual(pilot.read_json(self.root / "active-diff.json")["diff"], "mjolnir/first")
        report = json.loads(out.getvalue().splitlines()[-1])["retained_attempts"]
        self.assertEqual([record["retained_session"] for record in report], receipts)
        self.assertTrue(all(Path(record["receipt"]).is_file() for record in report))

    def test_revoke_failure_keeps_writer_done_and_retries_without_relaunch(self):
        job = next(job for job in self.manifest["jobs"] if job["kind"] == "writer" and job["arm"] == "R")
        path = pilot.job_path(self.root, job)
        sid = "a" * 32
        brief = b"Accepted brief.\n"
        hook = self.root / "rql-hook"
        hook.write_text("""#!/usr/bin/env python3
from pathlib import Path
import sys

assert sys.argv[1] == "--revoke"
attempt = Path(sys.argv[2]).parent
assert (attempt / "done.json").is_file()
if not (attempt / "recover").exists():
    sys.exit(1)
(attempt / "revoked").touch()
""")
        hook.chmod(0o700)
        runner = pilot.Pilot.__new__(pilot.Pilot)
        runner.args = SimpleNamespace(run_root=self.root, rql_hook=hook)
        runner.writer_model = ("gpt-6.1-sol", "high")
        runner.gate, runner.attested = None, set()
        inputs = {"rql_tools": ["rql_query"], "prompt_sha256": pilot.digest(b"prompt")}
        runner.render = lambda _: {"path": path, "bug": {"id": "first"}, "inputs": inputs}
        launches = []

        def write(bug_id, gate, *, pilot):
            launches.append(sid)
            driver = brief_driver.driver
            driver.save(path / "launch.json", {"session_id": sid})
            driver.save(path / "tool-assignment.json", {"session_id": sid, "arm": "R"})
            driver.save(path / "accepted.json", {"brief_sha256": hashlib.sha256(brief).hexdigest()})
            driver.save(path / "mcp-reconnect-result.json",
                        {"session_id": sid, "passed": True, "expected_tools": ["rql_query"]})
            driver.save(path / "audit.json", {"passed": True})
            driver.save(path / "usage.json", {"totals": {"total_tokens": {"tokens": 12}}})
            (path / "prompt.txt").write_bytes(b"prompt")
            (path / "brief-candidate.md").write_bytes(brief)
            (path / f"queries-{sid}.jsonl").touch()
            return brief_driver.finish_pilot_writer(pilot, sid, bug_id, "fixture")

        def destroy(root, attempt, session, *args, **kwargs):
            pilot.driver.save(attempt / "destroy.json", {"session_id": session, "accepted": True})
            return "done"

        self.addCleanup(setattr, brief_driver, "run", brief_driver.run)
        self.addCleanup(setattr, brief_driver.driver, "destroy_captured_pilot", brief_driver.driver.destroy_captured_pilot)
        brief_driver.run = write
        brief_driver.driver.destroy_captured_pilot = destroy
        groups = {job["diff"]: [job]}
        with redirect_stdout(StringIO()), redirect_stderr(StringIO()) as reports:
            self.assertEqual(pilot.run_matrix(self.root, groups, "fixture", runner.complete, runner.execute,
                                              FakeQuota("90")), 0)
        self.assertIn("revocation still pending", reports.getvalue())
        marker = path / "pending-revoke.json"
        self.assertEqual(pilot.read_json(marker), {"session_id": sid})
        self.assertFalse((path / "failed.json").exists())
        accepted = (path / "done.json").read_bytes()
        (path / "recover").touch()
        runner.retry_pending_revokes()
        self.assertTrue((path / "revoked").exists())
        self.assertFalse(marker.exists())
        self.assertEqual(pilot.run_matrix(self.root, groups, "fixture", runner.complete, runner.execute,
                                          FakeQuota("90")), 0)
        self.assertEqual(launches, [sid])
        self.assertEqual((path / "done.json").read_bytes(), accepted)
        self.assertEqual((path / "brief.md").read_bytes(), brief)
        self.assertFalse(list(path.parent.glob("writer.superseded-*")))

    def captured_job(self, name, sid, model, output, *, forbidden=False):
        path = self.root / name
        path.mkdir()
        selection = {"model": model, "effort": "high"}
        artifacts = {
            "launch.json": {"session_id": sid, "instance": "test-pilot"},
            "launch-intent.json": {"profile": "pinned"},
            "pre-inventory.json": {"commits": ["a" * 40], "head_tree": "b" * 40, "base_tree": "c" * 40},
            "settings.json": selection,
            "prompt-completion.json": {"command_id": "prompt"},
            "cache-attestation.json": {"passed": True},
            "mcp-reconnect-result.json": {"session_id": sid, "passed": True},
            "usage.json": {"session_id": sid, "totals": {"total_tokens": {"tokens": 21}},
                           "turn_selections": {"prompt": selection}},
        }
        call = ({"seq": 1, "role": "tool", "body": {"call": {"title": "ls /run/hel/git-cache"}}}
                if forbidden else {"seq": 1, "role": "agent", "body": {"text": "Captured"}})
        artifacts["transcript.json"] = [{"session_id": sid, "items": [call]}]
        for filename, artifact in artifacts.items():
            pilot.driver.save(path / filename, artifact)
        (path / output).write_bytes(b"Captured")
        return path

    def test_writer_and_reviewer_argv_destroy_only_after_capture_with_no_ack_flag(self):
        driver = pilot.driver
        original_args, original_run = driver.MJ_INSTANCE_ARGS, driver.subprocess.run
        self.addCleanup(setattr, driver, "MJ_INSTANCE_ARGS", original_args)
        self.addCleanup(setattr, driver.subprocess, "run", original_run)
        driver.MJ_INSTANCE_ARGS = ("--instance", "test-pilot")
        seen = []
        paths = {}
        rollout_name = "rollout-test.jsonl"
        failed_export = set()
        # Hard-won: d51b915: rollout retry used the container ID recorded at launch, not the current one
        original_lookup = driver.session_container_id
        self.addCleanup(setattr, driver, "session_container_id", original_lookup)
        driver.session_container_id = lambda sid, missing_ok=False: f"current-{sid}"

        def run(argv, **kwargs):
            seen.append(argv)
            if argv[:2] == ["podman", "exec"]:
                self.assertTrue(argv[2].startswith("current-"), "capture must use the session's current container")
                sid = argv[2].removeprefix("current-")
                self.assertEqual(argv[-1], f"/var/lib/hel/profiles/{sid}/sessions")
                if sid not in failed_export:
                    failed_export.add(sid)
                    raise subprocess.CalledProcessError(125, argv)
                model = pilot.read_json(paths[sid] / "settings.json")["model"]
                rows = [
                    {"type": "session_meta", "payload": {"id": "thread", "cwd": f"/workspace/{sid}/repo"}},
                    {"type": "turn_context", "payload": {"turn_id": "native", "model": model, "effort": "high"}},
                    {"type": "event_msg", "payload": {
                        "type": "item_completed", "thread_id": "thread", "turn_id": "native",
                        "completed_at_ms": 105, "item": {"type": "UserMessage", "id": "input",
                                                        "content": [{"type": "text", "text": "Prompt"}]},
                    }},
                    {"type": "event_msg", "payload": {"type": "item_completed", "item": {
                        "type": "CommandExecution", "id": "exec-captured", "command": ["/bin/bash", "-lc", "ls"],
                        "cwd": f"file:///workspace/{sid}/repo",
                    }}},
                ]
                data = "".join(json.dumps(row) + "\n" for row in rows).encode()
                with tarfile.open(fileobj=kwargs["stdout"], mode="w") as archive:
                    member = tarfile.TarInfo(rollout_name)
                    member.size = len(data)
                    archive.addfile(member, BytesIO(data))
                return SimpleNamespace(returncode=0)
            if "sessions" in argv:
                answer = {"sessions": [{"id": sid, "lifecycle": "live", "title": "same title"} for sid in paths]}
            else:
                sid = argv[argv.index("--session") + 1]
                path = paths[sid]
                receipt = pilot.read_json(path / "receipt.json")
                self.assertEqual(receipt["session_id"], sid)
                self.assertTrue(pilot.read_json(path / "audit.json")["passed"])
                self.assertIn(f"codex-rollout/{rollout_name}", receipt["evidence_sha256"])
                for name, sha in receipt["evidence_sha256"].items():
                    self.assertEqual(pilot.digest((path / name).read_bytes()), sha)
                answer = {"session_id": sid, "accepted": True}
            return SimpleNamespace(returncode=0, stdout=json.dumps(answer).encode())

        driver.subprocess.run = run
        for kind, model, output in (("writer", "gpt-6.1-sol", "brief.md"),
                                    ("reviewer", "gpt-6.1-terra", "output.md")):
            sid = "a" * 32 if kind == "writer" else "b" * 32
            path = self.captured_job(kind, sid, model, output)
            driver.save(path / "placement.json", {"container": sid, "cwd": f"/workspace/{sid}/repo"})
            driver.save(path / "transcript.json", [{"session_id": sid, "items": [
                {"seq": 1, "position": 1, "stable_id": "user:prompt", "role": "user", "created_at_ms": 100,
                 "body": {"content": [{"type": "text", "text": "Prompt"}]}},
                {"seq": 2, "position": 2, "stable_id": "tool:exec-captured", "role": "tool",
                 "body": {"call": {"toolCallId": "exec-captured",
                 "name": "exec_command", "title": "List files", "kind": "read"}}},
            ]}])
            usage = pilot.read_json(path / "usage.json")
            usage["turns"] = [{"command_id": "prompt", "turn_start_position": 1, "completed_at_ms": 200}]
            usage["turn_selections"]["prompt"]["effort"] = None
            driver.save(path / "usage.json", usage)
            paths[sid] = path
            params = {"run_root": self.root, "path": path, "inputs": {"rql_tools": []}, "model": (model, "high")}
            sha = pilot.digest((path / output).read_bytes())
            if kind == "writer":
                (path / "brief-candidate.md").write_bytes(b"Captured")
                pilot.driver.save(path / "accepted.json", {"brief_sha256": sha, "fix_rounds": 0})
                def finish():
                    return brief_driver.finish_pilot_writer(params, sid, "bug", "pinned")
            else:
                params.update(bug={}, profile="pinned", tag="reviewer")
                pilot.driver.save(path / "captured.json", {"output_sha256": sha, "session_id": sid})
                # An earlier build could freeze the receipt without a rollout.
                names = ("transcript.json", "usage.json", output, "cache-attestation.json",
                         "mcp-reconnect-result.json", "pre-inventory.json", "settings.json",
                         "launch.json", "prompt-completion.json")
                driver.save(path / "receipt.json", {
                    "session_id": sid, "instance": "test-pilot", "profile": "pinned",
                    "model": model, "effort": "high", "output": output,
                    "evidence_sha256": {name: pilot.digest((path / name).read_bytes()) for name in names},
                })

                def finish():
                    return pilot_review.run(params)

            receipt = path / "receipt.json"
            before = receipt.read_bytes() if receipt.exists() else None
            # Hard-won: f0beee63: a failed export froze audit evidence before a retry.
            self.assertIn("retry Codex rollout capture", finish())
            self.assertTrue((path / "rollout-capture-error.json").exists())
            self.assertFalse((path / "audit.json").exists())
            self.assertFalse((path / "failed.json").exists())
            self.assertEqual(receipt.read_bytes() if receipt.exists() else None, before)
            self.assertEqual(finish(), "done")
            self.assertEqual(pilot.read_json(path / "receipt.json")["model"], model)
            self.assertTrue((path / "destroy.json").exists())
        self.assertNotIn("--acknowledge-unpublished-work", [word for argv in seen for word in argv])
        self.assertEqual([argv[1:] for argv in seen if "destroy" in argv],
                         [["--instance", "test-pilot", "destroy", "--session", sid, "--json"] for sid in paths])

    def test_failed_capture_or_audit_never_reaches_mj_destroy(self):
        driver = pilot.driver
        original_args, original_mj = driver.MJ_INSTANCE_ARGS, driver.mj
        self.addCleanup(setattr, driver, "MJ_INSTANCE_ARGS", original_args)
        self.addCleanup(setattr, driver, "mj", original_mj)
        driver.MJ_INSTANCE_ARGS = ("--instance", "test-pilot")

        def unexpected(*args, **kwargs):
            self.fail(f"failed capture reached mj: {args}")

        driver.mj = unexpected
        model = "gpt-6.1-terra"
        path = self.captured_job("forbidden", "forbidden-id", model, "output.md", forbidden=True)
        rollouts = path / "codex-rollout"
        rollouts.mkdir()
        (rollouts / "rollout-test.jsonl").write_text('{"type": "session_meta"}\n')
        args = (self.root, path, "forbidden-id", "pinned", model, "high", "output.md")
        self.assertEqual(driver.destroy_captured_pilot(*args), "retained-failure")
        self.assertEqual(pilot.read_json(path / "failed.json")["retained_session"], "forbidden-id")
        with (path / "transcript.json").open("a") as transcript:
            transcript.write("\n")
        with self.assertRaisesRegex(ValueError, "capture changed"):
            driver.destroy_captured_pilot(*args)
        (path / "output.md").unlink()
        with self.assertRaises(FileNotFoundError):
            driver.destroy_captured_pilot(*args)

    def test_missing_brief_fetch_is_terminal_but_transport_failure_is_resumable(self):
        original = brief_driver.driver.mj_invocation
        self.addCleanup(setattr, brief_driver.driver, "mj_invocation", original)
        for error in ("brief.md does not exist on the session's target", "daemon connection refused"):
            with self.subTest(error=error):
                def fetch(*args):
                    return ["python3", "-c", f"import sys; sys.exit({error!r})"]

                brief_driver.driver.mj_invocation = fetch
                if error.startswith("brief.md"):
                    with self.assertRaises(brief_driver.WriterBriefMissing):
                        brief_driver.fetch_brief("writer")
                else:
                    with self.assertRaises(RuntimeError) as raised:
                        brief_driver.fetch_brief("writer")
                    self.assertNotIsInstance(raised.exception, brief_driver.WriterBriefMissing)

    def test_fidelity_gate_requires_exact_manifest_and_every_served_step(self):
        path, engine = self.pilot_engine_fixture(["contract-cases", "value-transfers"])
        raw = path.read_bytes()
        manifest = pilot.load_manifest(raw)
        rql = pilot.select_jobs(manifest, arms=["R", "R-loc"])
        gate = self.root / "fidelity/gate-report.json"
        pilot.require_fidelity(self.root, raw, manifest, self.groups)  # N/G can run before a report exists.
        with self.assertRaisesRegex(ValueError, "require a passing fidelity gate"):
            pilot.require_fidelity(self.root, raw, manifest, rql)
        outcomes = {"contract-cases": {"pass": True}, "value-transfers": {"pass": False}}
        report = {**engine.identity, "served_steps": manifest["served_steps"], "steps": outcomes,
                  "prescience_pooled": {"pass": True}}
        pilot.driver.save(gate, report)
        with self.assertRaisesRegex(ValueError, "no passing result for value-transfers"):
            pilot.require_fidelity(self.root, raw, manifest, rql)
        outcomes["value-transfers"]["pass"] = True
        pilot.driver.save(gate, report)
        self.assertEqual(pilot.require_fidelity(self.root, raw, manifest, rql), engine.identity)
        with self.assertRaisesRegex(ValueError, "another manifest"):
            pilot.require_fidelity(self.root, raw + b"\n", manifest, rql)
        pilot.driver.save(gate, {**report, "served_steps": ["value-transfers", "contract-cases"]})
        with self.assertRaisesRegex(ValueError, "another manifest, engine identity, or step list"):
            pilot.require_fidelity(self.root, raw, manifest, rql)
        pilot.driver.save(gate, {**report, "engine_inputs_sha256": "0" * 64})
        with self.assertRaisesRegex(ValueError, "engine identity"):
            pilot.require_fidelity(self.root, raw, manifest, rql)

    # Hard-won: 8c9d945: pooled prescience failure did not stop G writers after a re-freeze.
    def test_g_writer_admission_sees_pooled_failure_from_the_previous_freeze(self):
        manifest = self.manifest
        raw = json.dumps(manifest).encode()
        groups = pilot.select_jobs(manifest, arms=["G"])
        self.assertIsNone(pilot.require_fidelity(self.root, raw, manifest, groups))
        pilot.driver.save(self.root / "fidelity/gate-report.json",
                          {"manifest_sha256": "1" * 64, "engine_inputs_sha256": "2" * 64,
                           "served_steps": ["contract-cases"], "steps": {"contract-cases": {"pass": True}},
                           "prescience_pooled": {"pass": False}})
        reduced = {**manifest, "served_steps": []}
        with self.assertRaisesRegex(ValueError, "pooled prescience failed"):
            pilot.require_fidelity(self.root, json.dumps(reduced).encode(), reduced, groups)

    def pilot_engine_fixture(self, steps):
        from rql_mock import engine

        reference = pilot.driver.SCRIPTS / "brief/rql-census"
        manifest = {**self.manifest, "served_steps": steps, "input_paths": {}, "inputs": {},
                    "model_versions": {"claude": "Sonnet 5.5", "claude_provider_id": "claude-sonnet-5-5"}}
        documents = {"catalogue": (reference / "rql-documented-steps.md").read_bytes(),
                     "steps": (reference / "menu-steps.md").read_bytes(), "menu": b"{}",
                     "eligible_labels": "\n".join(map(json.dumps, manifest["diffs"])).encode()}
        for name, data in documents.items():
            path = self.root / name
            path.write_bytes(data)
            manifest["input_paths"][name] = path.name
            manifest["inputs"][f"{name}_sha256"] = pilot.digest(data)
        path = self.root / "manifest.json"
        pilot.driver.save(path, manifest)
        config = self.root / "engine.json"
        pilot.driver.save(config, {"manifest": str(path), "verifier": "/bin/true",
                                   "logins": [{"profile": "claude5", "engine_profile": "claude5-engine"}]})
        self.addCleanup(setattr, engine, "CONFIG", engine.CONFIG)
        engine.CONFIG = config
        return path, engine.load_engine()

    def test_writer_menu_lists_only_the_frozen_two_steps(self):
        refdir = pilot.driver.SCRIPTS / "brief/rql-census"
        semantics = (refdir / "menu-steps.md").read_bytes()
        raw = json.dumps({**self.manifest, "served_steps": ["contract-cases", "guarded-paths"]}).encode()
        runner = pilot.Pilot(SimpleNamespace(), {}, pilot.load_manifest(raw), raw, "image", b"Documented catalogue",
                             semantics)
        prompts = runner.prompts
        self.assertEqual(prompts["R"], prompts["R-loc"])
        self.assertIn("(contract-cases ", prompts["R"])
        self.assertIn("(guarded-paths ", prompts["R"])
        self.assertNotIn("value-transfers", prompts["R"])
        self.assertNotIn("rql_query", prompts["G"])

    # Hard-won: dfb55808: G received a shorter reference than the tool arms.
    def test_g_and_r_writers_receive_the_identical_reference_block(self):
        refdir = pilot.driver.SCRIPTS / "brief/rql-census"
        catalogue = (refdir / "rql-documented-steps.md").read_text()
        semantics = (refdir / "menu-steps.md").read_text()
        prompts = pilot.prompt_set(catalogue, semantics, ["value-transfers"])
        heading = "### Documented RQL reference"
        self.assertEqual(prompts["G"].split(heading, 1)[1], prompts["R"].split(heading, 1)[1])
        self.assertEqual(prompts["R"], prompts["R-loc"])
        self.assertIn(catalogue, prompts["G"])
        self.assertIn("You have no query tool.", prompts["G"])
        self.assertNotIn("rql_query", prompts["G"])
        self.assertIn("rql_query(query, page=1)", prompts["R"])

    def test_single_profile_rejects_duplicate_codex_homes_even_when_alias_disabled(self):
        (self.root / "study.config.toml").write_text('[mcp_servers.rql]\ncommand = "test"\n')
        spec = {"kind": "codex", "home": str(self.root), "codex_profile": "study"}
        targets = {"podman": {"image": "a" * 64, "pull_policy": "never"}}
        config = {"profiles": {"codex3": spec}, "targets": targets}
        self.assertEqual(pilot.preflight(config, "codex3"), "sha256:" + "a" * 64)
        config["profiles"]["alias"] = {**spec, "enabled": False}
        with self.assertRaisesRegex(ValueError, "exactly one Codex profile"):
            pilot.preflight(config, "codex3")

    def test_assignments_only_expose_rql_to_tool_arm_writers(self):
        manifest_path, engine = self.pilot_engine_fixture(["contract-cases"])
        hook = self.root / "hook"
        hook.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            "with open(sys.argv[1]) as f: assignment = json.load(f)\n"
            "tools = [] if assignment['arm'] == 'none' else ['rql_query']\n"
            "print(json.dumps({'session_id': assignment['session_id'], 'tools': tools}))\n"
        )
        hook.chmod(0o700)
        args = SimpleNamespace(rql_hook=hook, manifest=manifest_path)
        diff = self.manifest["diffs"][0]
        jobs = pilot.select_jobs(self.manifest, diff=diff["key"])[diff["key"]]
        for index, job in enumerate(jobs):
            path = pilot.job_path(self.root, job)
            sid = f"{index:032x}"
            identity = engine.identity if pilot.tool_arm(job) != "none" else None
            if identity is not None:
                with self.assertRaisesRegex(ValueError, "admitted engine identity"):
                    pilot.publish_tool_assignment(path, args, job, diff, sid, engine_identity=None)
            pilot.publish_tool_assignment(path, args, job, diff, sid, engine_identity=identity)
            assignment = pilot.read_json(path / "tool-assignment.json")
            endpoint = pilot.read_json(path / "tool-endpoint.json")
            if job["kind"] == "writer" and job["arm"] in ("R", "R-loc"):
                self.assertEqual(assignment["arm"], job["arm"])
                self.assertEqual(assignment["intro"], diff["intro"])
                self.assertEqual({key: assignment[key] for key in identity}, identity)
                self.assertEqual(endpoint, {"session_id": sid, "tools": ["rql_query"]})
                self.assertTrue((path / f"queries-{sid}.jsonl").exists())
            else:
                self.assertEqual(assignment, {"session_id": sid, "arm": "none", "manifest": str(args.manifest)})
                self.assertEqual(endpoint, {"session_id": sid, "tools": []})
                self.assertFalse(list(path.glob("queries-*.jsonl")))

    def test_reconnect_only_reads_the_live_connected_server_and_separates_turns(self):
        response = "**Failed**\n- `rql`: 1 tool\n\n**Connected**\n- `another`: 1 tool"
        self.assertIsNone(brief_driver.live_rql_count(response))
        for expected in (0, 1):
            label = "tool" if expected == 1 else "tools"
            text = f"**Connected**\n- `rql`: {expected} {label}"
            self.assertEqual(brief_driver.live_rql_count(text), expected)
        pages = [{"items": [{"position": 2, "role": "agent", "body": {"streaming": False}, "text": response},
                            {"position": 4, "role": "agent", "body": {"streaming": True}, "text": "partial"}]}]
        self.assertEqual(brief_driver.final_messages(pages, {"turn_start_position": 3}), [])
    def test_replicates_and_slots_have_distinct_paths(self):
        jobs = self.manifest["jobs"]
        paths = {job["job"]: pilot.job_path(self.root, job).relative_to(self.root) for job in jobs}
        self.assertEqual(len(set(paths.values())), len(jobs))
        self.assertEqual(paths["bifrost-dev/second/G/r2/writer"], Path("runs/bifrost-dev/second/G/r2/writer"))
        self.assertEqual(paths["bifrost-dev/second/R/r2/reviewer-2"],
                         Path("runs/bifrost-dev/second/R/r2/reviewers/2"))
        self.assertEqual(paths["mjolnir/first/N/r1/reviewer-1"], Path("runs/mjolnir/first/N/r1/reviewers/1"))

    def test_upstream_writer_replacement_invalidates_reviews_with_identical_brief(self):
        args = SimpleNamespace(run_root=self.root)
        writer = next(job for job in self.groups["mjolnir/first"] if job["kind"] == "writer")
        reviewer = next(job for job in self.groups["mjolnir/first"] if job["arm"] == "G" and job["slot"] == 1)

        class LocalPilot(pilot.Pilot):
            def render(self, job):
                path = pilot.job_path(self.args.run_root, job)
                inputs = {"prompt_sha256": pilot.digest(b"prompt"), "menu_steps_sha256": None}
                if job["kind"] == "reviewer":
                    upstream = self.complete(writer)
                    if not upstream:
                        return None
                    inputs.update(writer_session_id=upstream["session_id"], brief_sha256=upstream["brief_sha256"])
                return {"path": path, "inputs": inputs}

        runner = object.__new__(LocalPilot)
        runner.args = args

        def record(job, session):
            path = pilot.job_path(self.root, job)
            path.mkdir(parents=True, exist_ok=True)
            writer_job = job["kind"] == "writer"
            output = "brief.md" if writer_job else "output.md"
            prompt = "prompt.txt"
            field = "brief_sha256" if writer_job else "output_sha256"
            (path / output).write_bytes(b"output")
            (path / prompt).write_bytes(b"prompt")
            pilot.driver.save(path / "usage.json", {"totals": {"total_tokens": {"tokens": 12}}})
            pilot.driver.save(path / "audit.json", {"passed": True})
            proof = path / "mcp-reconnect-result.json"
            pilot.driver.save(proof, {"passed": True, "session_id": session, "expected_tools": []})
            pilot.driver.save(path / "done.json", {"session_id": session, field: pilot.digest(b"output"),
                                                   "mcp_reconnect_sha256": pilot.digest(proof.read_bytes()),
                                                   "inputs": runner.render(job)["inputs"]})
            return path

        writer_path = record(writer, "writer-one")
        reviewer_path = record(reviewer, "review-one")
        self.assertTrue(runner.complete(reviewer))
        pilot.archive(writer_path)
        self.assertFalse(runner.complete(reviewer))
        record(writer, "writer-two")
        self.assertFalse(runner.complete(reviewer))
        self.assertTrue((writer_path.with_name("writer.superseded-1") / "brief.md").is_file())
        self.assertEqual((reviewer_path / "output.md").read_bytes(), b"output")

    def test_usage_and_prompt_hash_are_both_required_for_skip(self):
        (self.root / "prompt.txt").write_bytes(b"used prompt")
        (self.root / "brief.md").write_bytes(b"valid brief")
        inputs = {"prompt_sha256": pilot.digest(b"used prompt")}
        pilot.driver.save(self.root / "done.json", {"session_id": "writer", "inputs": inputs,
                                                   "brief_sha256": pilot.digest(b"valid brief")})
        pilot.driver.save(self.root / "audit.json", {"passed": True})
        usage = self.root / "usage.json"
        pilot.driver.save(usage, {"totals": {"total_tokens": {"tokens": None}}})
        self.assertIsNone(pilot.result_record(self.root, "writer"))
        pilot.driver.save(usage, {"totals": {"total_tokens": {"tokens": 100}}})
        self.assertIsNotNone(pilot.result_record(self.root, "writer"))
        (self.root / "prompt.txt").write_bytes(b"edited prompt")
        self.assertIsNone(pilot.result_record(self.root, "writer"))

    def test_native_reviewer_prompt_matches_current_rust_sources(self):
        checkout = pilot.driver.SCRIPTS.parents[2]
        lanes = (checkout / "mj-review/src/lanes.rs").read_text()
        fn = lanes.split("pub fn review_prompt", 1)[1].split("pub use ", 1)[0]
        template = re.search(r'format!\(\s*("## Task(?:\\[\s\S]|[^"\\])*")', fn)[1]
        literal = json.loads(re.sub(r"\\\n\s*", "", template))
        rubric = (checkout / "mj-core/assets/instructions/review_rubric.md").read_bytes()
        self.assertEqual(pilot_review.RUBRIC_PATH.read_bytes(), rubric)
        expected = literal.format(
            repositories="Repository root: .\nRun this exact command: `git -C . diff --no-ext-diff --ignore-submodules=none tree-a tree-b`",
            brief="\n\n## Relations computed from the repository\n\nThese are relations computed from the repository at this commit, not findings. They may be incomplete.\n\nBrief.",
            declined="",
            *[2, '<changed_files source="git diff --numstat of the captured trees" trust="deterministic" changed_lines="2">\nTable\n</changed_files>',
              '<user_message index="1" current_outer_turn="true">\nTask\n</user_message>'],
        )
        actual = pilot_review.prompt_with_rubric(rubric, ("tree-a", "tree-b", 2, "Table"), "Task", "Brief.")
        self.assertEqual(actual.encode(), rubric + b"\n" + expected.encode())

    def test_audit_rejects_unpinned_history_network_and_cache(self):
        transcript = self.root / "transcript.json"
        calls = [
            "git diff " + "a" * 40 + " HEAD",
            "git log " + "b" * 40,
            "ls /run/hel/git-cache",
            "ssh an.example cat README",
        ]
        pilot.driver.save(transcript, [{"items": [
            {"seq": i, "role": "tool", "body": {"call": {"title": command, "kind": "execute"}}}
            for i, command in enumerate(calls)]}])
        report = audit.audit_pilot(transcript, "sandbox", commits=["a" * 40])
        self.assertFalse(report["passed"])
        self.assertEqual({hit["seq"] for hit in report["hits"]}, {1, 2, 3})
        self.assertEqual({hit["kind"] for hit in report["hits"]},
                         {"git-revision-outside-pin", "git-history-escape", "network-command"})
        self.assertFalse(audit.audit_pilot(self.root / "missing.json", "sandbox")["passed"])

    def test_pilot_audit_ignores_real_checkout_text_returned_by_git_diff(self):
        checkout = pilot.driver.SCRIPTS.parents[2]
        intro = "435b4ed941720b8dfa04fdb454ffa3ae3645f625"  # frozen child-model-wrong-efforts diff
        patch = subprocess.check_output(["git", "-C", checkout, "show", "--format=", intro]).decode()
        line = next(line for line in patch.splitlines() if line.startswith("+") and "/home/jonathan" in line)
        transcript = self.root / "transcript.json"
        call = {"seq": 1, "role": "tool", "body": {"call": {"title": "git diff HEAD^ HEAD",
                                                          "kind": "execute", "rawOutput": line}}}
        terminal = {"seq": 2, "role": "terminal", "body": {"kind": "terminal_output", "output": line}}
        pilot.driver.save(transcript, [{"items": [call, terminal]}])
        self.assertTrue(audit.audit_pilot(transcript, "sandbox", commits=[intro])["passed"])
        self.assertIn("output-outside-path", {hit["kind"] for hit in audit.audit_reviewer(transcript, "sandbox")["hits"]})

    def test_cache_attestation_enumerates_bundle_objects_not_just_workspace_refs(self):
        source = self.root / "source"
        checkout = self.root / "checkout"
        cache = self.root / "cache"
        cache.mkdir()

        def git(repo, *args):
            return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.PIPE).decode().strip()

        git(self.root, "init", "--quiet", str(source))
        git(source, "-c", "user.name=Pilot", "-c", "user.email=pilot@example.invalid",
            "commit", "--quiet", "--allow-empty", "-m", "pin")
        pin = git(source, "rev-parse", "HEAD")
        git(self.root, "clone", "--quiet", "--no-local", str(source), str(checkout))
        git(source, "bundle", "create", str(cache / "pin.bundle"), "HEAD")
        clean = pilot_cache_probe.attest(checkout, pin, pin, cache)
        self.assertTrue(clean["passed"])
        self.assertEqual(clean["snapshots"]["pin.bundle"]["commits"], [pin])
        git(source, "-c", "user.name=Pilot", "-c", "user.email=pilot@example.invalid",
            "commit", "--quiet", "--allow-empty", "-m", "later")
        later = git(source, "rev-parse", "HEAD")
        git(source, "bundle", "create", str(cache / "later.bundle"), "HEAD")
        exposed = pilot_cache_probe.attest(checkout, pin, pin, cache)
        self.assertFalse(exposed["passed"])
        self.assertEqual(exposed["snapshots"]["later.bundle"]["outside_pin"], [later])

    def test_concurrent_matrix_orders_reviewers_after_writers_and_drains_on_halt(self):
        import threading

        groups = pilot.select_jobs(self.manifest, diff="mjolnir/first", arms=["N", "G", "R"])
        started = []
        done = set()
        lock = threading.Lock()
        active_count = 0
        max_active = 0
        writer_barrier = threading.Barrier(2, timeout=5)
        reviewer_order_ok = True

        def complete(job):
            with lock:
                return job["job"] in done

        def execute(job):
            nonlocal active_count, max_active, reviewer_order_ok
            key = job["job"]
            with lock:
                active_count += 1
                max_active = max(max_active, active_count)
                started.append(key)
                if job["kind"] == "reviewer" and job["arm"] != "N":
                    writer_key = f"{job['diff']}/{job['arm']}/r{job['replicate']}/writer"
                    if writer_key not in done:
                        reviewer_order_ok = False
            try:
                if job["kind"] == "writer":
                    writer_barrier.wait()
                with lock:
                    done.add(key)
                return "done"
            finally:
                with lock:
                    active_count -= 1

        with redirect_stdout(StringIO()):
            rc = pilot.run_matrix(self.root, groups, "codex", complete, execute, FakeQuota("90"), concurrency=4)
        self.assertEqual(rc, 0)
        self.assertTrue(reviewer_order_ok)
        self.assertGreaterEqual(max_active, 2)
        self.assertEqual(len(done), len(groups["mjolnir/first"]))

        # Verify halt drains already-running peers cleanly without launching dependents.
        done.clear()
        started.clear()
        release_peer = threading.Event()
        peer_finished = threading.Event()
        both_started = threading.Barrier(2, timeout=5)

        def execute_halt(job):
            key = job["job"]
            with lock:
                started.append(key)
            if key == "mjolnir/first/G/r1/writer":
                both_started.wait()
                return "interrupted: test"
            if key == "mjolnir/first/R/r1/writer":
                both_started.wait()
                release_peer.wait(timeout=5)
                with lock:
                    done.add(key)
                peer_finished.set()
                return "done"
            with lock:
                done.add(key)
            return "done"

        def trigger_release():
            both_started_check = 0
            while both_started_check < 100 and not peer_finished.is_set():
                if "mjolnir/first/G/r1/writer" in started and "mjolnir/first/R/r1/writer" in started:
                    release_peer.set()
                    return
                both_started_check += 1
                threading.Event().wait(0.01)

        releaser = threading.Thread(target=trigger_release, daemon=True)
        releaser.start()
        with redirect_stdout(StringIO()):
            rc = pilot.run_matrix(self.root, groups, "codex", complete, execute_halt, FakeQuota("90"), concurrency=2)
        releaser.join(timeout=5)
        self.assertEqual(rc, 1)
        self.assertTrue(peer_finished.is_set())
        self.assertIn("mjolnir/first/R/r1/writer", done)
        self.assertNotIn("mjolnir/first/G/r1/reviewer-1", started)
        self.assertTrue((self.root / "active-diff.json").exists())


if __name__ == "__main__":
    unittest.main()
