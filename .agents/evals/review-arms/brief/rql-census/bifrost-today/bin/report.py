#!/usr/bin/env python3
"""Write report/README.md: setup, boundaries and counts from measurements.jsonl."""
import collections, json, statistics

rows = [json.loads(l) for l in open('/tmp/rql-census/report/measurements.jsonl')]
st = collections.Counter(r['status'] for r in rows)
ran = [r for r in rows if r['status'] == 'ran']
present = sum(1 for r in ran if r['decisive_row_present'] is True)
absent = sum(1 for r in ran if r['decisive_row_present'] is False)
complete = sum(1 for r in ran if r['complete'])
counts = sorted(r['row_count'] for r in ran)
fail_open = [r for r in rows if r.get('possible_fail_open')]
mapped = [r for r in rows if r['query_run'] and r['query_run'] != r['query_labelled']]
idx = [r['index_peak_rss_gib'] for r in rows if r.get('index_peak_rss_gib') is not None]
per_repo = collections.defaultdict(lambda: collections.Counter())
for r in rows:
    c = per_repo[r['repo']]
    c['rows'] += 1
    c[r['status']] += 1
    if r['decisive_row_present'] is True:
        c['present'] += 1

out = []
w = out.append
w('# RQL census: Bifrost measurements (PLAN-rql-census.md lines 49-50)\n')
w('Bifrost: bifrost-dev master `683ae81b014143806991cac731edbfd7523dfe1f`, built with `mbx build --profile measure --bin bifrost` '
  '(binary sha256 `14a9d9d51d6133d6e1f6d27d9079cb77160cdcabc66a5c251b66607497f96d47`). Input: documented-queries.jsonl '
  '(sha256 prefix ca8caeee91a6f69f), 57 rows, 12 repositories. Host: ec2-m8id16-c, container memory.max 64 GiB, no swap.\n')
w('## Setup and boundaries\n')
w('- Memory cap: no systemd and a read-only cgroup filesystem in the container, so each Bifrost process tree runs under '
  '`bin/watchdog.py`. It polls the tree RSS every 2 s and sends SIGKILL at 48 GiB (recorded as cap-kill, with `rss_at_kill_gib`). '
  'The container cgroup (64 GiB) is the backstop and its `memory.events` oom_kill was watched. The owner approved this setup; '
  'prlimit --as was rejected because of mmap.\n')
w('- Index step: per introducing commit, an MCP server (`bin/mcp_warm.py`) is polled with `get_active_workspace` until '
  'analyzer_ready, usage_index_ready and build completed. Readiness boundary: **7200 s** (status `index_timeout`). Rows of prek, '
  'worktrunk, coreutils, rmux, mago and nushell ran before this step existed; their first query per commit included indexing. '
  'The first-query timeouts among them were rerun with the index step in front.\n')
w('- Query boundary: **1800 s** per query (status `timeout`, with `elapsed_s`).\n')
w('- Queries: `bifrost --root <checkout> --query-file <q>.rql`. The primary result uses the default interactive limits; '
  '`scaled_limits` records the `--workspace-scaled-limits` rerun. One private BIFROST_CACHE_ROOT/XDG_CACHE_HOME per repository '
  'series on ZFS (/tmp), reused across its introducing commits (incremental indexing).\n')
w('- Complete means the result carries no truncation and no diagnostic with impact `incomplete`. Decisive row: a row whose '
  'fields state the claim at its sites (pointing at the code is not enough). Judgments are in `judgments.json` and quoted per row.\n')
w('## Counts\n')
w(f'- Status: {dict(st)}.\n')
w(f'- Decisive row present: {present}; absent: {absent} (of {len(ran)} ran). Complete: {complete} of {len(ran)} ran.\n')
if counts:
    w(f'- Row count over ran rows: median {statistics.median(counts)}, max {max(counts)}.\n')
w(f'- Mapped queries (query_run differs from the label): {len(mapped)}. ' +
  '; '.join(f"{r['id']}: `{r['query_run']}`" for r in mapped) + '\n')
w(f'- possible_fail_open (complete, decisive row absent, construct silently skipped): {len(fail_open)}. ' +
  '; '.join(f"{r['repo']} {r['id']}: {r['possible_fail_open']}" for r in fail_open) + '\n')
if idx:
    w(f'- Index peak RSS (index-step rows): max {max(idx)} GiB. cap-kill: 0; container oom_kill: 0.\n')
w('\n| repository | rows | ran | timeout | query_error | decisive present |\n|---|---|---|---|---|---|\n')
for repo, c in sorted(per_repo.items()):
    w(f"| {repo} | {c['rows']} | {c['ran']} | {c['timeout']} | {c['query_error']} | {c['present']} |\n")
w('\n## Bifrost defects found (issues)\n')
w('- #15016: `call-sites-to` panic at route_kinds.rs:973 (target-specific Cargo dependency).\n')
w('- #15017: `call-input` over Rust `call-sites-from` returns 0 rows (outgoing sites unbound, and one gap ended the step).\n')
w('- #15034: `call-sites-from` omits calls inside closure bodies and reports complete (fail-open).\n')
w('- #15120: Rust UFCS calls bind the type path as receiver in the shared binder, plus cfg-disabled formals.\n')
w('- Not filed yet: `(declaration :name X)` does not match a Rust `const` item and reports complete-empty (nushell mkdir row); '
  '`#[cfg(windows)]` callers are absent from `call-sites-to` with no inactive-cfg diagnostic (rmux diagnose row).\n')
w('\n## Files\n- `measurements.jsonl`: one row per input row.\n- `evidence/`: reproductions and backtraces.\n'
  '- Raw outputs: `/tmp/rql-census/run/<repo>/out/` (superseded first attempts in `run/_superseded/`).\n')
open('/tmp/rql-census/report/README.md', 'w').write(''.join(out))
print(''.join(out)[:3000])
