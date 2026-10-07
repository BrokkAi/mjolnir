#!/usr/bin/env python3
"""Offline tests for Jev action policy, frozen experiments, and input scoring.

Run: python3 scripts/jev-scenarios-eval.test.py
The cases mirror `mj-core/src/assessment.rs` tests so the two policies cannot drift.
"""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("eval", Path(__file__).with_name("jev-scenarios-eval.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def verdict(failure=("none", 0.95), input_=("none", 0.95), work=("finished", 0.95)):
    result = {}
    for axis, (choice, confidence) in zip(("failure", "input", "work"), (failure, input_, work)):
        probabilities = {name: (confidence if name == choice else (1-confidence)/(len(module.CHOICES[axis])-1)) for name in module.CHOICES[axis]}
        result[axis] = {"choice": choice, "confidence": confidence, "probabilities": probabilities}
    return result


class Experiments(unittest.TestCase):
    def test_replay_preserves_probabilities_separately_from_provider_confidence(self):
        answers = {
            axis: {"type": "choice", **answer, "probabilities": {name: (0.8 if name == answer["choice"] else 0.2/(len(module.CHOICES[axis])-1)) for name in module.CHOICES[axis]}}
            for axis, answer in verdict(input_=("required", 0.42)).items()
        }

        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, limit):
                return json.dumps({"answers": answers, "model": "offline"}).encode()[:limit]

        original = module.urllib.request.urlopen
        module.urllib.request.urlopen = lambda request, timeout: Response()
        try:
            result = module.ask("offline-test", {}, self.fixture()["evidence"])
        finally:
            module.urllib.request.urlopen = original
        self.assertEqual(result["verdict"]["input"], {"choice": "required", "confidence": 0.42, "probabilities": answers["input"]["probabilities"]})
        self.assertEqual(result["answers"], answers)
        self.assertEqual(result["answers"]["input"]["probabilities"]["required"], 0.8)

    def test_account_failure_stops_admission_and_reports_incomplete_run(self):
        originals = module.api_key, module.load_fixtures, module.ask
        try:
            for status in (401, 402):
                with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                    calls = []

                    def denied(key, questions, evidence):
                        calls.append(evidence)
                        return {"error": f"HTTP {status}", "http_status": status}

                    module.api_key = lambda: "offline-test"
                    module.load_fixtures = lambda only: [self.fixture(f"case-{i}") for i in range(20)]
                    module.ask = denied
                    output = Path(directory)
                    with self.assertRaisesRegex(SystemExit, f"stopped after HTTP {status}"):
                        module.run(SimpleNamespace(questions=module.QUESTIONS, only=None, repeats=3, output=output))
                    records = [json.loads(line) for line in (output / "results.jsonl").read_text().splitlines()]
                    self.assertGreater(len(calls), 0)
                    self.assertLessEqual(len(calls), 2)
                    self.assertEqual(len(records), len(calls))
                    self.assertTrue(all(record["http_status"] == status for record in records))
                    self.assertEqual(json.loads((output / "stopped.json").read_text())["requested_jobs"], 60)
                    self.assertIn("Incomplete run:", (output / "report.md").read_text())
                    with self.assertRaisesRegex(ValueError, "new output directory"):
                        module.run(SimpleNamespace(questions=module.QUESTIONS, only=None, repeats=3, output=output))
        finally:
            module.api_key, module.load_fixtures, module.ask = originals

    def test_bounded_admission_completes_all_successful_repeats(self):
        originals = module.api_key, module.load_fixtures, module.ask
        try:
            module.api_key = lambda: "offline-test"
            module.load_fixtures = lambda only: [self.fixture("one"), self.fixture("two")]
            module.ask = lambda key, questions, evidence: {"verdict": verdict(input_=("required", 0.95))}
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory)
                module.run(SimpleNamespace(questions=module.QUESTIONS, only=None, repeats=3, output=output))
                records = [json.loads(line) for line in (output / "results.jsonl").read_text().splitlines()]
                self.assertEqual({(r["id"], r["repeat"]) for r in records}, {(i, r) for i in ("one", "two") for r in range(3)})
                self.assertEqual(len(records), 6)
                self.assertTrue(all(r["action"] == "await_input" for r in records))
                self.assertFalse((output / "stopped.json").exists())
        finally:
            module.api_key, module.load_fixtures, module.ask = originals

    def test_malformed_prompt_is_rejected_before_credentials_or_requests(self):
        def forbidden_credentials():
            self.fail("malformed prompts must not resolve credentials")

        original = module.api_key
        module.api_key = forbidden_credentials
        try:
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "questions.json"
                path.write_text(json.dumps({axis: {"instructions": "Decide", "criteria": {"none": {"what": "No request"}}} for axis in ("failure", "input", "work")}))
                with self.assertRaisesRegex(ValueError, "question type must be choice"):
                    module.run(SimpleNamespace(questions=path))
        finally:
            module.api_key = original

    @staticmethod
    def fixture(identity="D01", input_="required"):
        return {
            "id": identity,
            "category": "decision",
            "evidence": {"assistant_text_tail": "Choose a destination.", "background_commands": 1},
            "expected": {"failure": "none", "input": input_, "work": "waiting", "action": "await_input" if input_ == "required" else "wait"},
        }

    def test_resume_accepts_identical_experiment_and_rejects_changed_prompt_or_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            fixtures = [self.fixture()]
            questions = {"input": {"instructions": "Classify the user request."}}
            module.prepare_run(output, questions, fixtures)
            (output / "results.jsonl").write_text('{"id":"D01","repeat":0}\n')
            module.prepare_run(output, questions, fixtures)
            with self.assertRaisesRegex(ValueError, "new output directory"):
                module.prepare_run(output, {"input": {"instructions": "Changed prompt"}}, fixtures)
            changed = json.loads(json.dumps(fixtures))
            changed[0]["evidence"]["assistant_text_tail"] = "No decision remains."
            with self.assertRaisesRegex(ValueError, "new output directory"):
                module.prepare_run(output, questions, changed)

    def test_policy_change_prevents_resume(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            module.prepare_run(output, {}, [self.fixture()])
            manifest = json.loads((output / "run.json").read_text())
            del manifest["policy"]
            (output / "run.json").write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "policy.*new output directory"):
                module.prepare_run(output, {}, [self.fixture()])

    def test_historical_report_keeps_its_original_confidence_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            module.prepare_run(output, {}, [self.fixture()])
            record = {"id": "D01", "repeat": 0, "verdict": verdict(input_=("required", .6)), "action": "await_input"}
            (output / "results.jsonl").write_text(json.dumps(record) + "\n")
            module.report(SimpleNamespace(output=output))
            self.assertIn("| required | 1 | 1 | 0 | 1 | 0 | 0 | 0 |", (output / "report.md").read_text())
            manifest = json.loads((output / "run.json").read_text())
            del manifest["policy"]
            (output / "run.json").write_text(json.dumps(manifest))
            module.report(SimpleNamespace(output=output))
            self.assertIn("| required | 1 | 1 | 0 | 0 | 1 | 0 | 0 |", (output / "report.md").read_text())

    def test_existing_unidentified_results_cannot_be_resumed(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            (output / "results.jsonl").write_text("{}\n")
            with self.assertRaisesRegex(ValueError, "no experiment manifest"):
                module.prepare_run(output, {}, [self.fixture()])

    def test_report_measures_input_detection_independently_and_exposes_control_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            fixtures = [self.fixture("frozen-required"), self.fixture("frozen-control", "none")]
            ambiguous = self.fixture("frozen-ambiguous", "unclear")
            ambiguous["context"] = {"strict_input_scoring": False}
            fixtures.append(ambiguous)
            module.prepare_run(output, {}, fixtures)
            records = [
                {"id": "frozen-required", "repeat": 0, "verdict": verdict(input_=("required", 0.41)), "action": "uncertain"},
                {"id": "frozen-required", "repeat": 1, "verdict": verdict(input_=("required", 0.85), failure=("unclear", 0.3)), "action": "await_input"},
                {"id": "frozen-required", "repeat": 2, "error": "HTTP 503"},
                {"id": "frozen-control", "repeat": 0, "verdict": verdict(input_=("required", 0.99)), "action": "await_input"},
                {"id": "frozen-ambiguous", "repeat": 0, "verdict": verdict(input_=("required", 0.99)), "action": "await_input"},
            ]
            (output / "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))
            module.report(SimpleNamespace(output=output))
            report = (output / "report.md").read_text()
            # The 0.41 answer agrees on the choice but does not notify; failure
            # uncertainty does not invalidate a confident required-input answer.
            self.assertIn("| required | 3 | 2 | 1 | 1 | 1 | 0 | 1 |", report)
            self.assertIn("| none | 1 | 0 | 0 | 0 | 0 | 1 | 0 |", report)
            self.assertIn("Ambiguous cases excluded from strict input counts: 1 requests.", report)
            self.assertNotIn("| unclear |", report)


class ActionPolicy(unittest.TestCase):
    def test_shared_rust_probability_gate_cases(self):
        cases = json.loads((module.ROOT / "mj-core/tests/jev-gates.json").read_text())
        for case in cases:
            with self.subTest(case=case["name"]):
                parsed = module.parse_answers(case["answers"])
                self.assertEqual(module.action(parsed, case["authorization_complete"]), case["action"])
                self.assertEqual(module.requires_input(parsed["input"]), case["requires_input"])

    def test_malformed_distributions_fail_visibly(self):
        cases = json.loads((module.ROOT / "mj-core/tests/jev-gates.json").read_text())
        for bad in (None, {}, {"required": 1}, {"required": .5, "none": .2, "redundant_request": .2, "unclear": -.1}):
            answers = json.loads(json.dumps(cases[0]["answers"]))
            answers["input"]["probabilities"] = bad
            with self.assertRaises(ValueError):
                module.parse_answers(answers)

    def test_confidence_requires_a_bounded_number_like_the_runtime(self):
        cases = json.loads((module.ROOT / "mj-core/tests/jev-gates.json").read_text())
        for bad in (True, "0.99", None, float("nan"), float("inf"), -.1, 1.1):
            answers = json.loads(json.dumps(cases[0]["answers"]))
            answers["input"]["confidence"] = bad
            with self.assertRaises(ValueError):
                module.parse_answers(answers)

    def test_required_input_beats_everything_but_a_confident_quota_stop(self):
        self.assertEqual(module.action(verdict(input_=("required", 0.85), failure=("transient_provider", 0.99)), True), "await_input")
        self.assertEqual(module.action(verdict(input_=("required", 0.85), failure=("quota", 0.99)), True), "recover_quota")
        self.assertEqual(module.action(verdict(input_=("required", 0.85), failure=("quota", 0.89)), True), "await_input")
        self.assertEqual(module.action(verdict(input_=("required", 0.49), failure=("none", 0.99)), True), "uncertain")

    def test_provider_recovery_is_independent_of_work(self):
        self.assertEqual(module.action(verdict(failure=("transient_provider", 0.91), work=("unclear", 0.3)), False), "retry_provider")
        self.assertEqual(module.action(verdict(failure=("transient_provider", 0.89)), False), "uncertain")
        self.assertEqual(module.action(verdict(failure=("quota", 0.9)), False), "recover_quota")
        self.assertEqual(module.action(verdict(failure=("other", 0.9)), False), "await_input")

    def test_unsure_failure_blocks_finished_and_continue(self):
        self.assertEqual(module.action(verdict(failure=("none", 0.89)), True), "uncertain")
        self.assertEqual(module.action(verdict(failure=("unclear", 0.5)), True), "uncertain")

    def test_continue_needs_complete_authorization_and_both_bars(self):
        unfinished = verdict(work=("authorized_unfinished", 0.9), input_=("none", 0.9))
        self.assertEqual(module.action(unfinished, True), "continue")
        self.assertEqual(module.action(unfinished, False), "uncertain")
        redundant = verdict(work=("authorized_unfinished", 0.9), input_=("redundant_request", 0.9))
        self.assertEqual(module.action(redundant, True), "continue")
        self.assertEqual(module.action(verdict(work=("authorized_unfinished", 0.89), input_=("none", 0.99)), True), "uncertain")
        self.assertEqual(module.action(verdict(work=("authorized_unfinished", 0.99), input_=("none", 0.89)), True), "uncertain")

    def test_finished_and_wait_need_the_activity_bar(self):
        self.assertEqual(module.action(verdict(work=("finished", 0.85), input_=("none", 0.85)), False), "finished")
        self.assertEqual(module.action(verdict(work=("waiting", 0.85), input_=("none", 0.85)), False), "wait")
        self.assertEqual(module.action(verdict(work=("finished", 0.79)), False), "uncertain")
        self.assertEqual(module.action(verdict(work=("finished", 0.95), input_=("redundant_request", 0.95)), False), "uncertain")


class Authorization(unittest.TestCase):
    def messages(self):
        return [{"id": "user:1", "role": "user", "text": "fix it"}, {"id": "agent:2", "role": "assistant", "text": "done"}]

    def test_complete_history_is_accepted(self):
        evidence = {"authorization": {"messages": self.messages(), "authorization_complete": True, "final_reply_omitted": False}}
        self.assertTrue(module.authorization_complete(evidence))

    def test_missing_user_or_trailing_user_is_rejected(self):
        only_assistant = {"authorization": {"messages": self.messages()[1:], "authorization_complete": True}}
        self.assertFalse(module.authorization_complete(only_assistant))
        trailing_user = {"authorization": {"messages": self.messages()[::-1], "authorization_complete": True}}
        self.assertFalse(module.authorization_complete(trailing_user))
        self.assertFalse(module.authorization_complete({"authorization": None}))
        omitted = {"authorization": {"messages": self.messages(), "authorization_complete": True, "final_reply_omitted": True}}
        self.assertFalse(module.authorization_complete(omitted))


if __name__ == "__main__":
    unittest.main()
