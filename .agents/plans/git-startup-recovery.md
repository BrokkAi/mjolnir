# Recover startup after project discovery on older Git

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir must start with Git 2.25.1 on an SSH machine without storing command options as repository paths. A migration that legitimately takes more than a minute must have time to finish, and startup failures must name the daemon's actual diagnostic log. Restore the user's stopped instance without touching accepted worker turns.

## Progress

- [x] (2026-09-30) Diagnosed a 74-second schema upgrade and confirmed Git 2.25.1 echoes `--path-format=absolute` with exit status zero. Identified one invalid project-location row and no invalid catalog snapshots.
- [x] (2026-09-30) Replaced all unsupported path-format queries, shared relative Git-path interpretation, and rejected invalid locations before writer admission.
- [x] (2026-09-30) Startup failures now include the launched daemon's structured log and detached stderr. Startup retains its early notice and a five-minute bound; paused-time regression reaches readiness after 75 seconds.
- [x] (2026-09-30) Isolated regressions, the full dev-profile test suite, strict all-target Clippy, rustfmt, and diff checks passed.
- [x] (2026-09-30) Backed up and repaired the single diagnosed live row under both ownership locks. Reused the user's completed installation, verified its binary equals the release artifact, started and stopped an isolated smoke daemon, and verified the user's production daemon responds.
- [x] (2026-09-30) Source changes, regression tests, and this completed recovery record form the required current-branch commit.

## Surprises & Discoveries

Git 2.25.1 prints an unknown `rev-parse` option as a result and exits successfully. The remote checkout `/home/jonathan/Projects/bifrost-fuzz` was recorded with both roots equal to `--path-format=absolute`. Schema migration 68 rebuilt the sessions table; the migration ledger proves that this step took 74 seconds. Diagnostics were present in `logs/mj-daemon-20260930T230741.477Z-1327367.log`, while the launcher read only detached stderr in `daemon.log`.

## Decision Log

Use the supported `--git-common-dir` command everywhere, with one shared interpreter that resolves relative results against the Git working directory and rejects invalid output. Validate project-location roots before entering the serialized database writer, so invalid discovery cannot poison its committed state. Retain a finite startup bound of five minutes to accommodate large-store migrations, and retain the existing early wait notice and prompt exit detection. Do not rewrite shipped migrations or change schema revisions for this code-only repair.

## Outcomes & Retrospective

The live instance is restored: daemon 1643799 reports version 2.24.0, runs the installed build, and serves the user's attached client. Corrected discovery has populated three precision-3260 locations with absolute roots, and its log has no path-related database publication failure. Schema remains revision 69. The backup is `/home/jonathan/.local/share/mjolnir/backups/mj-before-git-path-repair-20260930T232756Z.sqlite3`. Full dev-profile `cargo test` completed successfully, including 2,072 controller tests, 595 core tests, 908 TUI tests, 257 CLI unit tests, 12 daemon-startup tests, and 12 terminal regressions. Strict `cargo clippy --all-targets -- -D warnings`, rustfmt, and diff checks passed. Logs are `/tmp/mj-git-startup-tests-final.log` and `/tmp/mj-git-startup-clippy.log`. Existing ignored tests remain ignored. No worker turns were cancelled for this repair.

## Context and Orientation

`mj-core/src/repository.rs` discovers checkout identities using a local or SSH command executor. `mj-core/src/local_git.rs` and `mj-controller/src/controller/worktree.rs` also interpret Git common-directory output. `mj-controller/src/database/projects.rs` admits discovered locations to the sole database writer. `mj-cli/src/daemon.rs` launches and waits for the daemon. `mj-cli/src/logging.rs` owns diagnostic filenames and retention. Cargo's target symlink is owned by mbx and must not change.

## Plan of Work

Replace all `--path-format=absolute` queries with supported commands and share relative-common-directory interpretation. Require an absolute checkout result and the expected output shape. Reject malformed location paths before SQL writes. Discover the launched process's structured log using the logger's existing filename parser; include detached stderr as well, since failures before logger initialization still go there. Extend the existing paused-time regression past 60 seconds and test poisoned Git output and launch-specific diagnostics.

## Concrete Steps

Work on the current branch in `/home/jonathan/Projects/mjolnir`. Run `cargo test` and `cargo clippy --all-targets -- -D warnings` with elevated permissions and the dev profile. Tests must retain their isolated directories; manual new-build checks must use `--instance git-startup-recovery`. Run `cargo fmt --all -- --check` and `git diff --check`. The user ran `scripts/install.sh` concurrently after these edits and requested reuse of that completed installation. The installed `mj` exactly matches `target/release/mj`; no duplicate installer was run. Stage only this task's files and commit without pushing.

## Validation and Acceptance

Tests must show an older Git executor resolves ordinary and linked checkouts correctly, malformed successful output is rejected before storage, a daemon can become ready after 75 seconds, and failure reports show the launched daemon's structured log while excluding older launches. Verify the corrected Git query on precision-3260. After isolated validation and installation, the production daemon must accept a read-only status request and its log must show no path-related writer failure.

## Idempotence and Recovery

For the live repair, obtain the same exclusive file locks used by startup and controller ownership, make a SQLite backup before mutation, and update only the verified invalid location using the exact previous values as predicates. Resolve its correct checkout and main repository on precision-3260 first. Never kill workers or remove their files. If the daemon is running, do not write its store behind its owner. Installation is authorized by the user's request to fix the failure; production recovery uses the installed build, never a target-directory test binary.

## Artifacts and Notes

The failing daemon logged: `database publication failed after store_project_location ... mount history for "project:precision-3260" contains a non-absolute source path`. Correct roots from the remote Git query are `/home/jonathan/Projects/bifrost-fuzz` and `/home/jonathan/Projects/bifrost`.

## Interfaces and Dependencies

Use existing `CommandExecutor`, `CommandSpec`, `Path`, `PathBuf`, logger filename parsing, and shared subprocess helpers. No dependency, crate, protocol, or database shape changes are needed.

Initial plan recorded the diagnosed failure and recovery scope.

Update: recorded source fixes, isolated smoke verification, live recovery, and user-owned installation reuse. The first full run passed 2,072 controller tests but one TUI fixture expected the default port; the suite-wide MJ_INSTANCE setting changed it. The rerun keeps explicit isolated directories without that suite-wide setting.

Final update: recorded passing full validation and successful production recovery. The user-owned installation already contains the final source changes; the isolated smoke instance was stopped after verification.
