"""Exercise the pilot scheduler and output contract without a daemon."""
from contextlib import redirect_stdout
from decimal import Decimal
from io import StringIO
import json
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest

import pilot_run as pilot
import audit
import brief_driver
import pilot_cache_probe
import pilot_review


def manifest_fixture():
    manifest = {"version": 1, "seed": 20261120, "plan_commit": "c" * 40, "inputs": {},
                "models": {"writer": "gpt-6.1-sol/high", "reviewer": "gpt-6.1-terra/high",
                           "judge": "fable/high", "engine": "fable/high"},
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

        def run(argv, **kwargs):
            seen.append(argv)
            if "sessions" in argv:
                answer = {"sessions": [{"id": sid, "lifecycle": "live", "title": "same title"} for sid in paths]}
            else:
                sid = argv[argv.index("--session") + 1]
                path = paths[sid]
                receipt = pilot.read_json(path / "receipt.json")
                self.assertEqual(receipt["session_id"], sid)
                self.assertTrue(pilot.read_json(path / "audit.json")["passed"])
                for name, sha in receipt["evidence_sha256"].items():
                    self.assertEqual(pilot.digest((path / name).read_bytes()), sha)
                answer = {"session_id": sid, "accepted": True}
            return SimpleNamespace(returncode=0, stdout=json.dumps(answer).encode())

        driver.subprocess.run = run
        for kind, model, output in (("writer", "gpt-6.1-sol", "brief.md"),
                                    ("reviewer", "gpt-6.1-terra", "output.md")):
            sid = f"{kind}-id"
            path = self.captured_job(kind, sid, model, output)
            paths[sid] = path
            params = {"run_root": self.root, "path": path, "inputs": {"rql_tools": []}, "model": (model, "high")}
            sha = pilot.digest((path / output).read_bytes())
            if kind == "writer":
                (path / "brief-candidate.md").write_bytes(b"Captured")
                pilot.driver.save(path / "accepted.json", {"brief_sha256": sha, "fix_rounds": 0})
                state = brief_driver.finish_pilot_writer(params, sid, "bug", "pinned")
            else:
                params.update(bug={}, profile="pinned", tag="reviewer")
                pilot.driver.save(path / "captured.json", {"output_sha256": sha, "session_id": sid})
                state = pilot_review.run(params)
            self.assertEqual(state, "done")
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
        manifest = {**self.manifest, "served_steps": ["contract-cases", "value-transfers"]}
        raw = json.dumps(manifest).encode()
        rql = pilot.select_jobs(manifest, arms=["R", "R-loc"])
        gate = self.root / "fidelity/gate-report.json"
        pilot.require_fidelity(self.root, raw, manifest, self.groups)  # N/G can run before a report exists.
        with self.assertRaisesRegex(ValueError, "require a passing fidelity gate"):
            pilot.require_fidelity(self.root, raw, manifest, rql)
        outcomes = {"contract-cases": {"pass": True}, "value-transfers": {"pass": False}}
        report = {"manifest_sha256": pilot.digest(raw), "served_steps": manifest["served_steps"], "steps": outcomes}
        pilot.driver.save(gate, report)
        with self.assertRaisesRegex(ValueError, "no passing result for value-transfers"):
            pilot.require_fidelity(self.root, raw, manifest, rql)
        outcomes["value-transfers"]["pass"] = True
        pilot.driver.save(gate, report)
        pilot.require_fidelity(self.root, raw, manifest, rql)
        with self.assertRaisesRegex(ValueError, "another manifest"):
            pilot.require_fidelity(self.root, raw + b"\n", manifest, rql)
        pilot.driver.save(gate, {**report, "served_steps": ["value-transfers", "contract-cases"]})
        with self.assertRaisesRegex(ValueError, "another manifest or step list"):
            pilot.require_fidelity(self.root, raw, manifest, rql)

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
        hook = self.root / "hook"
        hook.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            "with open(sys.argv[1]) as f: assignment = json.load(f)\n"
            "tools = [] if assignment['arm'] == 'none' else ['rql_query']\n"
            "print(json.dumps({'session_id': assignment['session_id'], 'tools': tools}))\n"
        )
        hook.chmod(0o700)
        args = SimpleNamespace(rql_hook=hook, manifest=self.root / "manifest.json")
        diff = self.manifest["diffs"][0]
        jobs = pilot.select_jobs(self.manifest, diff=diff["key"])[diff["key"]]
        for index, job in enumerate(jobs):
            path = pilot.job_path(self.root, job)
            sid = f"{index:032x}"
            pilot.publish_tool_assignment(path, args, job, diff, sid)
            assignment = pilot.read_json(path / "tool-assignment.json")
            endpoint = pilot.read_json(path / "tool-endpoint.json")
            if job["kind"] == "writer" and job["arm"] in ("R", "R-loc"):
                self.assertEqual(assignment["arm"], job["arm"])
                self.assertEqual(assignment["intro"], diff["intro"])
                self.assertEqual(endpoint, {"session_id": sid, "tools": ["rql_query"]})
                self.assertTrue((path / "queries.jsonl").exists())
            else:
                self.assertEqual(assignment, {"session_id": sid, "arm": "none", "manifest": str(args.manifest)})
                self.assertEqual(endpoint, {"session_id": sid, "tools": []})
                self.assertFalse((path / "queries.jsonl").exists())

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


if __name__ == "__main__":
    unittest.main()
