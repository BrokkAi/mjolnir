# Cancel active turns before stopping sessions

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Stopping an active session currently restarts its worker to interrupt the agent before saving a resumable checkpoint. One observed stop spent about 73 seconds recovering before checkpoint export. Stop should instead send an explicit non-steering cancel operation, wait for the agent to finish cancelling, save the checkpoint, and shut down. Restart remains recovery for an unresponsive or incompatible worker.

## Progress

- [x] (2026-09-05) Traced the active-close restart and existing Cancel semantics.
- [x] (2026-09-05) Add worker CancelTurn operation and behavior tests.
- [x] (2026-09-05) Use bounded cancellation in the controller close checkpoint path.
- [x] (2026-09-05) Review integrated behavior; full cargo test, focused race tests, and Clippy pass.
- [x] (2026-09-05) Commit cancellation as `73ac5ff0`.
- [x] (2026-09-05) Fix the additionally requested CI failures; the three-client smoke and TUI tests pass. Publish both changes together after final checks.

## Surprises & Discoveries

Existing RelayCommand::Cancel can steer a queued prompt instead of cancelling, rejects autonomous harness turns, and cannot pass a queued checkpoint. Reusing it unchanged would not satisfy Stop. A cancel notification acknowledgment also does not prove that the agent has stopped writing; checkpoint admission must still wait for terminal state.

Review found that accepting a late CancelTurn after barrier admission would advance the journal beyond the ready cursor and leave a queued cancellation for later. The worker instead rejects it without recording an event. The controller resynchronizes on rejection and accepts the already-ready checkpoint. The regression asserts the entire operational state is unchanged.

## Decision Log

Use a dedicated protocol-7 CancelTurn command, leaving interactive Cancel behavior intact. CancelTurn never steers, tolerates the turn ending before dispatch, and may pass a pending checkpoint but cannot pass an admitted checkpoint. An admitted checkpoint is the frozen state used to produce the archive. The controller requests cancellation only for a close; periodic checkpoints continue to defer during active work. These decisions avoid running queued work during shutdown and preserve verified checkpoint ordering.

## Context and Orientation

`src/hel_worker/snapshot.rs` defines the serialized commands and deterministic event state. `src/hel_worker.rs` validates and schedules those commands. `mj-worker/src/hel_worker_runtime/unix.rs` translates worker commands to ACP, the agent communication protocol. Existing ACP cancellation can send session/cancel without steering. `mj-controller/src/hel_controller/checkpoint.rs` requests a checkpoint and currently treats an active turn during close as immediate reason to restart. `mj-controller/src/hel_controller/lifecycle.rs` saves the verified checkpoint before destroying the target; that ordering remains required.

The working tree already contains unrelated changes in `src/hel_worker.rs` and `.agents/docs/claude-autonomous-turns.md`. Preserve them and stage only task-owned changes.

## Plan of Work

First add CancelTurn to the worker command and command-kind enums, advertise protocol 7, and map it to existing ACP cancellation with no steering payload. Schedule it past a pending checkpoint while retaining the protection of an admitted barrier. Test prompt, autonomous-turn, queued-work, idle-race, and frozen-barrier behavior.

Next update the controller barrier wait to send CancelTurn once when close encounters running work. Continue waiting for the real checkpoint barrier within a bounded deadline. Only a deadline failure, stopped runtime, or worker that cannot support cancellation should enter restart recovery. Record useful cancellation diagnostics. Keep periodic checkpoints non-interrupting.

Finally review the combined changes and run the full repository validations. Checkpoint acknowledgment alone must never permit teardown. Confirm cancellation does not restart responsive workers and cannot promote or steer queued prompts.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Run focused tests for the modified worker scheduler and controller checkpoint module outside the sandbox. Then run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Cargo tests require elevated permissions because they use sockets. Use the normal target directory for build output and diagnostic logs. Stage task-owned changes on the current branch and commit after validation; the user authorized pushing when complete.

## Validation and Acceptance

A responsive active agent receives non-steering cancellation, reaches terminal state, admits the checkpoint, and closes without replacing its worker. Pending user prompts remain queued. Autonomous turns are cancellable. An already idle worker handles the cancellation race harmlessly. An admitted barrier prevents new ACP effects. A worker that never settles reaches bounded recovery, and periodic checkpointing never cancels work. Tests should exercise these state transitions rather than simply compare implementation lists.

## Idempotence and Recovery

CancelTurn is safe when the turn has already ended. A failed checkpoint retains the target under the existing lifecycle rules. No live user session needs to be stopped to validate this change. Preserve unrelated working-tree changes when staging, including edits in shared files.

## Artifacts and Notes

The observed restart log was `active ACP turn interrupted before checkpoint barrier`; code inspection showed that this is raised immediately on Running during close, without first trying ACP cancellation.

## Interfaces and Dependencies

Add `RelayCommand::CancelTurn` and its kind with minimum protocol 7. Reuse `CommandRequest::Cancel` with `steering_prompt: None`. Use existing relay submission, checkpoint observation, timeout, and restart infrastructure. No new dependency or crate is needed.

## Outcomes & Retrospective

Responsive cancellation now reaches a checkpoint without replacing the worker. The controller regression exercises this through a live relay fixture; worker runtime tests prove cancellation acknowledgment does not release the barrier early or steer queued prompts. Full `cargo test`, focused late-cancellation tests (5 passed), and `cargo clippy --all-targets -- -D warnings` passed.

The additional CI fixes align text-modal cancellation with the existing platform accelerator and update the reliability fixture to discover Codex ACP through PATH and advertise its required access mode. The full TUI tests pass locally. The exact CI smoke command passed with zero leaked processes and successful database integrity checks (artifact directory `target/reliability-artifacts/multi-client-happy-path-seed-1-1240971`). This Linux host cannot execute the macOS test binary; remote CI will validate that platform after publication.

Initial plan records the controller/worker boundary and why existing interactive cancellation is insufficient.

Updated after integration to record the admitted-barrier race, completed validation, and the user's additional CI request.

Updated after CI repair to record fixture compatibility changes and the passing smoke scenario.
