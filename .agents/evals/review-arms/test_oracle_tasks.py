"""Offline serialized-contract checks for the oracle edit/leak/factcheck lifecycle."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
BUG = "sample-diff"


def command(*args, env):
    return subprocess.run([sys.executable, *map(str, args)], env=env, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)


class OracleTaskLifecycle(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="oracle-task-lifecycle-")
        self.root = Path(self.temp.name) / "data"
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        def git(*args):
            subprocess.run(["git", "-C", str(self.repo), *args], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        git("init", "-q")
        git("config", "user.email", "fixture@example.invalid")
        git("config", "user.name", "Fixture")
        (self.repo / "src").mkdir()
        (self.repo / "src/a.rs").write_text("fn before() {}\n")
        (self.repo / "src/b.rs").write_text("fn index() {}\n")
        git("add", ".")
        git("commit", "-qm", "base")
        self.base = subprocess.check_output(["git", "-C", str(self.repo), "rev-parse", "HEAD"], text=True).strip()
        (self.repo / "src/a.rs").write_text("fn after() {}\n")
        git("add", ".")
        git("commit", "-qm", "intro")
        self.intro = subprocess.check_output(["git", "-C", str(self.repo), "rev-parse", "HEAD"], text=True).strip()
        self.corpus = Path(self.temp.name) / "corpus.jsonl"
        self.corpus.write_text(json.dumps({"id": BUG, "intro_commit": self.intro, "base_commit": self.base,
            "defect": "fixture defect", "location": "src/a.rs", "trigger": "fixture",
            "match_rule": "match", "near_misses": [], "why_findable": "fixture",
            "fix_commit": self.intro}) + "\n")
        self.census = Path(self.temp.name) / "census.jsonl"
        self.census.write_text(json.dumps({"id": BUG, "fact": "The parser calls the indexer.",
            "evidence": ["src/b.rs:1"], "other_facts": [], "kind": "value flow", "query": "secret query"}) + "\n")
        self.jobs = Path(self.temp.name) / "jobs.json"
        self.jobs.write_text(json.dumps([{"bug": BUG, "arm": "OT"}]))
        self.name = "oracle-fixture"
        self.env = {**os.environ, "REVIEW_ARMS_DIR": str(self.root), "REVIEW_ARMS_REPO": str(self.repo),
                    "REVIEW_ARMS_CORPUS": str(self.corpus), "PYTHONDONTWRITEBYTECODE": "1"}
        self.bt = "## Affected callers\n\n## Paths\n\n- Existing relation (`src/b.rs:1`).\n\n## Value flow\n"
        bt_dir = self.root / "briefs/blind" / BUG
        bt_dir.mkdir(parents=True)
        (bt_dir / "brief.md").write_text(self.bt)
        self.work = self.root / "briefs" / f"{self.name}-work"
        self.candidates = self.work / "candidates"

    def tearDown(self):
        self.temp.cleanup()

    def run_oracle(self, step, *extra):
        return command(SCRIPTS / "brief_oracle.py", step, "--census", self.census,
                       "--bugs-from", self.jobs, "--name", self.name, *extra, env=self.env)

    def run_leak(self, step, round_number=None):
        args = [SCRIPTS / "brief_writer_terra.py", step, "--kind", "oracle", "--brief-root", self.candidates,
                "--bugs-from", self.jobs, "--name", self.name]
        if round_number is not None:
            args += ["--round", str(round_number)]
        return command(*args, env=self.env)

    def assert_ok(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)

    def write_leak_output(self, round_number, label):
        leak_dir = self.root / "briefs" / f"{self.name}-leak-{round_number}"
        key = json.loads((leak_dir / "key.json").read_text())
        verdicts = []
        for oid, entry in key.items():
            for judge in ("A", "B"):
                verdicts.append({"output_id": oid, "judge": judge, "bug": entry["bug"], "label": label,
                                 "matching_finding": "candidate relation" if label != "missed" else None,
                                 "rationale": "wording leaks" if label != "missed" else "no match",
                                 "confidence": "high"})
        (leak_dir / "workflow-out.json").write_text(json.dumps({"verdicts": verdicts, "missing": []}))

    def test_single_item_preserves_existing_blank_separator_and_rejects_other_edits(self):
        self.assert_ok(self.run_oracle("edit-build"))
        inserted = "- The parser calls the indexer (`src/b.rs:1`)."
        existing = "- Existing relation (`src/b.rs:1`)."
        final = self.bt.replace(existing + "\n", existing + "\n" + inserted + "\n")
        output = self.work / "edit" / BUG / "edit.json"
        edit = {"id": BUG, "contains": "no", "matching_item": None, "inserted_item": inserted,
                "replaced_item": None, "oracle_brief": final, "rationale": "One relation inserted."}
        output.write_text(json.dumps({**edit, "oracle_brief": final.replace("Existing relation", "Changed relation")}))
        rejected = self.run_oracle("edit-collect")
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("changed text beyond", rejected.stderr)
        output.write_text(json.dumps({**edit, "replaced_item": existing,
                                     "oracle_brief": self.bt.replace(existing, inserted)}))
        rejected = self.run_oracle("edit-collect")
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("already has 25 items", rejected.stderr)
        output.write_text(json.dumps(edit))
        self.assert_ok(self.run_oracle("edit-collect"))
        self.assertEqual((self.candidates / BUG / "candidate.md").read_text(), final)

    def test_complete_candidate_lifecycle_hashes_literal_quote_and_isolation(self):
        self.assert_ok(self.run_oracle("edit-build"))
        item_path = self.work / "edit" / BUG / "item.json"
        item = json.loads(item_path.read_text())
        self.assertEqual(set(item["census"]), {"fact", "evidence", "other_facts", "kind"})
        self.assertNotIn("query", item_path.read_text())

        inserted = "- The parser calls the indexer (`src/b.rs:1`)."
        edit = {"id": BUG, "contains": "no", "matching_item": None, "inserted_item": inserted,
                "replaced_item": None, "oracle_brief": self.bt.replace("## Value flow\n", f"## Value flow\n\n{inserted}\n"),
                "rationale": "The source locations describe the relation."}
        (self.work / "edit" / BUG / "edit.json").write_text(json.dumps(edit))
        self.assert_ok(self.run_oracle("edit-collect"))
        candidate = self.candidates / BUG / "candidate.md"
        self.assertEqual(candidate.read_text(), edit["oracle_brief"])
        self.assertFalse((self.root / "briefs/oracle" / BUG / "brief.md").exists())

        self.assert_ok(self.run_leak("leak-build", 0))
        self.write_leak_output(0, "caught")
        self.assert_ok(self.run_leak("leak-collect", 0))
        self.assert_ok(self.run_leak("rewrite-build"))
        rewritten = edit["oracle_brief"].replace("calls", "invokes")
        rewrite_dir = self.root / "briefs" / f"{self.name}-rewrite" / BUG
        (rewrite_dir / "brief.md").write_text(rewritten)
        self.assert_ok(self.run_leak("rewrite-collect"))
        self.assertEqual((self.candidates / BUG / "final.md").read_text(), rewritten)
        self.assert_ok(self.run_leak("leak-build", 1))
        self.write_leak_output(1, "missed")
        self.assert_ok(self.run_leak("leak-collect", 1))
        self.assert_ok(self.run_oracle("task-sync"))

        stale = {"decision": "carries", "brief_sha256": "stale", "item": "- The parser invokes the indexer (`src/b.rs:1`)."}
        (self.work / "factcheck-decisions.json").write_text(json.dumps({BUG: stale}))
        pending = self.run_oracle("collect")
        self.assertNotEqual(pending.returncode, 0)
        self.assertFalse((self.root / "briefs/oracle" / BUG / "brief.md").exists())

        digest = hashlib.sha256((rewritten.rstrip() + "\n").encode()).hexdigest()
        wrong_quote = {"decision": "carries", "brief_sha256": digest, "item": "not literally present"}
        (self.work / "factcheck-decisions.json").write_text(json.dumps({BUG: wrong_quote}))
        rejected = self.run_oracle("collect")
        self.assertNotEqual(rejected.returncode, 0)
        self.assertFalse((self.root / "briefs/oracle" / BUG / "brief.md").exists())

        good = {"decision": "carries", "brief_sha256": digest,
                "item": "- The parser invokes the indexer (`src/b.rs:1`)."}
        (self.work / "factcheck-decisions.json").write_text(json.dumps({BUG: good}))
        self.assert_ok(self.run_oracle("collect"))
        self.assertEqual((self.root / "briefs/oracle" / BUG / "brief.md").read_text(), rewritten.rstrip() + "\n")


if __name__ == "__main__":
    unittest.main()
