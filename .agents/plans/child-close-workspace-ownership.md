# Close sub-agents without checkpointing their parent's workspace

This ExecPlan follows `.agents/PLANS.md` and is a living document. Maintain its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections throughout implementation.

## Purpose / Big Picture

Issue #1384 reports a child close blocked by an unrelated nested Git worktree in the parent's workspace. A sub-agent borrows its parent's workspace; only the parent owns the responsibility to archive that workspace. After this change, closing a child preserves its conversation and report, stops its own processes, and succeeds without exporting repositories. A parent checkpoint still refuses unsupported nested repositories, with an explanation naming the directory and how to exclude disposable scratch work. A fresh close can retry a completed failure even when its original caller disappeared.

## Progress

- [x] (2026-10-28) Trace workspace export, borrowed-worker cleanup, and lifecycle admission; assign #1384 and add agent-in-progress.
- [x] (2026-10-28) Reproduce the completed-close request-key conflict: the isolated regression failed with the issue's exact different-selections error after the original waiter disconnected.
- [x] (2026-10-28) Implement child teardown with durable conversation synchronization, durable Destroying state before private cleanup, and rollback when cancellation wins before commit.
- [x] (2026-10-28) Improve nested-repository diagnostics and add a real-Git regression.
- [x] (2026-10-28) Pass the three child-close regressions and real-Git nested-worktree regression.
- [x] (2026-10-28) Run full touched-crate suites: checkpoint 166 passed / 2 ignored; controller 2,177 passed / 17 ignored / one shell-startup timeout. The failed container-runtime case passed alone in 1.56 seconds.
- [x] (2026-10-28) Pass default-workspace `cargo clippy --all-targets -- -D warnings` and formatting checks.
- [x] (2026-10-28) Pass the strengthened completed-request regression, which replays after completion without removing the cached result, then admits a fresh key.
- [x] (2026-10-28) Finish documentation tests (both crates have zero runnable doc tests) and pass the two affected cancellation-message regressions.
- [x] (2026-10-28) Commit the implementation on master as `97c6fb7d6`; merge origin/master cleanly as `a51be83ae` after the required push was rejected.
- [x] (2026-10-28) Pass all three child-close regressions and the worker process-cleanup regression on the merged source; pass default-workspace Clippy and formatting again.
- [x] (2026-10-28) Push the validated implementation and merge to origin/master at `a51be83ae`.
- [x] (2026-10-28) Prepare this completed execution/validation record for publication.

## Surprises & Discoveries

Git's `ls-files --others --exclude-standard -z` reports a nested linked worktree as one directory path ending in a slash. Both `.mj/lanes/` and arbitrary locations behave identically unless ignore rules differ. The archive writer rejects the directory before inspecting any symlink inside it.

The existing lifecycle owner already marks completion under its lock and retains the result for waiters. Admission nevertheless compares request keys on completed operations of the same kind. A disconnected waiter can leave that retained result in place, and a fresh request is then rejected as if work were still running.

The existing no-checkpoint teardown distinguishes borrowed children from workspace owners, but live children currently go through the ordinary archive path. Final relay events must be synchronized to the controller's database before deleting a child's private relay journal.

The first live-child regression completed teardown and preserved the parent, report, nested-worktree content, and large conversation. Its final title assertion was incorrect: the relay deliberately discards harmless metadata received after the sealed close cut. The test now checks that the final Closed event is durably projected. The nested-worktree regression's empty-input check also needed to inspect tar entries because an empty tar still contains terminator bytes.

## Decision Log

- Decision: Keep ordinary parent checkpointing strict and improve its diagnostic rather than silently skipping nested repositories.
  Rationale: A nested repository can hold unique uncommitted work. Only an explicit Git ignore rule establishes that it is disposable for the outer checkpoint.
  Date/Author: 2026-10-28, Codex.
- Decision: Reuse the existing lifecycle owner, relay barrier, and borrowed-worker cleanup. Treat a retained completed result as history rather than an active reservation.
  Rationale: A second ownership flag or a parallel close mechanism would introduce races. Existing relay synchronization persists conversation events without a workspace archive.
  Date/Author: 2026-10-28, Codex.
- Decision: Restore an unsealed child to Running when cancellation wins before the close commits, and keep interrupted sealed closes recoverable.
  Rationale: Leaving the cancelled close intent durable would let startup recovery later close a child the user had chosen to keep running.
  Date/Author: 2026-10-28, Codex.

## Outcomes & Retrospective

Child close no longer exports the parent's repositories or native-session files. The isolated relay regression preserves a conversation larger than 128 KiB, the child's report, the parent worker, and a registered nested worktree. Cancellation before commit restores Running, and an interrupted Destroying child finishes cleanup without a worker or archive. Retained completed results answer identical request keys without blocking fresh keys.

The complete checkpoint suite passed (166 passed, 2 ignored). The complete controller suite passed 2,177 cases with 17 ignored and one failure caused by Bash not initializing within the harness probe's 10-second deadline; that unchanged container-runtime case passed alone in 1.56 seconds. Default-workspace Clippy, formatting, documentation tests, and final cancellation-message checks passed. The fix is committed as `97c6fb7d6`. The push encountered four concurrent upstream commits, merged cleanly in `a51be83ae`. All three child-close regressions and the worker process-cleanup regression passed on that merged source, as did Clippy and formatting. The validated implementation and merge are now published on origin/master. No implementation work remains; full nested-worktree checkpoint support remains outside this issue's approved scope.

## Context and Orientation

The daemon is the controller process. Its `RuntimeStateOwner` in `mj-controller/src/daemon/owner.rs` serializes lifecycle decisions; `mj-controller/src/daemon/lifecycle.rs` admits work and publishes results. A lifecycle is a session operation such as close, restart, or destroy. A worker owns the harness process and a journal of conversation events. `StandaloneSession::sync` in `mj-controller/src/session_manager/standalone.rs` durably applies journal events to the controller store.

