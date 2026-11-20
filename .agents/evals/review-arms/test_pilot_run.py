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
            pilot.driver.save(path / "done.json", {"session_id": session, field: pilot.digest(b"output"),
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
