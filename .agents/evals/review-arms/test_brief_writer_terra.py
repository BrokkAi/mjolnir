"""Constructed serialized-round checks; never uses live experiment data or scoring."""
import contextlib
import io
import json
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest

from writer_test_fixture import SCRIPTS, WriterTestCase


class ReadoutChecks(WriterTestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix='writer-terra-readout-')
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.writer = self.load_writer(self.root)
        self.writer.SCRIPTS = self.root / 'scripts'
        sets = self.writer.brief_oracle.sets()
        self.groups = {group: sets[group] for group in ('P', 'S')}

    def fixture(self):
        key, merged, decisions = {}, [], {}
        for group, bugs in self.groups.items():
            for i, bug in enumerate(bugs):
                # Correlated contrasts: P's TT and BT agree on every diff, with a
                # constant TT-no-brief advantage. S strongly favors BT, to catch
                # accidental pooling into the primary decision.
                tt = i % 3 + 1 if group == 'P' else 0
                for arm, cond, catches in [('A', 'no-brief', max(0, tt - 1)),
                                           ('BT', 'blind', tt if group == 'P' else 3),
                                           ('TT', 'blind-terra', tt)]:
                    for n in range(3):
                        oid = f'{bug}/{arm}/{n}'
                        label = 'caught' if n < catches else 'partial' if n == catches else 'missed'
                        key[oid] = dict(bug=bug, condition=cond, model=self.writer.REVIEW_MODEL, arm=arm, session=oid)
                        merged.append(dict(output_id=oid, bug=bug, labels=[label, label]))
                        if label != 'missed':
                            decisions[oid] = label
        return key, merged, decisions


    def write_round(self, key, merged, decisions):
        (self.root / 'private').mkdir(exist_ok=True)
        (self.root / 'brief-judge-j').mkdir(exist_ok=True)
        (self.root / 'private/key-brief-judge-j.json').write_text(json.dumps({'outputs': key}))
        (self.root / 'brief-judge-j/merged.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in merged))
        (self.root / 'brief-judge-j/handcheck-decisions.json').write_text(json.dumps(decisions))

        # Independent rounds assign different blinded output IDs to the same session.
        ablation = set(self.writer.brief_ablation.diffs())
        gkey = {'g:' + oid: k for oid, k in key.items()
                if k['bug'] in ablation and k['condition'] in ('blind', 'no-brief')}
        gmerged = [{**r, 'output_id': 'g:' + r['output_id']} for r in merged if 'g:' + r['output_id'] in gkey]
        gdecisions = {'g:' + oid: label for oid, label in decisions.items() if 'g:' + oid in gkey}
        (self.root / 'brief-judge-g').mkdir(exist_ok=True)
        (self.root / 'private/key-brief-judge-g.json').write_text(json.dumps({'outputs': gkey}))
        (self.root / 'brief-judge-g/merged.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in gmerged))
        (self.root / 'brief-judge-g/handcheck-decisions.json').write_text(json.dumps(gdecisions))

    # Hard-won:67efbd6f4: Terra-only brief arms made generic scoring assert on unplanned Sol cells.
    def test_generic_scorer_terra_only_plan_and_incomplete_planned_cells(self):
        data = self.root / 'generic-scorer'
        (data / 'default-instance').mkdir(parents=True, exist_ok=True)
        (data / 'default-instance/workspace.json').write_text('{"workspace":{"id":"fixture"}}')
        (data / 'private').mkdir(exist_ok=True)
        round_dir = data / 'brief-judge-j'
        round_dir.mkdir(exist_ok=True)
        bugs = self.groups['P'][:2]
        key, merged, decisions = {}, [], {}
        for bug in bugs:
            for arm, cond, model in [('A', 'no-brief', self.writer.REVIEW_MODEL),
                                     ('D', 'no-brief', 'gpt-6.1-sol'),
                                     ('BT', 'blind', self.writer.REVIEW_MODEL),
                                     ('TT', 'blind-terra', self.writer.REVIEW_MODEL)]:
                for n, label in enumerate(('caught', 'partial', 'missed')):
                    oid = f'{bug}/{arm}/{n}'
                    key[oid] = dict(bug=bug, arm=arm, condition=cond, model=model,
                                    session=oid, total_tokens=100, latency_s=None, tool_calls=None)
                    merged.append(dict(output_id=oid, bug=bug, labels=[label, label]))
                    if label != 'missed':
                        decisions[oid] = label
        key_path = data / 'private/key-brief-judge-j.json'
        saved = dict(arms=['TT', 'BT'], bugs=bugs, outputs=key)
        key_path.write_text(json.dumps(saved))
        (round_dir / 'merged.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in merged))
        (round_dir / 'handcheck-decisions.json').write_text(json.dumps(decisions))
        env = {**os.environ, 'REVIEW_ARMS_DIR': str(data), 'PYTHONDONTWRITEBYTECODE': '1'}
        command = [sys.executable, str(SCRIPTS / 'brief_score.py'), 'score', '--name', 'brief-judge-j']
        result = subprocess.run(command, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        output = round_dir / 'score.json'
        report = json.loads(output.read_text())
        for section in ('rates', 'per_arm', 'expected_catches_of_30', 'effects'):
            self.assertFalse(any(r['model'] == 'gpt-6.1-sol' and r.get('condition', r.get('brief')) != 'no-brief'
                                 for r in report[section]), section)
        self.assertTrue(any(r['model'] == 'gpt-6.1-sol' and r['condition'] == 'no-brief'
                            for r in report['per_arm']))
        tt = next(r for r in report['rates'] if r['condition'] == 'blind-terra')
        self.assertEqual(tt['catch_rate'], .333)
        self.assertEqual(tt['partial_rate'], .333)
        for mode in ('missing-terra-brief', 'missing-sol-baseline', 'planned-sol-brief', 'unexpected-sol-brief'):
            with self.subTest(mode=mode):
                changed = dict(key)
                arms = ['TT', 'BT']
                if mode == 'unexpected-sol-brief':
                    oid = f'{bugs[0]}/BS/extra'
                    changed[oid] = {**key[f'{bugs[0]}/BT/0'], 'model': 'gpt-6.1-sol',
                                    'arm': 'BS', 'session': oid}
                elif mode == 'planned-sol-brief':
                    arms.append('BS')
                else:
                    arm = 'TT' if mode == 'missing-terra-brief' else 'D'
                    # Remove the entire planned cell, not just one observation.
                    changed = {oid: k for oid, k in changed.items() if not (k['bug'] == bugs[0] and k['arm'] == arm)}
                key_path.write_text(json.dumps({**saved, 'arms': arms, 'outputs': changed}))
                output.unlink(missing_ok=True)
                result = subprocess.run(command, env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('incomplete sample; not scoring a subset', result.stderr)
                if mode == 'unexpected-sol-brief':
                    self.assertIn('gpt-6.1-sol: 1 in an unplanned cell (expected 0)', result.stderr)
                self.assertFalse(output.exists())

    def test_serialized_round_pairing_partial_and_secondary_exclusion(self):
        key, merged, decisions = self.fixture()
        self.write_round(key, merged, decisions)
        with contextlib.redirect_stdout(io.StringIO()):
            self.writer.readout('brief-judge-j')
        out = self.writer.SCRIPTS / 'results/brief-writer-terra'
        first = (out / 'summary.json').read_text()
        report = json.loads(first)
        self.assertEqual(report['default_writer'], 'Terra/high')
        self.assertEqual(report['sets']['P']['arms']['TT']['sessions'], 54)
        self.assertEqual(report['sets']['S']['arms']['TT']['sessions'], 12)
        self.assertEqual(report['sets']['P']['contrasts']['TT-BT']['ci95'], [0, 0])
        for bound in report['sets']['P']['contrasts']['TT-no-brief']['ci95']:
            self.assertAlmostEqual(bound, 1 / 3)
        data = json.loads((out / 'per-diff.json').read_text())
        self.assertTrue(any(r['TT']['partial_rate'] > 0 for r in data['sets']['P']))
        # Input serialization order cannot create positional arm pairings.
        self.write_round(dict(reversed(list(key.items()))), list(reversed(merged)), decisions)
        with contextlib.redirect_stdout(io.StringIO()):
            self.writer.readout('brief-judge-j')
        self.assertEqual(first, (out / 'summary.json').read_text())

    def test_correlated_nonconstant_contrasts_use_same_resamples(self):
        # BT and no-brief agree; both contrasts must have exactly the same CI,
        # even though their diff effects vary and input dictionaries are shuffled.
        per = {str(i): {arm: dict(catch_rate=value, caught=round(value * 9), partial_rate=0, sessions=9)
                        for arm, value in [('TT', i / 9), ('BT', 0), ('no-brief', 0)]}
               for i in reversed(range(10))}
        result = self.writer.paired_readout([str(i) for i in range(10)], per)
        self.assertEqual(result['contrasts']['TT-BT'], result['contrasts']['TT-no-brief'])
        lo, hi = result['contrasts']['TT-BT']['ci95']
        self.assertLess(lo, .5)
        self.assertGreater(hi, .5)
        self.assertEqual(result, self.writer.paired_readout([str(i) for i in range(10)], per))

    # Hard-won: 0d78337f4: cancelling rational effects produced a positive floating lower bound.
    def test_unequal_rational_effects_cancel_at_zero_bootstrap_lower_bound(self):
        per = {}
        for i in range(5):
            # Effects [-1/3, 1/2, 1/2, 1/2, 1/2]. Resamples with
            # three negative and two positive effects cancel exactly;
            # they occupy the 2.5th percentile for the registered seed.
            tc, ts, nc, ns = (0, 3, 1, 3) if i == 0 else (2, 3, 1, 6)
            per[str(i)] = {arm: dict(caught=c, sessions=n, catch_rate=c / n, partial_rate=0)
                           for arm, c, n in [('TT', tc, ts), ('BT', tc, ts), ('no-brief', nc, ns)]}
        result = self.writer.paired_readout(list(per), per)
        lower = result['contrasts']['TT-no-brief']['ci95'][0]
        self.assertEqual(lower, 0)
        self.assertFalse(lower > 0)
        self.assertAlmostEqual(result['contrasts']['TT-no-brief']['difference'], 1 / 3)
        self.assertEqual(result['contrasts']['TT-BT']['ci95'], [0, 0])

    def test_zero_gain_interval_keeps_sol_despite_secondary_gain(self):
        key, merged, decisions = self.fixture()
        # No-brief now has exactly TT's P labels, so lower95 is zero.
        # Make S favor TT to ensure it cannot rescue the primary decision.
        for r in merged:
            oid = r['output_id']
            bug, arm, n = oid.split('/')
            if bug in self.groups['P'] and arm == 'A':
                label = decisions.get(f'{bug}/TT/{n}', 'missed')
            elif bug in self.groups['S'] and arm == 'TT':
                label = 'caught'
            else:
                continue
            r['labels'] = [label, label]
            decisions[oid] = label
        self.write_round(key, merged, decisions)
        with contextlib.redirect_stdout(io.StringIO()):
            self.writer.readout('brief-judge-j')
        report = json.loads((self.writer.SCRIPTS / 'results/brief-writer-terra/summary.json').read_text())
        self.assertEqual(report['default_writer'], 'Sol/high')
        self.assertEqual(report['sets']['P']['contrasts']['TT-no-brief']['ci95'], [0, 0])

    def test_consistency_label_drift_and_exact_ten_point_boundary(self):
        key, merged, decisions = self.fixture()
        for r in merged:
            if key[r['output_id']]['condition'] == 'blind':
                r['labels'] = ['missed', 'missed']
                decisions.pop(r['output_id'], None)
        self.write_round(key, merged, decisions)
        gpath = self.root / 'brief-judge-g/merged.jsonl'
        rows = [json.loads(line) for line in gpath.read_text().splitlines()]
        bt = [r for r in rows if '/BT/' in r['output_id']]
        gd = self.root / 'brief-judge-g/handcheck-decisions.json'
        dec = json.loads(gd.read_text())
        for n in (3, 4):
            for r in bt[:n]:
                r['labels'] = ['caught', 'caught']
                dec[r['output_id']] = 'caught'
            gpath.write_text(''.join(json.dumps(r) + '\n' for r in rows))
            gd.write_text(json.dumps(dec))
            result = self.writer.judge_consistency('brief-judge-j')
            self.assertEqual(result['arms']['BT']['differs_over_10_points'], n == 4)
            self.assertAlmostEqual(result['arms']['BT']['difference_points'], -100 * n / 30)
            self.assertEqual(bool(result['caveats']), n == 4)
            labels = result['per_diff'][0]['arms']['BT']['labels'][0]
            self.assertNotEqual(labels['outputs']['brief-judge-j']['output_id'],
                                labels['outputs']['brief-judge-g']['output_id'])

    def test_consistency_refuses_nonoverlapping_session_even_with_full_counts(self):
        self.write_round(*self.fixture())
        path = self.root / 'private/key-brief-judge-g.json'
        key = json.loads(path.read_text())
        next(iter(key['outputs'].values()))['session'] += '-different-review'
        path.write_text(json.dumps(key))
        with self.assertRaisesRegex(SystemExit, 'identical sessions'):
            self.writer.judge_consistency('brief-judge-j')

    def test_missing_coverage_refuses_before_writing(self):
        for mode in ('missing-cell', 'missing-merged', 'duplicate-session'):
            with self.subTest(mode=mode):
                key, merged, decisions = self.fixture()
                oid = next(k for k in key if '/TT/' in k)
                if mode == 'missing-cell':
                    del key[oid]
                    merged = [r for r in merged if r['output_id'] != oid]
                    decisions.pop(oid, None)
                elif mode == 'missing-merged':
                    merged = [r for r in merged if r['output_id'] != oid]
                else:
                    key[oid]['session'] = next(iter(key.values()))['session']
                self.write_round(key, merged, decisions)
                with self.assertRaises(SystemExit):
                    self.writer.readout_data('brief-judge-j', self.groups)

    def test_unknown_or_unhandchecked_never_becomes_missed(self):
        for mode in ('unknown-decision', 'unknown-judge', 'missing-handcheck'):
            with self.subTest(mode=mode):
                key, merged, decisions = self.fixture()
                oid = next(iter(decisions))
                if mode == 'unknown-decision':
                    decisions[oid] = 'unknown'
                elif mode == 'unknown-judge':
                    merged[0]['labels'][0] = 'unknown'
                else:
                    del decisions[oid]
                self.write_round(key, merged, decisions)
                with self.assertRaises(SystemExit):
                    self.writer.readout_data('brief-judge-j', self.groups)


if __name__ == '__main__':
    unittest.main(verbosity=2)
