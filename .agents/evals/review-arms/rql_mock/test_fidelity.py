"""Offline fidelity gate fixtures. No session tools are invoked."""
from collections import Counter
from contextlib import ExitStack, redirect_stdout
from io import StringIO
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import driver
from driver import expected_provider_model, verify_session_selection
from rql_engine_tools import STAGED_ENGINE_SETTINGS

from . import fidelity_live
from .fidelity import (BARS, CENSUS, Query, accuracy, bar_failures, gate_report, load_inputs,
                       measure, pooled_prescience, prescience_metrics, sample, step_result, stratum_metrics)
from .engine.grammar import MENU
from .engine.runner import EngineRunner, SealedRunner
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

    # Hard-won: 770af03f3: a failed first answer with an empty re-ask scored as perfectly stable
    def test_a_failed_first_answer_scores_zero_stability_even_against_an_empty_reask(self):
        stall = {"status": "engine_error", "failure": "repair_stalled", "rows": []}
        empty = {"status": "ok", "rows": []}
        records = [{"engine": [], "gold": [], "fresh": [], "engine_answer": stall, "fresh_answer": empty},
                   {"engine": [], "gold": [], "fresh": [], "engine_answer": empty, "fresh_answer": empty}]
        report = stratum_metrics(records, "fixed")
        self.assertEqual(report["stability_n"], 2)
        self.assertEqual(report["stability_jaccard"], 0.5)

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
        self.failures = {}
        self.stop_at = None
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
        if self.stop_at is not None:
            stop_on, request = self.stop_at
            if call == stop_on:
                request.touch()
        if call in self.failures:
            return self.failures[call]
        login = "claude4" if fresh else "claude5"
        return {"status": "ok", "rows": self.rows,
                "provenance": {"login_profile": login, "engine_profile": f"{login}-engine"}}

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
        self.failures[self.expected[held]] = {"status": "quota_hold", "message": "fixture hold"}
        status, output = self.run_phase(self.root, "--phase", "engine")
        self.assertEqual(status, 1)
        self.assertEqual(output[-1]["status"], "quota_hold")
        self.assertEqual(self.calls, self.expected[:held + 1])
        self.calls.clear()
        self.failures.clear()
        self.assertEqual(self.run_phase(self.root, "--phase", "engine")[0], 0)
        self.assertEqual(self.calls, self.expected[held:])
        directory = next(self.root.rglob(f"{self.queries[1].key}/fresh-engine.json")).parent
        for name, login in (("engine.json", "claude5"), ("fresh-engine.json", "claude4")):
            provenance = json.loads((directory / name).read_bytes())["provenance"]
            self.assertEqual(provenance, {"login_profile": login, "engine_profile": f"{login}-engine"})
        self.assertEqual(self.gold_calls, [])
        self.assertFalse((self.root / "fidelity/gate-report.json").exists())

    def test_double_stalls_resume_from_zero_row_receipts_and_count_against_the_gate(self):
        reasked, decisive, documented = self.queries[1], self.queries[-2], self.queries[-1]
        failure = {"status": "engine_error", "failure": "repair_stalled", "message": "invalid rows",
                   "rows": [], "repair_error_counts": [{"session": "first", "previous": 2, "current": 3},
                                                       {"session": "second", "previous": 2, "current": 2}]}
        self.failures.update({(reasked.text, True): failure, (decisive.text, False): failure,
                              (documented.text, False): failure})
        # Stop after checkpointing the decisive failure. Resume through the documented query.
        request = self.root / "fidelity/stop-requested"
        self.stop_at = ((decisive.text, False), request)
        self.assertEqual(self.run_phase(self.root, "--phase", "engine")[0], 3)
        self.assertEqual(self.calls, self.expected[:-2])
        for query, receipt in ((reasked, "fresh-engine.json"), (decisive, "engine.json")):
            path = next(self.root.rglob(f"{query.key}/{receipt}"))
            self.assertEqual(json.loads(path.read_bytes()), failure)
        request.unlink()
        self.calls.clear()
        self.assertEqual(self.run_phase(self.root, "--phase", "engine")[0], 0)
        self.assertEqual(self.calls, self.expected[-2:])

        self.assertEqual(self.run_phase(self.root, "--terra-profile", "fixture")[0], 1)
        self.assertEqual(self.calls, self.expected[-2:])
        report = json.loads((self.root / "fidelity/gate-report.json").read_bytes())
        step = report["steps"][decisive.step]
        self.assertEqual(report["engine_failure_count"], 3)
        self.assertEqual(step["engine_failure_count"], 2)
        self.assertEqual(report["documented"]["engine_failure_count"], 1)
        self.assertEqual(report["documented"]["by_step"]["function"]["engine_failure_count"], 1)
        self.assertEqual(report["engine_failures"], [
            {"query_key": query.key, "step": step_name, "stratum": query.stratum, "fresh": fresh,
             "reason": "repair_stalled", "repair_error_counts": failure["repair_error_counts"]}
            for query, step_name, fresh in ((reasked, reasked.step, True), (decisive, decisive.step, False),
                                            (documented, "function", False))
        ])
        self.assertEqual(step["metrics"]["random"]["stability_n"], 1)
        self.assertEqual(step["metrics"]["random"]["stability_jaccard"], 0.0)
        accuracy = step["metrics"]["decisive"]
        self.assertEqual((accuracy["gold"], accuracy["returned"], accuracy["recall"]), (2, 0, 0.0))
        self.assertEqual(report["prescience_pooled"], {"gap": 0.0, "interval": [0.0, 0.0],
                                                     "decisive_n": 1, "pass": True})

    def test_other_engine_errors_stop_without_a_receipt(self):
        self.failures[(self.queries[0].text, False)] = {"status": "engine_error"}
        for failure in ("engine session failed transcript isolation audit", "verifier digest changed"):
            with self.subTest(failure=failure):
                self.failures[(self.queries[0].text, False)]["message"] = failure
                self.calls.clear()
                with self.assertRaisesRegex(RuntimeError, f"engine engine_error: {failure}"):
                    self.run_phase(self.root, "--phase", "engine")
                self.assertEqual(self.calls, self.expected[:1])
                self.assertFalse(list(self.root.rglob("engine.json")))

    def test_stop_request_finishes_the_current_query_before_stopping(self):
        request = self.root / "fidelity/stop-requested"
        self.stop_at = ((self.queries[1].text, False), request)
        status, output = self.run_phase(self.root, "--terra-profile", "fixture")
        self.assertEqual(status, 3)
        self.assertEqual(output[-1], {"status": "stop_requested"})
        self.assertEqual(self.calls, self.expected[:3])
        self.assertTrue(request.is_file())
        self.assertEqual(len(list(self.root.rglob("measurement.json"))), 2)
        self.assertFalse((request.parent / "gate-report.json").exists())

        self.calls.clear()
        self.gold_calls.clear()
        self.assertEqual(self.run_phase(self.root, "--terra-profile", "fixture")[0], 3)
        self.assertEqual(self.calls, [])
        self.assertEqual(self.gold_calls, ["started"])
        request.unlink()
        self.stop_at = None
        self.assertEqual(self.run_phase(self.root, "--terra-profile", "fixture")[0], 0)
        self.assertEqual(self.calls, self.expected[3:])


