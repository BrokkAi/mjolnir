# Load session usage with bulk database queries

This ExecPlan follows `.agents/PLANS.md` and remains a living record of the implementation.

## Purpose / Big Picture

Opening Usage by session should return promptly even when active parent sessions retain thousands of historical descendants. Preserve the existing report exactly, including profile and period filters, retained children, cost attribution, and unfinished-turn coverage. Read recent completed turns and current active turns in bulk instead of executing two queries for each historical descendant.

## Progress

- [x] Reproduced the original Rust report taking 22.698 seconds against a disposable copy of the report tables, with 178 parent sessions and roughly 11,000 descendants.
- [x] Verified that time indexing, statement reuse, and lazy role reads preserve the complete serialized report. Normal reads still vary between one and three seconds; tracing found tens of thousands of page reads.
- [x] (2026-11-11) Replaced per-descendant turn queries with bulk reads and a single descendant-to-parent ownership map. SQLite plans use both time indexes; the copied-store SQL reads 566 completed turns and 14 active turns in about 156 ms.
- [x] (2026-11-11) Validated migration compatibility and query work independent of historical turns and inactive sessions. All six focused checks passed; the complete copied-store report matches the original exactly and takes 813.907 ms instead of 22.698 seconds.
- [x] (2026-11-11) Workspace Clippy passed with warnings denied; removed temporary measurement code after recording its results.
- [x] (2026-11-11) Ran the full controller suite: 2,545 passed, with two failures resolved by targeted reruns. The Nitro migration regression now asserts the current schema revision, and the settings golden passes with isolated config/data directories and its expected default port. Final workspace Clippy passed.
- [x] (2026-11-11) Independent implementation review of commit `4bbdadcac` finished clean in parallel with the full suite. The final test-and-notes follow-up goes through the standard review and publication workflow.

## Surprises & Discoveries

The existing report walks roughly 11,000 descendants to return only hundreds of hourly turn records. Its JSON time predicate has no time index. It also parses every child's potentially large launch record before knowing whether that child has recent usage. A time index fixes historical body scans, but retaining individual turn and unfinished-turn queries leaves latency sensitive to cache misses and filesystem load.

The original report snapshot is saved locally in `/tmp/mj-usage-1501/before.json`. The copied database is `/tmp/mj-usage-1501/usage.sqlite3`; it contains only session and usage tables and schema metadata. This directory is private. Never run a new binary against live session data.

## Decision Log

- Decision: Build one map from every included root and retained descendant to its parent report entry, then ask SQLite for the selected completed time range and the active turns once each.
  Rationale: Parent ownership is decided once from one transaction snapshot. Database work scales with the selected usage and active turns, and no report read issues thousands of individual historical-session lookups.
  Date/Author: 2026-11-11 / Codex.
- Decision: Use compatible migration 125 for a completion-time expression index and a partial active-turn expression index. Keep the compatibility floor at 124.
  Rationale: Stored bodies and writer SQL remain unchanged, and SQLite maintains both indexes for previous writers. An expression index stores a JSON field in a searchable tree; a partial index includes only rows satisfying its condition, here non-null active turns.
  Date/Author: 2026-11-11 / Codex.

## Outcomes & Retrospective

The bulk implementation preserves the complete report and reduces measured development-build read time from 22.698 seconds to 813.907 ms on the same 178-parent snapshot. All focused checks pass. The full controller run passed 2,545 cases; its two failures were a hardcoded previous revision in an existing migration test and the settings golden inheriting a named instance's port. Both targeted reruns passed after the revision assertion correction and explicit isolated config/data setup. Final workspace Clippy passed, and the independent implementation review was clean. The new regression protects both completed and active reads against work growing with inactive history. No new build was installed and the live store was not migrated.

## Context and Orientation

`mj-cli/src/dashboard/usage_report.rs` loads the report asynchronously and gives up after 20 seconds. The HTTP backend runs `mj-controller/src/database/usage_report.rs::load_session_usage_report` in a blocking background task. `load_with` opens a single SQLite transaction, determines parent sessions and retained descendants through `subagent_accounting`, prices full-turn usage, and returns a sorted report. Keep this snapshot and the existing pricing helpers. `mj-controller/src/database/schema.rs` owns migrations, and `mj-controller/src/database.rs` owns `SCHEMA_VERSION`.

