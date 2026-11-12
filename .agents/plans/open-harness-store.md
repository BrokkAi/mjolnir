# Preserve unsupported harness records

This living ExecPlan follows .agents/PLANS.md.

## Purpose / Big Picture

New harness kinds must require no schema changes. Unsupported sessions remain stored and produce existing load warnings.

## Progress

- [x] 2026-11-12: traced snapshot deletion and explicit deletion APIs; implemented revision 126 and snapshot ownership change.
- [x] Migration regression, 42 schema integration tests, 30 prior database failures, and final workspace clippy passed.
- [x] Final controller suite: 2570 passed, 20 ignored; one unrelated worker-stop fixture failed with OS Text file busy and has a focused retry pending. Store review had no findings.
- [x] Collected the passing worker-stop retry and clean combined login-fix review through dae8ecdde.
- [x] Restored the revision-43 harness constraints in the CLI upgrade fixture; all three affected isolated startup regressions pass (12.14 seconds).
- [x] Installed reviewed source a31379ca4 with user approval. Ordinary startup completed the live migration; revision and minimum compatible revision are 126, and both tables no longer constrain harness names.

## Surprises & Discoveries

Snapshot saves inferred deletion from absence. Explicit operational deletion already exists in database/sessions.rs. Hidden native session writes already target individual keys and loading warns about unknown kinds.

## Decision Log

Remove absence-based session deletion; snapshots only own present records. Preserve subagent relations if either endpoint is absent. Revision 126 is breaking because prior writers delete unknown sessions. Historical harness migration helpers remain unchanged for shipped migrations.

## Outcomes & Retrospective

Revision 126 and snapshot ownership changes pass focused validation. Historical fixture rewinds now restore the removed constraints; explicit deletion replaces snapshot absence in deletion fixtures. Full controller validation passed all store cases; the only failure was an unrelated executable-busy worker-stop fixture. Workspace clippy also passed after the concurrent login correction.

The worker-stop fixture passed its focused retry. The final CLI suite separately exposed three startup regressions sharing an old-store fixture: rewinding its ledger left modern unconstrained harness columns behind. Restore the actual revision-43 CHECK lists in that fixture so the shipped OpenCode, MiMo and DSH migrations still run during the upgrade test; no production migration changes are needed.

The corrected fixture passes ordinary startup, automatic upgrade after admitted work, and concurrent-client daemon replacement. The focused command was `cargo test -p brokk-mjolnir --test daemon_startup -- ordinary_startup_migrates_an_old_store_without_a_daemon automatic_upgrade_waits_for_work_then_migrates_without_another_invocation concurrent_clients_replace_obsolete_daemons_once_and_reuse_the_winner`, run outside the sandbox with the tests' existing isolated stores.

The final merged controller and CLI suites passed all store and startup cases. After the authorized installation, normal startup migrated the live 23 GB store and started the protocol-93 daemon. Read-only verification found revision/floor 126 and no harness CHECK on sessions or hidden_native_sessions. New harness kinds now need application support without another schema admission migration.

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

2026-11-12: Parent verified the final child reports, accepted the clean combined review and passing worker-stop retry, and took ownership of the CLI historical-fixture correction before installation.

2026-11-12: Recorded final validation and the completed live migration after installation. The revision-126 compatibility boundary is now active; preserve forward progress rather than attempting to downgrade the live daemon.
