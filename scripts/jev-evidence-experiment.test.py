"""Behavior checks for offline evidence preparation, without network requests."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('experiment', Path(__file__).with_name('jev-evidence-experiment.py'))
e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e)


def fixture():
    items = []
    for position, body in enumerate([
        {'kind': 'user', 'content': [{'type': 'text', 'text': 'Old request'}]},
        {'kind': 'agent', 'chunks': [{'content': {'type': 'text', 'text': 'OLD_REPLY'}}], 'streaming': False},
        {'kind': 'user', 'content': [{'type': 'text', 'text': 'Current request'}]},
        {'kind': 'tool', 'call': {'toolCallId': 'call', 'title': 'build', 'status': 'completed', 'rawInput': {'command': 'NOISE' * 20000}}},
        {'kind': 'agent', 'chunks': [{'content': {'type': 'text', 'text': 'Done.'}}], 'streaming': False},
    ], 1):
        items.append({'stable_id': str(position), 'position': position, 'latest_content_event_ordinal': None,
                      'created_at_ms': 100, 'last_changed_at_ms': 100, 'body': body})
    return {'id': 'fixture', 'session_id': 'fixture', 'expected': 'finished', 'split': 'synthetic',
            'rationale': 'Explicit completion', 'provenance': 'Synthetic', 'evaluation_time_ms': 100,
            'latest_user_position': 3,
            'facts': {'harness': 'codex', 'phase': 'replied', 'silent_for_s': 0,
                      'tools_in_flight': [], 'background_commands': 0, 'queued_commands': 1},
            'snapshot': {'event_frontier': 5, 'event_frontier_digest': '',
                         'session': {'execution': {'state': 'idle'}, 'last_activity_at_ms': 100,
                                     'session_title': None, 'configuration': {}},
                         'transcript': items, 'queued_prompts': [{'command_id':'queued', 'content':[{'type':'text','text':'FUTURE_REQUEST'}], 'queued_at_ms':100}]}}


class EvidenceTests(unittest.TestCase):
    def test_filtering_keeps_current_messages_and_identical_runtime_facts(self):
        case = fixture()
        original = copy.deepcopy(case)
        scoped = e.classification_payload(case, 'scoped')['state']
        filtered = e.classification_payload(case, 'no_tools')['state']
        self.assertNotIn('OLD_REPLY', scoped['transcript_summary'])
        self.assertNotIn('FUTURE_REQUEST', scoped['transcript_summary'])
        self.assertEqual(scoped['user_prompt_tail'], 'Current request')
        self.assertNotIn('NOISE', filtered['transcript_summary'])
        self.assertIn('Current request', filtered['transcript_summary'])
        self.assertIn('Done.', filtered['transcript_summary'])
        for field, value in case['facts'].items():
            self.assertEqual(scoped[field], value)
            self.assertEqual(filtered[field], value)
        self.assertEqual(case, original)
        self.assertLessEqual(len(e.encoded(e.classification_payload(case, 'scoped'))), e.MAX_REQUEST)

    def test_selector_candidates_exclude_calls_removed_by_budget(self):
        case = fixture()
        snapshot = case['snapshot']
        snapshot['transcript'] = snapshot['transcript'][:3]
        for index in range(20):
            snapshot['transcript'].append({
                'stable_id': f'tool-{index}', 'position': index + 4,
                'latest_content_event_ordinal': None, 'created_at_ms': 100,
                'last_changed_at_ms': 100,
                'body': {'kind': 'tool', 'call': {'toolCallId': f'call-{index}',
                         'title': f'operation-{index}', 'status': 'completed',
                         'rawOutput': 'BODY' * 20000}},
            })
        snapshot['event_frontier'] = 23
        view = e.project(snapshot, 'scoped', limit=2000)
        ids = {candidate['id'] for candidate in view['candidates']}
        self.assertIn('tool-19', ids)
        self.assertNotIn('tool-0', ids)
        self.assertEqual(len(ids), 8)
        self.assertLessEqual(len(view['text'].encode()), 2000)

    def test_unknown_runtime_is_explicit_and_never_replaced_with_zero(self):
        case = fixture()
        case['facts']['background_commands'] = None
        with self.assertRaises(AssertionError):
            e.validate_case(case)
        case['unknown_facts'] = ['background_commands']
        e.validate_case(case)
        for mode in ('baseline', 'scoped', 'no_tools'):
            state = e.classification_payload(case, mode)['state']
            self.assertIsNone(state['background_commands'])
            self.assertEqual(state['evidence_availability']['unknown_fields'], ['background_commands'])

    def test_later_materialized_update_is_rejected(self):
        case = fixture()
        case['snapshot']['transcript'][-1]['last_changed_at_ms'] = 101
        with self.assertRaises(AssertionError):
            e.validate_case(case)

    def test_selector_rejects_unknown_ids_and_invalid_scores(self):
        candidates = [{'id': 'a'}, {'id': 'b'}]
        kept, scores = e.selected_ids({'answers': {'tool_0': {'noul': .5}, 'tool_1': {'noul': .49}}}, candidates)
        self.assertEqual(kept, ['a'])
        self.assertEqual(len(scores), 2)
        for answers in ({'tool_2': {'noul': 1}}, {'tool_0': {'noul': float('nan')}, 'tool_1': {'noul': 1}}):
            with self.assertRaises(ValueError):
                e.selected_ids({'answers': answers}, candidates)

    def test_phase_and_threshold_preserve_production_actions(self):
        self.assertEqual(e.decision('running', 'finished', 1), 'keep_current')
        self.assertEqual(e.decision('running', 'user', .85), 'awaiting_input')
        self.assertEqual(e.decision('replied', 'background_work', .85), 'expect_continuation')
        self.assertEqual(e.decision('replied', 'finished', .84), 'keep_current')

    def test_ambiguous_labels_are_excluded_from_accuracy_not_hidden(self):
        base = {'variant': '0', 'split': 'development', 'phase': 'replied',
                'expected': 'finished', 'choice': 'background_work', 'confidence': .99,
                'decision': 'expect_continuation', 'expected_decision': 'infer_idle',
                'latency_s': 1, 'stages': []}
        ambiguous = dict(base, label_ambiguous=True)
        result = e.summarize([base, ambiguous], 'all_real', '0')
        self.assertEqual(result['n'], 2)
        self.assertEqual(result['ambiguous'], 1)
        self.assertEqual(result['scored'], 1)
        self.assertEqual(result['wrong_high_confidence'], 1)
        self.assertEqual(result['wrong_action'], 1)

    def test_utf8_tail_is_bounded(self):
        self.assertLessEqual(len(e.tail('💡' * 1000, 1023).encode()), 1023)


if __name__ == '__main__':
    unittest.main()
