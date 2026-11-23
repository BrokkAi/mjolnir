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

from . import bifrost_gold, fidelity_live
from .fidelity import (BARS, CENSUS, Query, accuracy, bar_failures, gate_report, load_inputs,
                       measure, pooled_prescience, prescience_metrics, sample, step_result, stratum_metrics)
from .engine.grammar import MENU
from .engine.runner import EngineRunner, SealedRunner
from .fidelity_live import (SONNET, TERRA, adjudication_input, candidate_lines,
                            model_usage, partition_candidates, partition_reconciliation_groups,
                            planned_calls, reconciliation_groups, reconciliation_input,
                            validate_reconciliation)
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
        digest, manifest, _served, documents, _ = load_inputs(self.manifest_path)
        step = next(iter(MENU))
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
        self.bifrost_bin = self.root / "fake-bifrost"
        self.bifrost_bin.write_text("#!/bin/sh\necho fixture\n")
        self.bifrost_bin.chmod(0o700)
        # The real phase selection, checkpoints, measurement and reporting run offline.
        for name, fake in {
            "load_inputs": lambda path: (digest, None, [step], documents, None),
            "load_engine": lambda *args, **kwargs: engine,
            "touched_symbols": lambda *args: [],
            "sample": lambda *args: (self.queries, self.fresh),
            "prepared_snapshot": lambda *args: None,
            "BifrostRunner": self.fake_bifrost_runner,
            "LiveModels": self.gold_models,
        }.items():
            self.addCleanup(setattr, fidelity_live, name, getattr(fidelity_live, name))
            setattr(fidelity_live, name, fake)
        self.grammar = self.root / "grammar"
        (self.grammar / "src").mkdir(parents=True)
        (self.grammar / "src/parser.c").write_text("fixture")

    def fake_bifrost_runner(self, binary, repos, work_root, checkpoint_root, **kwargs):
        self.assertEqual(binary, self.bifrost_bin)

        def query(repo, commit, text):
            self.gold_calls.append(("bifrost", text))
            return {"rows": self.rows, "complete": True, "truncated": False, "diagnostics": [],
                    "bifrost_binary_sha256": "0" * 64, "bifrost_build_identity": "fixture-bifrost"}

        return SimpleNamespace(query=query, binary_sha256="0" * 64, build_identity="fixture-bifrost")

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

    def gold_models(self, *args, bifrost=None):
        self.gold_calls.append("started")

        def enumerate_rows(repo, commit, text):
            self.gold_calls.append(("terra", text))
            return {"rows": self.rows}

        def adjudicate(query, candidates):
            self.gold_calls.append(("sonnet", query.text))
            return {key: {"in_scope": True, "key": row["name"], "reason": "fixture evidence"}
                    for key, row in candidates.items()}

        return SimpleNamespace(enumerate_rows=enumerate_rows, adjudicate=adjudicate,
                               bifrost=bifrost, bifrost_rows=bifrost.query if bifrost else None)

    def run_phase(self, root, *options):
        output = StringIO()
        extra = ["--bifrost-bin", str(self.bifrost_bin)] if "--phase" not in options else []
        with redirect_stdout(output):
            status = fidelity_live.main(["--live", "--run-root", str(root), "--manifest", str(self.manifest_path),
                                         "--rust-grammar", str(self.grammar), "--js-grammar", str(self.grammar),
                                         *extra, *options])
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

    # Hard-won: 82a0658a0: Terra bailed out with non-semantic coverage.open reasons and passed validation.
    def test_terra_partial_enumeration_bailout_is_rejected(self):
        model = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
        model.manifest = {"diffs": [], "served_steps": []}
        model.catalogue = model.menu = ""
        model.directory = Path("fixture")
        model.terra_profile = "terra"
        site = {"path": "source.rs", "start_line": 1, "end_line": 1}

        for reason in (
            "The matching callers have not been enumerated.",
            "Contract cases and guard alternatives in this seed were not fully evaluated.",
        ):
            with self.subTest(reason=reason):
                bailout = {"rows": [], "coverage": {"complete": False, "open": [{"reason": reason, "site": site}]},
                           "excluded": []}

                def sealed_task(commit, directory, name, profile, files, prompt, check_output):
                    self.assertIn("incomplete enumeration is rejected as a validation error",
                                  prompt("/fixture/.scratch"))
                    return check_output(json.dumps(bailout), {"source.rs": b"fn f() {}\n"}.get), {"model": name}

                model.sealed_task = sealed_task
                with self.assertRaisesRegex(fidelity_live.TerraEvidenceError, "incomplete or partial enumeration"):
                    model.enumerate_rows("fixture", "a" * 40, '(callers (enclosing-decl (callable :name "f")))')

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

        good, bad, missing = answer(operand), answer({**operand, "extra": True}), None
        # Normalizes cleanly but fails answer validation with three errors.
        staged = json.dumps({
            "rows": [{"kind": "structural_match", "primary_site": site, "fields": {"operand": operand}}],
            "coverage": {"complete": True, "open": [{"reason": 3}, {"reason": 4}]}, "excluded": [],
        }).encode()
        errors_named = {bad: "written evidence must contain only site and verbatim",
                        staged: "coverage.complete must be true exactly when coverage.open is empty",
                        missing: "output.json was not written"}
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
                if "--ls" in args:
                    return json.dumps({"path": args[3], "entries": []}).encode()
                payload = schedule[int(sid[0]) - 1][len(turns) - 1]
                if payload is missing:
                    raise RuntimeError("mj fetch failed: output.json does not exist on the session's target")
                return payload
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
            # Hard-won: 1a919b31c: Terra withheld output.json instead of enumerating, crashing turn 1 without repair
            for schedule in (
                ((bad, good),),
                ((missing, good),),
                ((bad, staged, good),),
                ((bad, bad), (good,)),
                ((missing, missing), (good,)),
                ((bad, bad), (bad, bad)),
            ):
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

    def test_adjudication_batches_reconcile_cross_batch_identities_and_resume(self):
        blobs = {
            "a.rs": "".join(f"line_a_{i}\n" for i in range(1, 31)).encode(),
            "b.rs": "".join(f"line_b_{i}\n" for i in range(1, 31)).encode(),
            "c.rs": "".join(f"line_c_{i}\n" for i in range(1, 31)).encode(),
        }
        # 0, 1, 2 connect transitively across a.rs -> b.rs -> c.rs;
        # 3 is a disjoint census claim in a.rs that denotes the same relation as {0, 1, 2};
        # 4 is a disjoint invalid candidate in c.rs that reuses the same raw key "shared".
        candidates = {
            "4": {"sites": [{"path": "c.rs", "start_line": 20, "end_line": 21}], "detail": "z" * 300},
            "3": {"claim": "census relation", "scope": "Covered scoped claim",
                  "sites": [{"path": "a.rs", "start_line": 20, "end_line": 21}], "detail": "y" * 450},
            "2": {"sites": [{"path": "c.rs", "start_line": 4, "end_line": 6}], "detail": "x" * 120},
            "0": {"sites": [{"path": "a.rs", "start_line": 2, "end_line": 3},
                            {"path": "b.rs", "start_line": 10, "end_line": 12}], "detail": "x" * 120},
            "1": {"sites": [{"path": "b.rs", "start_line": 11, "end_line": 13},
                            {"path": "c.rs", "start_line": 2, "end_line": 4}], "detail": "x" * 120},
        }
        lines = candidate_lines(candidates, blobs.get)
        rendered = adjudication_input("(q)", candidates, lines)
        self.assertGreater(rendered.count("\n"), 10)
        decoded = json.loads(rendered)
        self.assertEqual(decoded["query"], "(q)")
        self.assertEqual(decoded["candidates"], candidates)
        self.assertEqual(decoded["source"]["a.rs:2-3"], ["line_a_2", "line_a_3"])

        # With a cap that fits the {0, 1, 2} component together, overlapping {0, 1, 2} stay atomic while {3} and {4} split.
        comp012_bytes = len(adjudication_input("(q)", {k: candidates[k] for k in ("0", "1", "2")}, lines).encode("utf-8"))
        batches = partition_candidates("(q)", candidates, lines, max_bytes=comp012_bytes)
        self.assertEqual([list(batch) for batch in batches], [["0", "1", "2"], ["3"], ["4"]])
        # An oversized connected component and an empty-site candidate still split into bounded batches.
        single_comp = {key: candidates[key] for key in ("0", "1", "2")}
        split_comp = partition_candidates("(q)", single_comp, lines, max_bytes=400)
        self.assertEqual([list(batch) for batch in split_comp], [["0"], ["1"], ["2"]])
        with_empty = {**candidates, "5": {"sites": [], "detail": "w" * 450}}
        split_empty = partition_candidates("(q)", with_empty, candidate_lines(with_empty, blobs.get),
                                           max_bytes=comp012_bytes)
        self.assertEqual([list(batch) for batch in split_empty], [["0", "1", "2"], ["3"], ["4", "5"]])
        self.assertTrue(all(len(adjudication_input("(q)", b, candidate_lines(with_empty, blobs.get)).encode()) <= comp012_bytes
                            for b in split_empty))

        query = Query("guarded-paths", "decisive", "fixture", "c" * 40, "(q)")
        with TemporaryDirectory() as root, patch.object(fidelity_live, "ADJUDICATION_INPUT_LIMIT", comp012_bytes), \
                patch.object(fidelity_live, "blob_reader", lambda *args: blobs.get):
            adapter = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
            adapter.directory = Path(root)
            adapter.sources = {query.commit: "fixture"}
            adapter.catalogue = adapter.menu = adapter.rulings = ""

            seen_tasks = []
            fail_stage = "batch-001"
            reconcile_files_seen = {}

            def fake_sealed_task(commit, directory, model, profile, files, prompt, check_output):
                directory.mkdir(parents=True, exist_ok=True)
                output_file = directory / "output.json"
                if (directory / "completed.json").exists():
                    return check_output(output_file.read_bytes(), blobs.get), {"reused": True}
                seen_tasks.append(directory.name)
                if directory.name == fail_stage:
                    (directory / "failed.json").write_text("{}")
                    raise RuntimeError(f"transient {directory.name} failure")
                payload = json.loads(files["input.json"])
                if directory.name == "reconcile":
                    reconcile_files_seen.update(files)
                    # Merge disjoint census group b1:shared with enumerated group b0:shared;
                    # keep invalid group b2:shared distinct.
                    raw = {"merges": [{"keys": ["b0:shared", "b1:shared"],
                                       "reason": "census claim 3 matches enumerated row 0/1/2"}]}
                else:
                    # Every batch emits the same raw key "shared", with batch-002 invalid.
                    in_scope = directory.name != "batch-002"
                    raw = {cid: {"in_scope": in_scope, "key": "shared", "reason": f"from {directory.name}"}
                           for cid in payload["candidates"]}
                raw_bytes = json.dumps(raw).encode()
                checked = check_output(raw_bytes, blobs.get)
                output_file.write_bytes(raw_bytes)
                (directory / "completed.json").write_text("{}")
                return checked, {"reused": False}

            adapter.sealed_task = fake_sealed_task
            with self.assertRaisesRegex(RuntimeError, "transient batch-001 failure"):
                adapter.adjudicate(query, candidates)
            self.assertEqual(seen_tasks, ["batch-000", "batch-001"])

            # Reconciling batch-001 reuses batch-000, then fails in reconcile to test reconcile resume.
            adj_dir = adapter.directory / query.key / "adjudicator"
            (adj_dir / "batch-001").rename(adj_dir / "batch-001-failed-deadbeef")
            fail_stage = "reconcile"
            with self.assertRaisesRegex(RuntimeError, "transient reconcile failure"):
                adapter.adjudicate(query, candidates)
            self.assertEqual(seen_tasks, ["batch-000", "batch-001", "batch-001", "batch-002", "reconcile"])

            # Reconciling reconcile reuses all three completed batches without re-running them.
            (adj_dir / "reconcile").rename(adj_dir / "reconcile-failed-cafebabe")
            fail_stage = None
            merged = adapter.adjudicate(query, candidates)
            self.assertEqual(seen_tasks, ["batch-000", "batch-001", "batch-001", "batch-002", "reconcile", "reconcile"])
            # Reconciliation is self-contained (no batch-*.json files staged) with path-aliased lossless groups.
            self.assertEqual(sorted(reconcile_files_seen), ["catalogue.md", "input.json", "menu.md", "rulings.md"])
            rec_payload = json.loads(reconcile_files_seen["input.json"])
            self.assertEqual(rec_payload["files"], {"f0": "a.rs", "f1": "b.rs", "f2": "c.rs"})
            self.assertEqual(rec_payload["groups"]["b1:shared"]["census"],
                             [{"id": "3", "claim": "census relation", "scope": "Covered scoped claim"}])
            self.assertEqual(rec_payload["groups"]["b0:shared"]["representative"]["detail"], "x" * 120)
            # Cross-batch census claim 3 merged onto 0/1/2; invalid 4 stayed distinct.
            self.assertEqual({merged[cid]["key"] for cid in ("0", "1", "2", "3")}, {"b0:shared"})
            self.assertEqual(merged["4"]["key"], "b2:shared")
            self.assertTrue(all(merged[cid]["in_scope"] for cid in ("0", "1", "2", "3")))
            self.assertFalse(merged["4"]["in_scope"])

            # Reconciliation validation rejects invalid merges.
            groups = rec_payload["groups"]
            for bad, pattern in (
                ({"merges": [{"keys": ["b0:shared", "b2:shared"], "reason": "bad"}]}, "in_scope=true and in_scope=false"),
                ({"merges": [{"keys": ["b0:shared", "b9:missing"], "reason": "bad"}]}, "unknown group keys"),
                ({"merges": [{"keys": ["b0:shared"], "reason": "bad"}]}, "at least two distinct"),
                ({"merges": [{"keys": ["b0:shared", "b1:shared"], "reason": "a"},
                             {"keys": ["b1:shared", "b0:shared"], "reason": "b"}]}, "overlap"),
            ):
                with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, pattern):
                    validate_reconciliation(query, groups, bad)

            # Mixed layouts or unexpected active batch directories fail visibly.
            (adj_dir / "batch-003").mkdir()
            with self.assertRaisesRegex(RuntimeError, "unexpected adjudication batch directories"):
                adapter.adjudicate(query, candidates)
            (adj_dir / "batch-003").rmdir()
            (adj_dir / "intent.json").write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "mixed single-session and batched"):
                adapter.adjudicate(query, candidates)
            (adj_dir / "intent.json").unlink()
            with patch.object(fidelity_live, "ADJUDICATION_INPUT_LIMIT", 10_000_000):
                with self.assertRaisesRegex(RuntimeError, "mixed single-session and batched"):
                    adapter.adjudicate(query, candidates)

    def test_oversized_reconciliation_runs_bounded_pairs_with_transitive_closure(self):
        blobs = {"a.rs": "".join(f"line_{i}\n" for i in range(1, 40)).encode()}
        # Three overlapping candidates that split into 3 batches under a tight limit,
        # whose 3 reconciliation groups exceed the limit together and must reconcile pairwise.
        candidates = {
            "0": {"kind": "guarded_path", "fields": {"note": "a" * 90},
                  "sites": [{"path": "a.rs", "start_line": 1, "end_line": 10}]},
            "1": {"kind": "guarded_path", "fields": {"note": "b" * 90},
                  "sites": [{"path": "a.rs", "start_line": 5, "end_line": 15}]},
            "2": {"kind": "guarded_path", "fields": {"note": "c" * 90},
                  "sites": [{"path": "a.rs", "start_line": 10, "end_line": 20}]},
        }
        # Measure a cap that forces 3 single-candidate batches and 3 reconciliation chunks (pairs <= cap).
        preliminary = {
            cid: {"in_scope": True, "key": f"b{i}:row", "reason": "ok"}
            for i, cid in enumerate(("0", "1", "2"))
        }
        file_table, groups = reconciliation_groups([{cid: candidates[cid]} for cid in ("0", "1", "2")], preliminary)
        pair01_bytes = len(reconciliation_input("(q)", file_table,
                                                {"b0:row": groups["b0:row"], "b1:row": groups["b1:row"]}).encode())
        full_rec_bytes = len(reconciliation_input("(q)", file_table, groups).encode())
        cap = pair01_bytes + 4
        self.assertLess(cap, full_rec_bytes)
        self.assertEqual(len(partition_reconciliation_groups("(q)", file_table, groups, max_bytes=cap)), 3)

        # Compact representatives preserve verbatim byte offsets so repeated tokens on one line stay distinct.
        line_site = {"path": "a.rs", "start_line": 1, "end_line": 1}
        repeated_batches = [
            {"0": {"kind": "structural_match", "sites": [line_site], "fields": {
                "operand": {"site": line_site, "verbatim": [{"start_byte": 0, "end_byte": 3, "text": "foo"}]}}}},
            {"1": {"kind": "structural_match", "sites": [line_site], "fields": {
                "operand": {"site": line_site, "verbatim": [{"start_byte": 6, "end_byte": 9, "text": "foo"}]}}}},
        ]
        repeated_prelim = {
            "0": {"in_scope": True, "key": "b0:first", "reason": "first foo"},
            "1": {"in_scope": True, "key": "b1:second", "reason": "second foo"},
        }
        _, repeated_groups = reconciliation_groups(repeated_batches, repeated_prelim)
        self.assertEqual(repeated_groups["b0:first"]["representative"]["fields"]["operand"],
                         {"site": "f0:1-1", "verbatim": [[0, 3, "foo"]]})
        self.assertEqual(repeated_groups["b1:second"]["representative"]["fields"]["operand"],
                         {"site": "f0:1-1", "verbatim": [[6, 9, "foo"]]})

        query = Query("guarded-paths", "random", "fixture", "c" * 40, "(q)")
        with TemporaryDirectory() as root, patch.object(fidelity_live, "ADJUDICATION_INPUT_LIMIT", cap), \
                patch.object(fidelity_live, "blob_reader", lambda *args: blobs.get):
            adapter = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
            adapter.directory = Path(root)
            adapter.sources = {query.commit: "fixture"}
            adapter.catalogue = adapter.menu = adapter.rulings = ""

            pair_sizes = {}
            fail_pair = "pair-001-002"
            ran = []

            def fake_sealed_task(commit, directory, model, profile, files, prompt, check_output):
                directory.mkdir(parents=True, exist_ok=True)
                output_file = directory / "output.json"
                if (directory / "completed.json").exists():
                    return check_output(output_file.read_bytes(), blobs.get), {"reused": True}
                ran.append(directory.name)
                if directory.name == fail_pair:
                    (directory / "failed.json").write_text("{}")
                    raise RuntimeError("transient pair failure")
                payload = json.loads(files["input.json"])
                if directory.name.startswith("pair-"):
                    pair_sizes[directory.name] = len(files["input.json"].encode("utf-8"))
                    # Merge b0:row==b1:row in pair-000-001 and b1:row==b2:row in pair-001-002;
                    # pair-000-002 emits no merge, testing Union-Find transitive closure.
                    if directory.name == "pair-000-001":
                        raw = {"merges": [{"keys": ["b0:row", "b1:row"], "reason": "0 equals 1"}]}
                    elif directory.name == "pair-001-002":
                        raw = {"merges": [{"keys": ["b1:row", "b2:row"], "reason": "1 equals 2"}]}
                    else:
                        raw = {"merges": []}
                else:
                    raw = {cid: {"in_scope": True, "key": "row", "reason": "ok"} for cid in payload["candidates"]}
                raw_bytes = json.dumps(raw).encode()
                checked = check_output(raw_bytes, blobs.get)
                output_file.write_bytes(raw_bytes)
                (directory / "completed.json").write_text("{}")
                return checked, {"reused": False}

            adapter.sealed_task = fake_sealed_task
            with self.assertRaisesRegex(RuntimeError, "transient pair failure"):
                adapter.adjudicate(query, candidates)
            rec_dir = adapter.directory / query.key / "adjudicator" / "reconcile"
            (rec_dir / "pair-001-002").rename(rec_dir / "pair-001-002-failed-deadbeef")
            fail_pair = None
            merged = adapter.adjudicate(query, candidates)
            # Every pair stayed within the byte cap and pair-000-001/pair-000-002 were reused on resume.
            self.assertEqual(ran, ["batch-000", "batch-001", "batch-002",
                                   "pair-000-001", "pair-000-002", "pair-001-002", "pair-001-002"])
            self.assertTrue(all(size <= cap for size in pair_sizes.values()), pair_sizes)
            self.assertEqual({merged[cid]["key"] for cid in ("0", "1", "2")}, {"b0:row"})

            # Mixed single-session and pairwise reconciliation artifacts fail visibly.
            (rec_dir / "intent.json").write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "mixed single-session and pairwise"):
                adapter.adjudicate(query, candidates)
            (rec_dir / "intent.json").unlink()
            (rec_dir / "pair-009-010").mkdir()
            with self.assertRaisesRegex(RuntimeError, "unexpected reconciliation pair directories"):
                adapter.adjudicate(query, candidates)
            (rec_dir / "pair-009-010").rmdir()
            with patch.object(fidelity_live, "ADJUDICATION_INPUT_LIMIT", 10_000_000):
                with self.assertRaisesRegex(RuntimeError, "mixed single-session and batched"):
                    adapter.adjudicate(query, candidates)

    def test_completed_legacy_single_line_adjudication_checkpoint_resumes(self):
        blobs = {"a.rs": b"line1\nline2\nline3\n"}
        candidates = {
            "0": {"sites": [{"path": "a.rs", "start_line": 1, "end_line": 1}], "detail": "x" * 120},
            "1": {"sites": [{"path": "a.rs", "start_line": 3, "end_line": 3}], "detail": "y" * 120},
        }
        decisions = {
            "0": {"in_scope": True, "key": "k0", "reason": "evidence 0"},
            "1": {"in_scope": True, "key": "k1", "reason": "evidence 1"},
        }
        query = Query("documented", "random", "fixture", "c" * 40, "(q)")
        sid, orphan, tree = "a" * 32, "b" * 40, "d" * 40
        lines = candidate_lines(candidates, blobs.get)
        legacy_input = fidelity_live.canonical({"query": query.text, "candidates": candidates, "source": lines})
        legacy_prompt = (fidelity_live.ADJUDICATION
                         + "\nRead .scratch/catalogue.md, .scratch/menu.md, "
                         + ".scratch/rulings.md and .scratch/input.json. "
                         + "Use Write to save the JSON object to .scratch/output.json.\n")
        legacy_files = {"catalogue.md": "", "menu.md": "", "rulings.md": "", "input.json": legacy_input}
        identity = {"model": SONNET, "effort": fidelity_live.EFFORT, "orphan": orphan, "tree": tree,
                    "inputs_sha256": fidelity_live.sha(fidelity_live.canonical([legacy_files, legacy_prompt]).encode())}

        with TemporaryDirectory() as root, ExitStack() as stack:
            directory = Path(root)
            adj_dir = directory / query.key / "adjudicator"
            adj_dir.mkdir(parents=True)
            driver.save(adj_dir / "intent.json", identity)
            driver.save(adj_dir / "session.json", {"session_id": sid})
            driver.save(adj_dir / "completed.json", {"session": sid, "turn": 1, "model": SONNET,
                                                     "version": "claude-sonnet-5-5", "effort": fidelity_live.EFFORT})
            (adj_dir / "output.json").write_text(json.dumps(decisions))
            driver.save(adj_dir / "audit.json", {"passed": True})
            driver.save(adj_dir / "destroy-receipt.json", {"session_id": sid, "accepted": True})

            adapter = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
            adapter.directory = directory
            adapter.config = {"instance": "fidelity-test", "bundle": "fixture", "snapshot_repo": "snapshot",
                              "destination": "engine-trees",
                              "logins": [{"profile": "claude5", "engine_profile": "claude5-engine"}]}
            adapter.manifest = {"eligible_diffs": [], "served_steps": [], "model_versions": {"claude": "Sonnet 5.5"}}
            adapter.sources, adapter.claude_version = {query.commit: "fixture"}, "claude-sonnet-5-5"
            adapter.catalogue = adapter.menu = adapter.rulings = ""

            for target, name, fake in (
                (driver, "use_named_instance", lambda _: directory),
                (fidelity_live, "blob_reader", lambda *args: blobs.get),
                (fidelity_live, "prepared_snapshot", lambda *args: {"orphan": orphan, "tree": tree}),
                (EngineRunner, "for_launch", lambda *args: self.fail("should not launch a new session")),
            ):
                stack.enter_context(patch.object(target, name, new=fake))

            # Resumes cleanly both when the query fits in one batch and when it would now batch.
            self.assertEqual(adapter.adjudicate(query, candidates), decisions)
            with patch.object(fidelity_live, "ADJUDICATION_INPUT_LIMIT", 100):
                self.assertEqual(adapter.adjudicate(query, candidates), decisions)

            # A legacy checkpoint whose inputs actually changed is still rejected.
            changed = {**candidates, "1": {**candidates["1"], "detail": "changed"}}
            with self.assertRaisesRegex(ValueError, "fidelity session inputs changed"):
                adapter.adjudicate(query, changed)

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


