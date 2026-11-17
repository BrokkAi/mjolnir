"""Isolated routing regressions for the historical verifier driver."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent


class VerifierInstanceChecks(unittest.TestCase):
    # Hard-won: dcd3d904e: private selection sent production verifier IDs to the retired daemon.
    def test_private_instance_refuses_before_external_commands_or_filesystem_writes(self):
        with tempfile.TemporaryDirectory(prefix="verify-instance-") as temp:
            root = Path(temp) / "root"
            bin_dir = Path(temp) / "bin"
            bin_dir.mkdir()
            marker = Path(temp) / "called"
            for command in ("git",):
                executable = bin_dir / command
                executable.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 90\n")
                executable.chmod(0o755)
            env = {**os.environ, "PATH": str(bin_dir), "REVIEW_ARMS_DIR": str(root),
                   "REVIEW_ARMS_INSTANCE": "private", "PYTHONDONTWRITEBYTECODE": "1"}
            result = subprocess.run([sys.executable, str(SCRIPTS / "verify_driver.py"), "batch"],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("private instance is retired", result.stderr)
            self.assertFalse(marker.exists())
            self.assertFalse(root.exists())

    # Hard-won: dcd3d904e: verifier workspace IDs were shared across instance selections.
    def test_default_workspace_and_quota_routing_are_isolated(self):
        with tempfile.TemporaryDirectory(prefix="verify-quota-") as temp:
            base = Path(temp)
            scripts, root, bin_dir = base / "scripts", base / "data", base / "bin"
            scripts.mkdir()
            bin_dir.mkdir()
            for name in ("verify_driver.py", "driver.py"):
                (scripts / name).write_bytes((SCRIPTS / name).read_bytes())
            (scripts / "corpus").mkdir()
            repo = root / "verify/repo"
            repo.mkdir(parents=True)
            verify = root / "judge"
            verify.mkdir()
            (verify / "batch-1.json").write_text(json.dumps([{"outputs": [{"output_id": "o1"}]}]))
            # This old location must be ignored; its ID must never reach mjx.
            (root / "verify/workspace.json").parent.mkdir(parents=True, exist_ok=True)
            (root / "verify/workspace.json").write_text('{"workspace":{"id":"legacy"}}')
            log = base / "calls.jsonl"
            mjx = scripts / "mjx"
            mjx.write_text(f'''#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
with open({str(log)!r}, "a") as f: f.write(json.dumps(args) + "\\n")
if args[:3] == ["workspaces", "create", "Review arms verify"]:
 print(json.dumps({{"workspace": {{"id": "new-default"}}}})); raise SystemExit
if args[0] == "quota":
 print(json.dumps({{"profiles": [{{"report": {{"windows": [{{"remaining_percent": 2}}]}}}}]}})); raise SystemExit
raise SystemExit("unexpected mjx command: " + repr(args))
''')
            mjx.chmod(0o755)
            git = bin_dir / "git"
            git.write_text(f'''#!/bin/sh
if [ "$1" = "-C" ] && [ "$3" = "rev-parse" ]; then printf '%s\\n' {str(base)!r}; exit 0; fi
exit 91
''')
            git.chmod(0o755)
            env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
                   "REVIEW_ARMS_DIR": str(root), "REVIEW_ARMS_REPO": str(base),
                   "REVIEW_ARMS_INSTANCE": "default",
                   "REVIEW_ARMS_QUOTA_FLOOR": "codex=25", "PYTHONDONTWRITEBYTECODE": "1"}
            result = subprocess.run([sys.executable, "-B", str(scripts / "verify_driver.py"),
                                     "judge", "--kind", "judge"], env=env,
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("held before verifier session start", result.stdout)
            self.assertEqual(json.loads((root / "default-instance/verify/workspace.json").read_text())
                             ["workspace"]["id"], "new-default")
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertTrue(any(call[0] == "quota" for call in calls))
            self.assertFalse(any(call[0] == "new" for call in calls))
            self.assertFalse(any("legacy" in call for call in calls))


if __name__ == "__main__":
    unittest.main()
