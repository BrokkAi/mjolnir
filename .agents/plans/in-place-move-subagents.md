# Preserve sub-agents across an in-place Move

This is a living ExecPlan maintained under `.agents/PLANS.md` in the repository root. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current at each stopping point.

## Purpose / Big Picture

An in-place Move replaces a session's harness while keeping its target, workspace, and session identity. After this work, children started through `mj-agents` remain owned by the same parent and keep running or parked when the destination harness can manage them. Requests already changing child state finish before the parent worker stops, and the new harness receives a compact roster and instructions for recovering interrupted reads. A destination without parent tools is refused when live children exist.

## Progress

- [x] (2026-10-05) Read the feature worktree's `AGENTS.md`, `.agents/PLANS.md`, and the child-side coupling report.
- [x] (2026-10-05) Trace the in-place checkpoint inputs and apply the requested stop condition to item 5.
- [x] Add an atomic worker-owned gate for mutating `mj-agents` requests and drain accepted mutations before an in-place source stop; reopening is explicit after replacement-worker reconnect.
- [x] Skip the child-stop hook for supported in-place Moves; retain child stopping for non-in-place Moves and the parked-only/no-parent-role case.
- [x] Refuse unsupported destination parent roles when live children exist, and tell the replacement harness about retained children.
- [x] Add focused behavior tests for retention, refusal, mutation admission/drain, notices, durable effects, and `ClosingSource` recovery.
- [x] Run workspace clippy and full suites for `mj-core`, `mj-worker`, and `mj-controller`; review the changes.
- [x] Commit the reviewed changes on the current branch.
- [x] (2026-10-05) Fix review findings: reopen ambiguous or recovered source gates, repair a closed gate on a safe relay reconnect, load the retained-child roster immediately before recording the notice, and fall back to stopping children for pre-v31 workers.
- [x] (2026-10-05) Run focused regression tests and the final workspace clippy plus complete touched-crate suite.
- [x] Commit the review follow-up on the current branch.
- [ ] Item 5 (checkpoint-free conversation handoff) is intentionally deferred under the user-provided stop condition; evidence and options are in the final report.

## Surprises & Discoveries

- The parent worker's `subagents.json` queue is durable under the stable session root; its Unix socket is process-local and is rebound by the replacement worker. Child workers and child records use their own session IDs and roots.
- A worker-side admission gate can share the queue mutex with `enqueue_marked`; this gives one serialized decision for whether each mutating request was accepted. The worker relay is the existing control route from the session actor to that endpoint.
- The checkpoint is not only a duplicate conversation log. `mj-checkpoint` validates and selects native artifacts before archiving; restore installs those artifacts under the destination harness home. Copying the raw profile home would not preserve this filtering or profile mapping.

## Decision Log

- Decision: Keep item 5 out of this implementation and retain the existing checkpointed in-place handoff.
  Rationale: `restore_session_in_place` consumes the checkpoint's verified canonical session and native artifact bundle; `mj-checkpoint` explicitly filters artifacts and secrets. Reconstructing that bundle from a live profile root is a substantial conversation-handoff redesign, which the user said is a stop condition. Items 1–4 remain independently implementable and will be committed.
  Date/Author: 2026-10-05, Codex.
- Decision: Close mutating-request admission in the worker endpoint, under the same lock used to persist requests; let read-only wait/list calls remain admissible and do not wait for them.
  Rationale: A daemon-only check could allow a request to enter the durable worker queue after the drain. The worker endpoint owns queue admission, so it must serialize the close against enqueue.
  Date/Author: 2026-10-05, Codex.
- Decision: Treat a destination as having parent tools only when the policy used for its launch produces a `SubagentMcpRole::Parent` or `FixedParent`.
  Rationale: `HarnessKind::supports_delegation_tools` alone is insufficient when a supported harness has a policy that launches without Mjolnir's parent role.
  Date/Author: 2026-10-05, Codex.

## Outcomes & Retrospective

Items 1–4 are implemented. Workspace clippy passed. Full suites passed for `mj-core` (639 unit tests plus 3 scenarios) and `mj-controller` (2,224 tests; 10 ignored). The `mj-worker` suite had 726 passes, 10 ignored, and one temp-worker ownership collision; that exact test passed when rerun alone. Item 5 remains deferred under its stop condition; the checkpoint and current retry/rollback path are unchanged. The focused fixture and clippy fixes are recorded in the test logs and commit.

Review follow-up: ambiguous admission-close replies now trigger an idempotent reopen attempt; if the relay remains unavailable, a worker actor retries after reconnect while holding the same lifecycle permit as Move and checking the durable Move phase. `ClosingSource` recovery runs through the same reopen finalizer. Retained-child prompt data and notices reload the child roster at installation time. Workers older than relay protocol 31 use the existing stop-children hook and get a notice explaining the fallback. Focused regressions passed, followed by workspace clippy and the full controller suite (2,230 passed, 10 ignored).

## Context and Orientation