## Plan of Work

In `usage_report.rs`, collect the included parent estimates and their per-model totals into a small local report-entry type. Walk retained descendants to record their count and populate a map from member ID to entry index. Query completion times globally with the profile predicate and join selections once; skip rows whose member is outside the included trees. Read agent kinds lazily for members contributing whole-turn usage, caching each contributing member's role. Query active turns once, apply the same profile and time rules, and increment the owning entry's unfinished count. Finish with the existing parent filtering, cost accumulation, and sorting.

In migration 125, put the completion time first in the index because the query selects one global time range. Add a partial index for active turns that covers the start time, session ID, and command ID. Do not rewrite a shipped migration or change the compatibility floor.

## Concrete Steps

From the repository root, run focused checks with `MJ_INSTANCE=usage-aggregation-test cargo test -p brokk-mj-controller usage_report`. Run Cargo tests outside the restricted sandbox and use the existing mbx Cargo wrapper without changing target directories. For the full unit suite, use explicit isolated config/data directories and unset the inherited instance name: `env -u MJ_INSTANCE MJ_CONFIG_DIR=/tmp/mj-usage-controller-check/config MJ_DATA_DIR=/tmp/mj-usage-controller-check/data cargo test -p brokk-mj-controller`. Unit cases already give themselves temporary stores; the explicit outer directories also prevent access to live data while allowing the settings golden's default-port fixture. Daemon, CLI, and end-to-end tests still require a named instance. Run workspace Clippy with `MJ_INSTANCE=usage-aggregation-check cargo clippy --all-targets -- -D warnings`.

The temporary ignored `usage_report_local_probe` was removed after measurement. It called `load_from` on the copied database with the default query and `now` frozen to `1794412800000`, measured elapsed time, and wrote `serde_json::to_vec(&report)` to a file for full comparison with `before.json`. Recreate that small ignored test inside the module's test block if measurements need repeating, and use a named `MJ_INSTANCE` for that probe.

## Validation and Acceptance

The existing report integration tests must preserve inclusive period boundaries, profile switches, destroyed descendants, missing or partial reports, model aliases, and agent-role totals. The migration regression must show that previous writer shapes keep working and that adding thousands of old turns does not increase the selected-range query's SQLite instruction count. Compare the entire copied-store report to the original snapshot, not just totals. Measure the same report after migration and confirm that its loading time no longer approaches the 20-second deadline. All touched controller tests and workspace Clippy must pass before commit and push.

## Idempotence and Recovery

Only the disposable copy may be rewound to revision 124 while iterating on unshipped migration 125. Drop its experimental indexes and delete migration metadata at or above 125 before rewinding. Never modify the live store, install a new build, or change build-cache configuration. Existing readers at revision 124 remain compatible after the new indexes are installed. Keep only the source changes and the completed plan in the final commit.

## Artifacts and Notes

Original Rust timing: 22.698 seconds, 178 report entries. The indexed per-member implementation produced exactly the same JSON, but normal timings ranged from 1.247 to 3.292 seconds. System-call tracing counted about 32,000 page reads without the active-turn index and 15,000 with it, motivating the bulk-read change. The final bulk implementation takes 813.907 ms in the development build; its complete output in `/tmp/mj-usage-1501/bulk.json` matches `before.json`. The one-time index creation took 4.402 seconds and happens before the production report reader runs.

## Interfaces and Dependencies

Keep the public `load_session_usage_report` signature and `SessionUsageReport` API unchanged. Use existing `BTreeMap`, rusqlite transactions, `super::usage::attribute`, `estimate_model_cost`, `merge_totals`, and `ModelCatalogs`. Add no crate or dependency. The new ownership map is local to one read transaction and does not introduce daemon state or a cache requiring invalidation.

Revision note: 2026-11-11 — created after measurement showed that indexing individual historical-session queries was insufficient.

Revision note: 2026-11-11 — recorded successful bulk-reader measurements and focused validation; full tests and requested parallel review remain in progress.

Revision note: 2026-11-11 — recorded the clean implementation review, complete controller coverage with successful targeted corrections, and final Clippy result. Documented isolated unit-suite configuration to preserve the settings golden's port.