class BifrostGoldTests(unittest.TestCase):
    def test_normalize_bifrost_results_handles_documented_types_and_filters_external(self):
        blobs = {
            "src/lib.rs": b"pub fn helper() {}\npub fn target() {\n    helper();\n}\n",
            "src/caller.rs": b"pub fn call_it() {\n    crate::target();\n}\n",
        }
        raw = {
            "isError": False,
            "structuredContent": {
                "truncated": False,
                "diagnostics": [{"code": "uses_candidates_omitted", "impact": "incomplete", "message": "partial"}],
                "results": [
                    {
                        "result_type": "call_site",
                        "site_id": "deadbeef" * 8,
                        "program_point_id": "cafebabe" * 8,
                        "target_id": "12345678" * 8,
                        "path": "src/caller.rs",
                        "range": {"start_line": 2, "start_column": 5, "end_line": 2, "end_column": 20},
                        "caller": {"path": "src/caller.rs", "fq_name": "p.call_it", "start_line": 1, "end_line": 3},
                        "callee": {"path": "src/lib.rs", "fq_name": "p.target", "start_line": 2, "end_line": 4},
                        "provenance": [{"seed": {"path": "src/lib.rs"}, "steps": []}],
                    },
                    {
                        "result_type": "declaration",
                        "path": "src/lib.rs",
                        "fq_name": "p.target",
                        "start_line": 2,
                        "end_line": 4,
                        "provenance": [{"seed": {}}],
                    },
                    {
                        "result_type": "occurrence",
                        "id": "aaa",
                        "ast_id": "bbb",
                        "path": "src/lib.rs",
                        "range": {"start_line": 3, "start_column": 5, "end_line": 3, "end_column": 11},
                        "raw_spelling": "helper",
                    },
                    {
                        "result_type": "file",
                        "path": "src/lib.rs",
                        "language": "rust",
                    },
                    {
                        "result_type": "declaration",
                        "path": "/external/stdlib/core.rs",
                        "start_line": 1,
                        "end_line": 2,
                    },
                ],
            },
        }
        rows, meta = bifrost_gold.normalize_bifrost_results(raw, blobs.get)
        self.assertEqual(len(rows), 4)
        self.assertEqual(meta["skipped_external"], 1)
        self.assertFalse(meta["complete"])
        self.assertEqual(meta["raw_row_count"], 5)
        call_row = next(row for row in rows if row["kind"] == "call_site")
        self.assertEqual(call_row["sites"], [
            {"path": "src/caller.rs", "start_line": 2, "end_line": 2},
            {"path": "src/caller.rs", "start_line": 1, "end_line": 3},
            {"path": "src/lib.rs", "start_line": 2, "end_line": 4},
        ])
        for secret in ("provenance", "site_id", "program_point_id", "target_id", "result_type"):
            self.assertNotIn(secret, call_row["fields"])
        file_row = next(row for row in rows if row["kind"] == "file")
        self.assertEqual(file_row["sites"], [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}])

        with self.assertRaisesRegex(RuntimeError, "query error"):
            bifrost_gold.normalize_bifrost_results({"isError": True, "error": "boom"}, blobs.get)
        bad_span = {"isError": False, "structuredContent": {"results": [
            {"result_type": "declaration", "path": "src/lib.rs", "start_line": 1, "end_line": 99},
        ]}}
        with self.assertRaisesRegex(ValueError, "outside its 4 lines"):
            bifrost_gold.normalize_bifrost_results(bad_span, blobs.get)

    def test_documented_measure_and_gate_report_pool_real_bifrost_rows(self):
        doc_query = Query("documented", "random", "mjolnir", "a" * 40, '(callers (enclosing-decl (callable :name "f")))')
        added_query = Query("contract-cases", "random", "mjolnir", "a" * 40, '(contract-cases (procedure-of (callable :name "f")))')
        bifrost_calls = []

        def fake_engine(commit, text, *, fresh):
            return {"status": "ok", "rows": [{"name": "found_by_engine", "sites": []}]}

        def fake_terra(repo, commit, text):
            return {"rows": [{"name": "found_by_engine", "sites": []}]}

        def fake_bifrost(repo, commit, text):
            bifrost_calls.append((repo, commit, text))
            return {
                "rows": [{"name": "found_by_engine", "sites": []}, {"name": "bifrost_only", "sites": []}],
                "complete": True, "truncated": False, "diagnostics": [],
                "bifrost_binary_sha256": "1" * 64, "bifrost_build_identity": "abc123",
            }

        def fake_adjudicate(query, candidates):
            return {cid: {"key": row["name"], "in_scope": True, "reason": "verified"} for cid, row in candidates.items()}

        doc_record = measure(doc_query, True, fake_engine, fake_terra, fake_adjudicate, bifrost_rows=fake_bifrost)
        added_record = measure(added_query, False, fake_engine, fake_terra, fake_adjudicate, bifrost_rows=fake_bifrost)
        self.assertEqual(bifrost_calls, [("mjolnir", "a" * 40, doc_query.text)])
        self.assertEqual(doc_record["bifrost"], ["bifrost_only", "found_by_engine"])
        self.assertEqual(doc_record["gold"], ["bifrost_only", "found_by_engine"])
        self.assertEqual(doc_record["engine"], ["found_by_engine"])
        self.assertNotIn("bifrost", added_record)

        report = gate_report("m" * 64, [], [doc_query], {doc_query.key: doc_record}, 20261120)
        self.assertEqual(report["candidate_sources"], ["engine", "terra_enumerator", "bifrost", "census"])
        self.assertEqual(report["documented"]["metrics"]["recall"], 0.5)
        self.assertEqual(report["documented"]["bifrost"], {
            "queries_ran": 1, "complete_queries": 1, "truncated_queries": 0, "total_rows": 2,
            "gold_rows_found": 2, "gold_rows_outside_engine": 1, "precision": 1.0, "recall": 1.0,
            "binary_sha256": ["1" * 64], "build_identity": ["abc123"],
        })

    def test_bifrost_runner_checkpoints_resumes_and_isolates_checkouts(self):
        import subprocess

        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / "repo"
            repo.mkdir()
            subprocess.run(["git", "-C", str(repo), "init", "--template="], check=True, capture_output=True)
            (repo / "src").mkdir()
            (repo / "src/lib.rs").write_text("pub fn target() {}\npub fn caller() { target(); }\n")
            subprocess.run(["git", "-C", str(repo), "add", "src/lib.rs"], check=True, capture_output=True)
            subprocess.run(["git", "-C", str(repo), "-c", "user.name=T", "-c", "user.email=t@t",
                            "-c", "commit.gpgsign=false", "commit", "-m", "v1"], check=True, capture_output=True)
            c1 = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
            (repo / "src/lib.rs").write_text("pub fn dirty() {}\n")

            invocations = root / "invocations.log"
            fake_bin = root / "fake-bifrost"
            fake_bin.write_text(f"""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
if args == ["--build-identity"]:
    print("fake-build-id")
    sys.exit(0)
root_dir = Path(args[args.index("--root") + 1])
with open({str(invocations)!r}, "a") as f:
    f.write(json.dumps(args) + "\\n")
if "--query-file" in args:
    qfile = root_dir / args[args.index("--query-file") + 1]
    qtext = qfile.read_text().strip()
    assert qtext.startswith("(limit 1000 "), qtext
    assert "--workspace-scaled-limits" in args
    if "trigger-error" in qtext:
        print(json.dumps({{"isError": True, "error": "simulated failure"}}))
        sys.exit(0)
    print(json.dumps({{"isError": False, "structuredContent": {{"truncated": False, "results": [
        {{"result_type": "declaration", "path": "src/lib.rs", "fq_name": "p.caller", "start_line": 2, "end_line": 2}}
    ]}}}}))
    sys.exit(0)
# MCP warm-up mode
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "initialize":
        print(json.dumps({{"jsonrpc": "2.0", "id": msg["id"], "result": {{}}}}), flush=True)
    elif msg["method"] == "tools/call":
        print(json.dumps({{"jsonrpc": "2.0", "id": msg["id"], "result": {{"structuredContent": {{
            "analyzer_ready": True, "usage_index_ready": True, "semantic_packs_ready": True,
            "workspace_build": {{"status": "completed"}}
        }}}}}}), flush=True)
""")
            fake_bin.chmod(0o700)

            runner = bifrost_gold.BifrostRunner(
                fake_bin, {"mjolnir": repo}, root / "work", root / "checkpoints",
                memory_cap_gib=None, use_systemd_cap=False,
            )
            q = '(callers (enclosing-decl (where "src/lib.rs" (callable :name "target"))))'
            first = runner.query("mjolnir", c1, q)
            self.assertEqual(len(first["rows"]), 1)
            self.assertEqual(first["rows"][0]["sites"], [{"path": "src/lib.rs", "start_line": 2, "end_line": 2}])
            self.assertEqual((repo / "src/lib.rs").read_text(), "pub fn dirty() {}\n")
            self.assertFalse((root / "work/checkouts/mjolnir" / bifrost_gold.QUERY_FILENAME).exists())

            # Second call resumes from checkpoint without invoking bifrost again.
            count_before = len(invocations.read_text().splitlines())
            self.assertEqual(count_before, 2)  # 1 warm + 1 query
            self.assertEqual(runner.query("mjolnir", c1, q), first)
            self.assertEqual(len(invocations.read_text().splitlines()), count_before)

            # Changed binary sha invalidates intent visibly.
            fake_bin.write_text(fake_bin.read_text() + "# bumped\n")
            bumped = bifrost_gold.BifrostRunner(
                fake_bin, {"mjolnir": repo}, root / "work", root / "checkpoints",
                memory_cap_gib=None, use_systemd_cap=False,
            )
            with self.assertRaisesRegex(ValueError, "bifrost checkpoint inputs changed"):
                bumped.query("mjolnir", c1, q)

            # Query error fails visibly and writes no completed.json.
            bad_q = '(callers (enclosing-decl (where "src/lib.rs" (callable :name "trigger-error"))))'
            with self.assertRaisesRegex(RuntimeError, "simulated failure"):
                runner.query("mjolnir", c1, bad_q)
            bad_key = fidelity_live.sha(fidelity_live.canonical(["documented", "mjolnir", c1, bad_q]).encode())
            self.assertFalse((root / "checkpoints" / bad_key / "bifrost/completed.json").exists())

    def test_concurrent_run_sample_enforces_commit_repo_bounds_and_resumes_cleanly(self):
        import threading
        import time

        c_shared = "a" * 40
        c_other = "b" * 40
        c_bf = "c" * 40
        queries = [
            Query("documented", "random", "mjolnir", c_shared, '(callers (callable :name "f1"))'),
            Query("documented", "random", "mjolnir", c_shared, '(callers (callable :name "f2"))'),
            Query("documented", "random", "mjolnir", c_other, '(callers (callable :name "f3"))'),
            Query("documented", "random", "bifrost-dev", c_bf, '(callers (callable :name "f4"))'),
            Query("documented", "random", "bifrost-dev", c_bf, '(callers (callable :name "f5"))'),
        ]
        fresh = {q.key for q in queries}

        def make_harness(stop_after_first_engine=None):
            lock = threading.Lock()
            active_models = 0
            peak_models = 0
            active_commits = set()
            active_repos = set()
            commit_overlap = []
            repo_overlap = []
            distinct_repo_overlap = False
            engine_done = 0

            def enter_model():
                nonlocal active_models, peak_models
                with lock:
                    active_models += 1
                    peak_models = max(peak_models, active_models)

            def leave_model():
                nonlocal active_models
                with lock:
                    active_models -= 1

            def answer(commit, text, *, fresh=False):
                nonlocal engine_done
                enter_model()
                with lock:
                    if commit in active_commits:
                        commit_overlap.append(commit)
                    active_commits.add(commit)
                try:
                    time.sleep(0.03)
                    return {"status": "ok", "rows": [{"kind": "declaration", "sites": [], "fields": {"text": text}}],
                            "coverage": {"complete": True, "open": []}}
                finally:
                    with lock:
                        active_commits.remove(commit)
                        engine_done += 1
                        if stop_after_first_engine is not None and engine_done == 1:
                            stop_after_first_engine.write_text("stop\n")
                    leave_model()

            def enumerate_rows(repo, commit, text):
                enter_model()
                try:
                    time.sleep(0.02)
                    return {"rows": [{"kind": "declaration", "sites": [], "fields": {"text": text, "src": "t"}}],
                            "coverage": {"complete": True, "open": []}}
                finally:
                    leave_model()

            def bifrost_rows(repo, commit, text):
                nonlocal distinct_repo_overlap
                with lock:
                    if repo in active_repos:
                        repo_overlap.append(repo)
                    active_repos.add(repo)
                    if len(active_repos) >= 2:
                        distinct_repo_overlap = True
                try:
                    time.sleep(0.04)
                    return {"rows": [{"kind": "declaration", "sites": [], "fields": {"text": text, "src": "b"}}],
                            "complete": True, "truncated": False,
                            "bifrost_binary_sha256": "1" * 64, "bifrost_build_identity": "fake"}
                finally:
                    with lock:
                        active_repos.remove(repo)

            def adjudicate(query, candidates):
                enter_model()
                try:
                    time.sleep(0.01)
                    return {cid: {"in_scope": True, "key": row["fields"]["text"], "reason": "ok"}
                            for cid, row in candidates.items()}
                finally:
                    leave_model()

            engine = SimpleNamespace(answer=answer)
            models = SimpleNamespace(enumerate_rows=enumerate_rows, adjudicate=adjudicate,
                                     bifrost_rows=bifrost_rows)
            stats = lambda: (peak_models, commit_overlap, repo_overlap, distinct_repo_overlap)
            return engine, models, stats

        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            serial_engine, serial_models, _ = make_harness()
            serial = fidelity_live.run_sample(
                root / "serial", queries, fresh, serial_engine, serial_models,
                stop_request=root / "no-stop", concurrency=1,
            )

            conc_engine, conc_models, stats = make_harness()
            conc = fidelity_live.run_sample(
                root / "conc", queries, fresh, conc_engine, conc_models,
                stop_request=root / "no-stop", concurrency=4,
            )
            peak_models, commit_overlap, repo_overlap, distinct_repo_overlap = stats()
            self.assertEqual(conc, serial)
            self.assertLessEqual(peak_models, 4)
            self.assertGreaterEqual(peak_models, 2)
            self.assertEqual(commit_overlap, [])
            self.assertEqual(repo_overlap, [])
            self.assertTrue(distinct_repo_overlap)

            # Mid-flight stop drains cleanly and subsequent resume finishes identically.
            stop_file = root / "stop-requested"
            stop_engine, stop_models, _ = make_harness(stop_after_first_engine=stop_file)
            with self.assertRaises(fidelity_live.StopRequested):
                fidelity_live.run_sample(
                    root / "resumed", queries, fresh, stop_engine, stop_models,
                    stop_request=stop_file, concurrency=3,
                )
            stop_file.unlink()
            resume_engine, resume_models, _ = make_harness()
            resumed = fidelity_live.run_sample(
                root / "resumed", queries, fresh, resume_engine, resume_models,
                stop_request=stop_file, concurrency=3,
            )
            self.assertEqual(resumed, serial)

            # Changing the Bifrost binary invalidates completed measurements on resume (serial and concurrent).
            changed_bifrost = SimpleNamespace(binary_sha256="2" * 64, build_identity="other")
            mismatch_models = SimpleNamespace(
                enumerate_rows=resume_models.enumerate_rows,
                adjudicate=resume_models.adjudicate,
                bifrost=changed_bifrost,
                bifrost_rows=resume_models.bifrost_rows,
            )
            for workers in (1, 3):
                with self.subTest(concurrency=workers):
                    with self.assertRaisesRegex(ValueError, "used a different Bifrost binary"):
                        fidelity_live.run_sample(
                            root / "resumed", queries, fresh, resume_engine, mismatch_models,
                            stop_request=stop_file, concurrency=workers,
                        )
