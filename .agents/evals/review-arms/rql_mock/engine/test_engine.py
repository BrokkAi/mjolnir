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
from .prompt import CONTEXT_RECALL, assert_blind, semantics_only, turn_prompt
from .runner import CHECKOUT_CLEAN, CONTEXT_BOUNDARY, IsolationFailure, LaunchHeld, SealedRunner, usage_delta
from .snapshot import blob_reader, git_output, manifest_sources, parentless_snapshot
from .validation import validate


CATALOGUE = (CENSUS / "rql-documented-steps.md").read_text(encoding="utf-8")
MENU = (CENSUS / "menu-steps.md").read_text(encoding="utf-8")
CASES = (CENSUS / "menu-steps-cases.md").read_text(encoding="utf-8")
COMMIT = "a" * 40
VERSION = "claude-fable-5-2"
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


def payload(rows=ROWS, coverage=None):
    return json.dumps({"rows": rows, "coverage": closed() if coverage is None else coverage})


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
                    "model_versions": {"fable": "Fable 5.2", "fable_provider_id": VERSION},
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
            config.write_text(json.dumps({"manifest": str(manifest)}))
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

    # Hard-won: 49e16b8: a prompt change reused prior raw answers and fidelity measurements.
    def test_fidelity_checkpoints_follow_the_answer_cache_input_hash(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "engine.json"
            config.write_text(json.dumps({"manifest": str(self.fixture(root))}))
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
        return "test-session"

    def prepare(self, sid, snapshot, directory):
        self.preparations.append((sid, snapshot))

    def context_probe(self, sid, directory):
        return {"status": "passed", "context_id": "probe-context"}

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
    def test_fable_launch_checks_both_weekly_windows_and_can_retry(self):
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
        for general, fable in ((24, 80), (25, 80), (80, 24), (80, 25), (None, 80), (80, None), (80, 80)):
            with self.subTest(general=general, fable=fable):
                windows[:] = [{"remaining_percent": percent, "duration_seconds": 604800, "model_scope": scope}
                              for percent, scope in ((general, None), (fable, "fable")) if percent is not None]
                windows.append({"remaining_percent": 0, "duration_seconds": 604800, "model_scope": "sonnet"})
                windows.append({"remaining_percent": 80, "duration_seconds": 18000})
                if (general, fable) == (80, 80):
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
            {"model": "claude-fable-5-1"},
        ]
        for change in changes:
            changed = self.engine(runner, **change).answer(COMMIT, QUERY)
            self.assertEqual(changed["status"], "ok", changed)
            self.assertNotEqual(changed["provenance"]["inputs_sha256"], initial["provenance"]["inputs_sha256"])
        self.assertEqual((len(runner.launches), len(runner.preparations), len(runner.turns)), (1, 1, 4))

    def test_stability_reasks_have_separate_contexts_and_never_change_writer_cache(self):
        runner = FakeRunner([payload(), payload([]), payload([])])
        engine = self.engine(runner)
        cached = engine.answer(COMMIT, QUERY)
        fresh = [engine.answer(COMMIT, QUERY, fresh=True) for _ in range(2)]
        self.assertTrue(all(answer["status"] == "ok" for answer in [cached, *fresh]))
        self.assertFalse(cached["provenance"]["fresh"])
        self.assertTrue(all(answer["provenance"]["fresh"] for answer in fresh))
        identifiers = {answer["provenance"]["context_id"] for answer in [cached, *fresh]}
        self.assertEqual(len(identifiers), 3)
        self.assertEqual(len(runner.launches), 1)
        self.assertEqual(engine.answer(COMMIT, QUERY), cached)
        paths = sorted((self.cache / COMMIT / "stability").glob("*/*.json"))
        self.assertEqual({path.stem for path in paths}, identifiers - {cached["provenance"]["context_id"]})
        self.assertEqual([json.loads(path.read_text()) for path in paths], fresh)

    def test_isolation_failure_moves_evidence_and_uses_a_new_session(self):
        class ViolatingRunner(FakeRunner):
            def start(self, snapshot, commit):
                super().start(snapshot, commit)
                return SESSION if len(self.launches) == 1 else REPLACEMENT

            def retire(self, directory, record):
                self.retired = record["session_id"]

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

    def test_parse_error_never_constructs_engine_session(self):
        runner = FakeRunner([])
        result = self.engine(runner).answer(COMMIT, "(what changed?)")
        self.assertEqual(result["status"], "parse_error")
        self.assertFalse(runner.launches)


