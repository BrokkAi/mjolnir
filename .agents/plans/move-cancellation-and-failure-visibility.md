# Finish move cancellation and failure reporting (#1137)

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

A move changes the profile or target of a session while preserving its work.
When it interrupts Claude, the worker must wait for the actual prompt response
before accepting a checkpoint barrier (a command that freezes a journal cut).
If the move fails, `mj sessions --json` must explain which phase failed, offer
the existing recovery guidance, and identify the detailed daemon log entry.
Private paths and credentials must not enter the public error for a live session.

## Progress

- [x] (2026-09-30) Investigate the original failures and current ownership.
- [x] (2026-09-30) Verify #1180 first: 41 cache tests passed on macbook; close it.
- [x] (2026-09-30) Publish safe move failures from the operation owner, atomically with its result.
- [x] (2026-09-30) Prove cancellation acknowledgements cannot admit a checkpoint early.
- [x] (2026-09-30) Focused tests passed: two outcome tests, one ACP/relay cancellation test, and both controller Close regressions.
- [x] (2026-09-30) Commit the fix separately as 4cd9c35e; integrate upstream in e9022119.
- [x] (2026-09-30) Complete full dev tests and all-targets Clippy before publication; formatting, docs, and all 70 browser unit tests also pass.

## Surprises & Discoveries

The report predates commits 34793fe0 (stopped-source Resume guidance), 664b6260
(retained source control connection), and 634af0a3 (worker ownership of the
checkpoint cut). Current ACP cancellation already waits for the prompt response;
a cancellation acknowledgement does not finish the prompt. Harmless journal
writes wait behind a ready cut; actual new work spoils it. A definitive Close
refusal restores Running rather than leaving an interrupted close.

The remaining reporting gap is in `Controller::finish_move_result`: the Move
record retains the detailed error, while SessionRecord retains a raw close
error that public live projections deliberately hide. Existing unrelated dirty
hunks in move_session.rs and daemon/session_move.rs add timing diagnostics and
must remain outside this commit.

## Decision Log

Decision: retain worker-owned cancellation and cut admission rather than adding
a timed controller idle check. Rationale: only the worker observes actual prompt
completion; an acknowledgement or elapsed time cannot prove it. Date: 2026-09-30.

Decision: use the existing public lifecycle-error prefix convention for Move,
and save the session's public message and Move result in one database transaction.
Rationale: the Move owner already has the failed phase and recovery decision;
publishing the outcome there also covers CLI, browser, and restart recovery without
competing supervisors. No stored shape or schema migration is required.
Date: 2026-09-30.

## Outcomes & Retrospective

The missing failure visibility is implemented and focused checks pass. The
Move owner records a safe phase/recovery/reference message together with its
durable result; a failed message write rolls both records back. The real ACP
adapter and relay regression confirms cancellation acknowledgement and an SDK
result cannot admit a checkpoint while the prompt response is outstanding.
No production sessions have been moved or interrupted during this investigation.
Full validation passed on integration commit e9022119. The controller reports
2,076 passed and 9 ignored; the TUI reports 901 passed and 2 ignored; the worker
reports 694 passed and 10 ignored. CLI unit and integration tests also pass.
All-targets Clippy with warnings denied completed successfully. Publication uses
the configured upstream `origin/master`; #1137 can be closed after that push.
The last plan-recording commit changes only this document and reuses validation
of the identical Rust source tree.

## Context and Orientation

`mj-controller/src/controller/move_session.rs` coordinates moves and persists
their final result. `mj-controller/src/database/session_move.rs` stores the Move
record through the shared database writer. `mj-core/src/state.rs` recognizes
public lifecycle errors; `mj-controller/src/server/viewer_types.rs` and
`server/api/types.rs` already publish these errors for live sessions.

`mj-worker/src/acp/claude_result_tests.rs` drives an actual ACP adapter with a
fake Claude peer. `worker_runtime/unix/dispatch.rs::record_runtime_event` feeds
adapter events to DurableRelay, the worker's durable command owner.
`relay/commands.rs` admits BeginCheckpoint only after effectful work and
autonomous harness turns finish. Existing controller checkpoint tests exercise
Close with late harmless output and late questions using isolated stores.

## Plan of Work

