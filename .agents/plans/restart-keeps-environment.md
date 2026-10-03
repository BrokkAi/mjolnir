# Restart a session without replacing its environment

This ExecPlan follows `.agents/PLANS.md` and is a living record of the implementation.

## Purpose / Big Picture

Restarting from the web viewer or TUI will remain one user operation, but the daemon will first try to checkpoint and seal the session, then reset the worker and harness inside the existing target. The existing target, workspace, `/tmp`, and container layer survive. If the target cannot pass the read-only in-place checks, Restart will use the existing suspend-and-resume behavior. A human can verify the behavior through controller tests that inspect target command stages and through dispatch tests proving both clients issue the same Restart action.

## Progress

- [x] (2026-10-03) Mapped current web/TUI Restart composition, in-place Move restore, target cleanup, and the stuck-Closing path.
- [x] Add one daemon Restart action and route web and TUI through it.
- [x] Persist restart intent and resume it during daemon startup.
- [x] Reuse in-place restore with queue replay; fall back only on preflight failures.
- [x] Add focused behavior and dispatch tests; run focused tests and clippy for changed crates.
- [x] Record final outcomes and any remaining limits here.

## Surprises & Discoveries

- `SessionState::Closing` is reported as active. Both old clients therefore call Suspend again before Resume, which enters interrupted-close recovery and waits for the worker actor. That fails in the reported case where the worker is gone while its container remains.
- The existing in-place Move restore has all target-independent reset behavior needed here, but it assumes a Move-owned failure and deliberately discards queued commands before Move admits them separately. Restart needs an explicit preflight error boundary and the ordinary `discard_queue: false` replay behavior.
- The existing `MoveOperation` record is not a suitable Restart record: it carries Move semantics, queue-admission policy, and user-facing recovery state. The Restart intent will use a separate additive SQLite table so a failed or interrupted restart is distinguishable from an ordinary suspend.

## Decision Log

- Decision: Keep restart orchestration in one daemon lifecycle operation and add one `DaemonAction::RestartSession`; web and TUI call that action instead of composing Suspend and Resume.
  Rationale: one owner serializes interruption, cleanup fallback, queue restoration, and cancellation. The daemon protocol must advance because non-management actions are only valid behind a protocol bump.
  Date/Author: 2026-10-03, Codex.
- Decision: Persist an additive restart-intent row with phases for stopping, in-place restore, and suspend/resume fallback; startup resumes unfinished rows.
  Rationale: accepted daemon work can span long target operations and must be recoverable after daemon replacement. A separate table is ignored and preserved by older writers, so the migration remains compatible.
  Date/Author: 2026-10-03, Codex.
- Decision: Treat `Closing` or `Error` with a checkpoint and target as eligible for in-place restore without asking the missing worker to seal again. A preflight failure selects the legacy fallback before worker reset or record transition; failures after reset begins retain the target for retry and do not fall back.
  Rationale: this handles the reported worker-gone/container-alive state while preserving the no-destruction boundary.
  Date/Author: 2026-10-03, Codex.
- Decision: Restart passes `discard_queued_prompts: false` and `replay_queue: true` into in-place restore. Same-target Move keeps its existing queue policy.
  Rationale: this matches Restart's current `discard_queue: false` semantics without changing Move.
  Date/Author: 2026-10-03, Codex.

## Outcomes & Retrospective

Restart is now a single daemon-owned lifecycle. Running sessions are checkpointed and sealed while retaining their target; sessions already in `Closing`, `Error`, `Stopped`, or interrupted provisioning can use a retained checkpoint without requiring the old worker actor. The in-place path resets the worker and harness in the existing target, replays queued commands, and avoids target cleanup. Missing/unreachable targets and other preflight failures log and enter the legacy suspend/resume path; failures after restore starts keep the target for retry. An additive compatible schema migration (revision 73) stores restart phases for daemon-startup recovery.

Focused controller Restart tests passed (28 tests), the client and TUI single-action dispatch tests passed, strict clippy passed for controller/client/CLI, and the web suite passed 77 unit tests plus 125 deterministic browser tests (3 lab-only skips). The web unit suite must run outside the restricted sandbox because it spawns Playwright. The existing same-target Move retention test was confirmed in source but its direct run did not finish: its isolated child stayed in the test without output for more than ten minutes; a retry then waited behind another Cargo invocation on the shared target. The focused Restart suite did pass its in-place Move recovery tests.

No commit was created, per request. The web create-size task's edits remain concurrent and were not modified here.

## Context and Orientation

