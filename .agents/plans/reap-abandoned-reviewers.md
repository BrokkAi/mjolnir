# Reap abandoned reviewer children before parent replacement

This ExecPlan follows `.agents/PLANS.md` and must be kept current as work proceeds.

## Purpose / Big Picture

After a reviewer fails to start, its parent must remain able to close or upgrade its container. Reviewer workers borrow the parent's environment but own private conversation journals. Cleanup must synchronize those journals, stop the worker and then release its borrowed target. A running review must survive daemon replacement. Issue #1448 tracks the defect; #1447 separately tracks startup reliability.

## Progress

- [x] (2026-11-04) Trace the driver, review host, child close and container replacement decision.
- [x] (2026-11-04) Claim #1448. Delegation was attempted but this session disallows children, so the parent owns all slices.
- [x] (2026-11-04) Correct cleanup dispatch and supervise asynchronous close retries.
- [x] (2026-11-04) Recover abandoned children from durable review state and preserve Error-child journals.
- [x] (2026-11-04) Report concrete blockers from the atomic container replacement decision, and exempt borrowed children from the API publication preflight.
- [x] (2026-11-04) Confirm the original regression fails, then pass all 53 reviewer-host tests, including retry and partial-group recovery.
- [x] (2026-11-04) Full controller suite passed: 2,269 passed, 19 ignored, no failures; isolated Error-reviewer journal and API publication regressions passed. Workspace Clippy and formatting checks passed.
- [x] (2026-11-04) Commit `a3c2ac0e6` passed pre-push review and was pushed to origin/master.

## Surprises & Discoveries

The driver emits CloseReviewer for every settled group, but `review_host/roles.rs` honored it only for cancellation. Push completion bypassed those requests and replaced the group with an empty state before looking up its children. Startup retained every registration, including roles with durable failed outcomes. Contrary to the initial hypothesis, SessionState::is_active already includes Error; target ownership must also include a stopped record retaining a target. Internal suspension already acknowledges publication; the external publication refusal came from `server/api/turns.rs` and now follows the existing sub-agent ownership projection.

The original failed-start regression failed after five seconds with “a failed reviewer is closed, retrying a transient cleanup refusal.” After the fix all 53 reviewer-host tests passed in 1.10 seconds. Tests use the existing mbx Cargo wrapper, with normal cache storage.

## Decision Log

Use the review driver's existing role outcomes to decide which children still belong to accepted work. Do not introduce a second liveness predicate or exempt reviewers from the container safety gate. Retain the child relation and target as the durable cleanup record; supervise retries in the host and let startup resume unfinished closes after handoff. Do not introduce a schema migration.

Keep implementation here because the child tool refused startup with “this session does not allow Mjolnir sub-agents.” No live store changes or live cleanup are authorized by this plan.

## Outcomes & Retrospective

Implementation and validation are complete. The failed-start retry, partial-group recovery, push cleanup, Error-child journal and parent upgrade, and API publication regressions passed. The complete controller suite passed (2,269 tests, 19 ignored), as did workspace Clippy with warnings denied and rustfmt checking. Commit `a3c2ac0e6` passed pre-push review and was pushed to origin/master. No live installation or default-instance changes were performed.

## Context and Orientation

`mj-review/src/driver.rs` owns review phases and role outcomes. `mj-controller/src/review_host/` serializes their events, persists snapshots through an ordered lane, and launches background operations. `environment.rs` enumerates durable reviewer relations and calls daemon suspension. `controller/lifecycle/subagent.rs` preserves the private relay journal before worker cleanup. `database/session_restart.rs::try_commit_container_upgrade` atomically decides whether any child target or accepted delegation still blocks replacement. `daemon/restart.rs` projects that decision to lifecycle state.

## Plan of Work

First extend the existing failed-review regression to require closing the failed child, including a transient close error. Correct dispatch before push completion can discard registrations. Add supervised cleanup jobs with bounded exponential retry delays; shutdown cancels these waits while durable relations remain for the next daemon.

Next change startup retention to preserve only registrations whose roles are still awaited by the restored driver. Invalid snapshots must fail recovery before closing any children. Include retained targets in discovery even if the session is stopped. Treat an Error child with a target as needing journal synchronization before deletion. Fix the failed-review snapshot write if it still contains the earlier running phase.

Finally return the child IDs and pending-delegation blocker from the existing database transaction, report them in the container upgrade notice, and exercise failed reviewers and subsequent parent replacement against an isolated database and real close machinery.

## Concrete Steps

Run from the repository root with the existing mbx Cargo wrapper and its normal build storage. Never set CARGO_TARGET_DIR or change Cargo cache configuration. All cargo test commands run with elevated sandbox permissions.

    cargo test -p brokk-mj-controller a_failed_review_clears_durable_active_state_and_the_prompt_hold
    cargo test -p brokk-mj-controller review_host::tests
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

Use existing isolated test helpers for store-backed cases, with their own MJ_DATA_DIR and named MJ_INSTANCE. Review the diff, stage only changed files, commit on master, request the required pre-push review, and push HEAD:master. If upstream advances, fetch and merge origin/master.

## Validation and Acceptance

The failed-start regression must show the child is eventually closed even if its first close attempt fails; the baseline stays unchanged. Startup must close orphaned or settled reviewers while keeping an unfinished peer. A retained Error target must block the same replacement transaction before close and permit it only after journal synchronization and target release. Parent files and child conversation history must survive. Existing review handoff regressions must continue to pass.

## Idempotence and Recovery

Child closes are idempotent, preserve shared repository data and retain their relation after stopping. Retry failed cleanup with a capped delay. Daemon shutdown aborts only the waiting cleanup task; accepted lifecycle close has its own durable intent. No live default instance, database edits, container changes, build-layout changes or installation is part of this work.

## Artifacts and Notes

The report identified six reviewers whose close released a blocked container upgrade. The reporting session confirmed completion after all six targets were cleared. That evidence is background context; tests use isolated fixtures only.

## Interfaces and Dependencies

Reuse TurnReviewDriver::restore and awaited_commands for ownership, tokio::task::JoinSet for supervised asynchronous cleanup, and ReviewEnvironment::close_child for the existing daemon lifecycle. Container commit must expose a structured committed-or-waiting result from its existing serialized database transaction so diagnostics cannot disagree with admission.

Revision 2026-11-04: initial plan after tracing the actual dispatch and snapshot ordering defects.

Revision 2026-11-04: record completed implementation, the red-to-green reviewer regression, and the API publication preflight correction found in the report.

Revision 2026-11-04: record successful complete validation; logs are `/tmp/mj-1448-controller-tests.log`, `/tmp/mj-1448-clippy.log`, and `/tmp/mj-1448-review-tests.log` in this session's host filesystem.

Revision 2026-11-04: record the clean review and completed implementation push.
