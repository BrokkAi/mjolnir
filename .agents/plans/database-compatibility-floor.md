# Keep older Mjolnir builds working across compatible migrations

This ExecPlan is maintained according to `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections remain current throughout implementation.

## Purpose / Big Picture

An unfinished feature must not make the installed `mj` reject its database merely because an additive migration advanced the revision. Keep migration revision separate from the minimum build revision allowed to read and write the store. After installing this baseline, an older build can continue using a newer compatible database. Breaking changes still require isolated testing until an intentional live upgrade.

## Progress

- [x] (2026-09-13) Inspect migration ladder, reader checks, persistent writer checks, and daemon divergence tests; confirm a clean working tree.
- [x] (2026-09-13) Implement revision 30 compatibility metadata and shared checks.
- [x] (2026-09-13) Add compatible, incompatible, rollback, malformed metadata, and migration failure coverage; update agent instructions.
- [x] (2026-09-13) Pass formatting, initial full Cargo suite, and all-target Clippy.
- [x] (2026-09-13) Validate final controller/database and daemon behavior; investigate the unrelated worker lease test failure and pass its isolated rerun.
- [x] (2026-09-13) Review the final diff and prepare the validated change for commit on the current branch.

## Surprises & Discoveries

The repository advanced from revision 28 at planning to revision 29 before implementation. Revision 29 adds `sessions.create_managed_worktree`; preserve it and introduce compatibility at revision 30. Writer startup also runs unconditional schema repair routines after the migration ladder. A compatible future store must bypass those routines as well as migrations. Unit-test readers currently open writers to support legacy migration fixtures; strict-reader tests and CLI integration coverage are necessary to prove production behavior.

The second full-suite run encountered an intermittent failure in unchanged worker installer code: `grok_launchers_execute_after_relocation_and_invalid_cache_repairs_itself` reported `managed harness repair deferred: ... is still in use`. It passed in the initial full run and again in isolation. No worker source changes were needed or made. The failing full run had already passed all 1,174 controller tests; the final daemon integration rerun passed all four tests.

## Decision Log

On 2026-09-13 the user selected compatibility first and deferred automatic repository isolation. Use a minimum compatible read/write revision rather than a second independent numbering system. Bootstrap at revision/floor 30, since older binaries reject every version difference and cannot be made compatible retroactively. Classify future migrations explicitly and treat uncertain compatibility as breaking. Existing live state is outside implementation validation.

## Outcomes & Retrospective

Implementation and validation are complete. Focused database tests passed (112 passed, one ignored); the first full workspace suite passed (3,175 passed, 22 ignored), including the real daemon's compatible-write/read and incompatible-shutdown scenario. Clippy passed with warnings denied. The final full-suite rerun passed the controller tests but stopped at the unrelated intermittent worker lease failure described above; that test passed alone, and final daemon integration tests passed separately. Formatting and diff checks are clean. The installed baseline must be intentionally upgraded before later compatible migrations receive this protection. Daemon protocol compatibility and arbitrary test writes remain separate concerns.

## Context and Orientation

`mj-controller/src/database/schema.rs` owns SQLite creation, migration, migration caching, strict reader opening, and schema repair. `mj-controller/src/database.rs` owns the revision constant and the persistent writer thread that processes queued operations. `mj-core/src/storage.rs` defines the typed `StoreSchemaMismatch` error; the controller daemon detects this error through its cause chain and shuts down. `mj-controller/src/database/tests.rs` and the schema module contain colocated tests. `mj-cli/tests/store_divergence.rs` starts an isolated daemon and tests its reaction to external database changes. A daemon is the background process that owns the database's write lane and controller lock.

## Plan of Work

The first milestone introduces a singleton `schema_compatibility` table containing `minimum_compatible_version`. Migration 30 creates it, writes floor 30, advances `PRAGMA user_version`, and records the migration atomically. The floor is the oldest migration revision whose executable may safely read and write the store. Compatible future migrations preserve it; breaking migrations raise it to their revision. Read revision and floor from a consistent SQLite snapshot, validate metadata, and share the compatibility decision across strict readers, writer startup, and queued writes. Readers reject stores behind their build; writer startup migrates known older stores. Newer compatible stores skip all migration and repair SQL. An open writer tracks its highest observed revision and rejects subsequent rollback. Metadata failures and I/O failures refuse the write and retain useful context.

The second milestone extends the typed mismatch error to explain the required minimum revision, malformed metadata, migration requirements, or observed rollback. Preserve daemon shutdown on incompatible divergence. Add a short migration policy to `AGENTS.md`: explicitly classify migration compatibility, consider old reads and writes including stored JSON and enum values, isolate breaking changes using both directory overrides, and never rewrite an already-applied migration.

The final milestone proves the behavior using temporary databases and existing isolated process fixtures, then validates and commits. A synthetic future migration must add real schema/data and update its ledger; current code must still read and write while preserving that newer state. Breaking floors must refuse writes and cause the daemon to exit. Test missing, malformed, and inconsistent metadata, backward revision movement, and failure of the baseline migration without partial metadata or version advancement.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Use existing rusqlite and tempfile dependencies. Run `cargo fmt --all`, focused controller database and CLI divergence tests during development, then `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings`. Run `cargo fmt --all -- --check` and `git diff --check`. Keep normal Cargo build storage. Stage only this task's changed files and commit on the current branch without pushing.

The final focused commands were `cargo test -p brokk-mj-worker --lib worker_runtime::harness::tests::grok_launchers_execute_after_relocation_and_invalid_cache_repairs_itself -- --exact --nocapture` (one passed) and `cargo test -p brokk-mjolnir --test store_divergence` (four passed). Both ran outside the sandbox with temporary directory overrides.

## Validation and Acceptance

Fresh databases and revision 29 databases reach revision/floor 30. Legacy migration fixtures continue working. Strict readers and real queued writes accept a synthetic revision 31 with floor 30, including writers open before the migration. New tables, new column values, revision, and ledger remain intact after reopening with current code. A floor of 31 rejects the same build with upgrade advice; malformed metadata refuses access instead of guessing. An open writer that observes revision 31 refuses a later revision 30. A failed baseline migration rolls back its own schema, floor, and ledger updates and can be retried. CLI integration coverage proves compatible divergence does not stop the daemon and incompatible divergence still removes discovery metadata during shutdown. All required checks must pass before committing.

## Idempotence and Recovery

All validation stores are temporary and explicitly isolated. Stop owned test processes before deleting their files. A migration transaction rolls back its own changes on error; do not manually lower a live revision or delete the live store. No installation, release, push, or live daemon restart is included. Existing pre-baseline binaries remain incompatible with the new revision until intentionally upgraded.

## Artifacts and Notes

The initial full-suite output is in `/mnt/optane/mj-schema-full.NYrgHI/cargo-test.log`; Clippy output is in `/mnt/optane/mj-schema-full.NYrgHI/clippy.log`. The final full-suite rerun is in `/mnt/optane/mj-schema-final.HvgChW/cargo-test.log`. All test invocations set fallback `MJ_CONFIG_DIR` and `MJ_DATA_DIR` to new isolated directories as well as using the individual fixtures' temporary stores. No new CLI flags, daemon protocol fields, or dependencies are required.

## Interfaces and Dependencies

Keep `PRAGMA user_version` and `schema_migrations` as migration tracking. Store the minimum read/write revision in `schema_compatibility(singleton, minimum_compatible_version)` with a singleton primary key. Use a shared schema-state reader and validator in the schema module. Preserve `StoreSchemaMismatch.found` and `.supported` for existing consumers while adding a reason that distinguishes incompatibility from migration requirements and rollback. The database write queue must carry general contextual errors as well as typed schema errors, so unreadable metadata never permits the queued mutation.

Revision note (2026-09-13): Created from the approved plan and adjusted the bootstrap revision to 30 because revision 29 is already committed.

Revision note (2026-09-13): Implemented the baseline and shared snapshot validation, updated legacy rewind fixtures, and extended daemon coverage to create and read a workspace after a compatible migration before testing incompatible shutdown.

Revision note (2026-09-13): Recorded successful initial validation. Distinguish temporary SQLite lock/I/O errors from structural metadata failures while refusing writes for either failure; rerun the full suite against that final adjustment.

Revision note (2026-09-13): Recorded the final controller and daemon passes and the unrelated worker lease flake with its successful isolated rerun. Prepared the reviewed implementation for the required current-branch commit; live state remains untouched.
