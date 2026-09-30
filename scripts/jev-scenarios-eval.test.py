#!/usr/bin/env python3
"""Offline tests for the Python port of the Jev action policy in jev-scenarios-eval.py.

Run: python3 scripts/jev-scenarios-eval.test.py
The cases mirror `mj-core/src/assessment.rs` tests so the two policies cannot drift.
"""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("eval", Path(__file__).with_name("jev-scenarios-eval.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def verdict(failure=("none", 0.95), input_=("none", 0.95), work=("finished", 0.95)):
    return {
        "failure": {"choice": failure[0], "confidence": failure[1]},
        "input": {"choice": input_[0], "confidence": input_[1]},
        "work": {"choice": work[0], "confidence": work[1]},
    }


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
