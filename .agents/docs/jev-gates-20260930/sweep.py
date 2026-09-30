#!/usr/bin/env python3
"""Offline gate sweep with source-session grouped cross-validation."""
import argparse
import collections
import hashlib
import json
import math
from pathlib import Path

from compare import evaluator, legacy_action


def candidates():
    gates = [{'name': 'current', 'family': 'confidence', 'cutoff': .85}]
    for cutoff in [.3, .4, .5, .55, .6, .65, .7, .75, .8, .9, .95]:
        gates.append({'name': f'confidence_{cutoff:g}', 'family': 'confidence', 'cutoff': cutoff})
    for cutoff in [.4, .5, .55, .6, .65, 2/3, .7, .75, .8, .85, .9, .95]:
        gates.append({'name': f'probability_{cutoff:.6g}', 'family': 'probability', 'cutoff': cutoff})
    for cutoff in [.1, .15, .2, .25, .3, .4, .5, .6, .7]:
        gates.append({'name': f'gap_{cutoff:g}', 'family': 'gap', 'cutoff': cutoff})
    for cutoff in [.2, .3, .4, .5, .6, .7, .8, .9]:
        gates.append({'name': f'concentration_{cutoff:g}', 'family': 'concentration', 'cutoff': cutoff})
    for ratio in [1.5, 2, 2.5, 3, 4, 5, 8, 10]:
        for floor in [0, .5, .6, .65, .7, .75, .8, .85, .9]:
            gates.append({'name': f'ratio_{ratio:g}_floor_{floor:g}', 'family': 'ratio', 'ratio': ratio, 'floor': floor})
    return gates


def features(answer):
    probabilities = answer['probabilities']
    total = sum(probabilities.values())
    # Normalize only for entropy, to account for the API's rounded distribution.
    ps = [p / total for p in probabilities.values() if p]
    entropy = -sum(p * math.log(p) for p in ps) / math.log(len(probabilities))
    return {
        'confidence': answer['confidence'],
        'probability': probabilities[answer['choice']],
        'runner_up': max(p for k, p in probabilities.items() if k != answer['choice']),
        'concentration': 1 - entropy,
    }


def accepts(f, gate):
    if gate['family'] == 'ratio':
        return f['probability'] + 1e-12 >= gate['floor'] and f['probability'] + 1e-12 >= gate['ratio'] * f['runner_up']
    value = f['probability'] - f['runner_up'] if gate['family'] == 'gap' else f[gate['family']]
    return value + 1e-12 >= gate['cutoff']


def group(fixture):
    # H01/H02/H03 are related synthetic variants, not independent sessions.
    return fixture['source'].get('session') or 'synthetic-H'


def load(run):
    fixtures = {f['id']: f for f in json.loads((run / 'fixtures.json').read_text())}
    rows = []
    records = [json.loads(line) for line in (run / 'results.jsonl').read_text().splitlines()]
    assert len(records) == len(fixtures) * 3
    assert len({(r['id'], r['repeat']) for r in records}) == len(records)
    for r in records:
        assert 'error' not in r
        f = fixtures[r['id']]
        assert r['action'] == legacy_action(r['verdict'], evaluator.authorization_complete(evaluator.fit_to_wire(f['evidence'])))
        strict = f.get('context', {}).get('strict_input_scoring', True)
        e = f['expected']
        finished = e['action'] == 'finished'
        finish_control = 'finished' in e.get('wrong_actions', [])
        rows.append({
            'id': f['id'], 'repeat': r['repeat'], 'group': group(f),
            'features': {a: features(r['answers'][a]) for a in ('failure', 'input', 'work')},
            'choices': {a: r['answers'][a]['choice'] for a in ('failure', 'input', 'work')},
            'input_eligible': strict,
            'input_positive': e['input'] == 'required',
            'finish_eligible': strict and f['evidence']['phase'] == 'replied' and (finished or finish_control),
            'finish_positive': finished,
        })
    return fixtures, rows


def prediction(row, gate, mode):
    c, f = row['choices'], row['features']
    if mode == 'input':
        return c['input'] == 'required' and accepts(f['input'], gate)
    return (
        c['failure'] == 'none' and f['failure']['confidence'] >= .90
        and c['input'] == 'none' and c['work'] == 'finished'
        and (accepts(f['input'], gate) if mode == 'finish_both' else f['input']['confidence'] >= .85)
        and accepts(f['work'], gate)
    )


