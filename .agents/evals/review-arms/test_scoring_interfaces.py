"""Offline subprocess regressions for persisted scoring and leak interfaces."""
import csv
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent


class ScoringInterfaces(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="scoring-interface-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        commit = subprocess.check_output(["git", "-C", str(SCRIPTS), "rev-parse", "HEAD"], text=True).strip()
        self.records = [{"id": b, "intro_commit": commit, "base_commit": commit,
                         **{k: "fixture" for k in ("defect", "location", "trigger", "match_rule",
                                                   "near_misses", "why_findable", "fix_commit")}}
                        for b in ("bug-one", "bug-two", "ordinary-one")]
        self.write("corpus.jsonl", "".join(json.dumps(r) + "\n" for r in self.records[:2]))
        self.write("diffs.jsonl", json.dumps(self.records[2]) + "\n")
        self.save("sets.json", {"P": ["bug-one"], "S": ["bug-two"], "C": []})
        self.save("jobs.json", [{"bug": "bug-one", "arm": a} for a in ("A", "BT")])
        self.env = {**os.environ, "REVIEW_ARMS_DIR": str(self.root),
                    "REVIEW_ARMS_CORPUS": str(self.root / "corpus.jsonl"),
                    "REVIEW_ARMS_DIFFS": str(self.root / "diffs.jsonl"),
                    "REVIEW_ARMS_SETS": str(self.root / "sets.json"),
                    "PYTHONDONTWRITEBYTECODE": "1"}

    def write(self, name, text):
        p = self.root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        return p

    def save(self, name, value):
        return self.write(name, json.dumps(value))

    def run_script(self, script, *args, ok=True, env=None):
        r = subprocess.run([sys.executable, str(SCRIPTS / script), *map(str, args)],
                           env=env or self.env, capture_output=True, text=True)
        if ok:
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        else:
            self.assertNotEqual(r.returncode, 0, r.stdout)
        return r

    def reviews(self, bug="bug-one", count=3):
        for arm in ("A", "BT"):
            reviewers = []
            for n in range(count):
                sid = hashlib.sha256(f"{bug}/{arm}/{n}".encode()).hexdigest()[:32]
                reviewers.append(dict(child=sid, model="gpt-6.1-terra", effort="high",
                                      profile="codex4", slot=n))
                sd = f"runs/{bug}/{arm}/reviewers/{sid[:12]}"
                self.write(sd + "/output.md", "- [P1] fixture finding\n")
                self.save(sd + "/usage.json", {"totals": {"total_tokens": {"tokens": 100},
                                                           "output_tokens": {"tokens": 10}},
                                                "turn_cost_usd_ticks": 100,
                                                "coverage": {"recorded_turns": 1, "estimated_cost_turns": 1}})
            self.save(f"runs/{bug}/{arm}/done.json", dict(bug=bug, arm=arm, reviewers=reviewers))

    def score_build(self, name="relation", **kw):
        return self.run_script("brief_score.py", "build", "--arms", "BT", "--models", "gpt-6.1-terra",
                               "--bugs-from", self.root / "jobs.json", "--name", name, **kw)

    def test_terra_only_round_persists_selection_and_requires_exact_coverage(self):
        self.reviews()
        self.score_build()
        key = json.loads((self.root / "private/key-relation.json").read_text())
        self.assertEqual(key["models"], ["gpt-6.1-terra"])
        self.assertTrue(key["exact_baseline"])
        self.assertEqual(key["sets"], {"P": ["bug-one"]})
        self.assertEqual(len(key["outputs"]), 6)
        verdicts = [dict(output_id=oid, bug=k["bug"], judge=j, label="missed")
                    for oid, k in key["outputs"].items() for j in ("A", "B")]
        self.save("relation/workflow-out.json", dict(missing=[], verdicts=verdicts))
        # Persisted sets/subset/models, not the current manifest, govern later steps.
        self.save("sets.json", {"P": ["bug-two"], "S": [], "C": []})
        self.run_script("brief_score.py", "collect", "--name", "relation")
        self.save("relation/handcheck-decisions.json", {})
        self.run_script("brief_score.py", "score", "--name", "relation")
        report = json.loads((self.root / "relation/score.json").read_text())
        self.assertEqual(report["sets"], {"P": 1})
        self.assertEqual({r["model"] for r in report["rates"]}, {"gpt-6.1-terra"})
        self.assertTrue(all(r["median_latency_s"] is None and r["median_tool_calls"] is None
                            for r in report["per_arm"]))
        # Entire missing cells, excess baselines and duplicate session IDs cannot become zeros.
        path = self.root / "private/key-relation.json"
        for mode in ("missing", "extra-baseline", "duplicate-session"):
            changed = json.loads(json.dumps(key))
            if mode == "missing":
                changed["outputs"] = {o: k for o, k in changed["outputs"].items() if k["arm"] != "BT"}
            elif mode == "extra-baseline":
                row = next(k for k in changed["outputs"].values() if k["arm"] == "A")
                changed["outputs"]["extra"] = {**row, "session": "extra"}
            else:
                rows = list(changed["outputs"].values()); rows[1]["session"] = rows[0]["session"]
            path.write_text(json.dumps(changed))
            r = self.run_script("brief_score.py", "score", "--name", "relation", ok=False)
            self.assertIn("incomplete sample", r.stderr)

    def test_reject_unknown_empty_subset_and_invalid_set_manifest(self):
        self.reviews()
        for ids in ([], ["unknown"]):
            self.save("jobs.json", [{"bug": b, "arm": "BT"} for b in ids])
            self.assertIn("empty or unknown", self.score_build(ok=False).stderr)
        self.save("jobs.json", [{"bug": "bug-one", "arm": "BT"}])
        for sets in ({"P": ["bug-one", "bug-one"], "S": [], "C": []},
                     {"P": ["bug-one"], "S": ["bug-one"], "C": []},
                     {"P": ["ordinary-one"], "S": [], "C": []},
                     {"P": [], "S": [], "C": []}):
            self.save("sets.json", sets)
            self.assertIn("REVIEW_ARMS_SETS", self.score_build(ok=False).stderr)

    def test_leak_subset_names_hash_binding_and_single_rewrite_recheck(self):
        brief = "## Affected callers\n- caller.rs:1 calls callee.rs:2.\n## Paths\n- None\n## Value flow\n- None\n"
        for b in ("bug-one", "bug-two"):
            self.write(f"briefs/blind/{b}/brief.md", brief)
            self.save(f"briefs/blind/{b}/done.json", {})
        flags = ["--kind", "blind", "--bugs-from", self.root / "jobs.json", "--name", "relation"]
        self.run_script("brief_writer_terra.py", "leak-build", *flags)
        directory = "briefs/relation-leak-0"
        key = json.loads((self.root / directory / "key.json").read_text())
        self.assertEqual([k["bug"] for k in key.values()], ["bug-one"])
        oid = next(iter(key))
        verdicts = [dict(output_id=oid, bug="bug-one", judge=j, label="partial", rationale="fixture")
                    for j in ("A", "B")]
        self.save(directory + "/workflow-out.json", dict(missing=[], verdicts=verdicts))
        self.write("briefs/blind/bug-one/brief.md", brief + "\n")
        r = self.run_script("brief_writer_terra.py", "leak-collect", *flags, ok=False)
        self.assertIn("changed since leak", r.stderr)
        self.assertFalse((self.root / "briefs/blind/bug-one/blind-leak.json").exists())
        self.write("briefs/blind/bug-one/brief.md", brief)
        self.run_script("brief_writer_terra.py", "leak-collect", *flags)
        self.run_script("brief_writer_terra.py", "rewrite-build", *flags)
        rewritten = brief.replace("calls", "reaches")
        self.write("briefs/relation-rewrite/bug-one/brief.md", rewritten)
        self.run_script("brief_writer_terra.py", "rewrite-collect", *flags)
        self.assertEqual((self.root / "briefs/blind/bug-one/brief-writer.md").read_text(), brief)
        self.run_script("brief_writer_terra.py", "leak-build", "--round", "1", *flags)
        k1 = json.loads((self.root / "briefs/relation-leak-1/key.json").read_text())
        self.save("briefs/relation-leak-1/workflow-out.json", {"verdicts": [
            dict(output_id=o, bug=k["bug"], judge=j, label="partial", rationale="still fixture")
            for o, k in k1.items() for j in ("A", "B")]})
        r = self.run_script("brief_writer_terra.py", "leak-collect", "--round", "1", *flags)
        self.assertIn('"partial": 1', r.stdout)
        self.assertEqual(len(json.loads((self.root / "briefs/blind/bug-one/blind-leak.json").read_text())["rounds"]), 2)
        self.save("jobs.json", [{"bug": "bug-two", "arm": "BT"}])
        self.assertIn("scope differs", self.run_script("brief_writer_terra.py", "leak-collect", *flags, ok=False).stderr)
        flags[-1] = "nonrelation"
        self.run_script("brief_writer_terra.py", "leak-build", *flags)
        self.assertTrue((self.root / "briefs/nonrelation-leak-0/key.json").exists())

    def test_control_verification_uses_selected_corpus_and_missing_timing_is_blank(self):
        self.reviews("ordinary-one")
        self.save("ordinary-jobs.json", [{"bug": "ordinary-one", "arm": a} for a in ("A", "BT")])
        self.run_script("build_verify.py", "control", "--arms", "A,BT", "--bugs-from", self.root / "ordinary-jobs.json")
        key = json.loads((self.root / "private/key-control.json").read_text())
        self.save("control/out/verify-out-0.json", {"commits": [{"groups": [
            {"group": "g", "findings": list(key["findings"]), "verdict": "confirmed", "summary": "fixture"}]}]})
        r = self.run_script("score.py", "control")
        self.assertEqual(json.loads(r.stdout)["missing_latency"], 6)
        with open(self.root / "control/sessions.csv") as f:
            rows = list(csv.DictReader(f))
        self.assertEqual(len(rows), 6)
        self.assertTrue(all(r["latency_s"] == "" for r in rows))
        # A transcript without the session completion record still cannot provide latency.
        sid = rows[0]["session"]
        self.save(f"runs/ordinary-one/A/reviewers/{sid[:12]}/transcript.json", [{"items": []}])
        self.run_script("score.py", "control")


if __name__ == "__main__":
    unittest.main()
