# Repair project discovery and clean local stale configuration

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture


Opening Add in the project picker should discover usable projects without reporting deleted scratch directories as failures. Existing repositories must survive backfilling project identity even when their old configured repository IDs differ from newly discovered IDs. The user authorized fixing, pushing, and local cleanup; preserve active sessions, history, and project memory.

## Progress


- [x] Inspected the live store read-only: 27 missing paths, 11 non-repositories, and three memory-identity failures.
- [x] Located the shared configured memory identity implementation and the catalog's durable retry records.
- [x] Implement unavailable-candidate handling and share legacy memory identity between launch and migration.
- [x] Focused isolated regressions pass for automatic retry, memory preservation, raw/configured separation, and genuine errors.
- [x] Validate focused regressions, full dev-profile Cargo tests, Clippy, formatting, and git diff --check.
- [x] Remove the obsolete bifrost2 replay source under the existing config lock; preserve all other configuration and save a backup.
- [x] Build committed merged tree using the normal install script; isolated named-instance smoke test and normal live handoff succeed.
- [x] Extend unavailable-path classification to remote Git absence; complete core tests and workspace Clippy pass.
- [ ] Install the remote correction and reconcile the three remaining failure records.
- [ ] Commit only owned files and push to origin/master.

## Surprises & Discoveries


The healthy bifrost5 checkout resolves to brokkai/bifrost-dev while its old configured ID is bifrost. The healthy sm-watch checkout belongs to jbellis/sm-watch while its old saved ID is anvil. Raw sessions were incorrectly included in configured-project reconciliation and also received their namesake configured bundle during checkout backfill. That both caused ID lookup failures and could mix unrelated memory. Migration also constructed a Bundle memory key for single-repository projects, unlike the launch helper which collapses that case to Repository.

The first live reconciliation exposed three SSH history records for /home/jonathan/Projects/bifrost2 and /home/jonathan/Projects/tree-sitter-kotlin. Git exits 128 with an explicit missing-directory diagnosis; remote paths cannot be checked with controller filesystem calls. Extend the shared classification using that diagnosis and Git exit status, preserving SSH exit 255 and permission failures.

Unavailable history errors are durable and included in every refresh. Native history is seeded once, so missing paths from completed test sessions keep poisoning catalog status. The local config still includes a deleted Claude replay scratch directory under bifrost2.

## Decision Log


- Decision: Reuse one configured-source memory identity implementation for worker launch and migration. Rationale: Old identities must come from old sources, independently of the new snapshot's names or members.
- Decision: Share the complete legacy session identity calculation and exclude raw-directory sessions from configured-project reconciliation. Rationale: The isolated test demonstrated that merely fixing the old ID lookup still mixed unrelated bundle memory into raw-session memory. Both reconciliation paths now honor the same source ownership.
- Decision: Use typed repository-unavailable errors for missing local directories and Git's explicit non-repository diagnosis. Retire those candidates without deleting session history. Preserve every other failure, including permission errors, cancellation, invalid remotes, and SSH failures. Reconcile previous failures on daemon startup so upgrading repairs stored mistakes without a manual retry.
- Decision: Avoid a schema migration. Existing progress and retry records suffice to retire unavailable candidates, and new sessions or explicit directory selection can rediscover restored paths.
- Decision: Retain unavailable saved definitions needed by existing configured sessions. Rationale: hel-2 still has one running and 22 stopped configured sessions; deleting that entry would remove their resume source. Unavailable configured sources no longer poison background discovery, while selecting them still fails visibly.
- Decision: Complete live error cleanup through a normally installed updated daemon, never by editing live database rows. Build installation artifacts from an archive of the committed tree so concurrent uncommitted move-session changes are not installed.

## Outcomes & Retrospective


Focused resolver and catalog regressions, full dev-profile cargo test, workspace Clippy with warnings denied, formatting, and diff checks pass. Local config cleanup removed only the confirmed-deleted replay repository, using config.toml.lock and an atomic rename; the backup is config.toml.before-discovery-cleanup-20260930. The merged workspace passed cargo test, Clippy, and 70 web unit tests. The committed merged build ed8e5a11 was staged from merged-src with scripts/install.sh, tested using --instance discovery-recovery-20260930, then installed atomically with previous binaries saved under /mnt/optane/mj-discovery-recovery-20260930/previous-bin. The normal daemon restart succeeded (PID 3877688). Live reconciliation cleared all 21 seed failures and 17 of 20 discovery failures. Three discovery records (two distinct paths) remain because they belong to SSH sessions on precision-3260, where Git explicitly reports deleted directories. The remote resolver must classify these as unavailable too; SSH transport and access errors must remain visible. Final remote validation, installation, reconciliation, and publication remain. Host load exceeded 300 during validation; the worker journal stress test remained active and eventually passed.

