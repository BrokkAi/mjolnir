"""Sonnet-study routing of brief_writer_terra.py through its CLI, on constructed persisted rounds.

Each test gets its own data root and scripts directory (corpus and judge spec linked from the real
ones), so nothing here reads live experiment data or writes into the repository's results/.
"""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from writer_test_fixture import SCRIPTS as REAL, WriterTestCase


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def labels_for(catches):
    """Three session labels: `catches` caught, then one partial if room, then missed."""
    return ['caught' if n < catches else 'partial' if n == catches else 'missed' for n in range(3)]


class Study(WriterTestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='writer-studies-')
        self.addCleanup(self.tmp.cleanup)
        self.root, self.scripts = Path(self.tmp.name) / 'data', Path(self.tmp.name) / 'scripts'
        self.root.mkdir()
        self.scripts.mkdir()
        for name in ('corpus', 'judge'):
            (self.scripts / name).symlink_to(REAL / name)
        self.writer = self.load_writer(self.root)
        self.writer.SCRIPTS = self.scripts
        sets = self.writer.brief_oracle.sets()
        self.bugs = sets['P'] + sets['S']
        self.terra_before = {p: sha(p) for p in (REAL / 'results/brief-writer-terra').glob('*') if p.is_file()}

    def tearDown(self):
        after = {p: sha(p) for p in (REAL / 'results/brief-writer-terra').glob('*') if p.is_file()}
        self.assertEqual(self.terra_before, after, 'historical Terra results were rewritten')

    def cli(self, *args):
        out = io.StringIO()
        with mock.patch.object(sys, 'argv', ['brief_writer_terra.py', *args]), contextlib.redirect_stdout(out):
            self.writer.main()
        return out.getvalue()

    def write_round(self, name, rows):
        """rows: (bug, arm, condition, model, session, label). Output IDs differ between rounds."""
        key, merged, decisions = {}, [], {}
        for bug, arm, cond, model, session, label in rows:
            oid = f'{name}:{session}'
            key[oid] = dict(bug=bug, arm=arm, condition=cond, model=model, session=session)
            merged.append(dict(output_id=oid, bug=bug, labels=[label, label]))
            if label != 'missed':
                decisions[oid] = label
        (self.root / 'private').mkdir(exist_ok=True)
        (self.root / name).mkdir(exist_ok=True)
        (self.root / 'private' / f'key-{name}.json').write_text(json.dumps({'outputs': key}))
        (self.root / name / 'merged.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in merged))
        (self.root / name / 'handcheck-decisions.json').write_text(json.dumps(decisions))

    def rows(self, ts, bt, nb, tt=None, solnb=True):
        """Per-arm caught counts of 3 sessions. Baseline and BT session IDs are shared by both rounds."""
        t = self.writer.REVIEW_MODEL
        out = []
        for bug in self.bugs:
            cells = [('BT', 'blind', t, bt), ('A', 'no-brief', t, nb)]
            if ts is not None:
                cells.append(('TS', 'blind-sonnet', t, ts))
            if tt is not None:
                cells.append(('TT', 'blind-terra', t, tt))
            if solnb:
                cells.append(('D', 'no-brief', 'gpt-6.1-sol', 0))  # judged but never read
            for arm, cond, model, catches in cells:
                for n, label in enumerate(labels_for(catches)):
                    out.append((bug, arm, cond, model, f'{bug}/{arm}/{n}', label))
        return out

    def briefs(self):
        for kind in ('blind', 'blind-sonnet'):
            for bug in self.bugs:
                d = self.root / 'briefs' / kind / bug
                d.mkdir(parents=True, exist_ok=True)
                (d / 'brief.md').write_text(f'{kind} {bug}\n')
        (self.root / 'briefs/blind-sonnet' / self.bugs[0] / 'launch-intent.json').write_text(
            json.dumps({'model': 'sonnet', 'effort': 'high', 'profile': 'claude6'}))

    def standard(self, k_ts=2, k_bt=2, k_nb=0, j_bt=1, j_nb=0, j_tt=3):
        self.write_round('brief-judge-k', self.rows(k_ts, k_bt, k_nb))
        self.write_round('brief-judge-j', self.rows(None, j_bt, j_nb, tt=j_tt))
        self.briefs()

    def sonnet_results(self):
        return self.scripts / 'results/brief-writer-sonnet'


class SonnetReadout(Study):
    def test_k_labels_decide_tt_stays_beside_from_j_and_drift_is_flagged_by_group(self):
        self.standard()
        self.cli('readout', '--writer', 'sonnet')
        report = json.loads((self.sonnet_results() / 'summary.json').read_text())
        primary = report['sets']['P']
        # TS has 2 caught and 1 partial of 3: the partial never counts as caught.
        self.assertAlmostEqual(primary['arms']['TS']['catch_rate'], 2 / 3)
        self.assertAlmostEqual(primary['arms']['TS']['partial_rate'], 1 / 3)
        self.assertEqual(sorted(primary['arms']), ['BT', 'TS', 'no-brief'])
        self.assertEqual(sorted(primary['contrasts']), ['TS-BT', 'TS-no-brief'])
        self.assertEqual(primary['contrasts']['TS-BT']['ci95'], [0, 0])
        # K's labels for BT (2/3), not J's (1/3), drive the reading.
        self.assertAlmostEqual(primary['arms']['BT']['catch_rate'], 2 / 3)
        self.assertTrue(report['acceptable_writer'])
        self.assertEqual(report['sonnet_decision'], 'acceptable writer')
        self.assertFalse(report['production_default_changed'])
        self.assertNotIn('default_writer', report)
        beside = report['historical_beside']
        self.assertEqual((beside['arm'], beside['source_round'], beside['pooled_with_primary']), ('TT', 'brief-judge-j', False))
        self.assertEqual(beside['sets']['P']['catch_rate'], 1.0)
        self.assertEqual(report['writer_models'], {'TS': 'sonnet', 'BT': 'gpt-6.1-sol'})
        consistency = report['judge_consistency']
        self.assertEqual(consistency['reference_round'], 'brief-judge-j')
        self.assertEqual(consistency['diffs'], 22)
        for scope in (consistency, consistency['groups']['P'], consistency['groups']['S']):
            self.assertTrue(scope['arms']['BT']['differs_over_10_points'])
            self.assertAlmostEqual(scope['arms']['BT']['difference_points'], 100 / 3)
            self.assertFalse(scope['arms']['no-brief']['differs_over_10_points'])
        self.assertEqual((consistency['groups']['P']['diffs'], consistency['groups']['S']['diffs']), (18, 4))
        self.assertEqual(len(consistency['caveats']), 1)
        # The reading names its inputs by hash and records the pinned writer launch.
        prov = report['provenance']
        self.assertEqual(prov['source_rounds'], ['brief-judge-k', 'brief-judge-j'])
        self.assertEqual(prov['raw_artifacts']['brief-judge-k']['key'], sha(self.root / 'private/key-brief-judge-k.json'))
        self.assertEqual(prov['writer']['launches'][self.bugs[0]]['profile'], 'claude6')
        self.assertIsNone(prov['writer']['launches'][self.bugs[1]])
        # Per-diff output keeps J's TT sessions apart from K's arms.
        per = json.loads((self.sonnet_results() / 'per-diff.json').read_text())
        self.assertNotIn('TT', per['sets']['P'][0])
        self.assertIn('TT', per['historical_beside']['P'][0])
        self.assertFalse((self.scripts / 'results/brief-writer-terra').exists())

    def test_zero_gain_over_no_brief_is_not_an_acceptable_writer(self):
        self.standard(k_nb=2)  # no-brief now equals TS on every diff: lower95 is exactly zero
        self.cli('readout', '--writer', 'sonnet')
        report = json.loads((self.sonnet_results() / 'summary.json').read_text())
        self.assertEqual(report['sets']['P']['contrasts']['TS-no-brief']['ci95'], [0, 0])
        self.assertFalse(report['acceptable_writer'])
        self.assertEqual(report['sonnet_decision'], 'Sol writes')

    def test_incomplete_or_mismatched_inputs_refuse_before_writing(self):
        def drop(rows, arm):
            return [r for r in rows if not (r[0] == self.bugs[0] and r[1] == arm)]

        def retag(rows, which, **changes):
            names = ('bug', 'arm', 'condition', 'model', 'session', 'label')
            return [tuple(changes.get(n, v) for n, v in zip(names, r)) if r[1] == which else r for r in rows]

        cases = {
            'missing TS cell': (lambda: self.write_round('brief-judge-k', drop(self.rows(2, 2, 0), 'TS')),
                                'incomplete sample'),
            'TS reviewer count': (lambda: self.write_round('brief-judge-k', [r for r in self.rows(2, 2, 0)
                                                                              if r[4] != f'{self.bugs[0]}/TS/2']),
                                  'incomplete sample'),
            'TS recorded as the Terra writer arm': (
                lambda: self.write_round('brief-judge-k', retag(self.rows(2, 2, 0), 'TS', arm='TT')),
                'wrong arm/condition'),
            'Sol-writer brief condition on a Sonnet arm': (
                lambda: self.write_round('brief-judge-k', retag(self.rows(2, 2, 0), 'TS', condition='blind')),
                'wrong arm/condition'),
            'reference missing a BT session': (
                lambda: self.write_round('brief-judge-j', [r for r in self.rows(None, 1, 0, tt=3)
                                                           if r[4] != f'{self.bugs[0]}/BT/0']),
                'brief-judge-j'),
            'reference sessions differ': (
                lambda: self.write_round('brief-judge-j', [r[:4] + (r[4] + '-again' if r[4].endswith(f'{self.bugs[0]}/BT/0') else r[4],
                                                                    r[5]) for r in self.rows(None, 1, 0, tt=3)]),
                'identical sessions'),
            'reference lacks the historical TT': (
                lambda: self.write_round('brief-judge-j', drop(self.rows(None, 1, 0, tt=3), 'TT')),
                'incomplete sample'),
        }
        for name, (break_it, message) in cases.items():
            with self.subTest(name):
                self.standard()
                break_it()
                with self.assertRaises(SystemExit) as caught:
                    self.cli('readout', '--writer', 'sonnet')
                self.assertIn(message, str(caught.exception))
                self.assertFalse(self.sonnet_results().exists())

    def test_reference_merged_ids_must_match_the_reference_key(self):
        self.standard()
        path = self.root / 'brief-judge-j/merged.jsonl'
        lines = path.read_text().splitlines()
        path.write_text('\n'.join(lines[1:]) + '\n')  # one reference verdict is missing
        with self.assertRaisesRegex(SystemExit, 'incomplete or duplicate merged verdict coverage'):
            self.cli('readout', '--writer', 'sonnet')
        self.assertFalse(self.sonnet_results().exists())

    def test_a_round_cannot_be_its_own_reference(self):
        self.standard()
        with self.assertRaisesRegex(SystemExit, 'reference round'):
            self.cli('readout', '--writer', 'sonnet', '--name', 'brief-judge-j')


class SonnetArtifacts(Study):
    def test_import_does_not_overwrite_an_exported_experiment_workspace(self):
        exported = self.root / 'exported'
        workspace = exported / 'default-instance/workspace.json'
        workspace.parent.mkdir(parents=True)
        original = '{"workspace":{"id":"existing-experiment"}}\n'
        workspace.write_text(original)
        env = {**os.environ, 'REVIEW_ARMS_DIR': str(exported), 'PYTHONDONTWRITEBYTECODE': '1'}
        code = 'import runpy,sys; runpy.run_path(sys.argv[1], run_name="isolated_bootstrap")'
        result = subprocess.run([sys.executable, '-B', '-c', code, str(Path(__file__).resolve())],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(workspace.read_text(), original)
        self.assertEqual(list(exported.rglob('*')), [workspace.parent, workspace])

    def test_usage_keeps_missing_measurements_null_and_compares_three_writers(self):
        def writer(kind, bug, usage, model, intent_extra=None):
            d = self.root / 'briefs' / kind / bug
            (d / 'reviewers/abc').mkdir(parents=True)
            (d / 'done.json').write_text(json.dumps({'fix_rounds': 1}))
            (d / 'launch-intent.json').write_text(json.dumps({'model': model, 'effort': 'high', 'time_ns': 10**18}))
            (d / 'reviewers/abc/usage.json').write_text(json.dumps(usage))

        turns = [{'completed_at_ms': 10**12 + 90_000}]  # 90 s after the launch intent below
        priced = {'turns': turns, 'turn_cost_usd_ticks': 5 * 10**10,
                  'coverage': {'estimated_cost_turns': 0, 'missing_cost_turns': 0},
                  'totals': {'total_tokens': {'tokens': 1234}}}
        # A Claude-style record: no cost, no coverage block and no token totals.
        bare = {'turns': turns}
        writer('blind', self.bugs[0], priced, 'gpt-6.1-sol')
        writer('blind-terra', self.bugs[0], priced, 'gpt-6.1-terra')
        writer('blind-sonnet', self.bugs[0], bare, 'sonnet')
        writer('blind-sonnet', self.bugs[1], bare, 'sonnet')
        out = [json.loads(line) for line in self.cli('usage', '--writer', 'sonnet').splitlines()]
        summary = {r['kind']: r for r in out}
        self.assertEqual(sorted(summary), ['blind', 'blind-sonnet', 'blind-terra'])
        sonnet = summary['blind-sonnet']
        self.assertEqual((sonnet['briefs'], sonnet['unknown_cost'], sonnet['total_cost_usd']), (2, 2, None))
        self.assertEqual(sonnet['median'], {'cost_usd': None, 'total_tokens': None, 'wall_s': 90.0, 'turns': 1})
        self.assertEqual(summary['blind']['median']['cost_usd'], 5.0)
        self.assertEqual(summary['blind']['median']['total_tokens'], 1234)
        rows = [json.loads(line) for line in (self.sonnet_results() / 'usage.jsonl').read_text().splitlines()]
        got = next(r for r in rows if r['kind'] == 'blind-sonnet' and r['bug'] == self.bugs[0])
        self.assertIsNone(got['cost_usd'])
        self.assertIsNone(got['total_tokens'])
        self.assertIsNone(got['cost_turns_missing'])
        self.assertEqual(got['wall_s'], 90.0)
        self.assertTrue(any(r.get('state') == 'not written' for r in rows))

        # A known subtotal cannot stand in for a complete price across writers or turns.
        first = self.root / 'briefs/blind-sonnet' / self.bugs[0] / 'reviewers/abc/usage.json'
        first.write_text(json.dumps(priced))
        partial = {**priced, 'coverage': {'estimated_cost_turns': 1, 'missing_cost_turns': 1}}
        second = self.root / 'briefs/blind-sonnet' / self.bugs[1] / 'reviewers/abc/usage.json'
        second.write_text(json.dumps(partial))
        summary = {r['kind']: r for r in map(json.loads, self.cli('usage', '--writer', 'sonnet').splitlines())}
        self.assertIsNone(summary['blind-sonnet']['total_cost_usd'])
        self.assertEqual(summary['blind-sonnet']['unknown_cost'], 1)
        self.assertEqual(summary['blind-sonnet']['known_cost_usd'], 10.0)
        rows = [json.loads(line) for line in (self.sonnet_results() / 'usage.jsonl').read_text().splitlines()]
        unknown = next(r for r in rows if r['kind'] == 'blind-sonnet' and r['bug'] == self.bugs[1])
        self.assertIsNone(unknown['cost_usd'])
        self.assertEqual(unknown['known_cost_subtotal_usd'], 5.0)

    def test_default_writer_is_terra_and_its_usage_leaves_out_sonnet(self):
        d = self.root / 'briefs/blind-sonnet' / self.bugs[0]
        (d / 'reviewers/abc').mkdir(parents=True)
        (d / 'done.json').write_text(json.dumps({'fix_rounds': 0}))
        self.assertEqual(self.cli('usage'), '')  # nothing written for Sol or Terra: no summary, no crash
        rows = [json.loads(line) for line in (self.scripts / 'results/brief-writer-terra/usage.jsonl').read_text().splitlines()]
        self.assertEqual({r['kind'] for r in rows}, {'blind', 'blind-terra'})
        self.assertFalse(self.sonnet_results().exists())

    def test_sonnet_contains_unit_pairs_sol_and_sonnet_briefs_without_a_writer_key(self):
        shared = self.root / 'briefs/bifrost-contains'
        shared.mkdir(parents=True)
        (shared / 'CONTAINS.md').write_text('rule\n')
        for kind in ('blind', 'blind-sonnet'):
            for bug in self.bugs:
                d = self.root / 'briefs' / kind / bug
                d.mkdir(parents=True, exist_ok=True)
                (d / 'brief.md').write_text(f'relations of {bug} written by {kind[-3:]}\n')
        for bug in self.bugs:
            (shared / f'item-{bug}.json').write_text(json.dumps({'census': []}))
        self.cli('contains-build', '--writer', 'sonnet')
        base = self.root / 'briefs/writer-contains-sonnet'
        unit = base / 'unit'
        items = sorted(unit.glob('item-*.json'))
        self.assertEqual(len(items), 44)
        key = json.loads((base / 'key.json').read_text())
        self.assertEqual({v['kind'] for v in key.values()}, {'blind', 'blind-sonnet'})
        self.assertEqual({v['bug'] for v in key.values()}, set(self.bugs))
        uploaded = ' '.join(p.read_text() for p in unit.iterdir())
        self.assertNotIn('blind-sonnet', uploaded)
        self.assertFalse((unit / 'key.json').exists())
        # The Terra study's own contains directory is untouched, and a second build refuses.
        self.assertFalse((self.root / 'briefs/writer-contains').exists())
        with self.assertRaisesRegex(SystemExit, 'already exists'):
            self.cli('contains-build', '--writer', 'sonnet')

    def test_sonnet_leak_round_is_bound_to_its_own_brief_hashes_and_dirs(self):
        self.briefs()
        for bug in self.bugs:
            (self.root / 'briefs/blind-sonnet' / bug / 'done.json').write_text('{"fix_rounds": 0}')
        self.cli('leak-build', '--writer', 'sonnet')
        d = self.root / 'briefs/blind-sonnet-leak-0'
        key = json.loads((d / 'key.json').read_text())
        self.assertEqual(len(key), 22)
        self.assertFalse((self.root / 'briefs/blind-terra-leak-0').exists())
        verdicts = [{'output_id': oid, 'bug': record['bug'], 'judge': j, 'label': 'missed'}
                    for oid, record in key.items() for j in 'AB']
        (d / 'workflow-out.json').write_text(json.dumps({'verdicts': verdicts}))
        # A brief edited after its leak judgment cannot reuse the verdict.
        (self.root / 'briefs/blind-sonnet' / self.bugs[0] / 'brief.md').write_text('changed after judging\n')
        with self.assertRaisesRegex(AssertionError, 'changed since leak round 0'):
            self.cli('leak-collect', '--writer', 'sonnet')
        (self.root / 'briefs/blind-sonnet' / self.bugs[0] / 'brief.md').write_text(f'{"blind-sonnet"} {self.bugs[0]}\n')
        out = self.cli('leak-collect', '--writer', 'sonnet')
        self.assertIn('"missed": 22', out)
        record = json.loads((self.root / 'briefs/blind-sonnet' / self.bugs[0] / 'blind-leak.json').read_text())
        self.assertEqual(record['rounds'][0]['label'], 'missed')


if __name__ == '__main__':
    unittest.main(verbosity=2)
