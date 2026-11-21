#!/usr/bin/env python3
"""Index warm-up: start `bifrost --root R` (MCP stdio), poll get_active_workspace
until analyzer_ready, usage_index_ready and workspace_build.status == completed,
then exit 0. Prints the final readiness JSON. Exit 2 if the server dies."""
import json, subprocess, sys, time
BIN, ROOT = sys.argv[1], sys.argv[2]
p = subprocess.Popen([BIN, '--root', ROOT], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr, text=True, bufsize=1)
nid = 0
def call(method, params=None, notify=False):
    global nid
    msg = {'jsonrpc': '2.0', 'method': method}
    if params is not None: msg['params'] = params
    if not notify: nid += 1; msg['id'] = nid
    p.stdin.write(json.dumps(msg) + '\n'); p.stdin.flush()
    if notify: return None
    while True:
        line = p.stdout.readline()
        if not line: print('server closed stdout', flush=True); sys.exit(2)
        try: r = json.loads(line)
        except Exception: continue
        if r.get('id') == nid: return r
call('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'rql-census-warm', 'version': '1'}})
call('notifications/initialized', notify=True)
t0 = time.time(); last = None
while True:
    r = call('tools/call', {'name': 'get_active_workspace', 'arguments': {}})
    res = r.get('result', {})
    s = res.get('structuredContent') or {}
    if not s:
        for c in res.get('content', []):
            try: s = json.loads(c.get('text', '')); break
            except Exception: pass
    st = dict(analyzer_ready=s.get('analyzer_ready'), usage_index_ready=s.get('usage_index_ready'),
              semantic_packs_ready=s.get('semantic_packs_ready'), workspace_build=s.get('workspace_build'))
    if st != last: print(json.dumps(dict(t=round(time.time() - t0, 1), **st)), flush=True); last = st
    wb = s.get('workspace_build') or {}
    if s.get('analyzer_ready') and s.get('usage_index_ready') and wb.get('status') == 'completed':
        break
    if wb.get('status') == 'failed': print('BUILD FAILED', flush=True); p.kill(); sys.exit(3)
    time.sleep(5)
p.stdin.close()
try: p.wait(timeout=60)
except subprocess.TimeoutExpired: p.kill()
print(json.dumps(dict(ready_after_s=round(time.time() - t0, 1))), flush=True)
