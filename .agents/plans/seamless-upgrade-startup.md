# Make upgrades complete automatically before serving clients

This is a living ExecPlan maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


After installing or accepting an upgrade, running `mj` must automatically replace an older daemon (the persistent background controller), migrate its database, and wait for usable services. Users must not run a daemon restart, migration command, or intermediate release to complete an ordinary upgrade. The same startup path serves direct downloads, package-manager installations, the terminal dashboard, desktop bootstrap, and CLI commands. Live workers must remain detached and running during the controller handoff. Errors caused by corruption, an incompatible downgrade, or unavailable operating-system resources must remain truthful and must not be hidden by resetting user data.

## Progress


- [x] (2026-09-22) Trace the reported schema-43 to schema-44 failure to reuse of an older daemon with the same protocol number.
- [x] (2026-09-22) Identify the removed pre-33 migration ladder and the API client's premature failure during viewer startup.
- [x] (2026-09-22) Establish a shared startup readiness gate for release version, protocol, and strict database compatibility under the startup lock.
- [x] (2026-09-22) Restore the original revision 2–33 migrations for historical SQLite stores; retain the baseline for new databases.
- [x] (2026-09-22) Add real-process regressions for concurrent upgrade clients, unchanged protocols, same-version schema changes, release-only changes, no-daemon startup, and newer compatible daemons/stores. Add interruption/resume and data-preservation coverage for every revision 1–43.
- [x] (2026-09-22) Add bounded API/desktop readiness waits and automatic terminal re-exec with private versioned state handoff. A real PTY test passes; unsent form answers and large drafts survive serialization. Document the invariant in AGENTS.md.
- [x] (2026-09-22) Run formatting, the full dev-profile Cargo suite, final CLI regressions, shared subprocess checks, and workspace Clippy; review the complete diff for the current-branch commit.

## Surprises & Discoveries


`mj-cli/src/daemon.rs::connect_or_start_holding` only replaces a daemon when its wire protocol differs. Release 2.16.0 changes the database from 43 to 44 without changing protocol 32. `run_workspace_dashboard` already starts/connects before reading the database, but that connection returns the old process. Reordering the dashboard cannot fix this.

`mj-controller/src/database/schema.rs` refuses databases before revision 33 and tells the user to install a release between 2.7.2 and 2.9.x. The complete original ladder is retained in Git before commit `87fd3c8e`; restoring that code permits unattended historical upgrades.

The database module substitutes a migrating writer for most readers in unit tests. Startup regression tests must exercise the compiled CLI and the production read-only check, or this bug can be invisible to tests.

The terminal keep-alive previously instructed users to restart a missing daemon. It now detects a newer daemon's executable, asks the terminal to drain operations, and restarts the terminal with a private handoff document. A daemon generation identifier prevents an exec loop if an installer changes the executable again. Passive keep-alive still never starts an older daemon in the replacement gap.

Paused Tokio clocks cannot reliably test real TCP readiness: time can jump to the timeout while the OS has not delivered socket readiness. The viewer tests use real loopback sockets and a short injected timeout for the stalled case instead.

## Decision Log


Decision: Put upgrade coordination in shared daemon connection/startup, rather than the updater or individual UI entry points. Rationale: external package-manager upgrades never execute the old updater, and every client needs the same readiness guarantee. Date: 2026-09-22.

Decision: Keep migration ownership in the daemon under its existing sole-writer lock. Rationale: clients must never race migrations against a live old writer. Preserve existing refusal of incompatible newer stores. Date: 2026-09-22.

Decision: Retain the fast revision-33 baseline for empty databases and restore historical migrations in a separate module for existing older databases. Rationale: new installations need not execute obsolete migrations, while skipping releases must remain supported. Date: 2026-09-22.

Decision: Preserve terminal state independently of the database before re-exec. Rationale: the retiring client's store reader and mutation protocol may already be incompatible. The 0600 atomic handoff carries composer drafts (including deliberately cleared inputs), unsubmitted elicitation forms, workspace layouts, transcript anchors, client identity, and read positions. It is removed only after restoration. Frozen version-one fixture coverage protects future readers. Date: 2026-09-22.

## Outcomes & Retrospective


The reported schema-43/44 failure is fixed in shared startup. The full workspace suite passed, including migration from every revision 1–43, concurrent startup, newer-store preservation, and real terminal re-exec. Final CLI tests include 149 unit tests plus integration tests. Formatting, shared subprocess checks, and `cargo clippy --all-targets -- -D warnings` pass. No release or push was requested. Runtime regressions use temporary configuration/data directories and named test instances; the host's live store was not migrated. Executables released before this fix cannot retroactively acquire terminal handoff support; the new startup gate fixes their upgrade into this build, and terminals running this build gain automatic handoff for subsequent releases. The final commit message is `Make upgrades complete automatically across daemon and terminal startup` on `master`.

Final review also corrected the non-Unix process-exit probe, which previously always reported a live process, and enabled executable discovery on Windows. Terminal replacement without Unix exec now inherits stdin through a shared subprocess helper. Validation was executed on macOS; Windows branches receive the repository's existing CI compile checks.

## Context and Orientation