## Context and Orientation


`mj-controller/src/project_catalog.rs` performs discovery in a supervised background task. `mj-controller/src/database/projects.rs` stores progress and failures. `mj-core/src/repository.rs` resolves Git checkouts through shared subprocess executors. `mj-controller/src/controller/worker_binary/project_memory.rs` computes legacy identities used by launched workers; move its configured-source interpretation into `mj-core/src/project_memory.rs` and reuse it. The daemon is the sole owner of live database mutation. Read-only SQLite inspection is permitted; do not change live database rows directly.

## Milestones and Plan of Work


First introduce an explicit unavailable-repository error in the shared resolver and prove missing/non-repository candidates are distinguished from genuine failures. Use that result in catalog discovery to log unavailable candidates at debug, advance progress, and clear previous failure records. Revalidate cached local locations rather than assuming a directory is still a Git checkout. Request one retry pass at daemon startup so old recorded errors are repaired by the new implementation.

Then move configured repository memory identity interpretation into the core helper, use it for launches and migration, and construct old bundle keys through the same bundle normalization used at launch. Isolated catalog tests must show that IDs can change without losing memory and that deleted/non-Git paths do not fail a refresh while genuine failures remain visible.

Finally validate the code and apply authorized local cleanup using serialized config mutation. Remove only the identified missing replay member and preserve the remaining primary repository. Use a validated updated daemon for live catalog reconciliation; do not run a target-directory build against the default store. Preserve workers across any normal daemon handoff. Commit owned code and plan files on the current branch and push to origin/master.

For local installation, archive the committed tree into `/mnt/optane/mj-discovery-recovery-20260930`, set MJ_BUILD_REVISION to that commit's full SHA, and run its scripts/install.sh using a staging CARGO_INSTALL_ROOT under the same directory. This uses the established native and musl worker build layouts and mbx rather than introducing a custom Cargo target layout. Smoke-test staged binaries with --instance discovery-recovery-20260930, stop that isolated daemon, back up existing installed binaries, and replace them atomically with the staged artifacts. Invoke the installed CLI to perform its normal daemon handoff. Read the daemon project catalog and the SQLite failure tables read-only to verify cleanup.

## Concrete Steps and Validation


From `/home/jonathan/Projects/mjolnir2`, run cargo fmt --all -- --check, cargo test, and cargo clippy --all-targets -- -D warnings. Every cargo test runs outside the sandbox; catalog tests use their existing child-process MJ_CONFIG_DIR/MJ_DATA_DIR isolation. Explicit daemon or CLI tests use --instance discovery-recovery-20260930. Tests should verify memory documents survive migration and late stale candidates do not change catalog success into failure. Real error fixtures must still return errors.

After local cleanup, inspect project-catalog status through the daemon and SQLite read-only, confirming obsolete failures are cleared and the configured bifrost2 project contains no replay source. Record exact test and cleanup outcomes here before marking completion.

## Idempotence and Recovery


Candidate retirement advances existing progress atomically and clears only its failure record. Session records and stored history remain untouched. Memory merge helpers preserve conflicting documents and create redirects for old writers. Config cleanup must acquire the existing config lock, reread the latest config, verify the exact obsolete source, and save atomically. A rerun should change nothing.

## Interfaces and Dependencies


Use anyhow downcasting for a typed RepositoryUnavailable error without new dependencies. Add configured-source identity interpretation to RepositoryMemoryIdentity and reuse it rather than keeping competing copies. All subprocesses use CommandExecutor and CommandSpec. Continue using the daemon's serialized database writer for live discovery updates.

Plan created for the authorized fix and cleanup; no schema or wire changes are planned.

Updated after focused validation: raw sessions must be excluded from configured-project reconciliation, and both launch and migration now share legacy session identity. Live config cleanup is complete; normal installation and daemon reconciliation are required to clear the previously persisted errors without violating database ownership.

Updated after merged-build validation and live handoff: local cleanup reduced 41 records to three remote records. Complete remote classification through the same shared resolver, without changing or deleting session history.

Remote correction validation: cargo test -p brokk-mj-core and cargo clippy --all-targets -- -D warnings both pass. The resolver regression distinguishes Git absence/non-repository responses (exit 128) from permission, ownership, and SSH transport failures (including exit 255). The prior full merged workspace and web validation remain valid for unchanged components.
