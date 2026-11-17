"""Isolated review-driver workspace checks; never starts mj or a daemon."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
IMPORT_SCRIPT = r'''
import importlib
import os
import sys
sys.path.insert(0, sys.argv[1])
driver = importlib.import_module("driver")
importlib.import_module("brief_driver")
importlib.import_module("brief_score")
importlib.import_module("brief_writer_terra")
assert not (driver.INSTANCE / "workspace.json").exists()
os.environ["REVIEW_ARMS_QUOTA_FLOOR"] = "codex4=25"
driver.mj = lambda *args, **kwargs: {"profiles": [{"report": {"windows": [{"remaining_percent": 50}]}}]}
assert driver.quota_hold() is None
driver.INSTANCE.mkdir(parents=True, exist_ok=True)
(driver.INSTANCE / "workspace.json").write_text('{"workspace":{"id":"selected-instance"}}')
assert driver.workspace_id() == "selected-instance"
'''


class ReviewerWorkspaceChecks(unittest.TestCase):
    # Hard-won: dcd3d904e: importing scorers or quota guards required a reviewer workspace.
    def test_imports_and_quota_do_not_need_workspace_record(self):
        for selected in ("default", "private"):
            with self.subTest(instance=selected), tempfile.TemporaryDirectory(prefix="review-workspace-") as tmp:
                root = Path(tmp)
                (root / "instance/config").mkdir(parents=True)
                (root / "default-instance").mkdir()
                env = {**os.environ, "REVIEW_ARMS_DIR": str(root),
                       "REVIEW_ARMS_INSTANCE": selected, "REVIEW_ARMS_QUOTA_FLOOR": "",
                       "PYTHONDONTWRITEBYTECODE": "1"}
                result = subprocess.run([sys.executable, "-c", IMPORT_SCRIPT, str(SCRIPTS)],
                                        env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)

    # Hard-won: dcd3d904e: missing local setup left a launch intent without a session.
    def test_missing_workspace_preflight_leaves_no_launch_intent(self):
        for cli in ("driver.py", "brief_driver.py"):
            with self.subTest(cli=cli), tempfile.TemporaryDirectory(prefix="review-preflight-") as tmp:
                root = Path(tmp)
                scripts = root / "scripts"
                corpus = scripts / "corpus"
                corpus.mkdir(parents=True)
                for name in ("driver.py", "brief_driver.py"):
                    (scripts / name).write_bytes((SCRIPTS / name).read_bytes())
                bug = {"id": "fixture-bug", "intro_commit": "intro", "base_commit": "base"}
                (corpus / "diffs.jsonl").write_text(json.dumps(bug) + "\n")
                call_log = root / "mjx-called"
                fake_mjx = scripts / "mjx"
                fake_mjx.write_text(f"#!/bin/sh\nprintf called >> '{call_log}'\nexit 99\n")
                fake_mjx.chmod(0o755)
                env = {**os.environ, "REVIEW_ARMS_DIR": str(root / "data"),
                       "REVIEW_ARMS_INSTANCE": "private", "REVIEW_ARMS_REPO": str(SCRIPTS.parents[2]),
                       "REVIEW_ARMS_QUOTA_FLOOR": "", "PYTHONDONTWRITEBYTECODE": "1"}
                if cli == "driver.py":
                    jobs = root / "jobs.json"
                    jobs.write_text('[{"bug":"fixture-bug","arm":"A"}]')
                    args = ["--jobs", str(jobs)]
                else:
                    args = ["fixture-bug"]
                result = subprocess.run([sys.executable, str(scripts / cli), *args],
                                        env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("workspace.json", result.stdout + result.stderr)
                intent = root / "data" / ("runs/fixture-bug/A/launch-intent.json"
                                           if cli == "driver.py" else
                                           "briefs/blind/fixture-bug/launch-intent.json")
                self.assertFalse(intent.exists())
                self.assertFalse(call_log.exists())


if __name__ == "__main__":
    unittest.main()
