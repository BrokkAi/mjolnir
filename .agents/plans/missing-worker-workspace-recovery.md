# Diagnose missing worker checkouts and stop invalid recovery

This ExecPlan follows `.agents/PLANS.md` and is updated through implementation.

## Purpose / Big Picture

A missing session checkout must produce an actionable session failure instead of endless worker restarts and misleading credential-sync warnings. Preserve the recovery archive and session identity so normal Resume can restore the session deliberately. Missing credentials are not the observed problem.

## Progress

- [x] (2026-09-08) Verified the failed worker's executable exists, but its recorded managed worktree does not exist on the host. Its source repository and session branch remain.
- [x] (2026-09-08) Implemented workspace-aware worker recovery, daemon-owned missing-target persistence, conditional database writes, and actionable ACP cwd diagnostics.
- [x] (2026-09-08) Verified the live recovery archive checksum and corrected the stale Running record to recoverable Error through the existing daemon RPC, preserving files and branch.
- [x] (2026-09-08) Verified regressions, workspace tests (with corrected CLI fixture rerun), doctests, Clippy, formatting, CLI build, and independent integrated review.
- [x] (2026-09-08) Completed the fix for commit on the current branch, with no push.

## Surprises & Discoveries

The live session `e0c11b7d8d0d1f622d0f0fbb5457ba63` was still `Running` during diagnosis. Its worker exit record reports an ACP bridge launch ENOENT; the missing path is actually its cwd, `/mnt/c/SSD2/Projects/autocull/.mj/worktrees/e0c11b7d8d0d1f622d0f0fbb5457ba63`. `git worktree list` contains only the main checkout, while the session branch remains. Before this fix, worker recovery checked process liveness only. Credential sync selects durable Running/Checkpointing rows. Generic relay failures do not change durable session state. Missing-target persistence previously depended on a TUI consuming the error.

The first full test run passed chat, controller, core, TUI, and worker tests, then exposed an incomplete credential-poller fixture: it had a session naming `codex` but no configured profile, so even its Running case was ineligible. Added that profile before rerunning CLI tests. This was a fixture failure, not a production failure.

## Decision Log

Check exact recorded working directories only during existing background worker recovery, after confirming a worker needs restarting. Reuse the existing local/SSH path helper and propagate local filesystem errors. Missing bare-target directories return a typed recovery outcome and the existing `ViewError::TargetMissing` with the specific path. Do not recreate the checkout from a branch alone: checkpoint restoration may be needed for dirty data.

The daemon must persist definitive missing-target failures even when no UI is attached. Use supervised background tasks, existing conditional missing-target persistence, and controller reload/revision publication so credential and worker targets converge. Preserve active lifecycle ownership and do not turn generic network failures into lost sessions. Include cwd in ACP launch errors so the diagnostic does not falsely implicate an executable that exists.

The user has been asked whether the live checkout was deliberately removed. Code repair and validation proceed independently; do not restore or stop the live session without resolving that intent.

## Outcomes & Retrospective

Implementation, independent integrated review, and validation are complete. Workspace suites passed before the CLI fixture failure; the corrected CLI package rerun passed all 207 unit tests and its enabled integration tests, including all five PTY termination tests. Workspace doctests, Clippy with warnings denied, formatting, diff checks, and the CLI build passed. The live session remains recoverable Error after checksum verification of its 2026-09-07 20:03 UTC checkpoint archive. No files were removed, no checkout was reconstructed, and no processes were stopped. The reason the checkout originally disappeared is unproven; this repair addresses the invalid restart and credential-sync loop after it disappears.

## Context and Orientation

`mj-controller/src/hel_controller/worker_binary.rs` constructs restart plans. `mj-controller/src/hel_session_manager.rs` executes them on a blocking background task after failed relay handshakes. A bare target runs directly on the local or SSH host. `mj-controller/src/hel_controller/worktree.rs` already interprets paths on these targets. `mj-cli/src/daemon.rs` owns runtime state and all durable writes; its manager update feed exists regardless of UI attachment. `mj-cli/src/pollers.rs` derives credential targets from session state. `src/hel_database.rs` already marks a missing target Error with a checkpoint or Lost without one. `src/hel_acp.rs` launches the agent bridge and currently omits cwd from spawn errors.

## Plan of Work

First extend `WorkerRecoveryPlan` with an optional `WorkerWorkspace` containing a `ManagedWorktreeTarget` and `PathBuf`. Populate it from the fresh launch configuration for local/SSH bare targets. Check existence before modifying worker binaries or restarting; report a missing workspace distinctly. Test missing versus existing checkouts and SSH construction.

Next move definitive failure recording into a daemon background path, preserving lifecycle guards and reporting failures. Existing database logic retains checkpoint metadata and removes Error/Lost sessions from credential targets after reload. Add behavioral coverage for this routing and state outcome. Improve ACP launch context and test a real executable launched in a missing cwd.

Finally run the integrated checks, build the local CLI, inspect the actual diff, and commit the authorized implementation. Keep source ownership disjoint: Luna owns controller recovery files; root owns daemon integration, shared helper exposure, ACP diagnostics, and final validation.

## Concrete Steps

From `/home/jonathan/Projects/hel2`, run every Cargo test with elevated permissions and normal repository build storage:

    cargo test -q -- --test-threads=1
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    cargo build -p brokk-mjolnir
    git diff --check

## Validation and Acceptance

A dead bare worker with a missing cwd must not execute binary refresh or restart commands. An otherwise identical worker with an existing cwd must still restart. A definitive missing-target update must become durable without a TUI, retain recovery data, and stop scheduling credential sync for that failed session. Ordinary transport failures remain retryable. ACP errors must name the working directory as well as the executable. Required tests and Clippy must pass.

## Idempotence and Recovery

All probes are read-only and run off event loops. Do not delete worker files, reconstruct a checkout from the retained branch, discard archives, or stop unrelated sessions. Normal Resume remains responsible for verified checkpoint/worktree restoration. Repeat checks and conditional persistence safely; retain ordinary lifecycle-state predicates.

## Artifacts and Notes

The worker-exit record names an existing `.../hel` executable. Host-side checks confirmed cwd missing and source repository present. Its retained branch tip is `b4e52b7b494ff7a3e37ba7a4e38f4a79146d40e0`. The latest observed successful checkpoint was logged on 2026-09-07 at 20:03 UTC.

## Interfaces and Dependencies

Reuse `ManagedWorktreeTarget`, `CommandExecutor`, existing subprocess helpers, supervised daemon tasks, `ViewError::TargetMissing`, and `mark_session_target_missing`. No new crate or schema is needed. Reuse existing test fakes; no external credentials are needed for regressions.

Initial revision records the confirmed filesystem failure and the repair boundaries.
