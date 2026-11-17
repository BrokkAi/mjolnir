#!/usr/bin/env python3
"""Offline regressions for attention scoring with incomplete or incompatible evidence."""
import importlib.util
import json
from pathlib import Path
import random
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("attention_score", Path(__file__).with_name("attention-live-score.py"))
score = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(score)


def turn(ordinal, required=0.8):
    return dict(session="fake-session", turn_id=f"fake-turn-{ordinal}", completed_ordinal=ordinal,
                glide={"input": {"probabilities": {"required": required}}})


def probabilities(kind):
    return {k: float(k == kind) for k in score.ASK_KINDS}


class ScoringEvidenceTests(unittest.TestCase):
    # Hard-won: 5c117f19d: a demoted onset hid an unanswered later unlabeled firing turn.
    def test_missing_unlabeled_firing_turn_excludes_episode_in_tables_r3_and_fixed_rule(self):
        first, later, good = turn(1), turn(2), turn(3)
        bad_episode = dict(session="fake-session", turns=[first, later])
        good_episode = dict(session="fake-session", turns=[good])
        episodes = [bad_episode, good_episode]
        lab = {("fake-session", n): dict(glyph="?", cls="blocking") for n in (1, 3)}
        answers = {w: {("fake-session", 1): probabilities("optional_or_default"),
                       ("fake-session", 3): probabilities("blocking")} for w in ("A", "B", "C")}
        self.assertEqual(score.ask_action(first, score.C_WORDING_GRID[0], answers), "other")
        for point in score.ASK_GRID["C"] + score.C_WORDING_GRID:
            with self.subTest(point=point):
                # The same evaluator handles fixed rules on confirmation traces.
                counts = score.sum_counts(score.evaluate(episodes, lab, point, answers))
                self.assertEqual((counts["ep"], counts["pos"], counts["pos_hit"], counts["ep_missing_answers"]), (1, 1, 1, 1))
        text = score.ask_score_block(episodes, lab, answers, random.Random(1))
        self.assertIn("Kept 1/2 full-trace episodes; excluded 0 for missing frozen wire state (Amendment 7), 1 pending answers.", text)
        self.assertIn("Kept 1/2 full-trace episodes; 0 declared exclusions; 1 pending answers.", text)
        with tempfile.TemporaryDirectory(prefix="attention-readiness-") as directory:
            Path(directory, "scoring").mkdir()
            report = score.r3_selection(directory, episodes, lab, answers, {"C": set()})
            record = json.loads(Path(directory, "scoring/r3-selection.json").read_text())
            self.assertEqual((record["episodes"], record["missing_answer_episodes"]), (1, 1))
            self.assertIn("1 episodes missing answers", report)
        # Coverage follows the actual rule's cut, not the current cut or label membership.
        later["glide"]["input"]["probabilities"]["required"] = 0.3
        self.assertFalse(score.missing_rule_answers(bad_episode, lab, ("C", "A", 0.5, 0.5), answers))
        self.assertEqual(score.missing_rule_answers(bad_episode, lab, ("C", "A", score.corpus.CUTS[2], 0.5), answers), {("fake-session", 2)})

        # Quota recovery bypasses the input branch even with a high required probability.
        later["glide"]["input"]["probabilities"]["required"] = 0.8
        later["glide"]["failure"] = {"probabilities": {"quota": 0.99}}
        self.assertFalse(score.missing_rule_answers(bad_episode, lab, score.C_WORDING_GRID[0], answers))

    # Hard-won: 5c117f19d: missing-wire record dictionaries were used as hash keys.
    def test_missing_wire_fallback_resolves_item_and_unlabeled_turn_records(self):
        rows = [turn(1), turn(2), turn(3, required=0)]
        by_turn = {(t["session"], t["completed_ordinal"]): t for t in rows}
        key = {"fake-item": dict(session="fake-session", completed_ordinal=1, populations=["P1"])}
        with tempfile.TemporaryDirectory(prefix="attention-wire-") as directory:
            family = Path(directory, "familyB")
            family.mkdir()
            (family / "missing-wire-requests.json").write_text(json.dumps([dict(item="fake-item")]))
            (family / "missing-wire-requests-all.json").write_text(json.dumps([
                dict(item=None, session="fake-session", turn_id=f"fake-turn-{n}", completed_ordinal=n) for n in (2, 3)]))
            excluded = score.answer_exclusions(directory, key, by_turn)
            self.assertEqual(excluded["B"], set(by_turn))
            self.assertEqual(excluded["C"], {("fake-session", 1), ("fake-session", 2)})

    # Hard-won: 5c117f19d: fresh ask-kind results could be mixed with historical input vectors.
    def test_answer_files_require_supported_metadata_for_labeled_and_full_traces(self):
        t = turn(1)
        key = {"fake-item": dict(session=t["session"], completed_ordinal=1, populations=["P1"])}
        by_turn = {(t["session"], 1): t}
        for full_trace, wording in ((False, "C"), (True, "A")):
            with self.subTest(full_trace=full_trace), tempfile.TemporaryDirectory(prefix="attention-branch-") as directory:
                family = Path(directory, "familyB")
                family.mkdir()
                path = family / f"answers-{'all-' if full_trace else ''}{wording}.jsonl"
                entry = dict(item="fake-item", session=t["session"], turn_id=t["turn_id"], wording=wording,
                             probabilities=probabilities("blocking"), shrunk=False)
                path.write_text(json.dumps(entry) + "\n")
                with self.assertRaisesRegex(ValueError, f"{path.name}: missing metadata sidecar"):
                    score.read_answers(directory, key, by_turn, full_trace=full_trace)
                metadata = path.with_suffix(".metadata.json")
                metadata.write_text(json.dumps(dict(branch="fresh_full_set")))
                with self.assertRaisesRegex(ValueError, f"{path.name}: unsupported answers branch 'fresh_full_set'"):
                    score.read_answers(directory, key, by_turn, full_trace=full_trace)
                metadata.write_text(json.dumps(dict(branch="ask_kind_alone")))
                loaded = score.read_answers(directory, key, by_turn, full_trace=full_trace)
                self.assertEqual(loaded[wording][t["session"], 1], entry["probabilities"])


if __name__ == "__main__":
    unittest.main()
