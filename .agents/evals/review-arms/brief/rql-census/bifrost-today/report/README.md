# RQL census: Bifrost measurements (PLAN-rql-census.md lines 49-50)
Bifrost: bifrost-dev master `683ae81b014143806991cac731edbfd7523dfe1f`, built with `mbx build --profile measure --bin bifrost` (binary sha256 `14a9d9d51d6133d6e1f6d27d9079cb77160cdcabc66a5c251b66607497f96d47`). Input: documented-queries.jsonl (sha256 prefix ca8caeee91a6f69f), 57 rows, 12 repositories. Host: ec2-m8id16-c, container memory.max 64 GiB, no swap.
## Setup and boundaries
- Memory cap: no systemd and a read-only cgroup filesystem in the container, so each Bifrost process tree runs under `bin/watchdog.py`. It polls the tree RSS every 2 s and sends SIGKILL at 48 GiB (recorded as cap-kill, with `rss_at_kill_gib`). The container cgroup (64 GiB) is the backstop and its `memory.events` oom_kill was watched. The owner approved this setup; prlimit --as was rejected because of mmap.
- Index step: per introducing commit, an MCP server (`bin/mcp_warm.py`) is polled with `get_active_workspace` until analyzer_ready, usage_index_ready and build completed. Readiness boundary: **7200 s** (status `index_timeout`). Rows of prek, worktrunk, coreutils, rmux, mago and nushell ran before this step existed; their first query per commit included indexing. The first-query timeouts among them were rerun with the index step in front.
- Query boundary: **1800 s** per query (status `timeout`, with `elapsed_s`).
- Queries: `bifrost --root <checkout> --query-file <q>.rql`. The primary result uses the default interactive limits; `scaled_limits` records the `--workspace-scaled-limits` rerun. One private BIFROST_CACHE_ROOT/XDG_CACHE_HOME per repository series on ZFS (/tmp), reused across its introducing commits (incremental indexing).
- Complete means the result carries no truncation and no diagnostic with impact `incomplete`. Decisive row: a row whose fields state the claim at its sites (pointing at the code is not enough). Judgments are in `judgments.json` and quoted per row.
## Counts
- Status: {'ran': 53, 'timeout': 4}.
- Decisive row present: 15; absent: 38 (of 53 ran). Complete: 11 of 53 ran.
- Row count over ran rows: median 1, max 100.
- Mapped queries (query_run differs from the label): 1. openrouter-rebind-breaks-default-provider-compat-profile: `(union (literal-occurrences (declaration :name "DEFAULT_API_KEY_NAME")) (literal-occurrences (declaration :name "DEFAULT_ENV_FILE")))`
- possible_fail_open (complete, decisive row absent, construct silently skipped): 4. 1jehuang/jcode openrouter-rebind-breaks-default-provider-compat-profile: Rust `const` item: (declaration :name "DEFAULT_API_KEY_NAME"/"DEFAULT_ENV_FILE") matches nothing for the `const ...: &str = "...";` items at openrouter.rs:59-60, with no diagnostic (same construct as the nushell IS_RECURSIVE row); Helvesec/rmux rmux-diagnose-windows-shell-misses-windowsapps-filter: cfg(windows)-gated caller: detected_windows_pane_shell and find_windows_command_on_path are both #[cfg(windows)]; enclosing-decl finds the callee but call-sites-to returns no rows with no inactive-cfg diagnostic; nushell/nushell mkdir-verbose-reports-existing-dir-as-created: Rust `const` item: (declaration :name "IS_RECURSIVE") matches nothing for `const IS_RECURSIVE: bool = true;` (umkdir.rs:9), with no diagnostic; (literal :boolean-value true) alone does find the literal; uutils/coreutils coreutils-printenv-localized-clap-exit-code-1-not-2: closure argument (call inside |err| ... passed to unwrap_or_else not enumerated by call-sites-from)
- Index peak RSS (index-step rows): max 8.56 GiB. cap-kill: 0; container oom_kill: 0.

| repository | rows | ran | timeout | query_error | decisive present |
|---|---|---|---|---|---|
| 1jehuang/jcode | 4 | 4 | 0 | 0 | 0 |
| Helvesec/rmux | 6 | 6 | 0 | 0 | 2 |
| carthage-software/mago | 6 | 6 | 0 | 0 | 4 |
| j178/prek | 6 | 5 | 1 | 0 | 0 |
| kreuzberg-dev/kreuzberg | 1 | 1 | 0 | 0 | 1 |
| max-sixty/worktrunk | 3 | 3 | 0 | 0 | 0 |
| nushell/nushell | 4 | 2 | 2 | 0 | 0 |
| rust-lang/rust-analyzer | 3 | 3 | 0 | 0 | 1 |
| rustfs/rustfs | 8 | 8 | 0 | 0 | 0 |
| sinelaw/fresh | 4 | 4 | 0 | 0 | 3 |
| uutils/coreutils | 9 | 9 | 0 | 0 | 2 |
| web-infra-dev/rspack | 3 | 2 | 1 | 0 | 2 |

## Bifrost defects found (issues)
- #15016: `call-sites-to` panic at route_kinds.rs:973 (target-specific Cargo dependency).
- #15017: `call-input` over Rust `call-sites-from` returns 0 rows (outgoing sites unbound, and one gap ended the step).
- #15034: `call-sites-from` omits calls inside closure bodies and reports complete (fail-open).
- #15120: Rust UFCS calls bind the type path as receiver in the shared binder, plus cfg-disabled formals.
- Not filed yet: `(declaration :name X)` does not match a Rust `const` item and reports complete-empty (nushell mkdir row); `#[cfg(windows)]` callers are absent from `call-sites-to` with no inactive-cfg diagnostic (rmux diagnose row).

## Files
- `measurements.jsonl`: one row per input row.
- `evidence/`: reproductions and backtraces.
- Raw outputs: `/tmp/rql-census/run/<repo>/out/` (superseded first attempts in `run/_superseded/`).
