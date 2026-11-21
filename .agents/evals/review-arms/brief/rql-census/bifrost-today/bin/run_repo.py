#!/usr/bin/env python3
"""Run every documented query of one repo at its intro commits, oldest first,
reusing one private cache root per repo (incremental). Each bifrost process runs
under watchdog.py (48 GiB RSS cap). Two variants per query: default interactive
limits, and --workspace-scaled-limits. Override query text via overrides.json
{"<id>|<set>|<fact>": "rql text"}."""
import json, os, subprocess, sys
REPO = sys.argv[1]
BIN = '/tmp/rql-census/bin/bifrost-683ae81b01'
WD = '/tmp/rql-census/bin/watchdog.py'
d = REPO.replace('/', '__')
R = f'/workspace/bdbef0e91da8b73cdee645eb5af7073b/rql-repos/{d}'
S = f'/tmp/rql-census/run/{d}'
OV = '/tmp/rql-census/overrides.json'
ov = json.load(open(OV)) if os.path.exists(OV) else {}
rows = [json.loads(l) for l in open('/tmp/rql-census/inputs/rql-measure/documented-queries.jsonl')]
rows = [r for r in rows if r['repo'] == REPO]
def git(*a): return subprocess.run(['git', '-C', R, *a], check=True, capture_output=True, text=True).stdout.strip()
date = {c: git('log', '-1', '--format=%ct', c) for c in {r['intro_commit'] for r in rows}}
commits = sorted(date, key=lambda c: int(date[c]))
for k in ('cache', 'xdg', 'tmp', 'out'): os.makedirs(f'{S}/{k}', exist_ok=True)
env = dict(os.environ, RUST_BACKTRACE='1', BIFROST_CACHE_ROOT=f'{S}/cache', XDG_CACHE_HOME=f'{S}/xdg', TMPDIR=f'{S}/tmp')
os.makedirs(f'{R}/.rql-census', exist_ok=True)
for ci, c in enumerate(commits):
    git('checkout', '-q', '--detach', '-f', c)
    assert git('rev-parse', 'HEAD') == c
    # Index step: MCP server until analyzer + usage index ready (separate from query time).
    ib = f'{S}/out/{c[:10]}.index'
    todo = [r for r in rows if r['intro_commit'] == c]
    need = any(not (os.path.exists(f"{S}/out/{c[:10]}-{r['id'][:60]}-s{r['set']}f{r['fact']}.default.json") and os.path.getsize(f"{S}/out/{c[:10]}-{r['id'][:60]}-s{r['set']}f{r['fact']}.default.json") > 0) for r in todo)
    if need:
        for ext in ('.json', '.err', '.rep'):
            if os.path.exists(ib + ext): os.remove(ib + ext)
        subprocess.run(['/usr/bin/python3', WD, '--report', ib + '.rep', '--stdout', ib + '.json', '--stderr', ib + '.err',
                        '--timeout', '7200', '--', '/usr/bin/python3', '/tmp/rql-census/bin/mcp_warm.py', BIN, R], env=env, cwd=S, check=True)
        irep = json.load(open(ib + '.rep'))
        print(json.dumps(dict(commit=c, step='index', **{k: irep[k] for k in ('exit','wall','peak_rss_kib','cap_kill','rss_at_kill_kib','oom_kill_delta','timed_out')})), flush=True)
        if irep['oom_kill_delta']:
            print('STOP: container oom_kill moved (index)', flush=True); sys.exit(3)
        if irep['exit'] != 0 or irep['timed_out']:
            continue  # index failed: rows of this commit stay unrun; builder marks index_failed
    for r in [r for r in rows if r['intro_commit'] == c]:
        key = f"{r['id']}|{r['set']}|{r['fact']}"
        q = ov.get(key, r['query'])
        tag = f"{c[:10]}-{r['id'][:60]}-s{r['set']}f{r['fact']}"
        qf = f".rql-census/{tag}.rql"
        p = f'{R}/{qf}'
        if os.path.exists(p): os.remove(p)
        open(p, 'w').write(q + '\n')
        skip_scaled = False
        for var, extra in (('default', []), ('scaled', ['--workspace-scaled-limits'])):
            base = f'{S}/out/{tag}.{var}'
            if var == 'scaled' and skip_scaled:
                print(json.dumps(dict(commit=c, key=key, variant=var, skipped='default timed out')), flush=True); continue
            if os.path.exists(base + '.rep') and os.path.getsize(base + '.json') > 0:
                print(json.dumps(dict(commit=c, key=key, variant=var, skipped='already done')), flush=True); continue
            for ext in ('.json', '.err', '.rep'):
                if os.path.exists(base + ext): os.remove(base + ext)
            subprocess.run(['/usr/bin/python3', WD, '--report', base + '.rep', '--stdout', base + '.json',
                            '--stderr', base + '.err', '--timeout', '1800', '--', BIN, '--root', R, '--query-file', qf, *extra],
                           env=env, cwd=S, check=True)
            rep = json.load(open(base + '.rep'))
            if rep['timed_out']: skip_scaled = True
            print(json.dumps(dict(commit=c, key=key, variant=var, query=q, **{k: rep[k] for k in ('exit','wall','peak_rss_kib','cap_kill','rss_at_kill_kib','oom_kill_delta','timed_out')})), flush=True)
            if rep['oom_kill_delta']:
                print('STOP: container oom_kill moved', flush=True); sys.exit(3)
