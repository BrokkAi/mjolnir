#!/usr/bin/env python3
"""Report `crate::` references that cross the layer boundaries in
.agents/plans/split-core-into-layered-crates.md in the wrong direction.

Before M2 every module lives in src/. After M2, pass the crate source roots
as arguments (default: src mj-worker/src mj-controller/src mj-chat/src);
`crate::` inside a root refers to that root, and cross-crate paths
(hel::, mj_worker::, mj_controller::, mj_chat::) are checked the same way.
Prints two reports; both must be empty."""
import os, re, sys

LAYERS = {
    'F': {'clock','termination','hel_config','hel_subprocess','hel_elicitation','hel_workspace',
          'hel_transcript','hel_diff','hel_archive','hel_state','hel_projection','hel_worker',
          'hel_worker_protocol','hel_worker_launch','hel_targets','hel_database','hel_second_opinion',
          'hel_acp','hel_credentials','hel_skills','hel_project_memory','hel_test_hooks',
          'hel_resources','hel_local_git','hel_checkpoint','hel_terminal','hel_review'},
    'W': {'hel_worker_runtime','hel_user_shell','hel_review_bifrost'},
    'C': {'hel_controller','hel_session_manager','hel_server','hel_import','hel_worker_client',
          'hel_quota','claude_usage','codex_usage','grok_usage','hel_doctor','hel_setup',
          'hel_git_proxy','hel_recovery','hel_compaction','hel_tailscale','hel_desktop',
          'hel_readline','hel_review_host','hel_worker_upgrade'},
    'CH': {'hel_chat','speech','hel_text_input','hel_selection','usage_format','hel_clipboard'},
}
LAYER = {m: l for l, ms in LAYERS.items() for m in ms}
ORDER = {'F': 0, 'W': 1, 'C': 2, 'CH': 3}
CRATE_PREFIX = {'hel': 'F', 'mj_worker': 'W', 'mj_controller': 'C', 'mj_chat': 'CH'}
# Files that are the daemon-side review host while it still lives under hel_review/.
HOST_FILES = ('hel_review/host.rs',)

def bad(a, b):
    la, lb = LAYER[a], LAYER[b]
    return ORDER[lb] > ORDER[la] or {la, lb} == {'W', 'C'}

def files_of(root, m):
    out = []
    if os.path.exists(f'{root}/{m}.rs'): out.append(f'{root}/{m}.rs')
    for d, _, fs in os.walk(f'{root}/{m}'):
        out += [os.path.join(d, f) for f in fs if f.endswith('.rs')]
    return out

def split_src(path):
    s = open(path, errors='ignore').read()
    name = os.path.basename(path)
    if name == 'tests.rs' or name.endswith('_tests.rs') or '/tests/' in path:
        return '', s
    i = s.find('#[cfg(test)]')
    return (s, '') if i < 0 else (s[:i], s[i:])

roots = sys.argv[1:] or [r for r in ('src','mj-worker/src','mj-controller/src','mj-chat/src') if os.path.isdir(r)]
edges = {'non-test': {}, 'test-only': {}}
for root in roots:
    lib = open(f'{root}/lib.rs').read()
    mods = re.findall(r'^(?:pub )?mod (\w+);', lib, re.M)
    for m in mods:
        if m not in LAYER:
            print(f'WARNING: {root}/{m} has no layer assignment', file=sys.stderr); continue
        for f in files_of(root, m):
            src_m = 'hel_review_host' if any(f.endswith(h) for h in HOST_FILES) else m
            for kind, text in zip(('non-test', 'test-only'), split_src(f)):
                refs = re.findall(r'\b(crate|hel|mj_worker|mj_controller|mj_chat)::(\w+)((?:::\w+)+)', text)
                # Braced imports (`use crate::m::{a, b::c, ...}`), which may span lines.
                for crate, target, body in re.findall(r'\b(crate|hel|mj_worker|mj_controller|mj_chat)::(\w+)::\{([^}]*)\}', text, re.S):
                    for item in re.split(r'[,\s]+', body):
                        if item: refs.append((crate, target, '::{' + item))
                for crate, target, sym in refs:
                    if target not in LAYER or target == src_m: continue
                    if crate != 'crate' and target not in LAYERS[CRATE_PREFIX[crate]]: continue
                    if bad(src_m, target):
                        d = edges[kind].setdefault((src_m, target), {})
                        d[sym] = d.get(sym, 0) + 1
for kind in ('non-test', 'test-only'):
    print(f'{kind.upper()} ESCAPING EDGES:')
    for (a, b), syms in sorted(edges[kind].items()):
        top = ', '.join(f'{k}x{v}' for k, v in sorted(syms.items(), key=lambda kv: -kv[1])[:8])
        print(f'  {a}({LAYER[a]}) -> {b}({LAYER[b]}): {top}')
