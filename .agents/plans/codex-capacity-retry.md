# Continue Codex sessions after capacity failures

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

The persistent worker sends `Continue` after capacity failures with delays of 1, 2, 4, 8, then 16 minutes, repeating the cap indefinitely. The UI shows the wait and Stop cancels it. Conversational pause interpretation is out of scope.

## Progress

- [x] Investigated the incident and worker relay scheduling.
- [x] Implement durable retry classification, scheduling, and cancellation.
- [x] Expose status and automatic prompt provenance in TUI/web.
- [x] Validate required checks; commit this coherent implementation checkpoint.

## Surprises & Discoveries

The native error is `server_overloaded`; ACP surfaced `Selected model is at capacity. Please try a different model.` as an agent message followed by a turn boundary. Relay command outcomes already preserve extensible stop-reason strings; durable command IDs support idempotent submission.

Routine background checkpoints use the same barrier as explicit close operations. They must preserve retry state; the explicit close/move latch sends CancelTurn before sealing. A monotonic timer is necessary so unrelated event wakes do not reset the wait (also exposed by the fake-clock test).

## Decision Log

- Retry indefinitely at 16 minutes after the cap, explicitly selected by the user.
- Own deadlines in the persistent worker. Journal live classified completion outcomes so historical ordinary capacity text cannot create retries.
- Use normal durable command admission and deterministic IDs. External prompts, cancellation, configuration, and explicit lifecycle operations invalidate retries. Routine checkpoint barriers defer dispatch and preserve the deadline.

## Context and Orientation

`src/hel_worker.rs` owns DurableRelay admission and journaling. Its snapshot module applies events deterministically and supplies operational state. `mj-worker/src/hel_worker_runtime/unix.rs` coordinates runtime events and dispatch in an async select loop. Controller and chat surfaces consume operational snapshots.

## Plan of Work

Add one bounded final-message classifier and durable retry state derived from classified live completion. Apply transitions in the snapshot reducer so completion/scheduling and retry admission/deadline consumption are atomic. Add an asynchronous timer to the coordinator and submit through durable admission. Expose optional retry status, render remaining wait, enable cancellation while waiting, and label generated prompts without changing their actual text.

## Milestones

1. Durable worker recovery: completed. Live text/structured capacity outcomes schedule a persisted deadline; tests exercise delay growth, ordinary checkpoint holds, cancellation, readiness, and journal-only reopening.
2. Client visibility: completed. TUI and web show the remaining delay, expose cancellation, and label the unchanged Continue prompt as automatic. UI behavior tests pass.
3. Validation and commit: complete. Focused capacity tests, the full default-workspace test suite, rustfmt, diff checks, and Clippy pass. Commit this validated checkpoint on the current branch.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Implement the state machine and behavior tests, then UI integration. Run `cargo fmt --check`, elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, and applicable web checks. Stage only changed files; commit on the current branch without pushing.

## Validation and Acceptance

Fake Codex capacity completion must produce no prompt before 60 seconds and exactly one Continue afterward. Cover the capped backoff, streamed output, nonmatches, reset after success, cancellation/input/config/lifecycle races, restart/replay idempotency, independent sessions, and disconnected clients. Verify waiting status and cancellation in TUI/web. Old journals and archives must remain readable.

## Idempotence and Recovery

Deadlines and generated IDs are durable. Replay restores the same deadline and cannot enqueue twice after acceptance. Old events without classified outcomes do not gain retries. Persistence and submission failures propagate visibly through worker supervision.

## Artifacts and Notes

Incident: 2026-09-10T06:19:39Z, native session 01a086e8-cd0a-7321-bcc6-57371fed3f6d.

## Interfaces and Dependencies

Optional retry state carries attempt, deadline, and command ID in operational snapshots. Use existing serde, Tokio, journal, ACP, and queue dependencies. Derive deadlines deterministically from event timestamps.

## Outcomes & Retrospective

Implementation is complete. The initial full run exposed a timer reset in the new worker test; the timer now retains an Instant across unrelated events. Focused capacity tests, `cargo fmt --check`, `git diff --check`, and `cargo clippy --all-targets -- -D warnings` pass. `cargo test --quiet` also passed across all default workspace packages, integration tests, and doc tests. The fake-clock worker test proves one Continue after the deadline without a controller; journal-only reopening preserves the deadline and accepted retries remain unique. Routine checkpoints preserve recovery, explicit controls cancel it, and TUI/web tests verify countdown and automatic provenance. No live session was modified or restarted for validation.

Revision: implementation clarified the distinction between routine checkpoints and lifecycle cancellation, and added monotonic timer coverage.