`mj-controller/src/server_runtime/actions.rs` handles web viewer actions inside the daemon. `mj-cli/src/dashboard/actions.rs` handles the TUI dashboard, and `mj-client/src/daemon.rs` defines the local daemon wire action. `mj-controller/src/daemon/` owns lifecycle admission and recovery. `mj-controller/src/controller/lifecycle.rs` checkpoints and seals a worker; `mj-controller/src/controller/resume/in_place.rs` resets the harness inside its current target. Ordinary Resume and Suspend remain the fallback. The term “preflight” here means checks that read or probe the target before the in-place restore changes the durable session state or resets worker files.

The SQLite schema started at revision 72. Restart adds revision 73 as a compatible migration: older binaries do not query or rewrite the additive intent table, and the compatibility floor remains unchanged. The task does not modify `mj-worker/src/main.rs` or the web create-session form/resource-allocation logic.

## Plan of Work

Add a SQLite table and controller database helpers for a restart operation ID and phase. Admission creates the row before target work. Startup loads unfinished rows and starts the same Restart lifecycle so accepted work can continue. Success or explicit cancellation removes the row; a recoverable error retains it.

Add a daemon Restart action and client method, then update the web Restart branch and TUI dashboard Restart branch to send it once. Use the current record's settings from the daemon rather than trusting potentially stale client snapshots. Bump the daemon protocol by one.

In the controller, retain the existing target while checkpointing active sessions, then call the existing in-place restore. Its Restart mode accepts a previously Closing/Error session with a checkpoint and target, returns a typed preflight failure before mutation, replays queued commands, and retains the target on failures after restore begins. A preflight failure records the fallback phase and runs the old Suspend/Resume flow within the same lifecycle admission. Log the selected path and the reason for fallback.

Keep the existing Move mode's eligibility, reset behavior, and queue policy unchanged. Add tests for no target cleanup during Restart, fallback when the target is missing, queue preservation, the existing same-target Move retention behavior, and web/TUI dispatch to the single Restart operation.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, run focused tests for each changed crate with `cargo test -p <crate> <filter>` and run `cargo clippy -p <crate> --all-targets -- -D warnings`. Per repository instructions, Rust tests run outside the restricted sandbox with normal Cargo/mbx storage; do not redirect `target/` or alter Cargo configuration. If a web-viewer JavaScript file changes, also run the web unit and e2e tests. Do not commit this work.

## Validation and Acceptance

The controller Restart test must observe one in-place worker-root reset and no target cleanup, target-volume removal, container removal, or target creation stage. A missing-target test must show the fallback branch is selected. A queued command present in the checkpoint must be replayed once by command identity after Restart. Existing same-target Move coverage must continue to show the workspace and target survive. Web and TUI dispatch tests must both reach `DaemonAction::RestartSession` with the session ID, with no client-side Suspend/Resume pair.

The stuck-Closing case is accepted when a session with a valid checkpoint and reachable target can enter in-place restore without a worker actor; the state must not remain orphaned in `Closing` after a failed restore. A preflight failure may fall back; an error after the worker reset begins must retain the target and durable intent for retry.

## Idempotence and Recovery

The migration uses `CREATE TABLE IF NOT EXISTS` and a monotonic migration revision. Restart intent creation is idempotent per session and operation ID; startup resumes any unfinished row. Once the new worker is ready, intent deletion is safe to repeat. Cancellation removes intent so startup will not perform a user-cancelled restart. Failed restore keeps intent and the environment; a later startup retries from the checkpoint. The existing daemon upgrade admission waits for the lifecycle task, while a process crash leaves enough SQLite state to resume it.

## Artifacts and Notes

The read-only lifecycle investigation is recorded at `.mj/agents/e93ae290e9a8bcd363cc5ccc365fa525/restart-move-investigation.md`.

## Interfaces and Dependencies

The daemon wire action will be `DaemonAction::RestartSession { session_id: String }`, with `DaemonClient::restart_session`. The daemon lifecycle kind will be `Restart` and will retain an external cancellation token for the web action plus the existing `CancelLifecycle` path for TUI cancellation. The SQLite intent exposes phases `Stopping`, `RestoringInPlace`, and `Fallback`. Controller in-place restore exposes a typed preflight failure distinct from a restore failure after mutation. Restart uses the existing `Controller::suspend_session_controlled_with_manager` retention disposition and `Controller::restore_session_in_place` worker-root reset; ordinary `SessionResumeOptions { discard_queue: false }` defines queue behavior.

## Plan revision notes

Created after tracing the current Restart call chain and identifying the stuck-Closing failure mode. The separate intent table was chosen to avoid overloading Move's persisted semantics and to make long-running Restart resumable after daemon replacement. Completed 2026-10-03; existing same-target Move coverage remains in place, but its direct rerun is inconclusive because of the isolated-test hang and shared Cargo lock.
