# Preserve profile-switch workspaces and restore Git state correctly

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries,
Decision Log, and Outcomes & Retrospective current throughout implementation.

## Purpose / Big Picture

A profile switch on an unchanged target must preserve its checkout, including
the Git index, ignored build artifacts, and untracked files. Failure or restart
must not turn that promise into deletion and recloning. Fresh restores must
reconstruct the checkpoint's HEAD, index, and files exactly. Queued commands
selected for execution must actually execute once after destination readiness.

## Progress

- [x] 2026-09-28: Audited Move and reproduced staged deletions caused by restoring
  the checked-out branch ref before checking out the archived commit.
- [x] 2026-09-28: Fixed shared Git restore; full checkpoint suite passed (146 passed, 2 ignored). Committed as 7414cc74.
- [x] 2026-09-28: Normalized resource selections and preserved prepared TUI selections; all 848 TUI tests passed.
- [x] 2026-09-28: Worker-only rollback and retained Move retry implemented; 12 focused in-place tests passed.
- [x] 2026-09-28: Added explicit restore/discard/defer policy with legacy bool decoding; durable receipt restart regression passed.
- [x] 2026-09-28: Skipped publication and clone-source checks for retained environments and fixed elapsed display.
- [x] 2026-09-28: Completed focused regressions, full dev-profile workspace tests (one timing failure passed in the complete worker-suite rerun), clippy, and isolated real-worker acceptance.
- [x] 2026-09-28: Completed the remaining Move changes for the implementation commit.

## Surprises & Discoveries

The observed session took 137 seconds, including 42 seconds deleting its clone,
55 seconds cloning, and 12 seconds checking out its original base f7117a8.
The TUI always requested resource clearing for bare targets, and eligibility
rejected that flag even when no resources existed. Git restore updated the
current branch ref before checkout, preserving the old index as staged changes.
Existing failure tests explicitly expected destruction of retained targets.

Real-worker validation exposed a dev-build stack overflow in nested Move
lifecycle futures. Heap-owning phase futures at their construction boundaries
fixed the crash without changing runtime stack sizes. The real-worker profile
switch then completed in about six seconds while retaining the checkout inode,
ignored build output, and staged/unstaged/untracked changes.

The full workspace test run passed every suite except one worker verdict timing
test, whose 200ms timeout elapsed under concurrent validation load. The complete
worker-suite rerun passed all 669 tests; no verdict implementation was changed. End-to-end cleanup
also needed explicit acknowledgement for its deliberately dirty checkout.

## Decision Log

- 2026-09-28: Limit environment preservation changes to profile switches with
  unchanged targets, mounts, and allocations, as selected by the user. Apply
  the shared Git correctness fix to all callers.
- 2026-09-28: Keep verified checkpoints and original queued-command IDs. Do not
  repair the live session, install a new daemon, push, or publish as part of
  this work. All runtime validation uses isolated instances.

## Outcomes & Retrospective

Implementation and validation are complete. All 1,973 controller tests, 848 TUI
tests, and 669 worker tests pass, along with the other workspace suites. Clippy
passes with warnings denied. Isolated real-worker acceptance passes with exact
Git state, checkout inode and ignored-output preservation, and exactly-once
queued execution. No live session repair, installation, or push was performed.

## Context and Orientation

`mj-controller/src/controller/move_session.rs` owns preparation and durable
Move intent. `lifecycle.rs` checkpoints and seals the old worker (preventing
further commands); `resume/in_place.rs` replaces the harness inside the retained
environment. `resume.rs` shares restoration and rollback with ordinary Resume.
`mj-checkpoint/src/archive/git.rs` reconstructs repository snapshots.
`mj-checkpoint/src/checkpoint/restore.rs` creates the destination worker's relay
seed, including command receipts used to deduplicate submissions. The TUI move
wizard lives in `mj-tui/src/wizards/dashboard/resume.rs`; elapsed display is in
`mj-tui/src/combined.rs`.

## Plan of Work

First check out the archived commit before restoring saved refs, then restore
patches and untracked files. Extend clone regressions to compare full Git state,
including committed additions absent from the destination's initial checkout.

Resolve resource-clearing requests centrally against actual source allocation,
including legacy CPU/memory fields. Let both unchanged-selection and in-place
decisions consume the resolved selection. The TUI must submit prepared values.

For retained moves, use worker-only stopping on failure; never invoke target
destruction or checkout retirement. Preserve the durable intent and target
identity for retries. Recovery must distinguish an unsealed source from an
already sealed checkpoint and a partially installed destination. Retained
targets that cannot be proven must report errors, not fall back to cloning.
Ordinary Resume recovery must not bypass retained Move ownership.

Give checkpoint restoration an explicit queue policy: restore, discard, or
defer admission to Move. Defer clears the destination's initial queue and only
its verified unexecuted queued receipts; Move then resubmits original IDs after
readiness. Preserve all other command receipts and existing destination-store
checks. Decode legacy restore specifications compatibly.

Skip publication and clone-source checks when retaining the environment while
preserving checkpoint and profile validation. Report total operation time and
log the retention decision. Add behavioral tests at the real boundaries.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir`, on the existing branch. Read local
AGENTS.md instructions, use normal mbx/Cargo storage, and do not redirect target.
Run focused package tests as each change becomes coherent, then run:

    cargo test
    cargo clippy --all-targets -- -D warnings

Run cargo test outside the restricted sandbox. Use existing isolated test
fixtures and `--instance move-preservation-test` for any direct new-build
runtime commands. Format only changed Rust files, review the diff, and stage
only owned files for each validated commit.

## Validation and Acceptance

Compare restored HEAD, branch, complete status, index, and file contents with
the captured source for clean and dirty clones, detached HEAD, refs, and stashes.
Exercise actual TUI bare-host selection and controller normalization. Inject
failures and restart at source sealing, worker reset, and destination readiness;
the same checkout and ignored/untracked files must survive successful retry.
Queue tests must use retained durable receipts and prove execution exactly once,
including restart and lost acknowledgement. A retained profile switch must not
run publication network checks or restart the elapsed clock between stages.

## Idempotence and Recovery

Never mutate live session data. Retain unrelated working-tree changes. Failed
switches stop worker processes before touching worker files and retain the
workspace for explicit retry. Once destination queue admission begins, retries
must use the same destination store and command IDs. No automatic replay of an
interrupted active prompt is permitted.

## Artifacts and Notes

The isolated Git experiment reported `D  new-file` with the original restore
ordering and a clean status with checkout before ref restoration.

## Interfaces and Dependencies

Reuse shared subprocess helpers, existing Move/session records, and target
ownership gates. Prefer retaining the existing persistent representation. If
implementation requires a new durable format, add a forward migration with
explicit compatibility classification and isolated upgrade tests. Preserve
legacy checkpoint restore-spec decoding; new workers receive current specs.

Revision 2026-09-28: Recovery reuses existing Move/session fields: failed retained
swaps use Error with their original target and checkpoint; ordinary Resume
refuses to bypass that ownership. No database migration is needed. Move uses
durable receipt admission and releases only pins acquired by Move, preserving
receipt ownership transferred from the source. The worker restore-spec reader
accepts the previous discard boolean; current writers emit a single queue policy.