`mj-controller/src/controller/update.rs` installs release archives or delegates to package managers, then replaces the calling process with the installed executable. `mj-cli/src/main.rs` dispatches to the dashboard, API commands, or desktop. These use `mj-cli/src/daemon.rs::connect_or_start`, which holds `daemon-start.lock` across replacement and launch. `ControllerStoreGuard` owns a separate `controller.lock` that prevents two controllers from writing simultaneously. The new daemon migrates its store before publishing its endpoint in `daemon.json`. Its web viewer starts asynchronously, so local protocol readiness and HTTP readiness differ.

`mj-controller/src/database/schema.rs` checks schema revision and the minimum compatible revision. A typed `StoreSchemaMismatch` distinguishes a database needing forward migration from an incompatible newer database. Existing migration steps use individual SQLite transactions, making interrupted upgrades resumable at the last committed revision. `mj-cli/tests/daemon_startup.rs` exercises the real CLI with isolated storage; `mj-cli/tests/common/mod.rs` owns daemon teardown so files are removed only after their writers stop.

## Plan of Work


First expose a strictly read-only database compatibility probe and use it before any successful return from daemon startup. An older release must be replaced even with an unchanged protocol; a same-version development daemon must also be replaced if the database still needs migration. Retain the startup lock through stop, launch, and readiness. Older clients must not automatically replace newer daemons. Prove replacement with isolated real-process tests, including simultaneous starting clients and metadata whose version matches but whose store is old.

Next restore the original pre-baseline migration ladder in `mj-controller/src/database/legacy_schema.rs`, called only for existing revisions below 33. Keep historical SQL unchanged and validate preserved sessions/history as well as schema revision. Test repeated opens and migration failure/retry. Keep newly created stores on `baseline.sql`.

Finally audit the service and attached-client paths for avoidable manual restarts during a handoff. Wait for asynchronous API readiness within a bounded interval and report the actual failure if startup fails. Add behavior coverage where the current design returns a retry instruction. Document the upgrade invariant in repository engineering guidance so future migrations and release changes must preserve it.

The terminal implementation lives in `mj-cli/src/dashboard/upgrade.rs`. `DaemonPresence::Upgraded` carries the newer daemon executable and generation. The dashboard captures local state and returns `DashboardExit::Restart` after draining durable work and restoring the terminal. `main.rs` shuts down the old runtime, writes the handoff atomically, and execs the new executable with the original arguments and instance environment. On startup the new process loads the handoff in a blocking task, selects the saved workspace if it still exists, and restores local state before its first frame. `mj-chat` supplies serialization for unanswered form drafts and transcript positions; text field editing internals are intentionally reconstructed from their text.

## Concrete Steps


From `/Users/ryansvihla/code/mjolnir`, inspect the diff and run:

    cargo fmt --all -- --check
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo test -p brokk-mj-controller database::
    cargo test
    cargo clippy --all-targets -- -D warnings

Run every Cargo test outside the restricted sandbox. Use the dev profile. Tests that launch `mj` pass `--instance upgrade-test` together with temporary `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. No test may read or migrate the default live store.

## Validation and Acceptance


A CLI started against a live older same-protocol daemon succeeds without input, replaces it exactly once, and reads a current database with original user rows intact. Concurrent clients converge on that same replacement. An old store with no daemon migrates on ordinary startup. Reopening is idempotent. A newer compatible daemon is reused; incompatible newer data is rejected without mutation. Historical stores migrate directly to the current schema. Delayed HTTP readiness is awaited automatically. Existing startup-failure diagnostics continue to name the underlying error instead of blaming missing metadata.

## Idempotence and Recovery


Startup and migrations must be retryable without manual cleanup. Never delete or reset a database to satisfy readiness. Keep the sole-writer guard across migrations, retain transaction boundaries, and validate that a failed step can be retried after its concrete cause is resolved. Test cleanup stops owning processes before deleting temporary storage. Stage only task changes and commit on the current branch; do not push.

## Artifacts and Notes


Reported reproduction:

    mj: upgraded to v2.16.0; restarting
    Error: Mjolnir database schema 43 is not the supported schema 44;
    start the Mjolnir daemon to migrate it

The regression must prove the absence of this failure on normal startup, not merely change its wording.

Validation evidence is retained locally in `target/seamless-upgrade-tests.log`, `target/seamless-upgrade-cli-tests.log`, `target/seamless-upgrade-process-tests.log`, and `target/seamless-upgrade-clippy.log`. All corresponding commands exited 0. The full suite reports more than 4,000 passing tests, including isolated child-process test runs; live-service tests retain their existing ignored status.

## Interfaces and Dependencies


Use the existing `StoreSchemaMismatchReason`, `DaemonMetadata`, management `Stop` protocol, subprocess helpers, and SQLite connection library. Add no workspace crate. Database readiness must call the strict reader even in test builds. Version ordering must use semantic version comparison rather than string comparison. All filesystem probes and blocking subprocess work in asynchronous paths run through `spawn_blocking`.

Revision note: Updated after final validation to record passing full-suite and Clippy checks, preserved handoff compatibility, and the small cross-platform process corrections. Existing older executables cannot be changed retroactively; the invariant is implemented at the receiving startup boundary and preserved for subsequent upgrades.
