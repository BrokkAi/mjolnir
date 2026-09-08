# Prepare Move before its final confirmation

This plan follows `.agents/PLANS.md` and is maintained through validation.

## Purpose / Big Picture

The final Move screen must show current interruption and queued-work information before asking the user to proceed. One activation of its ready Move button must close the dialog and start the operation. Loading and preparation failures must remain visible within the dialog.

## Progress

- [x] Confirmed the first Move activation only prepares, and the success reply explains the second activation in a temporary notice.
- [x] Prepare automatically on entering review, with persistent loading/error states and stale-reply protection.
- [x] Integrate CLI request identities and attachment-validation replies; remove the temporary second-click instruction.
- [x] Final TUI suite passed (358 tests, 2 ignored); Clippy passed with all targets and warnings denied.
- [x] Workspace tests, final TUI regression rerun, Clippy, formatting, and diff checks passed. Prepared the completed fix for commit and the requested upstream push.

## Surprises & Discoveries

Preparation is asynchronous already, but replies are matched only by session ID. Moving preparation earlier makes back navigation, changed selections, and reopening the same session important stale-reply cases.

Disabling Submit lets the shared form choose another focused control. Enter on the still-pending default Move action must be intercepted before form dispatch, otherwise it could activate Cancel. Queue disposition is separate from the prepared destination and does not invalidate preparation; actual attachment edits do.

## Decision Log

- Decision: Retain daemon preparation and execution APIs, changing when the UI requests preparation.
  Rationale: Preparation collects information for consent; it should precede the enabled final action.
- Decision: Give preparation requests an identity echoed by CLI replies.
  Rationale: A reply for an obsolete selection or closed dialog must not enable a new confirmation.

## Outcomes & Retrospective

Implementation, integration review, and validation are complete. The full workspace run passed, including all five PTY integration tests. The final TUI suite passed all 358 enabled tests, including preparation, loading, single execution, failure/retry, stale replies, prepared queue choices, and attachment removal. Clippy, formatting, and diff checks passed. No real session move was started as validation. The final screen now owns preparation status and consent; the ready Move action starts the operation once.

## Context and Orientation

`mj-tui/src/wizards/dashboard.rs` handles profile, target, and review input. `ResumeWizard` in `mj-tui/src/wizards.rs` also represents Move. `DashboardAction::MoveSession` in `mj-tui/src/lib.rs` distinguishes preparation from execution. `mj-cli/src/dashboard/actions.rs` sends the preparation request in a background task; `mj-cli/src/dashboard/io.rs` receives `MovePrepared`. Execution already consumes preparation and closes the dialog.

## Plan of Work

Luna owns TUI state, rendering, and behavioral tests. Prepare on entering review and after changes affecting preparation. While loading, show progress and prevent execution; preserve Cancel and Back. On failure, show the error and an explicit retry action. On success, display the prepared activity and queue information and enable Move. Reject obsolete asynchronous replies using request identity.

Root owns CLI integration, review, validation, and publication. Echo request identity through the background task. Remove the temporary instruction to click Move again. Keep failures in the matching dialog and do not surface obsolete errors.

## Concrete Steps

From `/home/jonathan/Projects/hel2`, run tests outside the sandbox:

    cargo test -q -- --test-threads=1
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Commit only the changed files on the current branch and push its configured upstream, with elevated permissions for Git operations if needed.

## Validation and Acceptance

Behavior tests must prove automatic preparation before confirmation, no execution during loading, one ready Move activation, visible failure with retry, and rejection of replies after selection changes or reopening. Existing Resume behavior must remain intact. Required Rust checks must pass; inspect the integrated diff before committing.

## Idempotence and Recovery

Preparation is read-only background work. Cancelling or navigating away must not move a session. Existing daemon lifecycle validation remains responsible for rejecting changed source state at execution. No live move is authorized as a test.

## Artifacts and Notes

The former success notice read: “review the current activity and queued work, then press Move again to confirm.” The revised dialog itself communicates readiness.

## Interfaces and Dependencies

Use the existing MoveSelection, MovePreparation, background dashboard IO channel, and form controls. Add a request identity to preparation actions and replies; do not add dependencies or change daemon wire APIs.
