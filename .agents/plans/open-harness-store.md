# Preserve unsupported harness records

This living ExecPlan follows .agents/PLANS.md.

## Purpose / Big Picture

New harness kinds must require no schema changes. Unsupported sessions remain stored and produce existing load warnings.

## Progress

- [x] 2026-11-12: traced snapshot deletion and explicit deletion APIs; implemented revision 126 and snapshot ownership change.
- [x] Migration regression, 42 schema integration tests, 30 prior database failures, and final workspace clippy passed.
- [x] Final controller suite: 2570 passed, 20 ignored; one unrelated worker-stop fixture failed with OS Text file busy and has a focused retry pending. Store review had no findings.
- [ ] Collect focused worker-stop retry and combined login-fix rereview.

## Surprises & Discoveries

Snapshot saves inferred deletion from absence. Explicit operational deletion already exists in database/sessions.rs. Hidden native session writes already target individual keys and loading warns about unknown kinds.

## Decision Log

Remove absence-based session deletion; snapshots only own present records. Preserve subagent relations if either endpoint is absent. Revision 126 is breaking because prior writers delete unknown sessions. Historical harness migration helpers remain unchanged for shipped migrations.

## Outcomes & Retrospective

Revision 126 and snapshot ownership changes pass focused validation. Historical fixture rewinds now restore the removed constraints; explicit deletion replaces snapshot absence in deletion fixtures. Full controller validation passed all store cases; the only failure was an unrelated executable-busy worker-stop fixture. Workspace clippy also passed after the concurrent login correction.

## Context and Orientation

mj-controller/src/database/state_io.rs loads and saves State snapshots. database/schema.rs owns migration transactions and compatibility metadata; database.rs declares the supported revision.

## Plan of Work

Rebuild both harness-constrained tables from their stored definitions, removing only the harness CHECK and replaying indexes/triggers with foreign keys checked. Advance revision and floor atomically. Change save_state_to to upsert sessions without inferred deletes. Existing decode warnings report unsupported kinds.

## Concrete Steps

From repository root run cargo test -p brokk-mj-controller unknown_harness_survives_migration_and_snapshot_save with isolated MJ_CONFIG_DIR and MJ_DATA_DIR outside the sandbox, then the full crate suite and cargo clippy --all-targets -- -D warnings. Store text logs in the assigned report directory.

## Validation and Acceptance

The migration regression stops at revision 125, creates a session and schema objects, upgrades, stores an unknown harness in both tables, then loads and saves the snapshot. The row, hidden record and schema objects must survive with no foreign key violations.

## Idempotence and Recovery

The migration transaction either commits revision 126 and its compatibility floor together or rolls back. Foreign key enforcement is restored on errors. Never run test builds against the default instance.

## Artifacts and Notes

Evidence is in .mj/agents/6856201e667dfc419aa2578076f6ed4f/.

## Interfaces and Dependencies

Reuse rusqlite transactions and existing explicit delete_session_from. No new public or persisted record format is introduced.