`mj-controller/src/daemon/close.rs` selects a close route and runs controller lifecycle operations. `mj-controller/src/controller/lifecycle.rs` implements teardown and has `stop_target_and_settle`, which removes only a child's private worker state when it borrows its parent's target. `mj-controller/src/controller/checkpoint/barrier.rs` can pause worker dispatch and cancel a running turn. The relay Close command seals that pause at a known event position. These operations can preserve the child's conversation without exporting filesystem content.

The implementation is `Controller::suspend_subagent_controlled` in `mj-controller/src/controller/lifecycle/subagent.rs`. `CloseRoute::Subagent` routes every child there before ordinary interrupted-close recovery. Running and Closing children synchronize and seal their relay first; parked, failed, and Destroying children finish private cleanup without requiring a live worker or checkpoint. Destroying is persisted only after the final conversation is durable, so a crash during cleanup does not require reconnecting to a removed worker.

`mj-checkpoint/src/archive/git.rs` gathers Git's nonignored untracked paths and writes files and symlinks to an archive. It currently reports directories as an unsupported file type without identifying a nested repository. Tests live beside these modules. Existing test helpers provide isolated database directories and subprocesses, plus a recorded relay protocol fixture.

## Plan of Work

First add a lifecycle regression that abandons the initial wait, lets a close fail, confirms replay of the same key returns that failure, and submits a fresh key. Change admission under the existing owner lock so a completed result with a different request key can be replaced, while active requests retain their conflict protection and identical retries retain their result.

Next route every sub-agent close according to child ownership before ordinary interrupted-close recovery. A live child must seal its relay and synchronize final conversation events using existing barrier primitives, then persist teardown intent before stopping and removing its private worker state. Parked, failed, and already-destroying children can use the existing no-checkpoint cleanup. Preserve report directories and session records. Keep parent cleanup and checkpoint gates unchanged. Exercise these transitions with isolated store and relay fixtures, including the failure path that leaves private state retryable.

Finally diagnose Git-reported directories that contain a `.git` entry as unsupported nested repositories. State the relative directory and the explicit scratch-ignore remedy. Do not follow or archive the nested repository. Test using a real registered linked worktree and actual Git enumeration, then demonstrate that an explicit ignore rule removes that path from the checkpoint input.

## Concrete Steps

From the repository root use normal Cargo/mbx configuration without changing target directories. Run every Cargo test outside the restricted sandbox. During iteration use filtered tests, for example:

    cargo test -p brokk-mj-controller child_close
    cargo test -p brokk-mj-controller completed_close
    cargo test -p brokk-mj-checkpoint nested_worktree

After the final edits, run the complete suites of both touched crates once and workspace Clippy on the dev profile:

    cargo test -p brokk-mj-controller -p brokk-mj-checkpoint
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Use existing test-isolation helpers for database tests, with their own MJ_DATA_DIR and MJ_CONFIG_DIR. Any manually launched binary must use `--instance issue-1384`. Stage only files changed for this issue, commit on the current branch, and push HEAD to origin/master. If upstream moved, fetch and merge origin/master; do not rebase or change branches.

## Validation and Acceptance

The lifecycle regression must fail before the fix with the reported different-selections error and pass afterward. It must also reject a different key while the first operation actually runs. Child-close regressions must show that unrelated nested-worktree content cannot trigger an export, the parent's session and workspace remain, the child's conversation and report survive, and no child archive is required. Cover live, parked, failed, and interrupted child states without weakening parent interrupted-close recovery.

The real-Git archive regression must report the nested worktree directory and `.git/info/exclude`, without blaming an inner target symlink. Ignoring that scratch path must allow collection. Parent checkpoint failures must continue preserving the target. All required checks must pass before commit, or remaining external failures must be documented precisely.

## Idempotence and Recovery

An interrupted child close must retain durable teardown intent so its next close continues without recreating a harness or requiring a workspace archive. Stop the process group before removing worker files. Completed request results remain replayable through the existing lifecycle/delegation machinery; fresh requests are admitted only after completion. Tests use temporary isolated stores and leave live sessions untouched.

## Artifacts and Notes

Initial Git reproduction returned `.mj/csharp-parity/bin-src/` and `.mj/lanes/probe/` as directories. Ignoring `/.mj/lanes/` removed only the second entry; ignoring both scratch roots removed both.

## Interfaces and Dependencies

Use existing `RuntimeStateOwner`, `LifecycleWatch`, `ControllerRelayLease`, `RelayCommand`, `StandaloneSession`, and borrowed-worker cleanup helpers. No schema revision, wire protocol change, new crate, or new dependency is needed. Keep path handling in `Path`/`PathBuf` and retain the existing platform cleanup scripts.

Plan created on 2026-10-28 after approval of the proposed fix.

Updated on 2026-10-28 with the reproduced request-key failure, implementation progress, pre-commit cancellation recovery, and corrected test expectations for existing relay and tar semantics.

Updated on 2026-10-28 with passing focused regressions, full-suite results, the successful isolated shell-timeout rerun, and passing Clippy/formatting checks. Documentation tests and final cancellation-message checks are still running.

Updated on 2026-10-28 after all planned local checks passed and the implementation was committed. The clean upstream merge includes worker-shutdown changes, so child-close/process-cleanup regressions and Clippy are being checked on that merged source before publication.

Completed on 2026-10-28 after merged-source regressions, Clippy, and formatting passed and the implementation was pushed to origin/master. This final record captures the approved ownership boundary, the reproduced defect, validation evidence, and the successful publication.
