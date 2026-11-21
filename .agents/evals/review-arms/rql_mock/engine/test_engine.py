"""Offline engine tests. The tests never call mj, Podman, or a provider."""
from copy import deepcopy
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
from types import MethodType
import unittest

import driver
from census_label import CENSUS
from rql_mock import fidelity_live
from rql_mock import engine as live_engine

from . import Engine, frozen_inputs, teardown
from .__main__ import audit
from .grammar import parse
from .prompt import assert_blind, semantics_only, turn_prompt
from .runner import CHECKOUT_CLEAN, IsolationFailure, LaunchHeld, SealedRunner, usage_delta
from .snapshot import blob_reader, git_output, manifest_sources, parentless_snapshot, verifier_snapshot
from .validation import canonical, validate


CATALOGUE = (CENSUS / "rql-documented-steps.md").read_text(encoding="utf-8")
MENU = (CENSUS / "menu-steps.md").read_text(encoding="utf-8")
CASES = (CENSUS / "menu-steps-cases.md").read_text(encoding="utf-8")
COMMIT = "a" * 40
VERSION = "claude-sonnet-5-5"
NONPILOT = "b" * 40
SESSION = "c" * 32
REPLACEMENT = "d" * 32
INSTANCE = "pilot-fixture"
QUERY = '(function :name "evaluate")'


def closed():
    return {"complete": True, "open": []}


def span(path="source.rs", line=2):
    return {"path": path, "start_line": line, "end_line": line}


ROWS = [{"kind": "structural_match", "sites": [span()],
         "fields": {"operand": {"site": span(),
                                "verbatim": [{"start_byte": 8, "end_byte": 12, "text": "0x10"}]}}}]
SOURCE = {"source.rs": b"fn evaluate() {\nlet n = 0x10;\n}\n"}
EXCLUDED_FIXTURE = ({"candidate_site": span(), "reason": "duplicate", "detail": ""},)


def payload(rows=ROWS, coverage=None, excluded=()):
    return json.dumps({"rows": rows, "coverage": closed() if coverage is None else coverage,
                       "excluded": list(excluded)})


def fixture_verifier(root, program="print('{\"version\":1,\"errors\":[],\"candidates\":[],\"unchecked\":[]}')"):
    binary = root / "rql-verify"
    binary.write_text("#!/usr/bin/env python3\n" + program + "\n")
    binary.chmod(0o700)
    return binary


def worked_queries():
    fenced = re.findall(r"```rql\n([\s\S]+?)```", CASES)
    inline = re.findall(r"\*\*Empty\.\*\*[^\n]*?`(\([^\n]*?\))`", CASES)
    return fenced + inline


class GrammarTests(unittest.TestCase):
    def test_all_worked_and_illustrative_queries(self):
        cases = worked_queries()
        self.assertEqual(len(cases), 15)
        for query in cases:
            with self.subTest(query=query):
                normalized, steps = parse(query, CATALOGUE)
                self.assertEqual(normalized, " ".join(query.split()))
                self.assertTrue(steps)
        examples = re.findall(r"```text\n([\s\S]+?)```", MENU)
        illustrative = [query.strip() for block in examples
                        for query in re.split(r"\n(?=\()", block) if query.strip()]
        self.assertEqual(len(illustrative), 4)
        for query in illustrative:
            with self.subTest(query=query):
                parse(query, CATALOGUE)

    def test_documented_and_menu_composition(self):
        queries = [
            '(export)',
            '(where "src/**" (language rust (call :callee (name "load"))))',
            '(filter :where ((role eq (enum reference))) (occurrences :class reference))',
            '(filter :where ((kind eq (enum return))) (contract-cases (procedure-of (function))))',
            '(project :columns ((selected_port port_id) sink) (value-transfers (procedure-of (call))))',
            '(file-of (guarded-paths (where "src/**" (inside (function) (call)))))',
            '(union (contract-cases (procedure-of (function))) (guarded-paths (call)))',
            '(filter :where ((pivot is-null)) (guarded-paths (call)))',
        ]
        for query in queries:
            with self.subTest(query=query):
                self.assertTrue(parse(query, CATALOGUE)[1])

    def test_invalid_forms_and_options(self):
        queries = [
            "explain what this code does",
            "(made-up (call))",
            "(call",
            "(call) (call)",
            "(function arbitrary prose)",
            "(procedure-of :bogus true (function))",
            "(procedure-of (function) (function))",
            "(call-argument (call-bindings (call-shape (call))))",
            "(contract-cases (call))",
            "(contract-cases :depth 5 (procedure-of (call)))",
            '(contract-cases :depth "0" (procedure-of (call)))',
            '(contract-cases :kind return (procedure-of (call)))',
            '(contract-cases :kind "result" (procedure-of (call)))',
            '(contract-cases :kind "all" :kind "all" (procedure-of (call)))',
            '(contract-cases (procedure-of (call)) :kind "return")',
            '(value-transfers :direction "outgoing" (procedure-of (call)))',
            '(value-transfers :kind "return" (procedure-of (call)))',
            '(guarded-paths :event "write" (where "src/**" (call)))',
            '(guarded-paths :direction "incoming" (call))',
            '(guarded-paths (where "src/**" (function)))',
            '(filter :where ((bogus eq "x")) (contract-cases (procedure-of (function))))',
            '(project :columns (bogus) (guarded-paths (call)))',
            '(limit lots (call))',
        ]
        for query in queries:
            with self.subTest(query=query), self.assertRaises(ValueError):
                parse(query, CATALOGUE)

    def test_prompt_is_blind(self):
        staged = Path(__file__).resolve().parents[2] / "brief/rql-census/pilot/manifest.json"
        manifest_path = staged if staged.is_file() else CENSUS / "pilot/manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        prompt = turn_prompt(QUERY, "output", "catalogue", "menu", [])
        bundle = (prompt, CATALOGUE, semantics_only(MENU))
        assert_blind(bundle, manifest)
        combined = "\n".join(bundle)
        self.assertNotIn(CASES, combined)
        self.assertNotIn("menu-steps-cases.md", combined)
        self.assertTrue(all(query.strip() not in combined for query in worked_queries()))
        with self.assertRaisesRegex(ValueError, "bug identifier"):
            assert_blind((*bundle, manifest["diffs"][0]["id"]), manifest)


