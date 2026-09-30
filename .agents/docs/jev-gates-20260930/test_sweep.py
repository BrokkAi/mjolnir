#!/usr/bin/env python3
"""Offline checks for probability gates, action isolation, and grouped selection."""
import unittest
from compare import action
from sweep import accepts, candidates, choose, counts, features, folds


class GateChecks(unittest.TestCase):
    def answer(self, choice, confidence, probabilities):
        return {'choice': choice, 'confidence': confidence, 'probabilities': probabilities}

    def test_ratio_boundary_and_absolute_floor(self):
        ratio = {'family': 'ratio', 'ratio': 3, 'floor': .6}
        f = features(self.answer('yes', .42, {'yes': .6, 'no': .2, 'unknown': .2}))
        self.assertTrue(accepts(f, ratio))
        self.assertFalse(accepts(f, {'family': 'confidence', 'cutoff': .6}))
        diluted = features(self.answer('a', .2, {'a': .4, 'b': .2, 'c': .2, 'd': .2}))
        self.assertTrue(accepts(diluted, {'family': 'ratio', 'ratio': 2, 'floor': 0}))
        self.assertFalse(accepts(diluted, {'family': 'ratio', 'ratio': 2, 'floor': .5}))

    def test_input_only_gate_does_not_relax_finished_inference(self):
        answers = {
            'failure': self.answer('none', 1, {'none': 1, 'other': 0}),
            'input': self.answer('none', .99, {'none': .99, 'required': .01}),
            'work': self.answer('finished', .62, {'finished': .72, 'authorized_unfinished': .24, 'waiting': .04}),
        }
        record = {'answers': answers, 'verdict': {k: {'choice': a['choice'], 'confidence': a['confidence']} for k, a in answers.items()}, 'action': 'uncertain'}
        fixture = {'evidence': {}}
        self.assertEqual(action(record, fixture, 2, .5, False, input_only=True), 'uncertain')
        self.assertEqual(action(record, fixture, 2, .5, False), 'finished')

    def test_related_synthetic_cases_and_sessions_stay_in_one_fold(self):
        fixtures = {}
        for i in range(12):
            for suffix in ('a', 'b'):
                identity = f'{i}{suffix}'
                fixtures[identity] = {'id': identity, 'source': {'session': f'session-{i}'}, 'expected': {'input': 'required' if i % 2 else 'none', 'action': 'finished'}}
        for i in range(3):
            fixtures[f'H{i}'] = {'id': f'H{i}', 'source': {}, 'expected': {'input': 'none', 'action': 'finished'}}
        assignment, loads = folds(fixtures)
        self.assertEqual(len(assignment), 13)
        self.assertIn('synthetic-H', assignment)
        self.assertEqual(sum(load['size'] for load in loads), len(fixtures))
        self.assertEqual(len(set(assignment.values())), 5)

    def test_budget_selection_reports_when_no_setting_can_meet_it(self):
        row = {'input_eligible': True, 'input_positive': False, 'choices': {'input': 'required'}, 'features': {'input': {'confidence': 1, 'probability': 1, 'runner_up': 0, 'concentration': 1}}}
        gate, score, met = choose([row], candidates(), 'input', 0)
        self.assertFalse(met)
        self.assertEqual(score['fp'], 1)
        self.assertEqual(counts([row], gate, 'input')['negatives'], 1)


if __name__ == '__main__':
    unittest.main()
