# Wait for current ACP startup before checkpointing

This ExecPlan follows `.agents/PLANS.md` and must remain current as implementation and validation proceed.

## Purpose / Big Picture

Stopping a recently restarted session must wait for its current ACP (Agent Client Protocol) process to open the native session. Restoring the native session ID from disk is not proof of readiness. Today this confusion starts a 30-second checkpoint timeout and unnecessarily restarts a healthy, slowly loading worker. Success means a startup lasting longer than the checkpoint timeout reaches one checkpoint without a restart, while cancellation and genuine startup failure remain bounded and visible.

## Progress

- [x] Confirmed incident: 30-second barrier timeout caused a restart, followed by another approximately two-minute native load; final checkpoint and stop succeeded.
- [x] Designed process-local readiness and an explicit pre-checkpoint startup wait.
- [x] Implemented relay readiness state, compatible wire field, controller gating, and transition tests.
- [x] Validate focused behavior, full Cargo tests, Clippy, and portable worker build (parallel worker-test caveat below).
- [x] Review the completed change; commit on the current branch.

## Surprises & Discoveries

`mj-controller/src/hel_controller/readiness.rs` treats any persisted native session ID as ready. `wait_for_idle_projection` then accepts restored idle state. Checkpointing has no initial startup gate and starts its 30-second barrier clock immediately. The previous ACP resume change removes redundant transcript replay but cannot remove the native application's own startup time.

## Decision Log

Add `acp_ready: Option<bool>` to operational state, not the durable snapshot. New workers always report a current-process boolean. Missing values mean legacy workers and retain the prior native-ID behavior; this is wire compatibility, not a fallback after a failed modern readiness report. Keep the protocol version unchanged for this additive optional field. Current workers reset readiness on open/restart/close and set it only after recording the live SessionConfigured observation. A failed observation append cannot publish readiness.

Use the existing cancellable, bounded native-session startup wait before submitting a checkpoint barrier. Share one readiness predicate with restart probes. A worker still initializing is not quiet for automatic worker upgrades. Preserve the existing checkpoint timeout/restart policy after real startup; do not extend the normal barrier timeout to mask stalls.

Implement the runtime and controller transitions directly because their persistence and cancellation behavior are coupled. Delegate the operational wire shape and mechanical test fixtures to Luna with nonoverlapping file ownership.

## Outcomes & Retrospective

Implementation, review, host build, and portable worker compilation are complete. Controller (660), core (820), chat (408), TUI (328), and CLI (175 plus integrations) tests passed. The full parallel run failed the unchanged obsolete-install shared-lease test in the worker package; it passed in isolation, and the entire worker package passed serially (105 plus 5 integrations). Clippy and diff checks passed. The user resumed the session and reported recurring checkpoint-disconnect notices. Live inspection found a background recovery checkpoint accepted at event 191489 and immediately interrupted at 191490 while the agent was working; the session remains running with no active barrier. Validation uses fakes and must not interrupt the user's active turn.

## Context and Orientation

`src/hel_worker.rs` owns DurableRelay, the in-memory owner of the journal and persisted snapshot. `src/hel_worker/snapshot.rs` separates persisted RelaySnapshot from transmitted RelayOperationalState. `mj-worker/src/hel_worker_runtime/unix.rs` records live ACP events. `mj-controller/src/hel_controller/readiness.rs` waits for native startup; `checkpoint.rs` obtains the connection and then requests a checkpoint barrier, which freezes dispatch at a verified event cursor. `worker_restart.rs` uses readiness before declaring a replacement worker ready.

## Plan of Work

First add the optional operational readiness field and shared predicate, updating explicit test literals. Initialize an in-memory DurableRelay boolean to false on every open, overlay it onto operational state, and update it only following successfully journaled live SessionConfigured or termination observations. Historical replay must not set the flag. Verify close and in-process ACP restarts reset it appropriately.

Then make the readiness probe use the shared predicate, checking Closed before native ID. Wait under a visible ACP startup stage before checkpoint submission, including the retry connection. Readiness timeout or cancellation must return without requesting another worker restart. Before submitting a routine recovery barrier, sync and defer if the agent is already running, explicitly returning the healthy connection lease to the session actor. This prevents intentionally abandoning a newly opened barrier for a routine busy observation. Preserve the existing post-submit busy handling for activity that races with submission, and keep genuine disconnect warnings visible. Keep normal checkpoint barrier failures separately eligible for the existing recovery path.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Review each changed file and run:

    cargo fmt --all
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    git diff --check

Run Cargo tests outside the restricted sandbox, per AGENTS.md. Use the established target directories; never redirect build storage to /tmp. Commit only files changed for this task on the current branch.

## Validation and Acceptance

A relay with a persisted native ID must report not ready immediately after reopen and ready only after this process records SessionConfigured. A terminal or restarting process becomes not ready. Legacy operational JSON without the field still decodes. A fake startup held beyond 30 seconds must not receive BeginCheckpoint before becoming ready, and must subsequently checkpoint without replacement. Cancellation during startup returns promptly, and closed startup errors rather than using the old native ID. Existing checkpoint timeout and retry tests must still pass. A background copy during an already active turn must defer without recording any new BeginCheckpoint acceptance or interruption, while the agent remains running and no barrier is active. Full tests, Clippy, and portable compilation must complete successfully.

## Idempotence and Recovery

No database migration or live session mutation is required. The new runtime flag is rebuilt on each worker launch and is never trusted from disk. Older controllers ignore the additive field. Tests use isolated temporary resources; preserve the user's active session and backup snapshot. If integration reveals a compatibility issue, correct the source and rerun affected tests rather than editing stored session state.

## Artifacts and Notes

Incident log on 2026-09-07: checkpoint timeout 03:05:30Z; replacement starts 03:05:35Z; checkpoint ready 03:07:38Z; target stopped 03:08:01Z. These establish an unnecessary restart during startup and a subsequent successful close.

## Interfaces and Dependencies

Add `RelayOperationalState::native_session_is_ready() -> bool` and the optional `acp_ready` field. Keep readiness storage private to DurableRelay. Reuse NativeSessionProbe and wait_for_native_session_in_stage for cancellable startup gating, without a new crate or dependency.

Revision note: initial plan records the confirmed failure, additive compatibility policy, implementation boundaries, and behavior-level acceptance criteria.

Revision note: readiness now explicitly follows SessionConfigured, the same boundary used by worker command dispatch. Added a real 31-second fake-startup regression; its archive fixture must cover the two startup metadata events to test unchanged-archive reuse correctly.

Revision note: the delayed-start regression passes after modeling an actual resumed session (SessionOpened with resumed=true), whose startup does not add a fresh-session transcript notice. Added pre-submission busy deferral and checkpoint-event regression coverage for the user's recurring disconnect notices.

Revision note: the busy-deferral regression found that merely returning the deferral error dropped the healthy managed connection. Explicitly release that lease before returning; this prevents the unnecessary reconnect as well as barrier creation. Full validation is running against this integrated correction.

Revision note: the busy regression now checks new checkpoint acceptance/interruption events directly rather than requiring the entire journal frontier to stay fixed; unrelated session metadata may advance during synchronization.

Revision note: final regression waits for the managed actor to acknowledge the asynchronously returned lease before checking state. Older quiet-state fixtures now explicitly model a configured ACP session.