class FakeAcpRunner(SealedRunner):
    """Tests the production prompt sequence against an in-memory mj adapter."""

    instance = INSTANCE

    def __init__(self, *, remember=False, acknowledge=True):
        self.driver = self
        self.config = {"profile": "claude-test", "model_version": VERSION}
        self.manifest = {"diffs": []}
        self.served_steps = ("contract-cases", "value-transfers", "guarded-paths")
        self.remember = remember
        self.acknowledge = acknowledge
        self.messages = []
        self.boundaries = []
        self.conversation = []
        self.turns = []
        self.artifact = None
        self.tmp_entries = set()
        self.tool_calls = []

    def start(self, snapshot, commit):
        return "test-session"

    def prepare(self, sid, snapshot, directory):
        pass

    def pilot_weekly_hold(self, profile, *, scope):
        assert (profile, scope) == ("claude-test", "fable")
        return getattr(self, "quota_hold", None)

    def wait_idle(self, sid):
        return {"profile_id": "claude-test",
                "config_options": [{"key": "model", "current": "fable"},
                                   {"key": "effort", "current": "high"}]}

    def clean_checkout(self, sid):
        self.artifact = None
        return f".rql-engine-{sid}"

    def audit_turn(self, sid, turn, directory):
        self.save(directory / f"turn-{turn}-transcript.json", self.transcript(sid))
        return True

    def tmp_admin(self, sid, remove=None):
        if remove is None:
            return sorted(self.tmp_entries)
        self.tmp_entries.difference_update(remove)

    def transcript(self, sid):
        return [{"session_id": sid, "items": self.tool_calls}]

    @staticmethod
    def save(path, value):
        path.write_text(json.dumps(value), encoding="utf-8")

    def command(self, operation, *args, data=None):
        if operation == "put-file":
            session_flag, sid, path_flag, path, source = args
            assert (session_flag, sid, path_flag, source) == ("--session", "test-session", "--path", "-"), args
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
        if text == "/clear":
            if self.acknowledge:
                self.boundaries.append({"seq": len(self.messages), "stable_id": CONTEXT_BOUNDARY + command})
                if not self.remember:
                    self.conversation.clear()
        else:
            self.conversation.append(text)
        return json.dumps({"session_id": sid, "via": "turn", "turn_id": len(self.messages)}).encode()

    def mj(self, operation, *args, **kwargs):
        if operation == "transcript":
            cursor = int(args[args.index("--after-seq") + 1]) if "--after-seq" in args else 0
            return {"latest_seq": len(self.messages), "next_after_seq": len(self.messages),
                    "items": [item for item in self.boundaries if item["seq"] > cursor]}
        if operation == "wait":
            command, text = self.messages[-1]
            if refusal := getattr(self, "turn_refusal", None):
                return {"turn_id": len(self.messages), **refusal}
            if text == CONTEXT_RECALL:
                response = json.dumps(self.conversation[-2] if len(self.conversation) > 1 else "")
            elif "synthetic query" in text:
                response = "ACK"
            else:
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
                "turn_selections": {row["command_id"]: {"model": "fable", "effort": "high"}
                                    for row in turns}}