The controller owns Move phase changes and the session database. `mj-controller/src/controller/move_session.rs` prepares and executes the operation; its `CommandExecutor::before_move_source_stop` hook is implemented by `mj-controller/src/daemon/support.rs` and currently stops children. `mj-controller/src/controller/resume/in_place.rs` reuses the target and worker root, while `mj-controller/src/controller/resume.rs` installs the worker and first-prompt context. The parent worker serves `mj-agents` from `mj-worker/src/worker_runtime/subagents.rs`; its MCP client is in `mj-worker/src/subagent_mcp.rs`. The worker relay contract is defined in `mj-core/src/relay.rs` and handled in `mj-worker/src/worker_runtime/unix.rs`; controller relay calls are in `mj-controller/src/worker_client` and `mj-controller/src/session_manager`.

“Mutating request” means `spawn`, `send_input`, `close`, or `interrupt`. `wait`, `list_agents`, and profile listing are observations and may be interrupted by stopping the parent. “Live child” means a child session whose `SessionState::has_live_worker()` is true; parked children have no worker and do not count against the live-child limit.

## Plan of Work

First add an endpoint gate that is shared by every clone of `SubagentEndpoint`. Request enqueue checks and updates the gate while holding the queue-state mutex, so closing admission and enqueueing cannot pass each other unnoticed. Expose the gate through a versioned worker relay request. The controller closes admission while it owns the source connection, returns the connection to the session actor so its existing delegation dispatcher can finish queued mutations, and reacquires the source connection after the queue contains no mutating requests. The wait reports progress and observes Move cancellation. If a source stop fails while the parent remains live, reopen admission. The worker preserves a closed gate across a process restart; the controller explicitly reopens it on the replacement worker before it is marked ready.

In `move_session.rs`, persist the `ClosingSource` phase before closing admission. For an in-place Move, drain mutations without invoking `before_move_source_stop`; for a normal Move keep the current hook. For an in-place destination with no parent role, refuse during preparation when a live child exists, and recheck after drain to cover a spawn accepted just before gate closure. If only parked children exist, stop and record them using the existing stopped-child path. The role-capable in-place path leaves both live and parked children untouched.

Build the retained-child prompt context from the current parent-child relations and session records, including child ID, task name, and running/parked state. Add it to `RestoreIntoTarget.resume_notices` so it is installed before the replacement harness is ready and also appears as a conversation notice. Keep the existing stopped-child context and notice.

Do not change checkpoint capture, archive restore, or rollback semantics. The checkpoint's verified canonical session feeds the projection and cross-harness compaction. The archive's selected native artifacts feed same-harness continuity and are restored under the destination profile home. Removing the archive would require changing that handoff contract, so it is outside this plan's implementation boundary.

## Concrete Steps

Work only in `.claude/worktrees/inplace-move-subagents` on branch `inplace-move-subagents`. Add focused tests beside the queue/gate code and in the existing controller Move/resume tests. Run affected tests after each implementation round with `cargo test -p <crate> <filter>` outside the restricted sandbox. After the final edit, run workspace `cargo clippy --all-targets -- -D warnings` and each touched crate's full test suite once, also outside the restricted sandbox. Do not redirect Cargo output to `/tmp`. Review `git diff`, stage only changed files, and commit on this branch without pushing.

## Validation and Acceptance

Focused tests must show that an in-place Move retains live and parked children, a non-in-place Move still stops them, and a role-less destination refuses live children but handles a parked-only roster by stopping and reporting it. A gate test must show that new mutations are rejected after closure, that each of spawn/send_input/close/interrupt remains pending until completed, and that wait/list are not part of the drain. Cancellation must return admission to a still-running parent. Notice tests must verify retained child IDs, names, states, interrupted-wait instructions, and the existing stopped-child notice. Crash/recovery tests for the gated `ClosingSource` phase must show the source is either completed through the existing checkpoint path or left retryable without children being stopped unintentionally.

At completion, all focused tests and the one-time full validation must pass. Report the commit hash, validation commands/results, and the item 5 stop-condition evidence.

## Idempotence and Recovery

Queue admission is closed before draining and remains closed through source stop. A cancelled or failed pre-stop operation reopens it only while the old parent worker is still live. The gate survives worker process restart; the controller opens it on the replacement relay before native readiness and first-prompt context installation. Move's durable `ClosingSource` phase precedes the gate side effect so daemon recovery can identify and finish the existing close path after a crash. No database migration or new session identifier is required. The worker full-suite lock collision passed on isolated rerun and did not reproduce.

## Artifacts and Notes

The child-side report at `.mj/agents/1aa6a921c3a59a979e2701de7206fb85/in_place_move_child_coupling.md` records that children retain their session IDs, roots, and DB relationships when the parent worker alone stops. In the checkpoint path, `mj-controller/src/controller/resume/in_place.rs` verifies the archive and retains its canonical snapshot; `mj-controller/src/controller/resume.rs` passes the archive into `CheckpointRestoreSpec`; `mj-checkpoint/src/archive.rs` stores explicit `NativeArtifact` entries rather than the full harness home.

## Interfaces and Dependencies

The endpoint must expose an atomic admission state transition and a shared predicate for mutating actions. The relay response must be version-gated so a controller refuses to perform an unsafe drain with an older source worker. The controller's drain must consume the existing `ManagedSessionView` updates and must not invent a second source of truth for child request completion. Child state/name data comes from the controller's persisted `SubagentRecord` and `SessionRecord`. No new crate, database table, or user-facing selection field is part of this work.
