#!/usr/bin/env python3
"""Compare gates on frozen responses; never sends API requests or changes runtime policy."""
import argparse
import collections
import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('jev_eval', ROOT / 'scripts/jev-scenarios-eval.py')
evaluator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evaluator)


def passes(answer, ratio, floor):
    probabilities = answer['probabilities']
    winner = probabilities[answer['choice']]
    runner_up = max(p for name, p in probabilities.items() if name != answer['choice'])
    return winner + 1e-12 >= floor and winner + 1e-12 >= ratio * runner_up


def action(record, fixture, ratio, floor, all_gates, input_only=False):
    # Reuse the existing action policy; replace only the confidence predicates
    # under comparison. No proxy, process, or runtime facts are modified.
    verdict = json.loads(json.dumps(record['verdict']))
    answers = record['answers']
    if all_gates:
        for axis in ('failure', 'input', 'work'):
            verdict[axis]['confidence'] = float(passes(answers[axis], ratio, floor))
        return evaluator.action(verdict, evaluator.authorization_complete(evaluator.fit_to_wire(fixture['evidence'])))
    if verdict['input']['choice'] == 'required' and passes(answers['input'], ratio, floor):
        return 'await_input'
    if input_only:
        if verdict['input']['choice'] == 'required':
            verdict['input']['confidence'] = 0
        return evaluator.action(verdict, evaluator.authorization_complete(evaluator.fit_to_wire(fixture['evidence'])))
    # Keep recovery and automatic continuation gates exactly as shipped.
    if record['action'] in ('retry_provider', 'recover_quota', 'continue'):
        return record['action']
    failure = verdict['failure']
    if failure['confidence'] >= .90 and failure['choice'] == 'other':
        return 'await_input'
    if failure['choice'] != 'none' or failure['confidence'] < .90:
        return 'uncertain'
    if verdict['input']['choice'] == 'none' and passes(answers['input'], ratio, floor) and passes(answers['work'], ratio, floor):
        return {'finished': 'finished', 'waiting': 'wait'}.get(verdict['work']['choice'], 'uncertain')
    return 'uncertain'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('run', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    fixtures = {f['id']: f for f in json.loads((args.run / 'fixtures.json').read_text())}
    records = [json.loads(line) for line in (args.run / 'results.jsonl').read_text().splitlines()]
    assert len({(r['id'], r['repeat']) for r in records}) == len(records)
    for r in records:
        if 'error' in r:
            raise ValueError(f"Incomplete comparison: {r['id']} {r['error']}")
        for axis in ('failure', 'input', 'work'):
            a = r['answers'][axis]
            probabilities = a['probabilities']
            assert set(probabilities) == set(json.loads((args.run / 'questions.json').read_text())[axis]['criteria'])
            assert all(0 <= p <= 1 for p in probabilities.values())
            assert abs(sum(probabilities.values()) - 1) <= .031  # API rounds to hundredths.
            assert probabilities[a['choice']] == max(probabilities.values())
    policies = [('current', None, 0, False), ('ratio2_activity', 2, 0, False), ('ratio2_floor50_activity', 2, .5, False), ('ratio3_floor60_activity', 3, .6, False), ('ratio4_floor70_activity', 4, .7, False), ('probability_two_thirds_activity', 0, 2/3, False), ('probability80_activity', 0, .8, False), ('ratio2_all', 2, 0, True)]
    policies += [('ratio2_input_only', 2, 0, False), ('ratio3_floor60_input_only', 3, .6, False)]
    result = {'run': str(args.run), 'requests': len(records), 'manifest': json.loads((args.run / 'run.json').read_text()), 'policies': {}}
    result['manifest'].pop('evidence_sha256', None)
    for name, ratio, floor, all_gates in policies:
        counts = collections.Counter()
        cases = collections.defaultdict(collections.Counter)
        for r in records:
            f = fixtures[r['id']]
            e = f['expected']
            a = r['action'] if ratio is None else action(r, f, ratio, floor, all_gates, name.endswith('_input_only'))
            required = r['verdict']['input']['choice'] == 'required' and (r['verdict']['input']['confidence'] >= .85 if ratio is None else passes(r['answers']['input'], ratio, floor))
            if f.get('context', {}).get('strict_input_scoring', True):
                if e['input'] == 'required':
                    counts['required_total'] += 1
                    counts['required_detected'] += required
                    if not required:
                        cases['required_misses'][r['id']] += 1
                else:
                    counts['controls_total'] += 1
                    counts['false_input'] += required
                    if required:
                        cases['false_input'][r['id']] += 1
            else:
                counts['ambiguous'] += 1
            if e['action'] == 'finished':
                counts['finished_total'] += 1
                counts['finished_correct'] += a == 'finished'
            counts['correct_actions'] += a == e['action']
            counts['uncertain'] += a == 'uncertain'
            counts['wrong_actions'] += a in e.get('wrong_actions', [])
            counts['wrong_finished'] += a == 'finished' and 'finished' in e.get('wrong_actions', [])
            counts['wrong_finished_replied'] += a == 'finished' and 'finished' in e.get('wrong_actions', []) and f['evidence']['phase'] == 'replied'
            counts['wrong_continue'] += a == 'continue' and 'continue' in e.get('wrong_actions', [])
            if a in e.get('wrong_actions', []):
                cases['wrong_actions'][f"{r['id']}:{a}"] += 1
            cases['actions'][f"{r['id']}:{a}"] += 1
        result['policies'][name] = {'counts': dict(counts), 'cases': {k: dict(v) for k, v in cases.items()}}
        print(name, dict(counts))
        print('  false input:', dict(cases['false_input']), 'wrong actions:', dict(cases['wrong_actions']))
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
