# Control native goals while work is running

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

The TUI must deliver `/goal clear` to both Codex and Claude while they are working, without waiting behind ordinary prompts. Codex additionally supports `/goal pause` and `/goal resume`. Claude does not support native pause/resume; reject these commands with an explanation instead of creating an objective named pause. Pause retains accounting and prevents subsequent goal turns. Clear removes the goal. Neither command additionally interrupts the current turn.

## Progress

- [x] (2026-09-13) Inspect Codex prior art, both installed adapter interfaces, relay dispatch and shared chat.
- [x] (2026-09-13) Implement capabilities and durable goal-control dispatch.
- [x] (2026-09-13) Implement TUI parsing, completion and supervised feedback.
- [x] (2026-09-13) Validate behavioral tests and disposable installed-adapter probes.
- [x] (2026-09-13) Complete Rust regression coverage, strict Clippy, formatting and diff review; commit on the current branch with this plan.

## Surprises & Discoveries

Claude 0.73.0 advertises `_session/goal` with set/clear only. Its implementation steers command text into running turns and submits a prompt when idle. Codex supports pause/resume/clear; an explicit resume response may stay open for an entire turn. Neither requires Hel to queue controls as ordinary prompts.

## Decision Log

The user selected Codex command names without a stop alias and native controls rather than simulated Claude pause (2026-09-13). Capability interpretation therefore belongs in shared goal state, without provider-name checks. Existing identity-guarded Codex restart recovery remains separate from explicit user commands. New workers require protocol-11 readers because protocol-10 readers cannot deserialize the newly persisted control events; new controllers still support reading older workers.

## Outcomes & Retrospective

Implementation and validation are complete. Native clear works on Codex and Claude during execution; Codex pause/resume preserves the existing goal. Unsupported Claude controls remain local with explanatory feedback. Regression coverage, strict Clippy, formatting and both installed-adapter probes passed. Existing running workers require a rebuild/restart to gain protocol-11 controls; no adapter release or deployment was performed.

## Context and Orientation

`mj-core/src/goal.rs` interprets goal metadata; `mj-core/src/relay/snapshot.rs` defines durable commands and folds journal events. `mj-worker/src/relay.rs` admits commands; `mj-worker/src/worker_runtime/unix.rs` forwards them into `mj-worker/src/acp.rs`, which owns the live agent connection. `mj-worker/src/acp/goal.rs` owns both native controls and restart recovery. The shared TUI composer is in `mj-chat/src/chat.rs`, completion is in its `autocomplete.rs` submodule, and `active.rs`/`remote.rs` schedule supervised background operations. `mj-transcript/src/projection.rs` projects goal metadata into configuration for the chat.

## Plan of Work

First add a goal action enum, optional advertised capability state, a version-gated relay command and its completion event. Publish initialize capabilities as session metadata. Advance relay protocol 10 to 11 and durable snapshot format 5 to 6, preserving old event serialization and upgrade paths. Admit controls independently of active prompts, autonomous turns and earlier pending goal responses; honor sealed checkpoint barriers. Run ACP control futures under supervision in both idle and prompt loops, continuing to drain notifications and accept cancellation. Do not replay uncertain controls after reconnect. Preserve recovery identity guards and its existing timeout behavior.

Second recognize exact goal control arguments locally, reject attachments and unsupported actions without discarding drafts, and add capability-aware completion/help. Keep objective submission and bare goal behavior. Use the existing remote supervisor for submission and completion; derive goal state from provider updates rather than late command acknowledgements.

Third add state transition, queue bypass, provider capability, streaming and recovery regression tests. Run installed-adapter probes only in disposable sessions/workspaces. Commit validated implementation on the current branch without pushing.

## Milestones

The first milestone supplies durable controls and live capability projection through the shared goal module, relay and ACP runtime. Behavioral relay tests prove controls pass a running prompt or native turn without consuming queued work, and replay interrupts an uncertain resume instead of resending it.

The second milestone wires the shared composer and background operation supervisor. Parser and completion tests prove the public commands, capability restrictions and attachment preservation. Provider updates remain authoritative after late acknowledgements.

The final milestone validates both installed providers in disposable sessions, runs regression tests and strict Clippy, reviews the diff and commits the implementation on the current branch. No deployment or push is part of this task.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Use `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. Run every cargo test outside the restricted sandbox. Focused tests should prove clear/pause delivery while a prior prompt or resume response remains open, with more than 64 KB of streamed output. Record actual commands and results below as work proceeds.

## Validation and Acceptance

Clear reaches both adapters without consuming queued prompts. Codex pause/resume preserves native goal identity and counters. Unsupported Claude controls never become prompts. Pending controls do not block UI cancellation, close, or unrelated output. Restart invalidates capabilities; old workers reject version-11 commands. Historical journal digests remain valid and existing recovery tests pass. Full tests and strict Clippy must pass before commit.

## Idempotence and Recovery

No database migrations or adapter releases are planned. Native live probes use disposable sessions only. Controls interrupted by bridge failure are reported rather than blindly retried. Existing unrelated untracked files remain untouched. New capability fields default to absent when reading old state.

## Artifacts and Notes

Installed-adapter evidence is in `target/goal-controls-live.log`. Both providers passed native controls during execution and no further goal continuation after clear. Profiles and workspaces were isolated in `target/goal-controls-live-wVpTmu`; goal state was cleared before process-group shutdown. Focused streaming tests passed, including 100–200 KB of output while a resume response remained outstanding. The initial relay reopen assertion incorrectly assumed restart adds no events or promotes no queued work; the test now uses checkpoint-only reopening and checks the historical event prefix plus interruption of the uncertain resume. The complete `cargo test --no-fail-fast` run is recorded in `target/goal-controls-tests-final.log`: all targets passed except the restart assertion subsequently updated for capability metadata. The complete worker rerun then encountered the unrelated documented fork/exec cache-lease race in its Grok installer test. `cargo test -p brokk-mj-worker -- --test-threads=1` passed all 399 library tests, 8 binary tests, 5 environment tests and 1 packaging test; evidence is in `target/goal-controls-worker-serial.log`. `cargo clippy --all-targets -- -D warnings` passed on the final code (`target/goal-controls-clippy-verified.log`), as did formatting and diff checks. Temporary credential copies used by the live probes were removed after process shutdown.

## Interfaces and Dependencies

Add `GoalControlAction` (Pause, Resume, Clear) and `GoalCapability` to shared goal interpretation. Add `RelayCommand::GoalControl { action }`, corresponding runtime request and completion outcome. Transport native requests through `_session/goal`; explicit controls omit recovery's expectedGoal constraint. Publish normalized capability metadata in existing SessionInfoUpdate events. Use existing Tokio supervision and ACP request futures; create no crates or new dependencies.

Revision (2026-09-13): recorded implementation, live probe evidence and the protocol reader floor required to preserve new command events.

Revision (2026-09-13): full regression tests exposed an existing restart test that forbade all session metadata; it now permits only the exact new capability refresh and continues rejecting replayed history. The two earlier login-environment failures passed both isolated recheck and the subsequent complete core suite.

Revision (2026-09-13): completed regression coverage with serial worker verification to avoid the unrelated cache-lease race; recorded final outcomes and deployment limitation.
