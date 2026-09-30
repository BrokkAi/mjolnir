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
    return {
        "failure": {"choice": failure[0], "confidence": failure[1]},
        "input": {"choice": input_[0], "confidence": input_[1]},
        "work": {"choice": work[0], "confidence": work[1]},
    }


class Experiments(unittest.TestCase):
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
    def test_required_input_beats_everything(self):
        self.assertEqual(module.action(verdict(input_=("required", 0.85), failure=("transient_provider", 0.99)), True), "await_input")
        self.assertEqual(module.action(verdict(input_=("required", 0.84), failure=("none", 0.99)), True), "uncertain")

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
        self.assertEqual(module.action(verdict(work=("finished", 0.84)), False), "uncertain")
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