class ContextTests(unittest.TestCase):
    # Hard-won: b9ceb96: engine scratch persisted across /clear
    def test_query_resets_mentioned_temporary_artifacts_and_records_reuse(self):
        runner = FakeAcpRunner()
        runner.tmp_entries.add("baseline")
        original_message = runner.message

        def answer_turn(sid, text, command):
            if text == "/clear":
                return original_message(sid, text, command)
            first = not runner.tool_calls
            if first:
                runner.tmp_entries.update(("x", "x_other", "hidden"))
                call = "mkdir /tmp/x; ls /tmp/baseline"
            else:
                self.assertNotIn("x", runner.tmp_entries)
                self.assertEqual(runner.tmp_entries, {"baseline", "hidden", "x_other"})
                runner.tmp_entries.add("x")
                call = "cat /tmp/hidden/script.py"
            turn = original_message(sid, text, command)
            runner.tool_calls.append({"seq": turn, "role": "tool", "body": {"call": {"rawInput": call}}})
            return turn

        runner.message = answer_turn
        with TemporaryDirectory() as directory:
            path = Path(directory)
            _, first, _ = runner.turn("test-session", '(function :name "first")', CATALOGUE, MENU, [], path)
            self.assertEqual(json.loads((path / "tmp-baseline.json").read_text()), ["baseline"])
            self.assertEqual(runner.tmp_entries, {"baseline", "hidden", "x_other"})
            self.assertEqual(first["cross_query_reuse"], [])
            _, second, _ = runner.turn("test-session", '(function :name "second")', CATALOGUE, MENU, [], path)
            self.assertEqual(runner.tmp_entries, {"baseline", "x", "x_other"})
            expected = [{"path": "/tmp/hidden", "prior_context_id": first["context_id"]}]
            self.assertEqual(second["cross_query_reuse"], expected)
            record = json.loads((path / "answers" / second["context_id"] / "turn.json").read_text())
            self.assertEqual(record["cross_query_reuse"], expected)
            history = json.loads((path / "residual-artifacts.json").read_text())
            self.assertEqual([(row["remove"], row["kept"]) for row in history],
                             [(["x"], ["hidden", "x_other"]), (["hidden"], ["x", "x_other"])])
            self.assertTrue(all(row["cleanup_complete"] for row in history))

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

    def test_probe_is_once_and_every_query_and_repair_waits_for_its_own_boundary(self):
        runner = FakeAcpRunner()
        with TemporaryDirectory() as directory:
            path = Path(directory)
            first_probe = runner.context_probe("test-session", path)
            self.assertEqual(first_probe["status"], "passed")
            before = list(runner.messages)
            self.assertEqual(runner.context_probe("test-session", path), first_probe)
            self.assertEqual(runner.messages, before)
            contexts = []
            for errors in ([], ["bad cited line"]):
                _, record, _ = runner.turn("test-session", QUERY, CATALOGUE, MENU, errors, path)
                contexts.append(record["context_id"])
                self.assertEqual(runner.messages[-2], (record["context_id"], "/clear"))
                self.assertIn("Query: " + json.dumps(QUERY), runner.messages[-1][1])
                self.assertIsNone(runner.artifact)
            self.assertEqual(len({first_probe["context_id"], *contexts}), 3)
            self.assertIn("bad cited line", runner.messages[-1][1])

    def test_recall_failure_stops_answering_and_is_persisted_in_provenance(self):
        runner = FakeAcpRunner(remember=True)
        with TemporaryDirectory() as directory:
            path = Path(directory)
            engine = Engine(path, (CATALOGUE, MENU),
                            lambda commit: (SOURCE.get, {"tree": "fake", "orphan": "root"}),
                            lambda: runner, VERSION)
            result = engine.answer(COMMIT, QUERY)
            self.assertEqual(result["status"], "engine_error")
            self.assertIn("recalled prior query", result["message"])
            probe = result["provenance"]["context_probe"]
            self.assertEqual(probe["status"], "failed")
            self.assertEqual(json.loads((path / COMMIT / "active" / "context-probe.json").read_text()), probe)
            self.assertEqual(engine.answer(COMMIT, QUERY), result)
            self.assertEqual(len(runner.messages), 3)

    def test_clear_does_not_accept_a_missing_or_old_boundary(self):
        runner = FakeAcpRunner(acknowledge=False)
        runner.boundaries.append({"seq": 0, "stable_id": CONTEXT_BOUNDARY + "earlier-clear"})
        with self.assertRaisesRegex(RuntimeError, "did not confirm /clear"):
            runner.clear_context("test-session", timeout=0)
        self.assertEqual([text for _, text in runner.messages], ["/clear"])

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
            command = [sys.executable, "-c", CHECKOUT_CLEAN, scratch.name]
            subprocess.run(command, cwd=path, check=True, capture_output=True)
            self.assertFalse(scratch.exists())
            bad = subprocess.run([*command[:-1], tracked.name], cwd=path, capture_output=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertTrue(tracked.is_file())

    def test_writer_stdio_rejects_fresh_and_cannot_select_a_host_reask(self):
        with TemporaryDirectory() as directory:
            temp = Path(directory)
            sid = "a" * 32
            diff = "repo/test"
            manifest = temp / "manifest.json"
            manifest.write_text(json.dumps({"page_rows": 50, "diffs": [{"key": diff, "intro": COMMIT}]}))
            (temp / "transport_fixture.py").write_text("""
def answer(commit, query, *, fresh=False, expected_inputs=None):
    return {"status": "ok", "rows": [], "total": 0,
            "coverage": {"complete": True, "open": []}, "provenance": {"fresh": fresh, "commit": commit}}
""")
            config, log = temp / "transport.json", temp / "queries.jsonl"
            config.write_text(json.dumps({
                "manifest": str(manifest), "engine": "transport_fixture:answer",
                "sessions": {sid: {"diff": diff, "intro": COMMIT, "arm": "R", "log": str(log)}},
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
            program = f"print({acknowledgement!r})"
        return [sys.executable, "-c", program, *argv]



class RetirementTests(unittest.TestCase):
    def test_teardown_cli_uses_the_cache_beside_its_explicit_config(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            default, selected = root / "default", root / "selected"
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"model_versions": {"fable_provider_id": VERSION}}))
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
        adapter.fable_version = VERSION
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
                 "turn_selections": {"cid": {"model": "fable", "effort": "high"}},
                 "totals": {"total_tokens": {"tokens": 29, "reported_turns": 2}}}
        self.assertEqual(usage_delta(before, after, "cid", 2, VERSION), {"total_tokens": 19})
        with self.assertRaisesRegex(RuntimeError, "expected provider usage"):
            usage_delta(before, after, "cid", 2, "claude-fable-5-1")


if __name__ == "__main__":
    unittest.main()