def counts(rows, gate, mode):
    kind = 'input' if mode == 'input' else 'finish'
    result = collections.Counter(tp=0, fp=0, positives=0, negatives=0)
    for r in rows:
        if not r[kind + '_eligible']:
            continue
        positive = r[kind + '_positive']
        result['positives' if positive else 'negatives'] += 1
        if prediction(r, gate, mode):
            result['tp' if positive else 'fp'] += 1
    return dict(result)


def folds(fixtures):
    groups = collections.defaultdict(list)
    for f in fixtures.values():
        groups[group(f)].append(f)
    assignment, loads = {}, [collections.Counter() for _ in range(5)]
    def weights(fs):
        return collections.Counter(
            required=sum(f['expected']['input'] == 'required' and f.get('context', {}).get('strict_input_scoring', True) for f in fs),
            finished=sum(f['expected']['action'] == 'finished' for f in fs),
            size=len(fs),
        )
    order = sorted(groups, key=lambda g: (-weights(groups[g])['required'], -len(groups[g]), hashlib.sha256(g.encode()).hexdigest()))
    for g in order:
        w = weights(groups[g])
        index = min(range(5), key=lambda i: (loads[i]['required'] if w['required'] else 0, loads[i]['finished'] if w['finished'] else 0, loads[i]['size'], i))
        assignment[g] = index
        loads[index].update(w)
    return assignment, [dict(l) for l in loads]


def choose(rows, gates, mode, budget):
    scored = [(gate, counts(rows, gate, mode)) for gate in gates]
    allowed = [(g, c) for g, c in scored if c['fp'] <= budget * c['negatives'] + 1e-12]
    if not allowed:
        # Some labels are confidently wrong for every tested threshold. Report
        # that impossibility and select the least-wrong setting, not a fake pass.
        minimum = min(c['fp'] for _, c in scored)
        allowed = [(g, c) for g, c in scored if c['fp'] == minimum]
    gate, score = min(allowed, key=lambda gc: (-gc[1]['tp'], gc[1]['fp'], gc[0]['name'] != 'current', gc[0]['family'] == 'ratio', gc[0]['name']))
    return gate, score, score['fp'] <= budget * score['negatives'] + 1e-12


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('run', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    fixtures, rows = load(args.run)
    gates = candidates()
    assignment, loads = folds(fixtures)
    assert len(set(assignment.values())) == 5
    modes = ['input', 'finish_work', 'finish_both']
    budgets = [0, .005, .01, .025, .05]
    result = {'run': str(args.run), 'gate_count': len(gates), 'gates': gates, 'fold_assignment': assignment, 'fold_loads': loads, 'modes': {}}
    for mode in modes:
        full = {g['name']: counts(rows, g, mode) for g in gates}
        frontier = []
        for g in gates:
            c = full[g['name']]
            dominated = any(o['tp'] >= c['tp'] and o['fp'] <= c['fp'] and (o['tp'] > c['tp'] or o['fp'] < c['fp']) for o in full.values())
            if not dominated:
                frontier.append({'gate': g['name'], **c})
        cv = []
        for budget in budgets:
            pooled = collections.Counter()
            selections = []
            for fold in range(5):
                train = [r for r in rows if assignment[r['group']] != fold]
                test = [r for r in rows if assignment[r['group']] == fold]
                assert {r['group'] for r in train}.isdisjoint(r['group'] for r in test)
                gate, train_score, met = choose(train, gates, mode, budget)
                test_score = counts(test, gate, mode)
                pooled.update(test_score)
                selections.append({'fold': fold, 'gate': gate['name'], 'training': train_score, 'training_budget_met': met, 'validation': test_score})
            cv.append({'training_false_positive_budget': budget, 'pooled_validation': dict(pooled), 'folds': selections})
        result['modes'][mode] = {'full': full, 'frontier': frontier, 'cross_validation': cv}
        print(mode, 'baseline', full['current'])
        print('frontier', sorted({(r['tp'], r['fp']) for r in frontier}))
        for item in cv:
            print('CV', item['training_false_positive_budget'], item['pooled_validation'], [f['gate'] for f in item['folds']], 'budgets_met', all(f['training_budget_met'] for f in item['folds']))
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