class TerraOutput(unittest.TestCase):
    def test_enumerator_translates_evidence_before_delivery(self):
        model = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
        model.manifest = {"diffs": [], "served_steps": []}
        model.catalogue = model.menu = ""
        model.directory = Path("fixture")
        model.terra_profile = "terra"
        site = {"path": "source.rs", "start_line": 1, "end_line": 1}
        written = {"site": site, "verbatim": [{"text": "α"}]}
        answer = {"rows": [{"kind": "structural_match", "primary_site": site,
                            "fields": {"operand": written}}],
                  "coverage": {"complete": True, "open": []}, "excluded": []}

        def sealed_task(commit, directory, name, profile, files, prompt, check_output):
            self.assertIn("never run `cd`", prompt("/fixture/.scratch"))
            self.assertNotIn("Use Write", prompt("/fixture/.scratch"))
            return check_output(json.dumps(answer), {"source.rs": "α\tα\n".encode()}.get), {"model": name}

        model.sealed_task = sealed_task
        with self.assertRaisesRegex(ValueError, "invalid Terra evidence: .*specify occurrence"):
            model.enumerate_rows("fixture", "a" * 40, '(function :name "f")')
        written["verbatim"][0]["occurrence"] = 2
        output = model.enumerate_rows("fixture", "a" * 40, '(function :name "f")')
        self.assertEqual(output["rows"], [{"kind": "structural_match", "sites": [site], "fields": {
            "operand": {"site": site, "verbatim": [{"start_byte": 3, "end_byte": 5, "text": "α"}]},
        }}])

    # Hard-won: d4028a66b: the live gate stopped on Terra's first repairable schema error.
    def test_enumerator_repairs_in_session_and_restarts_a_stall_once(self):
        query = Query("documented", "random", "fixture", "b" * 40, '(function :name "f")')
        site = {"path": "source.rs", "start_line": 1, "end_line": 1}
        operand = {"site": site, "verbatim": [{"text": "source"}]}

        def answer(written):
            return json.dumps({
                "rows": [{"kind": "structural_match", "primary_site": site, "fields": {"operand": written}}],
                "coverage": {"complete": True, "open": []}, "excluded": [],
            }).encode()

        good, bad = answer(operand), answer({**operand, "extra": True})
        # Normalizes cleanly but fails answer validation with three errors.
        staged = json.dumps({
            "rows": [{"kind": "structural_match", "primary_site": site, "fields": {"operand": operand}}],
            "coverage": {"complete": True, "open": [{"reason": 3}, {"reason": 4}]}, "excluded": [],
        }).encode()
        errors_named = {bad: "written evidence must contain only site and verbatim",
                        staged: "coverage.complete must be true exactly when coverage.open is empty"}
        orphan = "c" * 40
        model_usage = fidelity_live.model_usage
        sessions, checks, destroyed = {}, [], []

        def launch(*args, **kwargs):
            sid = str(len(sessions) + 1) * 32
            sessions[sid] = []
            return sid

        def session(sid):
            return {"id": sid, "at": orphan, "base": orphan, "no_later_history": True,
                    "harness_kind": "codex", "profile_id": "terra", "config_options": []}

        def attest(runner, sid, snapshot, directory):
            driver.save(directory / "pre-inventory.json", {"cwd": runner.checkout_root(sid)})
            return session(sid)

        def transport(runner, operation, *args, data=None):
            sid = args[1]
            turns = sessions[sid]
            if operation == "message":
                turns.append((args[3], data.decode()))
                return json.dumps({"session_id": sid, "via": "turn", "turn_id": len(turns)}).encode()
            if operation == "put-file":
                return b""
            if operation == "fetch":
                return schedule[int(sid[0]) - 1][len(turns) - 1]
            self.assertEqual(operation, "destroy")
            destroyed.append(sid)
            return json.dumps({"session_id": sid, "accepted": True}).encode()

        def mj(operation, *args, **kwargs):
            sid = args[1]
            if operation == "wait":
                return {"turn_id": len(sessions[sid]), "outcome": "finished"}
            self.assertEqual(operation, "usage")
            cursor = args[3] if "--after-seq" in args else 0
            turns = [{"command_id": command, "accepted_ordinal": n} for n, (command, _) in
                     enumerate(sessions[sid], 1) if n > cursor]
            count = len(sessions[sid])
            return {"session_id": sid, "latest_seq": count, "next_after_seq": count, "turns": turns,
                    "turn_selections": {turn["command_id"]: {"model": TERRA, "effort": "high"} for turn in turns}}

        def transcript(sid):
            count = len(sessions[sid])
            return [{"session_id": sid, "next_after_seq": count, "latest_seq": count, "items": [
                {"role": "tool", "seq": n, "body": {"call": {"name": "exec_command",
                 "kind": "read", "title": "List files", "toolCallId": f"exec-{n}"}}}
                for n in range(1, count + 1)]}]

        def capture(directory, sid, *, reuse_existing, directory_name):
            self.assertFalse(reuse_existing)
            destination = directory / directory_name
            destination.mkdir()
            rows = [{"type": "event_msg", "payload": {"type": "item_completed", "item": {
                "type": "CommandExecution", "id": f"exec-{n}", "command": ["ls"],
                "cwd": f"/workspace/{sid}/engine-trees",
            }}} for n in range(1, len(sessions[sid]) + 1)]
            (destination / "rollout-fixture.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
            return True

        def check(usage, command, turn, *args):
            model = model_usage(usage, command, turn, *args)
            checks.append((usage["session_id"], turn))
            return model

        with ExitStack() as stack:
            for target, name, fake in (
                (driver, "use_named_instance", lambda _: Path("fixture")),
                (driver, "pilot_weekly_hold", lambda *args, **kwargs: None),
                (driver, "wait_idle", session),
                (driver, "session", session),
                (driver, "verify_session_selection", lambda *args: TERRA),
                (driver, "capture_pilot_rollouts", capture),
                (driver, "transcript", transcript),
                (driver, "mj", mj),
                (fidelity_live, "blob_reader", lambda *args: lambda _: b"source\n"),
                (fidelity_live, "prepared_snapshot", lambda *args: {"orphan": orphan, "tree": "tree"}),
                (fidelity_live, "model_usage", check),
                (fidelity_live.brief_driver, "sealed_launch", launch),
                (fidelity_live.brief_driver, "configure_pilot_model_ready", lambda *args: None),
                (SealedRunner, "attest", attest),
                (SealedRunner, "clean_checkout", lambda runner, sid, *args: f".rql-engine-{sid}"),
                (SealedRunner, "command", transport),
            ):
                stack.enter_context(patch.object(target, name, new=fake))
            # Hard-won: 2da79e863: fixing normalization exposed more validation errors and counted as a stall
            for schedule in (((bad, good),), ((bad, staged, good),), ((bad, bad), (good,)), ((bad, bad), (bad, bad))):
                with self.subTest(rounds=[len(turns) for turns in schedule]), TemporaryDirectory() as root:
                    sessions.clear()
                    checks.clear()
                    destroyed.clear()
                    directory = Path(root)
                    adapter = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
                    adapter.config = {"instance": "terra-test", "snapshot_repo": "snapshot", "bundle": "fixture",
                                      "destination": "engine-trees"}
                    adapter.manifest = {"diffs": [], "eligible_diffs": [], "served_steps": [],
                                        "model_versions": {"claude": "Sonnet 5.5"}}
                    adapter.sources = {query.commit: "fixture"}
                    adapter.catalogue = adapter.menu = adapter.claude_version = ""
                    adapter.directory, adapter.terra_profile = directory, "terra"

                    if schedule[-1][-1] == bad:
                        engine = SimpleNamespace(answer=lambda *args, **kwargs: {"status": "ok", "rows": []})
                        with self.assertRaisesRegex(RuntimeError, "Terra repair stalled in both sessions"):
                            fidelity_live.run_sample(directory, [query], set(), engine, adapter,
                                                     stop_request=directory / "stop")
                        self.assertFalse((directory / query.key / "measurement.json").exists())
                    else:
                        result = adapter.enumerate_rows(query.repo, query.commit, query.text)
                        self.assertEqual(len(result["rows"]), 1)
                    self.assertEqual([len(turns) for turns in sessions.values()], [len(turns) for turns in schedule])
                    self.assertEqual(destroyed, list(sessions))
                    self.assertEqual(checks, [(sid, n) for sid, turns in sessions.items()
                                              for n in range(1, len(turns) + 1)])
                    stalls = directory / query.key / "terra-stalled"
                    self.assertEqual(stalls.is_dir(), len(schedule) == 2)
                    for index, turns in enumerate(sessions.values()):
                        evidence = stalls if index == 0 and stalls.is_dir() else directory / query.key / "terra"
                        self.assertTrue((evidence / "destroy-receipt.json").is_file())
                        for n, (_, prompt) in enumerate(turns, 1):
                            audit = json.loads((evidence / f"turn-{n}-audit.json").read_bytes())
                            self.assertTrue(audit["passed"])
                            self.assertTrue((evidence / audit["rollout_directory"]).is_dir())
                            self.assertEqual("failed mechanical verification" in prompt, n > 1)
                            if n > 1:
                                self.assertIn(errors_named[schedule[index][n - 2]], prompt)


class ModelIdentity(unittest.TestCase):
    # Hard-won: d7e5de2d6: the live adjudicator could run an unauditable scratch script.
    def test_adjudicator_uses_the_restricted_login_and_engine_audit(self):
        sid, orphan = "a" * 32, "b" * 40
        profile, provider = "restricted-alias", "claude-sonnet-5-5"
        checkout = f"/workspace/{sid}/engine-trees"
        scratch = f".rql-engine-{sid}"
        configured = {"id": sid, "at": orphan, "base": orphan, "no_later_history": True,
                      "harness_kind": "claude", "profile_id": profile, "config_options": [
                          {"key": "model", "current": SONNET, "choices": [
                              {"value": SONNET, "name": "Claude Sonnet 5.5"}]},
                          {"key": "effort", "current": "high"},
                      ]}
        query = Query("documented", "random", "fixture", "c" * 40, "(function)")
        candidates = {"0": {"sites": [{"path": "source.rs", "start_line": 1, "end_line": 1}]}}
        decisions = {"0": {"in_scope": True, "key": "definition", "reason": "the source declares it"}}
        calls = []
        command = None

        def quota(login, floor, **kwargs):
            calls.append(("quota", login, floor, kwargs["short_floor"]))
            return ((50, 50), None) if login == "claude5" else (None, "quota held")

        def sync(runner, session=None):
            calls.append(("sync", runner.login.login_profile, runner.login.engine_profile, session))

        def launch(bundle, at, alias, title, **kwargs):
            calls.append(("launch", alias))
            return sid

        def attest(runner, session, snapshot, directory):
            driver.save(directory / "pre-inventory.json", {"cwd": checkout})
            return configured

        def clean(runner, session, pin, *, clear=True):
            self.assertEqual((session, pin), (sid, orphan))
            return scratch

        def podman(argv, **kwargs):
            self.assertEqual(argv[:3], ["podman", "exec", "fixture-container"])
            if argv[3] == "cat":
                return SimpleNamespace(stdout=json.dumps(STAGED_ENGINE_SETTINGS).encode())
            self.assertEqual(argv[3:5], ["python3", "-c"])
            return SimpleNamespace(stdout="2.1.326\n")

        def transport(runner, operation, *args, data=None):
            nonlocal command
            self.assertEqual(args[:2], ("--session", sid))
            if operation == "message":
                command = args[args.index("--command-id") + 1]
                self.assertIn(("sync", "claude5", profile, sid), calls)
                self.assertIn(b"only Read, Grep, Glob, Write and Edit, with no shell", data)
                self.assertIn(f"Use Write to save the JSON object to {checkout}/{scratch}/output.json".encode(), data)
                return json.dumps({"session_id": sid, "via": "turn", "turn_id": 1}).encode()
            if operation == "put-file":
                self.assertTrue(args[3].startswith(scratch + "/"))
                return b""
            if operation == "fetch":
                self.assertEqual(args[3], f"{scratch}/output.json")
                return json.dumps(decisions).encode()
            self.assertEqual(operation, "destroy")
            return json.dumps({"session_id": sid, "accepted": True}).encode()

        def mj(operation, *args, **kwargs):
            if operation == "set-config":
                return {}
            if operation == "wait":
                return {"turn_id": 1, "outcome": "finished"}
            self.assertEqual(operation, "usage")
            if "--after-seq" not in args:
                return {"latest_seq": 0}
            return {"session_id": sid, "latest_seq": 5, "next_after_seq": 5,
                    "turns": [{"command_id": command, "accepted_ordinal": 1, "usage": {
                        "provider_details": {"model_usage": {provider: {}}}}}],
                    "turn_selections": {command: {"model": SONNET, "effort": "high"}}}

        with TemporaryDirectory() as root, ExitStack() as stack:
            directory = Path(root)
            adapter = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
            adapter.config = {"instance": "fidelity-test", "bundle": "fixture", "snapshot_repo": "snapshot",
                              "destination": "engine-trees", "logins": [
                                  {"profile": "held", "engine_profile": "held-alias"},
                                  {"profile": "claude5", "engine_profile": profile},
                              ]}
            adapter.manifest = {"eligible_diffs": [], "served_steps": [], "model_versions": {"claude": "Sonnet 5.5"}}
            adapter.sources, adapter.claude_version = {query.commit: "fixture"}, provider
            adapter.catalogue = adapter.menu = adapter.rulings = ""
            for target, name, fake in (
                (driver, "use_named_instance", lambda _: directory),
                (driver, "_quota_status", quota),
                (driver, "wait_idle", lambda _: configured),
                (driver, "session", lambda _: configured),
                (driver, "container", lambda _: {"container": "fixture-container"}),
                (driver, "capture_pilot_rollouts", lambda *args, **kwargs: self.fail("Claude exported a rollout")),
                (driver, "mj", mj),
                (driver.subprocess, "run", podman),
                (fidelity_live, "blob_reader", lambda *args: lambda _: b"def source(): pass\n"),
                (fidelity_live, "prepared_snapshot", lambda *args: {"orphan": orphan, "tree": "tree"}),
                (fidelity_live.brief_driver, "sealed_launch", launch),
                (EngineRunner, "attest", attest),
                (EngineRunner, "clean_checkout", clean),
                (EngineRunner, "credential_hold", lambda *args: None),
                (EngineRunner, "sync_login", sync),
                (EngineRunner, "command", transport),
            ):
                stack.enter_context(patch.object(target, name, new=fake))
            for tool in ({"name": "Write", "kind": "edit", "rawInput": {
                              "file_path": f"{checkout}/{scratch}/output.json", "content": json.dumps(decisions)}},
                         {"name": "Bash", "kind": "execute", "rawInput": {"command": "pwd"}}):
                with self.subTest(tool=tool["name"]):
                    adapter.directory = directory / tool["name"]
                    calls.clear()
                    pages = [{"session_id": sid, "next_after_seq": 5, "latest_seq": 5,
                              "items": [{"role": "tool", "seq": 5, "body": {"call": tool}}]}]
                    with patch.object(driver, "transcript", new=lambda _: pages):
                        if tool["name"] == "Bash":
                            with self.assertRaisesRegex(RuntimeError, "failed the pilot audit"):
                                adapter.adjudicate(query, candidates)
                        else:
                            self.assertEqual(adapter.adjudicate(query, candidates), decisions)
                    artifacts = adapter.directory / query.key / "adjudicator"
                    self.assertEqual(calls, [("quota", "held", 25, 20), ("quota", "claude5", 0, 0),
                                             ("sync", "claude5", profile, None), ("launch", profile),
                                             ("sync", "claude5", profile, sid), ("quota", "claude5", 0, 0)])
                    self.assertEqual(json.loads((artifacts / "engine-runtime.json").read_bytes()),
                                     {"session_id": sid, "claude_code_version": "2.1.326"})
                    self.assertTrue((artifacts / "engine-tool-settings.json").is_file())
                    self.assertTrue((artifacts / "destroy-receipt.json").is_file())
                    audit = json.loads((artifacts / "turn-1-audit.json").read_bytes())
                    self.assertEqual(audit["passed"], tool["name"] == "Write")
                    if not audit["passed"]:
                        self.assertIn("tool outside the engine allowlist: 'Bash'", audit["hits"][0]["snippet"])

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
        with self.assertRaisesRegex(ValueError, f"{TERRA}/high"):
            model_usage(usage, "call", 1, TERRA, "claude-sonnet-5-5")

    # Hard-won: 0674966cf: mailbox context made a valid Codex prompt fail the turn join.
    def test_codex_null_usage_effort_requires_the_exact_native_turn_context(self):
        sid, command, native_id = "a" * 32, "fidelity-command", "codex-turn"
        content = [{"type": "text", "text": "The same prompt may be retried."}]
        turn = {"command_id": command, "accepted_ordinal": 32, "turn_start_position": 33,
                "completed_at_ms": 200}
        usage = {"session_id": sid, "turns": [turn],
                 "turn_selections": {command: {"model": TERRA, "effort": None}}}
        pages = [{"session_id": sid, "items": [{"stable_id": f"user:{command}", "role": "user",
                  "position": 33, "created_at_ms": 100, "body": {"content": content}}]}]
        meta = {"type": "session_meta", "payload": {"id": "thread", "cwd": f"/workspace/{sid}/engine-trees"}}
        prompt_context = {"type": "text", "text_elements": [],
                          "text": 'mj://prompt-context\n<context ref="mj://prompt-context">\n'
                                  "<untrusted-mailbox-events>\nProject tools are being installed.\n"
                                  "</untrusted-mailbox-events>\n</context>"}
        message = {"type": "event_msg", "payload": {
            "type": "item_completed", "thread_id": "thread", "turn_id": native_id, "completed_at_ms": 105,
            "item": {"type": "UserMessage", "id": "item", "content": [prompt_context, *content]}}}
        context = {"type": "turn_context", "payload": {"turn_id": native_id, "model": TERRA, "effort": "high"}}

        def with_payload(row, **fields):
            return {**row, "payload": {**row["payload"], **fields}}

        def with_content(parts):
            return with_payload(message, item={**message["payload"]["item"], "content": parts})

        earlier_message = with_payload(message, turn_id="earlier", completed_at_ms=98)
        earlier_context = with_payload(context, turn_id="earlier")
        other_message = with_payload(message, turn_id="earlier", completed_at_ms=160)
        unknown = {**prompt_context, "text": prompt_context["text"] + "\ntrailing text"}
        different_prompt = {**content[0], "text": content[0]["text"][:-1] + "!"}
        cases = (
            ("matching", [earlier_message, earlier_context, message, context], None),
            ("unknown part", [with_content([unknown, *content]), context], "missing or ambiguous"),
            ("different prompt", [with_content([prompt_context, different_prompt]), context],
             "missing or ambiguous"),
            ("duplicate prompt part", [with_content([prompt_context, *content, *content]), context],
             "missing or ambiguous"),
            ("two matching messages", [message, context, other_message, earlier_context], "missing or ambiguous"),
            ("effort", [message, with_payload(context, effort="low")], "selected .*low"),
            ("model", [message, with_payload(context, model="gpt-6.1-sol")], "selected .*sol"),
            ("missing", [message], "missing or ambiguous"),
            ("wrong context", [message, earlier_context], "missing or ambiguous"),
            ("other turn", [earlier_message, earlier_context], "missing or ambiguous"),
            ("ambiguous", [message, context, context], "missing or ambiguous"),
        )
        with TemporaryDirectory() as root:
            directory = Path(root)
            rollout_dir = directory / "turn-32-codex-rollout-fixture"
            rollout_dir.mkdir()
            (directory / "turn-32-audit.json").write_text(json.dumps({"rollout_directory": rollout_dir.name}))
            (directory / "turn-32-transcript.json").write_text(json.dumps(pages))
            for scenario, rows, error in cases:
                with self.subTest(scenario=scenario):
                    rollout = rollout_dir / "rollout-fixture.jsonl"
                    rollout.write_text("".join(json.dumps(row) + "\n" for row in [meta, *rows]))
                    if error:
                        with self.assertRaisesRegex(ValueError, error):
                            model_usage(usage, command, 32, TERRA, "unused", directory)
                    else:
                        self.assertEqual(model_usage(usage, command, 32, TERRA, "unused", directory), TERRA)
