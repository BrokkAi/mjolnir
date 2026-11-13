#!/usr/bin/env python3
"""Merge blind AFK-question labels and prepare an opaque adjudication queue."""
import json
import secrets
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path('/mnt/optane/tmp/glide-afk-locator-20261113/afk')
LABELS = ROOT / 'labels'
OUT = LABELS / 'out'
ADJ = LABELS / 'adjudication'
PRIVATE = LABELS / 'private'


def read_jsonl(path):
    with path.open(encoding='utf-8') as stream:
        return [json.loads(line) for line in stream if line.strip()]


def write_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('w', encoding='utf-8') as stream:
        for row in rows:
            stream.write(json.dumps(row, ensure_ascii=False) + '\n')


def indexed(path):
    rows = read_jsonl(path)
    result = {row['case_id']: row for row in rows}
    if len(result) != len(rows):
        raise ValueError(f'duplicate case_id in {path}')
    return result


def main():
    construction = indexed(LABELS / 'construction.jsonl')
    a = indexed(OUT / 'queue_double.A.jsonl')
    b = indexed(OUT / 'queue_double.B.jsonl')
    c = indexed(OUT / 'queue_single.C.jsonl')
    double_queue = indexed(LABELS / 'queue_double.jsonl')
    single_queue = indexed(LABELS / 'queue_single.jsonl')
    cases = indexed(ROOT / 'cases.jsonl')
    if len(a) != 109 or len(b) != 109 or len(c) != 159:
        raise ValueError(f'unexpected label counts A={len(a)} B={len(b)} C={len(c)}')

    adjudicate = {}
    final = {}
    rates = defaultdict(Counter)
    reason_counts = Counter()
    repeat_mismatches = []
    for case_id in a:
        if case_id not in b or case_id not in double_queue or case_id not in cases:
            raise ValueError(f'double queue mismatch at {case_id}')
        kind = construction.get(case_id, {}).get('kind', 'real')
        la, lb = a[case_id]['label'], b[case_id]['label']
        if la not in {'same', 'different', 'unclear'} or lb not in {'same', 'different', 'unclear'}:
            raise ValueError(f'invalid double label for {case_id}')
        rates[kind]['total'] += 1
        if la == lb:
            rates[kind]['agreements'] += 1
        needs = False
        if la != lb:
            reason_counts['A/B disagreement'] += 1
            needs = True
        if la == 'unclear' or lb == 'unclear':
            reason_counts['unclear label'] += 1
            needs = True
        if la == lb and la != 'unclear':
            if kind == 'repeat_plus_new' and la != construction[case_id]['label']:
                reason_counts['repeat_plus_new vs construction'] += 1
                repeat_mismatches.append((case_id, la, construction[case_id]['label']))
                needs = True
            else:
                final[case_id] = {'case_id': case_id, 'label': la, 'source': 'A+B agreement'}
        if needs:
            adjudicate[case_id] = {'reason_codes': [name for name, cond in (
                ('A/B disagreement', la != lb),
                ('unclear label', la == 'unclear' or lb == 'unclear'),
                ('repeat_plus_new vs construction', la == lb and kind == 'repeat_plus_new' and la != construction[case_id]['label']),
            ) if cond]}

    single_mismatches = []
    for case_id, row in c.items():
        if case_id not in single_queue or case_id not in construction:
            raise ValueError(f'single queue mismatch at {case_id}')
        label = row['label']
        truth = construction[case_id]['label']
        if label == truth:
            final[case_id] = {'case_id': case_id, 'label': label, 'source': 'C+construction agreement'}
        else:
            reason_counts['C vs construction'] += 1
            adjudicate[case_id] = {'reason_codes': ['C vs construction']}
            single_mismatches.append((case_id, label, row.get('reason', ''), construction[case_id]['kind']))

    # Queue state comes from the blind queue; opaque identifiers and their map stay private.
    ids = {}
    queue = []
    for case_id in sorted(adjudicate):
        opaque = secrets.token_hex(12)
        while opaque in ids:
            opaque = secrets.token_hex(12)
        ids[opaque] = case_id
        queue.append({'case_id': opaque, 'state': double_queue.get(case_id, single_queue.get(case_id))['state']})
    ADJ.mkdir(parents=True, exist_ok=True)
    write_jsonl(ADJ / 'queue.jsonl', queue)
    PRIVATE.mkdir(parents=True, exist_ok=True)
    (PRIVATE / 'adjudication-id-map.json').write_text(json.dumps(ids, indent=2) + '\n', encoding='utf-8')
    write_jsonl(LABELS / 'final-agreed.jsonl', [final[k] for k in sorted(final)])

    instruction = (ROOT / 'LABELING-INSTRUCTIONS.md').read_text(encoding='utf-8')
    rules = '''\n## Settled rules\n\n- An added or removed option, or a replaced option set, is a materially changed choice set, so the case is different. A reworded option or a changed option description is not.\n- Single-select versus multi-select is a changed constraint, so the case is different.\n- A new decision that an earlier form mentioned only inside an option description is still a new decision, so the case is different.\n- The same decision with more or less scope stated in its text, but with unchanged options and constraints, is same.\n'''
    (ADJ / 'INSTRUCTIONS.md').write_text(instruction.rstrip() + '\n' + rules, encoding='utf-8')

    total = sum(x['total'] for x in rates.values())
    agrees = sum(x['agreements'] for x in rates.values())
    lines = [f'Double agreement: {agrees}/{total} ({agrees/total:.1%})']
    for kind in sorted(rates):
        x = rates[kind]
        lines.append(f'  {kind}: {x["agreements"]}/{x["total"]} ({x["agreements"]/x["total"]:.1%})')
    lines.append('Adjudication reasons: ' + ', '.join(f'{k}={v}' for k, v in sorted(reason_counts.items())))
    lines.append('Single C/construction mismatches:')
    lines.extend(f'  {cid} | C={label} | construction_kind={kind} | reason={reason}' for cid, label, reason, kind in single_mismatches)
    lines += [f'Adjudication cases: {len(queue)}', f'Settled cases: {len(final)}']
    report = '\n'.join(lines) + '\n'
    (ADJ / 'merge-report.txt').write_text(report, encoding='utf-8')
    print(report, end='')


if __name__ == '__main__':
    main()
