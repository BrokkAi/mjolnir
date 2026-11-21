#!/usr/bin/env python3
"""Assemble report/measurements.jsonl from run outputs (default limits = primary
variant) plus judgments.json {"repo|id|set|fact": {"decisive_row_present": bool,
"decisive_row": str|null, "notes": str, "status": optional override, "query_run": optional}}."""
import json, os
BC = '683ae81b014143806991cac731edbfd7523dfe1f'
J = json.load(open('/tmp/rql-census/judgments.json')) if os.path.exists('/tmp/rql-census/judgments.json') else {}
OV = json.load(open('/tmp/rql-census/overrides.json')) if os.path.exists('/tmp/rql-census/overrides.json') else {}
out = []
for r in map(json.loads, open('/tmp/rql-census/inputs/rql-measure/documented-queries.jsonl')):
    d = r['repo'].replace('/', '__'); S = f'/tmp/rql-census/run/{d}/out'
    tag = f"{r['intro_commit'][:10]}-{r['id'][:60]}-s{r['set']}f{r['fact']}"
    k = f"{r['repo']}|{r['id']}|{r['set']}|{r['fact']}"
    okey = f"{r['id']}|{r['set']}|{r['fact']}"
    j = J.get(k, {})
    m = dict(repo=r['repo'], id=r['id'], set=r['set'], fact=r['fact'], intro_commit=r['intro_commit'],
             bifrost_commit=BC, query_labelled=r['query'], query_run=j.get('query_run', OV.get(okey, r['query'])),
             status=None, row_count=None, decisive_row_present=None, decisive_row=None, complete=None,
             warnings=[], scaled_limits=None, possible_fail_open=j.get('possible_fail_open'), notes=j.get('notes', ''))
    def load(var):
        base = f'{S}/{tag}.{var}'
        if not os.path.exists(base + '.rep'): return None
        rep = json.load(open(base + '.rep'))
        try: res = json.load(open(base + '.json'))
        except Exception: res = None
        return rep, res
    dflt = load('default')
    if j.get('status') == 'unmappable':
        m.update(status='unmappable', query_run=None)
    elif dflt is None:
        ib = f"{S}/{r['intro_commit'][:10]}.index.rep"
        if os.path.exists(ib):
            irep = json.load(open(ib))
            if irep['cap_kill'] or irep['oom_kill_delta'] or irep['timed_out'] or irep['exit'] != 0:
                m.update(status='index_failed', index_wall_s=irep['wall'], index_peak_rss_gib=round(irep['peak_rss_kib'] / 1048576, 2))
                if irep['cap_kill']: m['rss_at_kill_gib'] = round(irep['rss_at_kill_kib'] / 1048576, 2); m['warnings'] = ['cap-kill: index process tree RSS reached 48 GiB']
                elif irep['oom_kill_delta']: m['warnings'] = ['container oom_kill during index']
                elif irep['timed_out']: m['status'] = 'index_timeout'; m['warnings'] = [f"index not ready after {irep['wall']} s (boundary 7200 s)"]
                else: m['warnings'] = [f"index exited {irep['exit']}: " + open(f"{S}/{r['intro_commit'][:10]}.index.json").read()[-300:]]
    else:
        rep, res = dflt
        if rep['cap_kill'] or rep['oom_kill_delta']:
            m.update(status='index_failed', rss_at_kill_gib=round((rep['rss_at_kill_kib'] or 0) / 1048576, 2),
                     notes=(m['notes'] + ' cap-kill' if rep['cap_kill'] else m['notes'] + ' container oom_kill').strip())
        elif rep['timed_out']:
            m.update(status='timeout', elapsed_s=rep['wall'], warnings=[f"timeout: query_code still executing at {rep['wall']} s (boundary 1800 s), killed by harness"])
        elif res is None:
            m.update(status='query_error', warnings=[f"no JSON output, exit {rep['exit']}"])
        elif res.get('isError'):
            err = res.get('error') or json.dumps(res)
            m.update(status='query_error', warnings=[err])
            if 'dependency fact retains its source value' in err:
                bt = f'{S}/{tag}.default.err'
                if not (os.path.exists(bt) and 'stack backtrace' in open(bt, errors='replace').read()): bt = f'{S}/{tag}.default.backtrace.txt'
                m['notes'] = (m['notes'] + f" route_kinds.rs:973 panic; query: {m['query_run']}; backtrace: " + (bt if os.path.exists(bt) else '/tmp/rql-census/report/evidence/panic-route_kinds-973-prek-48a9-call-sites-to-git_cmd.backtrace.txt (representative)')).strip()
        else:
            s = res['structuredContent']; rows = s.get('results', [])
            diags = s.get('diagnostics', [])
            warn = [f"{x.get('code')} [{x.get('impact')}]: {x.get('message')}" for x in diags]
            if s.get('truncated'): warn.insert(0, 'truncated: true')
            incomplete = bool(s.get('truncated')) or any(x.get('impact') == 'incomplete' for x in diags)
            m.update(status='ran', row_count=len(rows), warnings=warn, complete=not incomplete)
            m['decisive_row_present'] = j.get('decisive_row_present', False if not rows else None)
            m['decisive_row'] = j.get('decisive_row')
        sc = load('scaled')
        if sc:
            srep, sres = sc
            if srep['timed_out']: m['scaled_limits'] = 'timeout'
            elif sres and not sres.get('isError'):
                ss = sres['structuredContent']
                m['scaled_limits'] = dict(row_count=len(ss.get('results', [])), truncated=ss.get('truncated'),
                                          incomplete_codes=sorted({x.get('code') for x in ss.get('diagnostics', []) if x.get('impact') == 'incomplete'}))
            elif sres: m['scaled_limits'] = dict(error=sres.get('error'))
        else:
            m['scaled_limits'] = 'skipped (default timed out)' if rep['timed_out'] else None
    ibp = f"{S}/{r['intro_commit'][:10]}.index.rep"
    if os.path.exists(ibp) and 'index_wall_s' not in m:
        irep = json.load(open(ibp)); m['index_wall_s'] = irep['wall']; m['index_peak_rss_gib'] = round(irep['peak_rss_kib'] / 1048576, 2)
    out.append(m)
os.makedirs('/tmp/rql-census/report', exist_ok=True)
with open('/tmp/rql-census/report/measurements.jsonl', 'w') as f:
    for m in out: f.write(json.dumps(m) + '\n')
done = [m for m in out if m['status']]
print('rows', len(out), 'done', len(done))
import collections
print(collections.Counter(m['status'] for m in done))
