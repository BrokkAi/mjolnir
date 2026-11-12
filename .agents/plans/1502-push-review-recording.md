# Preserve completed push reviews while recording approval

## Purpose

A clean review must remain available while recording its reviewed-tree refs waits or fails ambiguously. The user sees completion only after recording succeeds, a reason when the worker rejects recording permanently, and can cancel pending recording.

## Progress

- [x] Inspect issue, durable snapshots, actor queues, and worker errors.
- [x] Obtain design decisions from parent: reuse terminal snapshot; no migration; preserve typed worker errors; allow cancellation.
- [x] Implement delivery and queue timing changes.
- [x] Validate tricky-condition tests and touched-crate clippy.
- [x] Prepare validated changes for commit with requested issue and coauthor trailers.

## Context and Orientation

`mj-controller/src/review_host/resolve.rs` records approval and delivers verdicts. `reviewer_child.rs` already saves the terminal driver snapshot, including its clean verdict and exact trees, before delivery. `recovery.rs` restores that snapshot. Session manager actor and remote channels carry reviewer operations to the worker. The worker already classifies acknowledged errors with RelayRejected and its retryable field. Git recording uses update-ref on refs/mj/reviewed/<tree> with the captured commit.

## Decision Log

- Reuse active_group.driver rather than add a pending flag. It already contains the verdict and trees; retry and startup restore can replay recording without reviewer launches.
- No migration: stored shape and meanings already exist. An older daemon's handling remains its current behavior, not a new unreadable data contract. The parent explicitly rejected a semantic migration because its live-store compatibility cost exceeds its benefit.
- Preserve anyhow errors through reviewer channels so the worker owns retryability. Transport and lifecycle reservation failures retry with existing backoff; nonretryable worker rejection delivers failure.
- One delivery state machine arbitrates cancellation versus final persistence. Cancellation before committing invalidates the epoch and wakes the recording caller. After acknowledgement, committing contains only final delivery and persistence, and cannot race cancellation writes.

## Plan of Work and Milestones

First remove the queue-inclusive ten-second timeout and instrument session lookup, actor queue, role queue, connection acquisition, and worker request. Then keep clean snapshots on ambiguous failure, carry the delivered verdict back to completion, and protect cancellation from stale asynchronous writes. Finally test the real host paths using the existing isolated fake environment and real Git for ref replay, run controller clippy, and commit.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir-wt/fix-1502`, run outside the restricted sandbox:

    cargo test -p brokk-mj-controller review_host::tests::push_recording
    cargo test -p brokk-mj-controller review_host::tests
    cargo clippy -p brokk-mj-controller --all-targets -- -D warnings
    cargo fmt --all -- --check

## Validation and Acceptance

A paused-time recording wait beyond ten seconds must retain the caller and clean snapshot. An ambiguous error must emit a recording-failure notice, retain the clean snapshot, and repeat exactly the frozen trees. Real Git refs must remain correct after lost-ack replay. A worker nonretryable error must close with a failed verdict and no clean success notice. Cancellation while awaiting acknowledgement must close with a cancellation notice and discard stale delivery results. Existing review-host tests protect restart and final persistence retries.

## Idempotence and Recovery

The existing retry loop doubles delays up to thirty seconds and has no retry budget. Epoch changes and owner removal stop stale retries. Repeating update-ref for the frozen trees is idempotent. Startup restores the same terminal snapshot. No build uses live default-instance data, and no schema or wire changes are needed.

## Surprises and Discoveries

The channel formatted worker errors into strings, removing the existing retryability fact. Preserving its typed cause is necessary to terminate permanent failures without guessing from diagnostic text.

## Outcomes and Artifacts

Validation logs are in `/mnt/optane/fix-1502-*.log`; final results will be recorded here before commit. Only text evidence is copied to the parent report directory.

## Interfaces and Dependencies

Reviewer reply channels now use anyhow::Result<ReviewerOutcome>. Delivery returns its stored state and actual delivered verdict. The delivery state machine uses a mutex solely for short state transitions; filesystem work stays in supervised background tasks. No new dependency is introduced.

Revision note: initial plan records the parent-approved no-migration design and permanent-failure/cancellation requirements.

Final validation (2026-11-11): `cargo test -p brokk-mj-controller -- review_host::tests session_manager:: daemon::tests::in_process_reviewer` passed all 137 tests. This includes the four new recording regressions, with production per-role queuing and real Git replay both under ordinary retry and startup restore. Controller all-target clippy with warnings denied passed; rustfmt and diff whitespace checks passed. No test daemon uses the default instance. Initial compile diagnostics from converting the typed channels were fixed by updating senders, fakes, and the rejection fixture. No test failures remain.

Review follow-up: worker recording waits must hold no daemon handoff admission. Admission now begins only for final durable delivery, and an isolated subprocess test closes the real process gate while recording is pending, then verifies shutdown retains the clean snapshot. The parent also ruled that cancellation stops waiting/retries/delivery without revoking a ref representing a real completed clean review. Its notice now explicitly says approval may already be recorded, push is then allowed, and otherwise the user requests review again. No wire or schema extension is needed. The request to revoke such approval was declined with this parent ruling before the follow-up commit.

Follow-up validation: all 71 review-host tests passed on the final source, including the five recording regressions; controller all-target clippy with warnings denied and rustfmt passed again. The first handoff regression run failed because FakeManager dropped an undrained initial publication, leaving its session-update admission in the shared coalescer. The fixture now consumes that publication like the daemon, and the isolated handoff test passes. This was a test-fixture correction; no additional production admission mechanism was needed. Evidence: `/mnt/optane/fix-1502-review-tests.log` (initial failure), `/mnt/optane/fix-1502-handoff-retest.log` (passing targeted rerun), `/mnt/optane/fix-1502-review-host-final.log`, and `/mnt/optane/fix-1502-review-clippy.log`.