class FrozenInputTests(unittest.TestCase):
    def fixture(self, root):
        pilot = {"repo": "mjolnir", "id": "pilot", "intro": COMMIT, "key": "mjolnir/pilot"}
        extra = {"repo": "bifrost-dev", "id": "fidelity-only", "intro": NONPILOT}
        documents = {"catalogue": CATALOGUE, "steps": MENU, "menu": "{}",
                     "eligible_labels": "\n".join(map(json.dumps, [pilot, extra]))}
        manifest = {"diffs": [pilot], "input_paths": {"cases": "must-not-open.md"}, "inputs": {},
                    "models": {"engine": "sonnet/high", "judge": "sonnet/high"},
                    "model_versions": {"claude": "Sonnet 5.5", "claude_provider_id": VERSION},
                    "served_steps": ["contract-cases", "guarded-paths"]}
        for key, text in documents.items():
            data = text.encode()
            path = root / f"frozen-{key}"
            path.write_bytes(data)
            manifest["input_paths"][key] = path.name
            manifest["inputs"][f"{key}_sha256"] = hashlib.sha256(data).hexdigest()
        path = root / "manifest.json"
        path.write_text(json.dumps(manifest), encoding="utf-8")
        return path

    def test_two_step_manifest_controls_parser_and_prompt(self):
        with TemporaryDirectory() as directory:
            path = self.fixture(Path(directory))
            manifest, documents, version = frozen_inputs(path)
            self.assertEqual(version, VERSION)
            catalogue, menu = documents
            served = manifest["served_steps"]
            parse("(contract-cases (procedure-of (call)))", catalogue, served)
            parse("(guarded-paths (call))", catalogue, served)
            with self.assertRaisesRegex(ValueError, "undocumented RQL form"):
                parse("(value-transfers (procedure-of (call)))", catalogue, served)
            sanitized = semantics_only(menu, served)
            self.assertIn("`guarded-paths`", sanitized)
            self.assertNotIn("value-transfers", sanitized)
            # Removing an unserved heading from the frozen document is also valid.
            removed = re.sub(r"## `value-transfers`.*?(?=## `guarded-paths`)", "", menu, flags=re.S)
            self.assertEqual(semantics_only(removed, served), sanitized)

    def test_frozen_sidecar_exposes_nonpilot_snapshot_to_host_lookup(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            manifest, _, _ = frozen_inputs(self.fixture(root))
            sources = manifest_sources(manifest, {"mjolnir": root / "mjolnir", "bifrost-dev": root / "bifrost"})
            self.assertEqual(sources, {COMMIT: root / "mjolnir", NONPILOT: root / "bifrost"})

    def test_modified_frozen_files_are_refused_before_any_answer(self):
        for key in ("catalogue", "steps", "menu", "eligible_labels"):
            with self.subTest(key=key), TemporaryDirectory() as directory:
                root = Path(directory)
                path = self.fixture(root)
                (root / f"frozen-{key}").write_bytes(b"tampered")
                with self.assertRaisesRegex(ValueError, key):
                    frozen_inputs(path)

    # Hard-won: 49e16b8: an old endpoint could answer for a different frozen writer assignment.
    def test_writer_endpoint_refuses_mismatched_frozen_inputs(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.fixture(root)
            raw = json.loads(manifest.read_text())
            raw["page_rows"] = 50
            manifest.write_text(json.dumps(raw))
            config = root / "engine.json"
            config.write_text(json.dumps({"manifest": str(manifest), "verifier": str(fixture_verifier(root))}))
            engine = live_engine.load_engine(config)
            query, _ = parse(QUERY, engine.catalogue, engine.served_steps)
            key = live_engine.cache_key(COMMIT, query, engine.identity["engine_inputs_sha256"])
            cache = engine.cache / COMMIT / f"{key}.json"
            cache.parent.mkdir(parents=True)
            cache.write_text(json.dumps({"status": "ok", "rows": ROWS, "total": len(ROWS), "coverage": closed()}))
            sid, diff = SESSION, raw["diffs"][0]["key"]
            transport, log = root / "transport.json", root / "queries.jsonl"
            wrapper = Path(__file__).resolve().parents[1] / "serve-rql.sh"
            env = {**os.environ, "SSH_ORIGINAL_COMMAND": sid, "REVIEW_ARMS_ENGINE_ROOT": str(root)}
            request = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": "rql_query", "arguments": {"query": QUERY}}}
            for altered in (None, "manifest_sha256", "engine_inputs_sha256"):
                with self.subTest(altered=altered):
                    identity = dict(engine.identity)
                    if altered:
                        identity[altered] = "0" * 64
                    transport.write_text(json.dumps({
                        "manifest": str(manifest), "engine": "rql_mock.engine:answer",
                        "sessions": {sid: {"diff": diff, "intro": COMMIT, "arm": "R", "log": str(log), **identity}},
                    }))
                    result = subprocess.run([wrapper, transport, sid], check=True, env=env, capture_output=True,
                                            text=True, input=json.dumps(request) + "\n")
                    response = json.loads(result.stdout)["result"]
                    body = json.loads(response["content"][0]["text"])
                    if altered:
                        self.assertTrue(response["isError"])
                        self.assertEqual(body["status"], "engine_error")
                        self.assertEqual(body["message"], "The engine could not produce a valid answer.")
                        delivery = json.loads(log.read_text().splitlines()[-1])
                        self.assertIn("inputs differ", delivery["engine_diagnostic"])
                    else:
                        self.assertEqual(body["rows"], ROWS)

    def test_engine_load_failure_stores_details_on_the_host(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.fixture(root)
            raw = json.loads(manifest.read_text())
            raw["page_rows"] = 50
            manifest.write_text(json.dumps(raw))
            config = root / "engine.json"
            descriptor, log = root / "transport.json", root / "queries.jsonl"
            descriptor.write_text(json.dumps({
                "manifest": str(manifest), "engine": "rql_mock.engine:load_engine",
                "sessions": {SESSION: {"diff": raw["diffs"][0]["key"], "intro": COMMIT, "arm": "R",
                                       "log": str(log), "manifest_sha256": "0" * 64,
                                       "engine_inputs_sha256": "1" * 64}},
            }))
            wrapper = Path(__file__).resolve().parents[1] / "serve-rql.sh"
            env = {**os.environ, "SSH_ORIGINAL_COMMAND": SESSION, "REVIEW_ARMS_ENGINE_ROOT": str(root)}
            catalogue = root / raw["input_paths"]["catalogue"]
            diagnostic = log.with_name("engine-load-error.jsonl")
            for failure in ("missing_config", "unreadable_input", "wrong_digest"):
                with self.subTest(failure=failure):
                    if failure != "missing_config":
                        config.write_text(json.dumps({"manifest": str(manifest), "verifier": str(fixture_verifier(root))}))
                    if failure == "unreadable_input":
                        catalogue.unlink()
                    elif failure == "wrong_digest":
                        catalogue.write_bytes(b"tampered")
                    result = subprocess.run([wrapper, descriptor, SESSION], env=env, input="",
                                            capture_output=True, text=True, timeout=15)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(result.stderr.strip(), "serve-rql: invalid host transport configuration")
                    record = json.loads(diagnostic.read_text().splitlines()[-1])
                    self.assertEqual(record["session"], SESSION)
                    self.assertIn("Traceback", record["traceback"])
                    self.assertIn(str(config if failure == "missing_config" else catalogue), record["error"])
                    if failure == "wrong_digest":
                        self.assertIn(raw["inputs"]["catalogue_sha256"], record["error"])
                        self.assertIn(hashlib.sha256(b"tampered").hexdigest(), record["error"])
            self.assertEqual(diagnostic.stat().st_mode & 0o777, 0o600)

    # Hard-won: 49e16b8: a prompt change reused prior raw answers and fidelity measurements.
    def test_fidelity_checkpoints_follow_the_answer_cache_input_hash(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "engine.json"
            config.write_text(json.dumps({"manifest": str(self.fixture(root)), "verifier": str(fixture_verifier(root))}))
            initial = live_engine.load_engine(config)
            before = fidelity_live.checkpoint_directory(root / "fidelity", initial) / "test-query"
            before.mkdir(parents=True)
            for name in ("measurement.json", "engine.json", "fresh-engine.json"):
                (before / name).write_text('{"from":"old prompt"}')
            self.addCleanup(setattr, live_engine, "REPAIR", live_engine.REPAIR)
            live_engine.REPAIR += "\nChanged repair instructions."
            updated = live_engine.load_engine(config)
            after = fidelity_live.checkpoint_directory(root / "fidelity", updated) / "test-query"
            self.assertEqual(before.parents[1], after.parents[1])
            self.assertEqual(after.parent.name, updated.identity["engine_inputs_sha256"])
            self.assertNotEqual(before.parent.name, after.parent.name)
            self.assertFalse(after.exists())
            self.assertTrue((before / "measurement.json").is_file())
            binary = Path(json.loads(config.read_text())["verifier"])
            binary.write_bytes(binary.read_bytes() + b"# changed verifier\n")
            changed = live_engine.load_engine(config)
            self.assertNotEqual(updated.identity, changed.identity)
            self.assertNotEqual(updated.verifier_sha256, changed.verifier_sha256)


class ValidationTests(unittest.TestCase):
    def test_missing_path_wrong_line_and_non_verbatim_value(self):
        for bad_span, text, message in [
            (span("missing.rs"), "0x10", "does not exist"),
            (span(line=4), "0x10", "outside its 3 lines"),
            (span(line=1), "0x10", "not the exact source slice"),
            (span(), "0016", "not the exact source slice"),
        ]:
            with self.subTest(span=bad_span, text=text):
                rows = deepcopy(ROWS)
                rows[0]["sites"] = [bad_span]
                operand = rows[0]["fields"]["operand"]
                operand["site"] = bad_span
                operand["verbatim"][0]["text"] = text
                self.assertIn(message, "\n".join(validate(payload(rows), SOURCE.get)[2]))

    # Hard-won: b9ceb96: exact nested citations were rejected unless copied to the row's site list.
    def test_row_sites_include_nested_citations_for_location_only_delivery(self):
        rows = deepcopy(ROWS)
        enclosing_span = {**span(line=1), "end_line": 3}
        rows[0]["sites"] = [enclosing_span]
        rows[0]["fields"]["nested"] = [{"context": {"site": span(line=3)}}]
        rows[0]["fields"]["duplicate"] = deepcopy(rows[0]["fields"]["operand"])

        result, _, errors = validate(payload(rows), SOURCE.get)
        self.assertFalse(errors)
        self.assertEqual(result[0]["sites"], [enclosing_span, span(), span(line=3)])

        rows[0]["fields"]["operand"]["verbatim"][0]["text"] = "0016"
        self.assertIn("not the exact source slice",
                      "\n".join(validate(payload(rows), SOURCE.get)[2]))

    def test_synthesized_citations_are_independent_of_field_key_order(self):
        rows = deepcopy(ROWS)
        fields = rows[0]["fields"]
        fields.update(z=span(line=3), a=span(line=1), middle={**span(line=1), "end_line": 2})
        reordered = [{**rows[0], "fields": dict(reversed(list(fields.items())))}]
        first, _, errors = validate(payload(rows), SOURCE.get)
        self.assertFalse(errors)
        second, _, errors = validate(payload(reordered), SOURCE.get)
        self.assertFalse(errors)
        self.assertEqual(first[0]["sites"], [span(), span(line=1), fields["middle"], span(line=3)])
        self.assertEqual(canonical(first), canonical(second))

    def test_open_coverage_and_false_complete(self):
        rows = deepcopy(ROWS)
        rows[0]["fields"]["proof"] = "open"
        rows[0]["fields"]["boundary"] = "unresolved dispatch"
        errors = validate(payload(rows), SOURCE.get)[2]
        self.assertEqual(sum("contradicts" in error for error in errors), 2)
        open_cover = {"complete": False, "open": [{"reason": "unresolved dispatch", "site": span()}]}
        self.assertEqual(validate(payload([], open_cover), SOURCE.get), ([], open_cover, []))
        self.assertTrue(validate(payload([], {"complete": True, "open": open_cover["open"]}),
                                 SOURCE.get)[2])

    # Hard-won: a19cfbf: a query depth boundary invalidated a complete answer.
    def test_complete_coverage_allows_depth_boundary_but_rejects_unresolved_dispatch(self):
        rows = deepcopy(ROWS)
        rows[0]["fields"].update(depth_boundary=True, endpoint={"boundary": "depth-boundary"})
        self.assertEqual(validate(payload(rows), SOURCE.get), (rows, closed(), []))
        rows[0]["fields"]["endpoint"]["boundary"] = "unresolved dispatch"
        self.assertIn("an open frontier contradicts coverage.complete",
                      "\n".join(validate(payload(rows), SOURCE.get)[2]))

    def test_coverage_rejects_bifrost_execution_limit_outcomes(self):
        for reason in ("traversal budget", "over_budget", "request cancelled", "exhaustion"):
            with self.subTest(reason=reason):
                coverage = {"complete": False, "open": [{"reason": reason, "site": span()}]}
                self.assertIn("only semantic boundaries", " ".join(validate(payload([], coverage), SOURCE.get)[2]))

    def test_source_text_is_not_a_coverage_diagnostic(self):
        rows = deepcopy(ROWS)
        operand = rows[0]["fields"]["operand"]
        operand["verbatim"] = [{"start_byte": 0, "end_byte": 13, "text": "open frontier"}]
        self.assertFalse(validate(payload(rows), {"source.rs": b"\nopen frontier\n"}.get)[2])

    def test_multiline_byte_slices_use_utf8_and_keep_internal_empty_lines(self):
        at = {"path": "source.rs", "start_line": 1, "end_line": 3}
        rows = [{"kind": "structural_match", "sites": [at], "fields": {"operand": {
            "site": at, "verbatim": [
                {"start_byte": 3, "end_byte": 5, "text": "α"},
                {"start_byte": 0, "end_byte": 0, "text": ""},
                {"start_byte": 0, "end_byte": 5, "text": "omega"},
            ]}}}]
        self.assertFalse(validate(payload(rows), {"source.rs": "   α\n\nomega".encode()}.get)[2])
        rows[0]["fields"]["operand"]["verbatim"][0]["end_byte"] = 4
        self.assertTrue(validate(payload(rows), {"source.rs": "   α\n\nomega".encode()}.get)[2])


class FakeRunner:
    instance = INSTANCE

    def __init__(self, outputs):
        self.outputs = iter(outputs)
        self.launches = []
        self.preparations = []
        self.turns = []

    def start(self, snapshot, commit):
        self.launches.append((snapshot, commit))
        return f"{len(self.launches):032x}"

    def prepare(self, sid, snapshot, directory):
        self.preparations.append((sid, snapshot))

    def retire(self, directory, record, *, require_clean_audit=False):
        (directory / "destroy-receipt.json").write_text("{}")

    def turn(self, sid, query, catalogue, menu, errors, directory):
        self.turns.append((sid, query, errors))
        record = {"session": sid, "turn": len(self.turns), "tokens": {"total_tokens": 15},
                  "context_id": f"query-context-{len(self.turns)}"}
        return next(self.outputs), record, []


class EngineTests(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.cache = Path(temp.name)

    def engine(self, runner, catalogue=CATALOGUE, menu=MENU, model=VERSION):
        def source(commit):
            self.assertEqual(commit, COMMIT)
            return SOURCE.get, {"tree": "frozen-tree", "orphan": "root"}

        return Engine(self.cache, (catalogue, menu), source, lambda: runner, model)

    # Hard-won: a19cfbf: an engine launch could ignore either Fable weekly quota window.
    def test_sonnet_launch_requires_overall_weekly_and_five_hour_windows(self):
        runner = FakeRunner([])
        runner.driver = driver
        runner.config = {"profile": "claude-fixture", "bundle": "test-bundle"}
        runner.start = MethodType(SealedRunner.start, runner)
        runner.check_quota = MethodType(SealedRunner.check_quota, runner)

        def launch(*args, **kwargs):
            runner.launches.append((args, kwargs))
            return SESSION

        runner.launch = launch
        windows = []

        def quota(operation, flag, profile):
            self.assertEqual((operation, flag, profile), ("quota", "--profile", "claude-fixture"))
            return {"profiles": [{"report": {"windows": windows}}]}

        self.addCleanup(setattr, driver, "mj", driver.mj)
        driver.mj = quota
        engine = self.engine(runner)
        record = self.cache / "session.json"
        for weekly, short in ((24, 80), (25, 80), (80, 19), (80, 20), (None, 80), (80, None), (80, 80)):
            with self.subTest(weekly=weekly, short=short):
                windows[:] = [{"remaining_percent": percent, "duration_seconds": seconds}
                              for percent, seconds in ((weekly, 604800), (short, 18_000)) if percent is not None]
                windows.append({"remaining_percent": 0, "duration_seconds": 604800, "model_scope": "opus"})
                if (weekly, short) == (80, 80):
                    self.assertEqual(engine.session(COMMIT, {"orphan": COMMIT}, record), (runner, SESSION))
                else:
                    with self.assertRaises(LaunchHeld):
                        engine.session(COMMIT, {"orphan": COMMIT}, record)
                    self.assertFalse(record.exists())
                    self.assertFalse(runner.launches)
        self.assertEqual(len(runner.launches), 1)

    def test_cache_normalization_and_changed_input(self):
        runner = FakeRunner([payload()] * 4)
        initial = self.engine(runner).answer(COMMIT, QUERY)
        self.assertEqual(initial["status"], "ok", initial)
        cached = self.engine(runner).answer(COMMIT, ' ( function  :name "evaluate" ) ; comment')
        self.assertEqual(cached, initial)
        changes = [
            {"catalogue": CATALOGUE + "\nCatalogue revision."},
            {"menu": MENU + "\nSemantics revision."},
            {"model": "claude-sonnet-5-4"},
        ]
        for change in changes:
            changed = self.engine(runner, **change).answer(COMMIT, QUERY)
            self.assertEqual(changed["status"], "ok", changed)
            self.assertNotEqual(changed["provenance"]["inputs_sha256"], initial["provenance"]["inputs_sha256"])
        self.assertEqual((len(runner.launches), len(runner.preparations), len(runner.turns)), (4, 4, 4))

    def test_stability_reasks_have_separate_contexts_and_never_change_writer_cache(self):
        runner = FakeRunner([payload(), payload([]), payload([])])
        engine = self.engine(runner)
        cached = engine.answer(COMMIT, QUERY)
        self.addCleanup(setattr, live_engine, "load_engine", live_engine.load_engine)
        live_engine.load_engine = lambda: engine
        fresh = [live_engine.answer(COMMIT, QUERY, fresh=True) for _ in range(2)]
        self.assertTrue(all(answer["status"] == "ok" for answer in [cached, *fresh]))
        self.assertFalse(cached["provenance"]["fresh"])
        self.assertTrue(all(answer["provenance"]["fresh"] for answer in fresh))
        identifiers = {answer["provenance"]["context_id"] for answer in [cached, *fresh]}
        self.assertEqual(len(identifiers), 3)
        self.assertEqual(len(runner.launches), 3)
        self.assertEqual(len({answer["provenance"]["session"] for answer in [cached, *fresh]}), 3)
        self.assertEqual(engine.answer(COMMIT, QUERY), cached)
        paths = sorted((self.cache / COMMIT / "stability").glob("*/*.json"))
        self.assertEqual({path.stem for path in paths}, identifiers - {cached["provenance"]["context_id"]})
        self.assertEqual([json.loads(path.read_text()) for path in paths], fresh)

    def test_isolation_failure_moves_evidence_and_uses_a_new_session(self):
        class ViolatingRunner(FakeRunner):
            def start(self, snapshot, commit):
                super().start(snapshot, commit)
                return SESSION if len(self.launches) == 1 else REPLACEMENT

            def retire(self, directory, record, *, require_clean_audit=False):
                if record["session_id"] == SESSION:
                    self.retired = SESSION
                super().retire(directory, record, require_clean_audit=require_clean_audit)

            def turn(self, sid, query, catalogue, menu, errors, directory):
                if sid == SESSION:
                    (directory / "violation.json").write_text("evidence")
                    record = {"session": sid, "turn": 1, "context_id": "rejected-context",
                              "tokens": {"total_tokens": 10}}
                    raise IsolationFailure("violated policy", record)
                return super().turn(sid, query, catalogue, menu, errors, directory)

        runner = ViolatingRunner([payload()])
        result = self.engine(runner).answer(COMMIT, QUERY)
        self.assertEqual(result["status"], "ok", result)
        self.assertEqual(result["provenance"]["session"], REPLACEMENT)
        self.assertEqual(runner.retired, SESSION)
        self.assertEqual(result["provenance"]["retired_sessions"][0]["session"], SESSION)
        retired = self.cache / COMMIT / "retired" / SESSION
        self.assertEqual((retired / "violation.json").read_text(), "evidence")
        self.assertEqual(json.loads((retired / "session.json").read_text())["session_id"], SESSION)

    def test_uncertain_destroy_is_quarantined_without_blocking_the_next_query(self):
        class LostDestroy(FakeRunner):
            def start(self, snapshot, commit):
                super().start(snapshot, commit)
                return SESSION if len(self.launches) == 1 else REPLACEMENT

            def retire(self, directory, record, *, require_clean_audit=False):
                if record["session_id"] == SESSION:
                    self.capture = SealedRunner.__new__(SealedRunner)
                    self.capture.config = {"instance": INSTANCE, "model_version": VERSION}
                    self.capture.driver = CaptureDriver(directory, "destroy", clean=True)
                    self.capture.retire(directory, record, require_clean_audit=require_clean_audit)
                else:
                    super().retire(directory, record, require_clean_audit=require_clean_audit)

        runner = LostDestroy([payload(), payload()])
        engine = Engine(self.cache, (CATALOGUE, MENU), lambda _: (SOURCE.get, {"orphan": COMMIT}),
                        lambda: runner, VERSION)
        first = engine.answer(COMMIT, QUERY)
        self.assertEqual(first["status"], "ok", first)
        sid = first["provenance"]["session"]
        quarantined = self.cache / COMMIT / "retired" / sid
        self.assertTrue((quarantined / "destroy-request.json").is_file())
        self.assertFalse((quarantined / "destroy-receipt.json").exists())
        failure = json.loads((quarantined / "retirement-failure.json").read_text())
        self.assertEqual(failure["session_id"], sid)
        self.assertIn("destroy was not confirmed", failure["reason"])
        self.assertTrue((quarantined / "capture.json").is_file())
        second = engine.answer(COMMIT, '(function :name "other")')
        self.assertEqual(second["status"], "ok", second)
        self.assertNotEqual(second["provenance"]["session"], sid)
        self.assertEqual(engine.answer(COMMIT, QUERY), first)
        self.assertEqual(len(runner.launches), 2)
        count, failures = teardown(self.cache, runner.capture)
        self.assertEqual(count, 0)
        self.assertIn("is uncertain", failures[0])
        self.assertEqual(len(runner.capture.driver.argv), 1)

    def test_retry_then_error_never_caches_unvalidated_rows(self):
        runner = FakeRunner(["{\"", payload([{"kind": "broken"}]), payload()])
        result = self.engine(runner).answer(COMMIT, QUERY)
        self.assertEqual(result["status"], "engine_error")
        self.assertIn("expected kind, sites and fields", result["message"])
        self.assertIn("invalid JSON", runner.turns[1][2][0])
        self.assertEqual(result["provenance"]["validation_retries"], 1)
        self.assertEqual(result["provenance"]["tokens"]["total_tokens"], 30)
        self.assertEqual(result["rows"], [])
        result = self.engine(runner).answer(COMMIT, QUERY)
        self.assertEqual(result["status"], "ok")
        self.assertEqual(len(runner.turns), 3)

    def test_retry_then_success_sorts_and_retains_open_coverage(self):
        first = {"kind": "structural_match", "sites": [span(line=1)], "fields": {}}
        coverage = {"complete": False, "open": [{"reason": "unresolved dispatch", "site": span(line=1)}]}
        runner = FakeRunner(["{}", payload(ROWS + [first], coverage)])
        result = self.engine(runner).answer(COMMIT, QUERY)
        self.assertEqual(result["status"], "ok", result)
        self.assertEqual(result["coverage"], coverage)
        self.assertEqual(result["rows"], [first, ROWS[0]])
        self.assertEqual(result["provenance"]["validation_retries"], 1)
        self.assertEqual(len(runner.turns), 2)
        self.assertEqual(len(runner.launches), 1)
        self.assertEqual(runner.turns[0][0], runner.turns[1][0])

    @staticmethod
    def verified_query(cache, round_errors):
        repo = cache / "repo"
        repo.mkdir(parents=True)
        git_output(repo, "init", "--template=")
        (repo / "source.rs").write_bytes(SOURCE["source.rs"])
        git_output(repo, "add", "source.rs")
        git_output(repo, "-c", "user.name=Test", "-c", "user.email=test@localhost",
                   "-c", "commit.gpgsign=false", "commit", "-m", "fixture")
        orphan = git_output(repo, "rev-parse", "HEAD").decode().strip()
        (repo / "source.rs").write_text("dirty work tree")
        binary = fixture_verifier(cache, f"""
import json, sys
from pathlib import Path

request = json.load(sys.stdin)
root = Path(request["root"])
assert (root / "source.rs").read_bytes() == {SOURCE["source.rs"]!r}
assert not (root / ".git").exists()
assert request["step"] == "contract-cases"
assert isinstance(request["query"], list)
assert request["answer"]["excluded"] == {list(EXCLUDED_FIXTURE)!r}
cursor = Path(__file__).with_suffix(".round")
index = int(cursor.read_text()) if cursor.exists() else 0
cursor.write_text(str(index + 1))
errors = {round_errors!r}[index]
print(json.dumps(dict(version=1, candidates=[], errors=errors, unchecked=[])))
""")
        runner = FakeRunner([payload(excluded=EXCLUDED_FIXTURE)] * len(round_errors))
        engine = Engine(cache, (CATALOGUE, MENU), lambda _: (SOURCE.get, {"orphan": orphan}),
                        lambda: runner, VERSION, verifier=binary,
                        verifier_source=lambda snapshot: verifier_snapshot(repo, snapshot["orphan"]))
        query = '(contract-cases :depth 0 :kind "return" (procedure-of (function :name "evaluate")))'
        return engine.answer(COMMIT, query), runner

    def test_verifier_errors_drive_repairs_while_the_error_set_shrinks(self):
        findings = [{"code": code, "message": code, "row": 0, "site": span(), "candidate": None}
                    for code in ("outside-seed", "unaccounted-candidate")]
        answer, runner = self.verified_query(self.cache, [findings, findings[1:], []])
        self.assertEqual(answer["status"], "ok", answer)
        self.assertEqual(answer["excluded"], list(EXCLUDED_FIXTURE))
        self.assertEqual(answer["provenance"]["validation_retries"], 2)
        self.assertIn("outside-seed", runner.turns[1][2][0])
        self.assertIn("unaccounted-candidate", runner.turns[2][2][0])
        self.assertEqual(len(runner.launches), 1)
        self.assertEqual(len(list(self.cache.glob("*/retired/*/answers/*/verifier.json"))), 3)

    def test_verifier_repair_stops_when_normalized_error_set_does_not_shrink(self):
        issue = {"code": "anchor-kind", "message": "first diagnostic", "row": 0,
                 "site": span(), "candidate": None}
        answer, runner = self.verified_query(self.cache, [[issue], [{**issue, "message": "reworded diagnostic"}]])
        self.assertEqual(answer["status"], "engine_error")
        self.assertIn("reworded diagnostic", answer["message"])
        self.assertEqual(len(runner.turns), 2)
        self.assertFalse(list((self.cache / COMMIT).glob("*.json")))

    def test_parse_error_never_constructs_engine_session(self):
        runner = FakeRunner([])
        result = self.engine(runner).answer(COMMIT, "(what changed?)")
        self.assertEqual(result["status"], "parse_error")
        self.assertFalse(runner.launches)


class FakeAcpRunner(SealedRunner):
    """Tests the production prompt sequence against an in-memory mj adapter."""

    instance = INSTANCE

    def __init__(self):
        self.driver = self
        self.config = {"profile": "claude-test", "model_version": VERSION, "model_display": "Sonnet 5.5",
                       "destination": "repo"}
        self.manifest = {"diffs": []}
        self.served_steps = ("contract-cases", "value-transfers", "guarded-paths")
        self.messages = []
        self.turns = []
        self.artifact = None
        self.launches = []

    def start(self, snapshot, commit):
        self.launches.append(snapshot)
        return f"{len(self.launches):032x}"

    def prepare(self, sid, snapshot, directory):
        pass

    def retire(self, directory, record, *, require_clean_audit=False):
        (directory / "destroy-receipt.json").write_text("{}")

    def pilot_claude_hold(self, profile):
        assert profile == "claude-test"
        return getattr(self, "quota_hold", None)

    def wait_idle(self, sid):
        return {"profile_id": "claude-test",
                "config_options": [{"key": "model", "current": "sonnet",
                                    "choices": [{"value": "sonnet", "name": "Claude Sonnet 5.5"}]},
                                   {"key": "effort", "current": "high"}]}

    def clean_checkout(self, sid, orphan="", *, clear=True):
        return f".rql-engine-{sid}"

    def audit_turn(self, sid, turn, directory):
        return True

    def command(self, operation, *args, data=None):
        if operation == "put-file":
            session_flag, sid, path_flag, path, source = args
            assert (session_flag, path_flag, source) == ("--session", "--path", "-"), args
            assert path.startswith(f".rql-engine-{sid}/"), args
            self.artifact = data
            return b""
        if operation == "fetch":
            return payload().encode()
        assert operation == "message"
        command = args[args.index("--command-id") + 1]
        sid = args[args.index("--session") + 1]
        text = data.decode()
        self.messages.append((command, text))
        return json.dumps({"session_id": sid, "via": "turn", "turn_id": len(self.messages)}).encode()

    def mj(self, operation, *args, **kwargs):
        if operation == "wait":
            command, text = self.messages[-1]
            if refusal := getattr(self, "turn_refusal", None):
                return {"turn_id": len(self.messages), **refusal}
            response = "Answer written"
            turn = len(self.messages)
            self.turns.append({"command_id": command, "accepted_ordinal": turn,
                               "usage": {"provider_details": {"model_usage": {VERSION: {}}}}})
            return {"turn_id": turn, "outcome": "finished", "final_message": response}
        assert operation == "usage"
        cursor = int(args[args.index("--after-seq") + 1]) if "--after-seq" in args else 0
        turns = [row for row in self.turns if row["accepted_ordinal"] > cursor]
        return {"latest_seq": self.turns[-1]["accepted_ordinal"] if self.turns else 0, "turns": turns,
                "totals": {"total_tokens": {"tokens": len(self.turns) * 15, "reported_turns": len(self.turns)}},
                "turn_selections": {row["command_id"]: {"model": "sonnet", "effort": "high"}
                                    for row in turns}}


class SessionTests(unittest.TestCase):
    # Hard-won: dfb55808: the wait exit code hid the structured quota refusal.
    def test_nonzero_wait_with_provider_quota_outcome_is_a_quota_hold(self):
        runner = FakeAcpRunner()
        with TemporaryDirectory() as directory:
            engine = Engine(Path(directory), (CATALOGUE, MENU),
                            lambda commit: (SOURCE.get, {"tree": "frozen", "orphan": COMMIT}),
                            lambda: runner, VERSION)
            self.assertEqual(engine.answer(COMMIT, QUERY)["status"], "ok")
            fake_mj = runner.mj

            def failed_wait(argv, **kwargs):
                self.assertEqual(argv[-1], "--json")
                turn = int(argv[argv.index("--turn") + 1])
                response = {"turn_id": turn, "outcome": "quota_limit", "stop_reason": "QuotaLimit"}
                return subprocess.CompletedProcess(argv, 1, json.dumps(response).encode(), b"turn ended as quota_limit")

            def observed_mj(operation, *args, **kwargs):
                if operation == "wait":
                    return driver.mj(operation, *args, **kwargs)
                return fake_mj(operation, *args, **kwargs)

            runner.mj = observed_mj
            original_run = driver.subprocess.run
            try:
                driver.subprocess.run = failed_wait
                answer = engine.answer(COMMIT, '(function :name "other")')
            finally:
                driver.subprocess.run = original_run
            self.assertEqual(answer["status"], "quota_hold")
            self.assertEqual(answer["provenance"]["turns"][-1]["stop_reason"], "QuotaLimit")

    def test_checkout_mutation_voids_the_turn_before_validation_and_cache(self):
        class CheckoutRunner(FakeAcpRunner):
            def clean_checkout(self, sid, orphan="", *, clear=True):
                scratch = super().clean_checkout(sid, orphan, clear=clear)
                check = subprocess.run([sys.executable, "-c", CHECKOUT_CLEAN, scratch,
                                        orphan, "clear" if clear else "check"],
                                       cwd=checkout, capture_output=True, text=True)
                if check.returncode:
                    raise RuntimeError(check.stderr.strip())
                return scratch

            def command(self, operation, *args, data=None):
                if operation == "message" and len(self.messages) == 0:
                    tracked.write_text("mutated source\n")
                    (checkout / "unauthorized").write_text("new content\n")
                return super().command(operation, *args, data=data)

            def retire(self, directory, record, *, require_clean_audit=False):
                super().retire(directory, record, require_clean_audit=require_clean_audit)
                if (directory / "void.json").is_file():
                    git_output(checkout, "checkout", "--", tracked.name)
                    (checkout / "unauthorized").unlink()

        with TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / "checkout"
            checkout.mkdir()
            tracked = checkout / "source.rs"
            tracked.write_bytes(SOURCE["source.rs"])
            git_output(checkout, "init", "--template=")
            git_output(checkout, "add", tracked.name)
            git_output(checkout, "-c", "user.name=Test", "-c", "user.email=test@localhost",
                       "-c", "commit.gpgsign=false", "commit", "-m", "source")
            orphan = git_output(checkout, "rev-parse", "HEAD").decode().strip()
            runner = CheckoutRunner()
            cache = root / "cache"
            engine = Engine(cache, (CATALOGUE, MENU), lambda _: (SOURCE.get, {"orphan": orphan}),
                            lambda: runner, VERSION)
            answer = engine.answer(COMMIT, QUERY)
            self.assertEqual(answer["status"], "ok", answer)
            self.assertEqual(len(runner.launches), 2)
            self.assertEqual(len(runner.messages), 2)
            first = answer["provenance"]["retired_sessions"][0]
            rejected = cache / COMMIT / "retired" / first["session"]
            self.assertTrue((rejected / "void.json").is_file())
            self.assertIn("unexpected changes", first["reason"])
            self.assertFalse(list(rejected.glob("answers/*/validation.json")))
            self.assertNotEqual(answer["provenance"]["session"], first["session"])
            self.assertEqual(engine.answer(COMMIT, QUERY), answer)
            self.assertEqual(len(runner.launches), 2)

    def test_checkout_cleaner_drops_old_answer_without_erasing_source(self):
        with TemporaryDirectory() as directory:
            path = Path(directory)
            git_output(path, "init", "--template=")
            tracked = path / "source.rs"
            tracked.write_text("fn example() {}\n")
            git_output(path, "add", "source.rs")
            git_output(path, "-c", "user.name=Test", "-c", "user.email=test@localhost",
                       "-c", "commit.gpgsign=false", "commit", "-m", "source")
            scratch = path / ".fixture"
            scratch.mkdir()
            (scratch / "answer.json").write_text("previous turn")
            command = [sys.executable, "-c", CHECKOUT_CLEAN, scratch.name, "", "clear"]
            subprocess.run(command, cwd=path, check=True, capture_output=True)
            self.assertFalse(scratch.exists())
            bad = subprocess.run([*command[:3], tracked.name, "", "clear"], cwd=path, capture_output=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertTrue(tracked.is_file())
            check = [*command[:-1], "check"]
            tracked.unlink()
            self.assertNotEqual(subprocess.run(check, cwd=path, capture_output=True).returncode, 0)
            git_output(path, "checkout", "--", tracked.name)
            (path / "empty").mkdir()
            self.assertNotEqual(subprocess.run(check, cwd=path, capture_output=True).returncode, 0)

    def test_writer_stdio_rejects_fresh_and_cannot_select_a_host_reask(self):
        with TemporaryDirectory() as directory:
            temp = Path(directory)
            sid = "a" * 32
            diff = "repo/test"
            manifest = temp / "manifest.json"
            manifest.write_text(json.dumps({"page_rows": 50, "diffs": [{"key": diff, "intro": COMMIT}]}))
            (temp / "transport_fixture.py").write_text("""
from types import SimpleNamespace
from rql_mock.stub_engine import STUB_IDENTITY

def load_engine():
    return SimpleNamespace(identity=STUB_IDENTITY, answer=answer)

def answer(commit, query, *, fresh=False):
    return {"status": "ok", "rows": [], "total": 0,
            "excluded": [{"reason": "duplicate"}], "coverage": {"complete": True, "open": []},
            "provenance": {"fresh": fresh, "commit": commit}}
""")
            config, log = temp / "transport.json", temp / "queries.jsonl"
            from rql_mock.stub_engine import STUB_IDENTITY
            config.write_text(json.dumps({
                "manifest": str(manifest), "engine": "transport_fixture:load_engine",
                "sessions": {sid: {"diff": diff, "intro": COMMIT, "arm": "R", "log": str(log), **STUB_IDENTITY}},
            }))
            def request(request_id, arguments):
                return {"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                        "params": {"name": "rql_query", "arguments": arguments}}

            requests = [{"jsonrpc": "2.0", "id": 1, "method": "initialize",
                         "params": {"protocolVersion": "2025-11-25"}},
                        {"jsonrpc": "2.0", "method": "notifications/initialized"},
                        request(2, {"query": QUERY, "fresh": True}),
                        request(3, {"query": QUERY, "intro": NONPILOT}),
                        request(4, {"query": QUERY})]
            env = {**os.environ, "SSH_ORIGINAL_COMMAND": sid,
                   "PYTHONPATH": os.pathsep.join((str(temp), os.environ.get("PYTHONPATH", "")))}
            wrapper = Path(__file__).resolve().parents[1] / "serve-rql.sh"
            proc = subprocess.run([wrapper, config, sid], check=True, text=True, capture_output=True,
                                  input="\n".join(map(json.dumps, requests)) + "\n", env=env)
            responses = [json.loads(line) for line in proc.stdout.splitlines()]
            for response in responses[-3:-1]:
                bad = json.loads(response["result"]["content"][0]["text"])
                self.assertEqual(bad["status"], "invalid_arguments")
            records = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertIsNone(records[0]["provenance"])
            self.assertIsNone(records[1]["provenance"])
            self.assertEqual(records[2]["provenance"], {"fresh": False, "commit": COMMIT})
            delivered = json.loads(responses[-1]["result"]["content"][0]["text"])
            self.assertEqual(delivered["status"], "ok")
            self.assertNotIn("excluded", delivered)


class CaptureDriver:
    def __init__(self, directory, fail=None, *, clean=False, models=(VERSION,)):
        self.directory = directory
        self.fail = fail
        self.clean = clean
        self.models = models
        self.argv = []
        self.usage_cursors = []

    def wait_idle(self, sid):
        assert sid == SESSION
        return {"id": SESSION if self.fail != "identity" else REPLACEMENT,
                "at": COMMIT, "base": COMMIT, "no_later_history": True}

    def transcript(self, sid):
        assert sid == SESSION
        if self.fail == "transcript":
            raise RuntimeError("transcript unavailable")
        command = "rg evaluate source.rs" if self.clean else "curl https://example.invalid"
        items = [{"seq": 20, "role": "tool", "body": {"call": {"rawInput": command}}}]
        return [{"session_id": sid, "next_after_seq": 20, "latest_seq": 20, "items": items}]

    def mj(self, operation, *args):
        assert operation == "usage" and args[:3] == ("--session", SESSION, "--after-seq")
        if self.fail == "usage":
            raise RuntimeError("usage unavailable")
        cursor = args[3]
        self.usage_cursors.append(cursor)
        next_seq = 11 if cursor == 0 else 20
        return {"session_id": SESSION, "next_after_seq": next_seq, "latest_seq": 20,
                "turns": [{"usage": {"provider_details": {"model_usage": dict.fromkeys(self.models, {})}}}]}

    def mj_invocation(self, *args):
        if args[0] == "destroy":
            # Capture and the actual transcript audit must already be durable.
            index = json.loads((self.directory / "capture.json").read_text())
            assert index["session"] == SESSION and index["audit_passed"] is self.clean
            assert (self.directory / "destroy-request.json").is_file()
        argv = ("--instance", INSTANCE, *args)
        self.argv.append(argv)
        if args[0] == "fetch":
            assert args[:3] == ("fetch", "--session", SESSION)
            if self.fail == "fetch":
                program = "raise SystemExit(1)"
            else:
                assert args[3:] == ("--path", f".rql-engine-{SESSION}/answer.json", "--stdout")
                program = "print('late answer')"
        else:
            acknowledgement = json.dumps({"session_id": SESSION, "accepted": True})
            program = "raise SystemExit(1)" if self.fail == "destroy" else f"print({acknowledgement!r})"
        return [sys.executable, "-c", program, *argv]



class RetirementTests(unittest.TestCase):
    def test_teardown_cli_uses_the_cache_beside_its_explicit_config(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            default, selected = root / "default", root / "selected"
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"model_versions": {"claude_provider_id": VERSION}}))
            for instance_root, commit in ((default, NONPILOT), (selected, COMMIT)):
                active = instance_root / "engine-cache" / commit / "active"
                active.mkdir(parents=True)
                # An unconfirmed launch elicits a diagnostic without calling mj.
                (active / "session.json").write_text(json.dumps({"commit": commit}))
                (instance_root / "engine.json").write_text(json.dumps(
                    {"instance": INSTANCE, "manifest": str(manifest)}))
            result = subprocess.run(
                [sys.executable, "-B", "-m", "rql_mock.engine", "teardown",
                 "--config", str(selected / "engine.json")],
                env={**os.environ, "REVIEW_ARMS_ENGINE_ROOT": str(default)},
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn(f"{COMMIT}: cannot retire an unconfirmed engine launch", result.stderr)
            self.assertNotIn(NONPILOT, result.stderr)

    def fixture(self, cache, fail=None, *, pending=False):
        active = cache / COMMIT / "active"
        answers = active / "answers" / "context-test"
        answers.mkdir(parents=True)
        record = {"commit": COMMIT, "orphan": COMMIT, "session_id": SESSION,
                  "instance": INSTANCE, "model_version": VERSION}
        (active / "session.json").write_text(json.dumps(record))
        (answers / "pending.json").write_text(json.dumps({"path": f".rql-engine-{SESSION}/answer.json"}))
        if not pending:
            (answers / "answer.raw").write_bytes(b"previous answer")
        runner = SealedRunner.__new__(SealedRunner)
        runner.config = {"instance": INSTANCE, "model_version": VERSION}
        retired = cache / COMMIT / "retired" / SESSION
        runner.driver = CaptureDriver(retired, fail)
        return runner, retired

    def test_teardown_hashes_full_evidence_audits_stored_transcript_and_destroys_only_recorded_id(self):
        with TemporaryDirectory() as directory:
            cache = Path(directory)
            runner, retired = self.fixture(cache, pending=True)
            self.assertEqual(teardown(cache, runner), (1, []))
            self.assertEqual(runner.driver.usage_cursors, [0, 11])
            self.assertEqual(runner.driver.argv[-1],
                             ("--instance", INSTANCE, "destroy", "--session", SESSION, "--json"))
            forbidden = "--acknowledge-unpublished-work"
            self.assertTrue(all(forbidden not in argv for argv in runner.driver.argv))
            with self.assertRaisesRegex(ValueError, "cannot acknowledge"):
                runner.command("suspend", "--session", SESSION, forbidden)
            index = json.loads((retired / "capture.json").read_text())
            hashes = index["files_sha256"]
            self.assertTrue({"transcript.json", "usage.json", "audit.json",
                             "answers/context-test/answer.raw"} <= set(hashes))
            for name, sha256 in hashes.items():
                self.assertEqual(sha256, hashlib.sha256((retired / name).read_bytes()).hexdigest())
            self.assertEqual((retired / "answers/context-test/answer.raw").read_bytes(), b"late answer\n")
            usage = json.loads((retired / "usage.json").read_text())
            self.assertEqual(usage["reported_provider_models"], [VERSION])
            self.assertIn("github-or-web", {hit["kind"] for hit in json.loads((retired / "audit.json").read_text())["hits"]})
            self.assertEqual(teardown(cache, runner), (0, []))

    def test_capture_or_identity_failure_preserves_session_and_reports_failure(self):
        for failure in ("transcript", "usage", "identity", "fetch"):
            with self.subTest(failure=failure), TemporaryDirectory() as directory:
                runner, retired = self.fixture(Path(directory), failure, pending=failure == "fetch")
                count, failures = teardown(Path(directory), runner)
                self.assertEqual(count, 0)
                self.assertIn("session preserved", failures[0])
                self.assertFalse((retired / "destroy-request.json").exists())
                self.assertFalse(any("destroy" in argv for argv in runner.driver.argv))
                self.assertTrue((retired / "answers/context-test/pending.json").exists())
                self.assertEqual((retired / "answers/context-test/answer.raw").exists(), failure != "fetch")

    # Hard-won: a19cfbf: fidelity cleanup masked turn failures and never retired completed sessions.
    def test_fidelity_retains_failed_turn_and_retires_success_only_once(self):
        def replace(target, name, replacement):
            self.addCleanup(setattr, target, name, getattr(target, name))
            setattr(target, name, replacement)

        snapshot = {"orphan": COMMIT, "tree": "frozen-tree"}
        replace(fidelity_live, "prepared_snapshot", lambda *args: snapshot)
        replace(fidelity_live, "blob_reader", lambda *args: SOURCE.get)
        replace(driver, "pilot_weekly_hold", lambda *args, **kwargs: None)
        launches = []

        def launch(*args, **kwargs):
            launches.append((args, kwargs))
            return SESSION

        replace(fidelity_live.brief_driver, "sealed_launch", launch)

        def make_runner(config, manifest):
            runner = SealedRunner.__new__(SealedRunner)
            runner.config = config
            runner.instance = config["instance"]
            runner.driver = capture
            runner.clean_checkout = lambda sid: None
            return runner

        replace(fidelity_live, "SealedRunner", make_runner)
        adapter = fidelity_live.LiveModels.__new__(fidelity_live.LiveModels)
        adapter.config = {"instance": INSTANCE, "snapshot_repo": "snapshot", "bundle": "fixture"}
        adapter.manifest = {}
        adapter.sources = {COMMIT: "source"}
        adapter.claude_version = VERSION
        failure = RuntimeError("original fidelity turn error")
        model = fidelity_live.TERRA
        result = {"session": SESSION, "turn": 1, "model": model, "version": model, "effort": "high"}

        def run_turn(*args):
            if capture.fail == "turn":
                raise failure
            return payload().encode(), result

        def checked(output, read):
            rows, _, errors = validate(output, read)
            self.assertFalse(errors)
            return rows

        adapter.run_turn = run_turn
        for scenario in ("turn", "transcript", "success"):
            with self.subTest(scenario=scenario), TemporaryDirectory() as root:
                directory = Path(root)
                capture = CaptureDriver(directory, scenario, clean=True, models=())
                launches.clear()

                def sealed():
                    return adapter.sealed_task(COMMIT, directory, model, "terra", {}, lambda _: QUERY, checked)

                if scenario == "turn":
                    with self.assertRaises(RuntimeError) as error:
                        sealed()
                    self.assertIs(error.exception, failure)
                    self.assertEqual(json.loads((directory / "failed.json").read_text()),
                                     {"session_id": SESSION, "reason": str(failure)})
                    self.assertFalse((directory / "completed.json").exists())
                    self.assertEqual(capture.argv, [])
                else:
                    if scenario == "transcript":
                        with self.assertRaisesRegex(RuntimeError, "transcript unavailable"):
                            sealed()
                        self.assertTrue((directory / "completed.json").exists())
                        self.assertFalse((directory / "destroy-request.json").exists())
                        self.assertEqual(capture.argv, [])
                        capture.fail = None
                    self.assertEqual(sealed(), (ROWS, result))
                    self.assertEqual(sealed(), (ROWS, result))
                    self.assertEqual(len(launches), 1)
                    self.assertEqual(capture.argv, [("--instance", INSTANCE, "destroy", "--session", SESSION, "--json")])
                    self.assertTrue((directory / "destroy-receipt.json").is_file())
                    usage = json.loads((directory / "usage.json").read_text())
                    self.assertEqual(usage["expected_provider_model"], model)


class SnapshotTests(unittest.TestCase):
    def test_git_snapshot_objects_exclude_real_history_and_audit_rejects_it(self):
        with TemporaryDirectory() as temp:
            source, destination = (Path(temp) / name for name in ("source", "engine-trees.git"))
            source.mkdir()
            destination.mkdir()
            git_output(source, "init", "--template=")
            git_output(destination, "init", "--bare", "--template=")
            git_output(source, "config", "user.name", "Test")
            git_output(source, "config", "user.email", "test@localhost")
            git_output(source, "config", "commit.gpgsign", "false")
            (source / "nested").mkdir()
            path = source / "nested/source.rs"
            path.write_bytes(b"older\n")
            git_output(source, "add", "nested/source.rs")
            git_output(source, "commit", "-m", "older")
            path.write_bytes(SOURCE["source.rs"])
            git_output(source, "commit", "-am", "introducing")
            commit = git_output(source, "rev-parse", "HEAD").strip().decode()
            info = parentless_snapshot(source, destination, commit)
            self.assertEqual(audit(destination, {commit: source}), {commit: info})
            self.assertEqual(blob_reader(source, commit)("nested/source.rs"), SOURCE["source.rs"])
            git_output(destination, "fetch", "--no-tags", str(source), commit)
            with self.assertRaisesRegex(ValueError, "unexpected commit object"):
                audit(destination, {commit: source})

    def test_usage_delta_attributed_to_one_resolved_engine_turn(self):
        before = {"totals": {"total_tokens": {"tokens": 10, "reported_turns": 1}}}
        after = {"turns": [{"command_id": "cid", "accepted_ordinal": 2,
                            "usage": {"provider_details": {"model_usage": {VERSION: {}}}}}],
                 "turn_selections": {"cid": {"model": "sonnet", "effort": "high"}},
                 "totals": {"total_tokens": {"tokens": 29, "reported_turns": 2}}}
        self.assertEqual(usage_delta(before, after, "cid", 2, VERSION), {"total_tokens": 19})
        with self.assertRaisesRegex(RuntimeError, "expected provider usage"):
            usage_delta(before, after, "cid", 2, "claude-sonnet-5-4")
        models = after["turns"][0]["usage"]["provider_details"]
        models["model_usage"] = {VERSION: {}, "claude-haiku-5-5": {}}
        self.assertEqual(usage_delta(before, after, "cid", 2, VERSION), {"total_tokens": 19})
        for reported in ({"claude-haiku-5-5": {}}, {VERSION: {}, "claude-opus-5-5": {}}):
            with self.subTest(reported=reported), self.assertRaisesRegex(RuntimeError, "expected provider usage"):
                models["model_usage"] = reported
                usage_delta(before, after, "cid", 2, VERSION)


if __name__ == "__main__":
    unittest.main()
