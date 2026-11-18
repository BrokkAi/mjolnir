import json
import pathlib
import tempfile
import threading
import unittest
from unittest import mock

import driver


class DriverRecovery(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.old_root = driver.ROOT
        driver.ROOT = pathlib.Path(self.tmp.name)
        self.addCleanup(setattr, driver, "ROOT", self.old_root)
        self.bug = next(iter(driver.CORPUS.values()))
        self.arm = "A"
        self.job = {"bug": self.bug["id"], "arm": self.arm}
        self.path = driver.ROOT / "runs" / self.bug["id"] / self.arm
        self.path.mkdir(parents=True)
        self.identity = {"bug": self.bug["id"], "arm": self.arm, "reviewers": driver.ARMS[self.arm],
                         "intro": self.bug["intro_commit"], "base": self.bug["base_commit"], "time_ns": 1}

    def put(self, name, value):
        (self.path / name).write_text(json.dumps(value))

    # Hard-won: 08fa2108a: a reservation refusal left a carrier that could not be safely resumed.
    def test_ambiguous_records_refuse_without_mutation(self):
        for marker in ("intent only", "launch only", "historical-context.txt",
                       "carrier-request.json", "review-start-intent.json"):
            with self.subTest(marker=marker):
                for path in self.path.iterdir():
                    path.unlink()
                if marker != "launch only":
                    self.put("launch-intent.json", self.identity)
                if marker != "intent only":
                    self.put("launch.json", {"session_id": "recorded"})
                if marker.endswith((".txt", ".json")):
                    (self.path / marker).write_text("ambiguous")
                with mock.patch.object(driver, "mj") as mj:
                    self.assertEqual("needs-recovery", driver.run_job(self.job, threading.Lock()))
                    mj.assert_not_called()

    def test_preprompt_recovery_uses_recorded_session_without_new(self):
        self.put("launch-intent.json", self.identity)
        self.put("launch.json", {"session_id": "recorded"})
        place = {"container": "c", "cwd": "/workspace/recorded/mjolnir"}
        before = {"session_start": {"x": self.bug["base_commit"]}, "remotes": [], "alternates": [],
                  "status": [], "base_tree": "a", "head_tree": "b", "commits": [], "refs": []}
        self.put("placement.json", place)
        self.put("pre-inventory.json", before)
        live = {"id": "recorded", "profile_id": "carrier", "workspace_id": "w", "is_idle": True,
                "lifecycle": "live", "no_later_history": True, "at": self.bug["intro_commit"],
                "base": self.bug["base_commit"], "config_options": [
                    {"key": "mode", "current": "plan"}, {"key": "web_fetch", "current": "disabled"}]}
        class StopBeforePrompt(Exception):
            pass

        with (mock.patch.object(driver, "quota_hold", return_value=None),
              mock.patch.object(driver, "workspace_id", return_value="w"),
              mock.patch.object(driver, "wait_idle", return_value=live),
              mock.patch.object(driver, "session", return_value=live),
              mock.patch.object(driver, "container", return_value=place),
              mock.patch.object(driver, "inventory", return_value=before),
              mock.patch.object(driver, "mj", side_effect=StopBeforePrompt) as mj):
            live["last_turn_outcome"] = {"command_id": "unexpected"}
            self.assertEqual("needs-recovery", driver.run_job(self.job, threading.Lock()))
            mj.assert_not_called()
            del live["last_turn_outcome"]
            with self.assertRaises(StopBeforePrompt):
                driver.run_job(self.job, threading.Lock())
            mj.assert_called_once()
            self.assertEqual(("message", "--session", "recorded"), mj.call_args.args[:3])


if __name__ == "__main__":
    unittest.main()
