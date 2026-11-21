"""Offline fidelity gate fixtures. No session tools are invoked."""
from collections import Counter
from contextlib import redirect_stdout
from io import StringIO
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest

from driver import expected_provider_model, verify_session_selection

from . import fidelity_live
from .fidelity import (BARS, CENSUS, Query, accuracy, bar_failures, gate_report, load_inputs,
                       measure, pooled_prescience, prescience_metrics, sample, step_result, stratum_metrics)
from .engine.grammar import MENU
from .fidelity_live import SONNET, TERRA, candidate_lines, model_usage, planned_calls
from .fidelity_symbols import capture_symbols, changed_intervals


GOOD = {"engine": ["d", "o"], "gold": ["d", "o"], "decisive": ["d"],
        "enumerated": ["d", "o"], "fresh": ["d", "o"]}
BAD = {"engine": ["bad"], "gold": ["wanted"], "decisive": ["wanted"],
       "enumerated": ["bad"], "fresh": []}


class Sampling(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.digest, cls.manifest, _, cls.documents, cls.eligible = load_inputs(
            CENSUS / "pilot/manifest.json")
        cls.served = list(MENU)

    def test_frozen_cohort_seed_and_strata(self):
        def symbols(diff):
            return [("src/lib.rs", "definition", "sampled_call_a"), ("src/lib.rs", "reference", "sampled_call_b")]
        first = sample(self.manifest, self.served, self.documents, self.eligible, symbols)
        reordered = {**self.manifest, "diffs": list(reversed(self.manifest["diffs"]))}
        self.assertEqual(first, sample(reordered, self.served, self.documents,
                                       list(reversed(self.eligible)), symbols))
        queries, fresh = first
        strata = Counter((q.step, q.stratum) for q in queries)
        self.assertEqual(strata, {("contract-cases", "random"): 10, ("contract-cases", "decisive"): 9,
                                  ("value-transfers", "random"): 10, ("value-transfers", "decisive"): 6,
                                  ("guarded-paths", "random"): 10, ("guarded-paths", "decisive"): 7,
                                  ("documented", "random"): 20})
        self.assertEqual(planned_calls(queries, fresh),
                         {"sonnet_engine": 92, "terra_enumerator": 72, "sonnet_adjudicator_at_most": 72})
        commits = {d["intro"] for d in self.manifest["diffs"]}
        self.assertTrue(all(q.commit in commits for q in queries if q.stratum == "random"))
        self.assertTrue(any(q.commit not in commits for q in queries if q.stratum == "decisive"))
        self.assertEqual(len({q.key for q in queries}), len(queries))
        self.assertEqual({(q.step, q.stratum) for q in queries if q.key in fresh}, set(strata))
        reseeded = {**self.manifest, "seed": self.manifest["seed"] + 1}
        changed, _ = sample(reseeded, self.served, self.documents, self.eligible, symbols)
        self.assertEqual([q for q in queries if q.census], [q for q in changed if q.census])
        self.assertNotEqual([q for q in queries if not q.census], [q for q in changed if not q.census])

    def test_reduced_menu_and_reported_only_documented_steps(self):
        served = ["value-transfers"]
        queries, fresh = sample(self.manifest, served, self.documents, self.eligible, lambda _: [("src/lib.rs", "definition", "s")])
        results = {q.key: BAD if q.step == "documented" else GOOD for q in queries}
        result = gate_report(self.digest, served, queries, results, self.manifest["seed"])
        self.assertEqual(result["manifest_sha256"], self.digest)
        self.assertEqual(result["served_steps"], served)
        self.assertTrue(result["steps"]["value-transfers"]["pass"])
        self.assertTrue(result["documented"]["reported_only"])
        self.assertEqual(result["documented"]["random_n"], 20)
        self.assertTrue(result["documented"]["reasons"])
        self.assertEqual(len(fresh), 20)

    def test_pooled_failure_keeps_the_independent_step_accuracy_verdicts(self):
        queries, _ = sample(self.manifest, self.served, self.documents, self.eligible,
                            lambda _: [("src/lib.rs", "definition", "s")])
        records = {q.key: GOOD for q in queries}
        passing = gate_report(self.digest, self.served, queries, records, self.manifest["seed"])
        self.assertEqual(passing["prescience_pooled"], {"gap": 0.0, "interval": [0.0, 0.0],
                                                      "decisive_n": 22, "pass": True})
        self.assertTrue(all(step["pass"] for step in passing["steps"].values()))
        decisive = [f"d{i}" for i in range(20)]
        other = [f"o{i}" for i in range(5)]
        preferred = {"engine": decisive + other[:4], "gold": decisive + other,
                     "enumerated": decisive + other, "decisive": decisive,
                     "fresh": decisive + other[:4]}
        for query in queries:
            if query.step == "contract-cases" and query.stratum == "decisive":
                records[query.key] = preferred
        failed = gate_report(self.digest, self.served, queries, records, self.manifest["seed"])
        pooled = failed["prescience_pooled"]
        self.assertEqual(pooled["gap"], 9 / 58)
        self.assertFalse(pooled["pass"])
        self.assertLess(pooled["interval"][0], pooled["gap"])
        self.assertGreater(pooled["interval"][1], pooled["gap"])
        for step in failed["steps"].values():
            self.assertTrue(step["pass"])
            self.assertEqual(step["reasons"], [])
            self.assertEqual(step["prescience"], "not_estimable")

    def test_symbol_ranges_include_body_edits_and_both_hunk_sides(self):
        patch = b"@@ -4,1 +4,2 @@ syntax\n-old_call();\n+new_call();\n+added();\n@@ -8 +9,0 @@ syntax\n-deleted();"
        before, after = changed_intervals(patch)
        self.assertEqual(before, [(3, 3), (7, 7)])
        self.assertEqual(after, [(3, 4)])
        captures = """
    capture: body, start: (1, 0), end: (6, 1)
    capture: 0 - definition, start: (1, 3), end: (1, 8), text: `owner`
    capture: 2 - reference, start: (2, 2), end: (2, 11), text: `untouched`
    capture: 2 - reference, start: (3, 2), end: (3, 10), text: `new_call`
"""
        self.assertEqual(capture_symbols(captures, after), {("definition", "owner"), ("reference", "new_call")})


class Arithmetic(unittest.TestCase):
    def test_row_weighted_metrics_bootstrap_and_stability(self):
        large = {"engine": ["a", "b", "c"], "gold": ["a", "b", "d", "e"], "fresh": ["a", "b", "x"]}
        exact = {"engine": ["a"], "gold": ["a"], "fresh": ["a"]}
        report = stratum_metrics([large, exact], "fixed")
        self.assertEqual(report["precision"], 3 / 4)
        self.assertEqual(report["recall"], 3 / 5)
        self.assertEqual(report["precision_lower95"], 2 / 3)
        self.assertEqual(report["recall_lower95"], 1 / 2)
        self.assertEqual(report["stability_jaccard"], 0.75)
        self.assertEqual(report["stability_n"], 2)
        self.assertEqual(accuracy([], 0)["recall"], None)

    def test_each_bar_is_inclusive_and_independently_enforced(self):
        self.assertFalse(bar_failures("random", BARS))
        for name, bound in BARS.items():
            with self.subTest(metric=name):
                failing = {**BARS, name: bound - 0.01}
                self.assertEqual(len(bar_failures("random", failing)), 1)
                self.assertIn(name, bar_failures("random", failing)[0])

    def test_decisive_bars_begin_at_five(self):
        random_rows = [GOOD] * 10
        self.assertTrue(step_result(random_rows, [BAD] * 4, 1)["pass"])
        failed = step_result(random_rows, [BAD] * 5, 1)
        self.assertFalse(failed["pass"])
        self.assertEqual(len(failed["reasons"]), 5)
        self.assertTrue(all(reason.startswith("decisive") for reason in failed["reasons"]))
        self.assertFalse(step_result(random_rows[:9], [], 1)["pass"])
        self.assertFalse(step_result([{**GOOD, "fresh": None}] * 10, [], 1)["pass"])

    def test_prescience_counts_shared_blind_spots_only_outside_gap(self):
        record = {"engine": ["d"], "gold": ["d", "o", "blind"], "decisive": ["d", "blind"],
                  "enumerated": ["d", "o"], "fresh": ["d"]}
        self.assertEqual(prescience_metrics([record] * 9)["status"], "not_estimable")
        result = prescience_metrics([record] * 10)
        self.assertEqual(result["gap"], 1)
        self.assertEqual(result["shared_blind_spots"], 10)
        self.assertEqual(accuracy([record], 0)["recall"], 1 / 3)
        self.assertEqual(prescience_metrics([{**record, "enumerated": ["d"]}] * 10)["gap"], None)

    def test_pooled_prescience_uses_the_signed_point_estimate(self):
        miss_other = {**GOOD, "engine": ["d"], "fresh": ["d"]}
        records = [GOOD] * 9 + [miss_other]
        tolerated = pooled_prescience(records, 2)
        self.assertEqual(tolerated["gap"], 0.1)
        self.assertEqual(tolerated["interval"], [0.0, 0.3])
        self.assertTrue(tolerated["pass"])
        miss_decisive = {**GOOD, "engine": ["o"], "fresh": ["o"]}
        self.assertTrue(pooled_prescience([miss_decisive] * 10, 2)["pass"])
        no_other = {**GOOD, "engine": ["d"], "enumerated": ["d"]}
        unavailable = pooled_prescience([no_other], 2)
        self.assertEqual((unavailable["gap"], unavailable["decisive_n"], unavailable["pass"]), (None, 1, False))
        self.assertIn("no comparable", unavailable["reason"])
        self.assertFalse(pooled_prescience([], 2)["pass"])
        unused = pooled_prescience([], 2, applicable=False)
        self.assertEqual((unused["gap"], unused["status"], unused["pass"]), (None, "inapplicable", True))

    def test_estimable_per_step_gap_is_descriptive(self):
        decisive = [f"d{i}" for i in range(10)]
        gold = decisive + ["o"]
        row = {"engine": decisive, "decisive": decisive, "gold": gold,
               "enumerated": gold, "fresh": decisive}
        report = step_result([GOOD] * 10, [row] * 10, 2)
        self.assertEqual(report["prescience"], "estimated")
        self.assertEqual(report["metrics"]["prescience"]["gap"], 1.0)
        self.assertTrue(report["pass"])


class FakeModels:
    def __init__(self):
        self.calls = []

    def engine(self, commit, text, *, fresh):
        self.calls.append(("engine", commit, text, fresh))
        rows = [{"name": "e"}, {"name": "new"}] if fresh else [{"name": "e"}, {"name": "invalid"}]
        return {"status": "ok", "rows": rows}

    def enumerate_rows(self, repo, commit, text):
        self.calls.append(("terra", repo, commit, text))
        return {"rows": [{"name": "equivalent"}, {"name": "other"}]}

    def adjudicate(self, query, candidates):
        self.calls.append(("sonnet", query.key))
        return {k: {"key": "e" if row["name"] == "equivalent" else row["name"],
                    "in_scope": row["name"] != "invalid", "reason": "fixed evidence"}
                for k, row in candidates.items()}


class Pool(unittest.TestCase):
    def test_fakes_pool_rows_and_keep_fresh_only_out_of_gold(self):
        query = Query("contract-cases", "decisive", "repo", "commit", "(q)", ({"name": "blind"},))
        fake = FakeModels()
        result = measure(query, True, fake.engine, fake.enumerate_rows, fake.adjudicate)
        self.assertEqual(result["engine"], ["e", "invalid"])
        self.assertEqual(result["enumerated"], ["e", "invalid", "other"])
        self.assertEqual(result["gold"], ["blind", "e", "other"])
        self.assertEqual(result["decisive"], ["blind"])
        self.assertEqual(result["fresh"], ["e", "new"])
        self.assertEqual(fake.calls, [("engine", "commit", "(q)", False), ("terra", "repo", "commit", "(q)"),
                                      ("engine", "commit", "(q)", True), ("sonnet", query.key)])

    def test_missing_judgment_and_failed_answer_abort_the_measurement(self):
        query = Query("contract-cases", "random", "repo", "commit", "(q)")
        fake = FakeModels()
        with self.assertRaisesRegex(ValueError, "every candidate"):
            measure(query, False, fake.engine, fake.enumerate_rows, lambda q, c: {})
        with self.assertRaisesRegex(RuntimeError, "engine_error"):
            measure(query, False, lambda *a, **k: {"status": "engine_error"},
                    fake.enumerate_rows, fake.adjudicate)

    def test_candidate_source_slices_use_exact_inclusive_lines(self):
        candidate = {"a": {"sites": [{"path": "src.rs", "start_line": 2, "end_line": 3}]}}
        self.assertEqual(candidate_lines(candidate, lambda path: b"first\nsecond\nthird\n"),
                         {"a": {"src.rs:2-3": ["second", "third"]}})
        with self.assertRaisesRegex(ValueError, "out-of-range"):
            candidate_lines(candidate, lambda path: b"first\nsecond\n")


class EnginePhase(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.manifest_path = CENSUS / "pilot/manifest.json"
        digest, manifest, served, documents, _ = load_inputs(self.manifest_path)
        step = served[0]
        commit = "a" * 40
        self.rows = [{"name": name, "sites": []} for name in ("decisive", "other")]
        self.queries = [
            Query(step, "random", "mjolnir", commit, f'(function :name "fixture_{i}")')
            for i in range(10)
        ] + [
            Query(step, "decisive", "mjolnir", commit, '(function :name "fixture_decisive")', (self.rows[0],)),
            Query("documented", "random", "mjolnir", commit, '(function :name "fixture_documented")'),
        ]
        self.fresh = {self.queries[1].key, self.queries[-1].key}
        self.expected = [(query.text, fresh) for query in self.queries
                         for fresh in ([False, True] if query.key in self.fresh else [False])]
        self.calls = []
        self.gold_calls = []
        self.hold_at = None
        self.identity = {"manifest_sha256": digest, "engine_inputs_sha256": "b" * 64}
        engine = SimpleNamespace(
            identity=self.identity, model_version="claude-sonnet-5-5", answer=self.answer,
            config={"repos": dict.fromkeys((row["repo"] for row in manifest["diffs"]), self.root),
                    "snapshot_repo": self.root},
            manifest={**manifest, "eligible_diffs": [{"intro": commit, "repo": "mjolnir"}]},
        )
        # The real phase selection, checkpoints, measurement and reporting run offline.
        for name, fake in {
            "load_inputs": lambda path: (digest, None, [step], documents, None),
            "load_engine": lambda *args, **kwargs: engine,
            "touched_symbols": lambda *args: [],
            "sample": lambda *args: (self.queries, self.fresh),
            "prepared_snapshot": lambda *args: None,
            "LiveModels": self.gold_models,
        }.items():
            self.addCleanup(setattr, fidelity_live, name, getattr(fidelity_live, name))
            setattr(fidelity_live, name, fake)
        self.grammar = self.root / "grammar"
        (self.grammar / "src").mkdir(parents=True)
        (self.grammar / "src/parser.c").write_text("fixture")

    def answer(self, commit, text, *, fresh=False):
        call = (text, fresh)
        self.calls.append(call)
        if call == self.hold_at:
            return {"status": "quota_hold", "message": "fixture hold"}
        return {"status": "ok", "rows": self.rows}

    def gold_models(self, *args):
        self.gold_calls.append("started")

        def enumerate_rows(repo, commit, text):
            self.gold_calls.append(("terra", text))
            return {"rows": self.rows}

        def adjudicate(query, candidates):
            self.gold_calls.append(("sonnet", query.text))
            return {key: {"in_scope": True, "key": row["name"], "reason": "fixture evidence"}
                    for key, row in candidates.items()}

        return SimpleNamespace(enumerate_rows=enumerate_rows, adjudicate=adjudicate)

    def run_phase(self, root, *options):
        output = StringIO()
        with redirect_stdout(output):
            status = fidelity_live.main(["--live", "--run-root", str(root), "--manifest", str(self.manifest_path),
                                         "--rust-grammar", str(self.grammar), "--js-grammar", str(self.grammar),
                                         *options])
        return status, [json.loads(line) for line in output.getvalue().splitlines()]

    def test_engine_phase_and_full_run_match_a_single_full_run(self):
        split = self.root / "split"
        self.assertEqual(self.run_phase(split, "--phase", "engine")[0], 0)
        gate = split / "fidelity/gate-report.json"
        self.assertFalse(gate.exists())
        self.assertFalse(list(split.rglob("measurement.json")))
        self.assertEqual(self.calls, self.expected)
        self.assertEqual(self.gold_calls, [])
        self.assertEqual(self.run_phase(split, "--terra-profile", "fixture")[0], 0)
        self.assertEqual(self.calls, self.expected)
        split_gold = self.gold_calls.copy()
        self.gold_calls.clear()
        self.calls.clear()
        combined = self.root / "combined"
        self.assertEqual(self.run_phase(combined, "--terra-profile", "fixture")[0], 0)
        self.assertEqual(self.calls, self.expected)
        self.assertEqual(self.gold_calls, split_gold)
        reports = [json.loads(path.read_text()) for path in (gate, combined / "fidelity/gate-report.json")]
        for report in reports:
            report.pop("created_at")
        self.assertEqual(*reports)

    def test_quota_hold_resumes_the_unanswered_stability_reask(self):
        held = self.expected.index((self.queries[1].text, True))
        self.hold_at = self.expected[held]
        status, output = self.run_phase(self.root, "--phase", "engine")
        self.assertEqual(status, 1)
        self.assertEqual(output[-1]["status"], "quota_hold")
        self.assertEqual(self.calls, self.expected[:held + 1])
        self.calls.clear()
        self.hold_at = None
        self.assertEqual(self.run_phase(self.root, "--phase", "engine")[0], 0)
        self.assertEqual(self.calls, self.expected[held:])
        self.assertEqual(self.gold_calls, [])
        self.assertFalse((self.root / "fidelity/gate-report.json").exists())


class ModelIdentity(unittest.TestCase):
    def test_sonnet_alias_and_provider_usage_must_both_match(self):
        manifest = {"models": {"engine": "sonnet/high", "judge": "sonnet/high"},
                    "model_versions": {"claude": "Sonnet 5.5", "claude_provider_id": "claude-sonnet-5-5"}}
        self.assertEqual(expected_provider_model(manifest, "engine"), "claude-sonnet-5-5")
        with self.assertRaisesRegex(ValueError, "versions disagree"):
            expected_provider_model({**manifest, "model_versions": {
                "claude": "Sonnet 5.5", "claude_provider_id": "claude-sonnet-5-4"}}, "engine")
        session = {"profile_id": "claude", "config_options": [
            {"key": "model", "current": "sonnet", "choices": [{"value": "sonnet", "name": "Claude Sonnet 5.5"}]},
            {"key": "effort", "current": "high"}]}
        self.assertEqual(verify_session_selection(session, SONNET, "claude", "Sonnet 5.5"), "Sonnet 5.5")
        with self.assertRaisesRegex(ValueError, "5.5.*5.6"):
            verify_session_selection(session, SONNET, "claude", "Sonnet 5.6")
        usage = {"turns": [{"command_id": "call", "accepted_ordinal": 1, "usage": {
            "provider_details": {"model_usage": {"claude-sonnet-5-4": {}}}}}],
            "turn_selections": {"call": {"model": SONNET, "effort": "high"}}}
        with self.assertRaisesRegex(ValueError, "claude-sonnet-5-5"):
            model_usage(usage, "call", 1, SONNET, "claude-sonnet-5-5")
        with self.assertRaisesRegex(ValueError, "Terra/high"):
            model_usage(usage, "call", 1, TERRA, "claude-sonnet-5-5")
