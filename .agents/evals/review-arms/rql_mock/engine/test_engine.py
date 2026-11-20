"""Offline engine tests. The tests never call mj, Podman, or a provider."""
from copy import deepcopy
import json
from pathlib import Path
import re
from tempfile import TemporaryDirectory
import unittest

from census_label import CENSUS

from . import Engine, read_documents
from .__main__ import audit
from .grammar import parse
from .prompt import assert_blind, semantics_only, turn_prompt
from .runner import usage_delta
from .snapshot import blob_reader, git_output, parentless_snapshot
from .validation import validate


CATALOGUE, MENU = read_documents()
CASES = (CENSUS / "menu-steps-cases.md").read_text(encoding="utf-8")
COMMIT = "a" * 40
VERSION = "claude-fable-5-1"
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

    def turn(self, sid, query, catalogue, menu, errors, directory):
        self.turns.append((sid, query, errors))
        record = {"session": sid, "turn": len(self.turns), "tokens": {"total_tokens": 15}}
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

        return Engine(self.cache, (catalogue, menu), source, lambda: runner, lambda: model)

    def test_cache_normalization_and_changed_input(self):
        runner = FakeRunner([payload()] * 4)
        initial = self.engine(runner).answer(COMMIT, QUERY)
        self.assertEqual(initial["status"], "ok", initial)
        cached = self.engine(runner).answer(COMMIT, ' ( function  :name "evaluate" ) ; comment')
        self.assertEqual(cached, initial)
        changes = [
            {"catalogue": CATALOGUE + "\nCatalogue revision."},
            {"menu": MENU + "\nSemantics revision."},
            {"model": "claude-fable-5-2"},
        ]
        for change in changes:
            changed = self.engine(runner, **change).answer(COMMIT, QUERY)
            self.assertEqual(changed["status"], "ok", changed)
            self.assertNotEqual(changed["provenance"]["inputs_sha256"], initial["provenance"]["inputs_sha256"])
        self.assertEqual((len(runner.launches), len(runner.preparations), len(runner.turns)), (1, 1, 4))

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
        coverage = {"complete": False, "open": [{"reason": "depth boundary", "site": span(line=1)}]}
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
            usage_delta(before, after, "cid", 2, "claude-fable-5-2")


if __name__ == "__main__":
    unittest.main()
