#!/usr/bin/env python3
"""Print, per input row of one repo, both variants' status, row count,
truncated flag, diagnostics (verbatim), and compact rows for judgment."""
import json, os, sys, glob
REPO = sys.argv[1]; maxrows = int(sys.argv[2]) if len(sys.argv) > 2 else 30
d = REPO.replace('/', '__'); S = f'/tmp/rql-census/run/{d}/out'
rows = [json.loads(l) for l in open('/tmp/rql-census/inputs/rql-measure/documented-queries.jsonl') if json.loads(l)['repo'] == REPO]
def compact(x):
    def rng(v):
        return f"L{v.get('start_line')}:{v.get('start_column')}-L{v.get('end_line')}:{v.get('end_column')}"
    def walk(v, depth=0):
        if isinstance(v, dict):
            if set(v) >= {'start_line', 'end_line'} and len(v) <= 4: return rng(v)
            o = {}
            for k, vv in v.items():
                if k in ('provenance', 'generation', 'language', 'site_id', 'program_point_id', 'target_id', 'signature_id', 'ast_id') or k == 'id' or k.endswith('_id') or k.endswith('_byte'): continue
                o[k] = walk(vv, depth + 1)
            return o
        if isinstance(v, list): return [walk(i, depth + 1) for i in v[:6]] + (['...%d more' % (len(v) - 6)] if len(v) > 6 else [])
        if isinstance(v, str) and len(v) > 160: return v[:160] + '...'
        return v
    return json.dumps(walk(x))[:900]
for r in rows:
    tag = f"{r['intro_commit'][:10]}-{r['id'][:60]}-s{r['set']}f{r['fact']}"
    print('=' * 100); print(r['id'], r['set'], r['fact'], r['intro_commit'][:10]); print('  Q', r['query']); print('  C', r['claim']); print('  S', r['sites'])
    for var in ('default', 'scaled'):
        base = f'{S}/{tag}.{var}'
        if not os.path.exists(base + '.rep'): print(f'  [{var}] NOT RUN'); continue
        rep = json.load(open(base + '.rep'))
        try: j = json.load(open(base + '.json'))
        except Exception as e: print(f'  [{var}] exit={rep["exit"]} wall={rep["wall"]} timed_out={rep["timed_out"]} NOJSON {e}; stderr tail:', open(base+'.err').read()[-300:]); continue
        if j.get('isError'): print(f'  [{var}] ERROR', j.get('error') or json.dumps(j)[:800]); continue
        s = j['structuredContent']; res = s.get('results', [])
        print(f'  [{var}] rows={len(res)} truncated={s.get("truncated")} wall={rep["wall"]} peak={rep["peak_rss_kib"]//1024}MiB')
        for x in s.get('diagnostics', []): print('     DIAG', x.get('code'), x.get('impact'), '|', x.get('message'))
        if var == 'default' or True:
            for x in res[:maxrows]: print('     ROW', compact(x))
