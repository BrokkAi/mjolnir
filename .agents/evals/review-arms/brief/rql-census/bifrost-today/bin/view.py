#!/usr/bin/env python3
"""One line per row. Usage: view.py <json> [line,line,...]  (only rows whose any range covers a listed line)."""
import json, sys
d = json.load(open(sys.argv[1]))
if d.get('isError'): print('ERROR', d.get('error')); sys.exit()
s = d['structuredContent']; rows = s.get('results', [])
lines = [int(x) for x in sys.argv[2].split(',')] if len(sys.argv) > 2 and sys.argv[2] else []
def R(v): return f"{v['start_line']}-{v['end_line']}" if isinstance(v, dict) and 'start_line' in v else '?'
def ranges(x, acc):
    if isinstance(x, dict):
        if 'start_line' in x and 'end_line' in x: acc.append((x['start_line'], x['end_line']))
        for k, v in x.items():
            if k != 'provenance': ranges(v, acc)
    elif isinstance(x, list):
        for v in x: ranges(v, acc)
    return acc
print(f"rows={len(rows)} truncated={s.get('truncated')}")
for i, x in enumerate(rows):
    if lines and not any(a <= l <= b for (a, b) in ranges(x, []) for l in lines if b - a < 40): continue
    t = x.get('result_type'); parts = [f'#{i}', t, x.get('path', ''), R(x.get('range'))]
    for k in ('relation', 'certainty', 'kind', 'fq_name', 'name', 'label', 'declared_type', 'effect', 'effect_kind', 'call_kind', 'input_kind', 'parameter_index', 'parameter_name', 'completeness', 'proof'):
        if k in x and not isinstance(x[k], (dict, list)): parts.append(f'{k}={x[k]}')
    for k in ('source', 'target', 'guard', 'callee', 'caller', 'site', 'value', 'write', 'written', 'subject', 'receiver_site', 'argument', 'declaration', 'field', 'member'):
        v = x.get(k)
        if isinstance(v, dict):
            sub = {kk: vv for kk, vv in v.items() if kk in ('fq_name', 'boundary', 'text', 'kind', 'name', 'value', 'start_line')}
            if 'range' in v: sub['range'] = R(v['range'])
            if 'site' in v and isinstance(v['site'], dict): sub['text'] = v['site'].get('text', '')[:120]; sub['range'] = f"{v['site'].get('start_line')}-{v['site'].get('end_line')}"
            parts.append(f'{k}={json.dumps(sub)[:260]}')
    if 'text' in x: parts.append('text=' + json.dumps(x['text'][:200]))
    print(' '.join(str(p) for p in parts))