First add a public Move failure prefix. At finish_move_result, compose a safe
sentence from the phase before failure, existing recovery guidance, and the
operation identifier. Log the detailed error under that identifier. Persist the
operation and only the session's last_error/updated_at fields together through
the existing writer. Mutate the controller's session copy only after commit.
Successful Move clears its own prior public failure without clearing unrelated
errors. Transaction failures must roll back both records.

Then connect ClaudeProbe to the real runtime event recorder in a regression.
Queue a checkpoint while a prompt is running, dispatch CancelTurn, and let
Claude acknowledge cancellation and emit an SDK cycle result plus more than
64 KiB of output while retaining the ACP prompt response. Check that the
checkpoint remains unclaimed. Deliver the prompt response and verify the
barrier becomes ready. Reuse existing exact-cut and refused-Close regressions.

## Milestones

The first milestone makes a failed Move visible after database reopen. A focused
controller test must persist an internal error containing a private path/token,
observe a safe error with failed phase and operation reference through ApiSession,
and verify successful completion clears that message. A database test must prove
a failed result write cannot leave only one of the two records updated.

The second milestone proves the existing worker ownership across ACP and relay
layers. The cancelled prompt's acknowledgement and SDK result must not release
the queued checkpoint. Only the actual prompt response admits it. Existing
controller tests must still retain checkpoints and restore Running on refusal.

The final milestone commits the fix on the current branch, completes the full
dev suite and all-targets Clippy, and pushes to the configured upstream. Close
#1137 after these checks pass.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir2`, use normal mbx Cargo storage. Run all
Cargo tests outside the restricted sandbox. Focused commands are:

    cargo test -p brokk-mj-controller move_outcome
    cargo test -p brokk-mj-worker a_move_checkpoint_waits_for_the_cancelled_claude_prompt_response
    cargo test -p brokk-mj-controller a_suspend_seals_when_the_worker_journals_after_the_close_cut
    cargo test -p brokk-mj-controller a_refused_close_returns_the_session_to_running_and_releases_its_barrier
    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

Tests use existing temporary stores or named isolated child processes. Never
invoke the test build against the default live instance.

## Validation and Acceptance

All focused tests and full checks must pass. The public failure test must fail
on the original code because a running session has no public Move error.
The cancellation regression must observe CancelApplied and an SDK result while
the prompt remains active, no claimed checkpoint, and subsequent admission after
PromptFinished. Existing Close tests prove harmless writes cannot spoil a cut
and genuine new work returns the source to Running with its checkpoint retained.

## Idempotence and Recovery

Tests can be repeated without touching live data. If persistence fails, the
transaction rolls back and the controller retains its previous session copy.
Leave unrelated working-tree edits intact and stage only this task's hunks.

## Artifacts and Notes

Mac validation used `/Users/jonathan/Projects/mjolnir-tf-2026-09-29` at 4a5ba374,
which contains the Linux-only cache fix ed937e10, with `MJ_INSTANCE=tier1180`.
Its `test-1180-cache-host.log` reports 41 passed, 0 failed.

Local evidence is retained in `/tmp/mj-1137-worker-focused.log`,
`/tmp/mj-1137-controller-focused.log`, `/tmp/mj-1137-close-cut.log`,
`/tmp/mj-1137-close-refusal.log`, `/tmp/mj-1137-full-tests.log`, and
`/tmp/mj-1137-clippy.log`. Docs report zero errors/warnings/hints in
`/tmp/mj-1137-docs-check.log`. Browser unit checks report 70 passed, zero failed
in `/tmp/mj-1137-web-tests.log`; run these outside the sandbox because the
sandboxed Playwright subprocess returned an empty listing.

## Interfaces and Dependencies

Use existing rusqlite transactions and the shared database writer; add no crate
or migration. Add `MOVE_FAILURE_PREFIX` to the existing prefix convention and
`database::save_move_outcome` for atomic result/message persistence. Runtime
ownership remains with DurableRelay and the real ACP event recorder.

Revision note (2026-09-30): initial plan records the existing fixes so this task
finishes the missing reporting behavior and proves cancellation without adding
another idle predicate.

Revision note (2026-09-30): focused validation completed. Corrected the new
test's RefusingExecutor tuple fixture after compilation caught its missing
argument. Upstream gained f9bced67 during testing; merge it into the current
branch after the separate fix commit and validate the combined result.

Revision note (2026-09-30): record passing combined validation and the separate
implementation commit. Only the two original timing edits remain uncommitted.
