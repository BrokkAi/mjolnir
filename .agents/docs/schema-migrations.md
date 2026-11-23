# SQLite Schema Migration Performance Guide

This note captures SQLite storage mechanics, measured timings, and design rules
for keeping Mjolnir's `mj.sqlite3` schema upgrades fast (milliseconds to a few
seconds, never minutes blocking daemon startup).

## Why migrations get slow on large stores

In production, `materialized_transcript_items` (and to a lesser extent
`api_events` and `native_agent_transcript`) accounts for almost all bytes in
`mj.sqlite3`. Four SQLite behaviors multiply migration cost on those tables:

1. **In-place `UPDATE` rewrites every overflow page.** SQLite stores each row as
   one packed record; it cannot patch a single column in place. Updating any
   column on a row with a multi-KB or multi-MB `body_json` re-serializes the
   entire row, allocates new overflow pages, frees the old ones, and writes the
   entire table into `-wal` (and later copies it again during checkpointing).
2. **Columns appended after a large blob live at the tail of the overflow chain.**
   SQLite lays out columns in declaration order. When `ALTER TABLE ADD COLUMN`
   appends metadata columns after `body_json`, every `CREATE INDEX` and every
   non-covering read of those trailing columns has to traverse the singly-linked
   overflow page chain of `body_json` to reach the last few bytes of each row.
3. **Repeated SQL `json_valid` / `json_extract` parses each blob many times.**
   Calling `json_valid(body_json)` twice and `json_extract(body_json, ...)` three
   or four times in one `UPDATE` scans every multi-MB tool payload 5–6 times on a
   single CPU core.
4. **Unscoped `PRAGMA foreign_key_check` scans the whole database.** Omitting the
   table argument checks every foreign key in `sqlite_schema`, scanning millions
   of rows in `materialized_transcript_items` and `api_events` even when
   rebuilding a small table like `sessions` or `session_targets`.

### Measured reference (164 MiB synthetic `materialized_transcript_items`, 10,000 rows, warm page cache)

| Strategy | Total | Breakdown | WAL written |
| --- | ---: | --- | ---: |
| Default PRAGMAs + `ALTER` + 6× SQL JSON `UPDATE` + 2× `CREATE INDEX` | 3.23 s | update 1.42 s, idx1 0.18 s, idx2+commit 0.69 s | 164.1 MiB |
| Tuned PRAGMAs + full table rebuild (`body_json` last) + 3× `CREATE INDEX` | 3.75 s | insert 0.51 s, drop 0.63 s, 3× idx+commit 2.61 s | 328.6 MiB |
| Tuned PRAGMAs + `ALTER` + single-pass extractor `UPDATE` + 2× `CREATE INDEX` | 1.54 s | update 0.07 s, idx1 0.02 s, idx2+commit 1.12 s | 164.1 MiB |
| **Narrow sidecar metadata table (no `body_json` rewrite)** | **0.36 s** | scan + insert + 2× idx + commit | **0.9 MiB** |

On multi-GB production stores that exceed RAM, the gap widens sharply: a sidecar
table writes ~180× less WAL and avoids rewriting gigabytes of overflow pages.

## Built-in migration fast path (`mj-store/src/database/schema.rs`)

- **`MigrationTuningGuard`**: Whenever `version < SCHEMA_VERSION`, `migrate_schema`
  temporarily applies `cache_size = -262144` (256 MiB, allocated lazily by
  SQLite), `temp_store = MEMORY`, `mmap_size = 1073741824` (1 GiB ceiling),
  `threads = 4`, and (in WAL mode) `synchronous = NORMAL`. On completion it
  restores every PRAGMA and commits a single 1-page WAL durability barrier under
  the restored `synchronous = FULL` so all migration frames are synced without
  blocking startup on a full WAL-to-main checkpoint. On failure, `Drop` restores
  settings safely even if an inner transaction was left open.
- **`has_foreign_key_violation_for_tables`**: Inspects `pragma_foreign_key_list`
  and runs `PRAGMA foreign_key_check("<table>")` only for the rebuilt table(s)
  and tables that directly reference them.

## Best practices for new schema migrations

1. **Keep startup DDL O(1) on large tables.** Never put a full-table `UPDATE` or
   table rebuild over `materialized_transcript_items`, `api_events`, or
   `native_agent_transcript` in `migrate_schema`.
   - Prefer a **narrow sidecar table** keyed by `(session_id, stable_id)` when
     adding indexed metadata or pointers alongside large blobs.
   - Prefer an **expression index** or **`VIRTUAL` generated column** (`ALTER TABLE
     ... ADD COLUMN ... GENERATED ALWAYS AS (...) VIRTUAL` + `CREATE INDEX`) when
     the value is derived from existing leading columns.
   - When existing rows must be transformed, do **O(1) DDL at startup** (nullable
     column or empty sidecar table) and run a **handoff-cancellable background
     backfill** in small transactions after the daemon starts serving (see
     `backfill_tool_details` in `mj-daemon/src/daemon/process.rs`), with fallback
     handling on reads until backfill completes.
2. **Keep large `TEXT`/`BLOB` columns last.** On every new or rebuilt table,
   declare fixed-width and frequently indexed/filtered columns first and large
   JSON/blob payload columns last. Do not append frequently indexed columns after
   a large blob via `ALTER TABLE ADD COLUMN`.
3. **Do not encode evolving enums in SQLite `CHECK(col IN (...))` constraints.**
   SQLite cannot widen a `CHECK` constraint without rebuilding the table. Validate
   evolving enum sets (harnesses, target kinds, session states) in Rust instead,
   following revision 126's removal of `harness_kind` constraints.
4. **Always use scoped foreign-key checks after table rebuilds.** Call
   `has_foreign_key_violation_for_tables(&transaction, &["rebuilt_table"])` rather
   than unscoped `PRAGMA foreign_key_check`.
5. **Parse JSON once in Rust if a scan is unavoidable.** Borrow `body_json`
   zero-copy (`row.get_ref(i)?.as_str()?`) and use a streaming `serde_json`
   visitor with `serde::de::IgnoredAny` so multi-MB fields are skipped without
   heap allocation (see `extract_v153_transcript_metadata`).
6. **Never rewrite an already-applied migration's schema contract.** You may
   optimize how an existing migration computes the exact same schema and row
   values for stores that have not yet reached that revision, but any change to
   columns, constraints, indexes, or semantics requires a new revision.

## Future structural option

If transcript-side schema changes become frequent even after `.mjtd` tool-detail
spilling shrinks `body_json`, split `body_json` into its own
`materialized_transcript_bodies(session_id, stable_id, body_json)` table—or move
transcript tables into a separate `transcripts.sqlite3` file—so hot control-plane
upgrades never touch transcript history.
