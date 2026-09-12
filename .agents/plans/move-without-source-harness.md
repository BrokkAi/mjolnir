# Move sessions without starting their old harness

This ExecPlan follows `.agents/PLANS.md` and must be maintained during implementation.

## Purpose / Big Picture

Move must preserve a session and restore it using another profile even when its old model or harness cannot start. Local development and installation must also replace the native worker that localhost actually uses, so existing model replacement questions become reachable.

## Progress

- [x] (2026-09-12) Traced native worker selection, startup recovery, and Move checkpoint gates.
- [x] (2026-09-12) Build and installation fixes; 14 script tests pass; committed as `30aae2a9`.
- [x] (2026-09-12) Checkpoint-only worker and durable lifecycle recovery; three recovery worker tests pass.
- [x] (2026-09-12) Move preparation and execution integration, including TUI/web source-unavailable status.
- [x] (2026-09-12) Behavior tests, full Cargo suite, Clippy, and wrapper/web tests pass, including the final protocol bump. Build-wrapper checkpoint committed; final recovery commit and push follow this completed validation record.

## Surprises & Discoveries

The affected worker's SHA256 exactly matched yesterday's `target/debug/mj-worker`, while the controller and portable worker had been rebuilt. Linux scripts build only the portable worker. Move synchronizes the source twice and checkpoints wait for ACP readiness (ACP is the protocol used to talk to the coding harness). Worker checkpoint dispatch is also gated on harness channel capacity. Close must remain pending until the controller releases its verified barrier; completing it immediately would reject the subsequent checkpoint-completion command.

## Decision Log

Use an explicit checkpoint-only worker mode, defaulting to ordinary execution for old launch configurations. This recovers the journal without starting the old harness, rather than treating missing relay state as a successful checkpoint. Keep healthy source checkpointing unchanged. Author: Codex, 2026-09-12.

## Outcomes & Retrospective

Implementation passed the full Cargo suite and Clippy, 14 wrapper tests, and 14 web viewer unit tests. The final full suite and Clippy also passed after incrementing the daemon protocol to 18 for the new Move confirmation field. Initial test failures exposed an unnecessary dependency on current profile configuration during ordinary checkpoint restart and a malformed test worker path; both were corrected. An unrelated npm-upgrade executable-copy test hit transient ETXTBSY and passed on retry.

Both native and portable workers were rebuilt. Restarting the development daemon installed a native worker in the affected bifrost5 session whose SHA256 matches `target/worker/debug/mj-worker`. The source now serves its relay socket and reports idle. Its journal records the withdrawn-model warning and the `session-config-recovery` question titled “Choose a replacement.” No live session was moved.

## Context and Orientation

`scripts/run.sh` and `scripts/install.sh` build workers; localhost selects an isolated native worker before a sibling portable binary. The daemon pins binaries at startup. `src/hel_worker_launch.rs` defines launch configuration, `src/hel_worker.rs` owns the durable journal and command transitions, and `mj-worker/src/hel_worker_runtime/unix.rs` serves the relay socket and starts the harness. `mj-controller/src/hel_controller/move_session.rs`, `lifecycle.rs`, `checkpoint.rs`, and `worker_restart.rs` implement verified capture, source shutdown, and destination restoration. `mj-cli/src/daemon/session_move.rs` admits Move requests.

## Plan of Work

First build native and portable workers on Linux in both wrappers and prove failure leaves launch/install unattempted. Next add a defaulted worker mode and a relay coordinator that can checkpoint and seal recovered state without executing queued harness or shell commands. Skip managed harness preparation in that mode. Expose mode separately from ACP readiness, retaining false readiness.

Move preparation must use typed connection errors to distinguish an unavailable source from other failures. After confirmation and under the existing lifecycle reservation, recover an unavailable or unready source in checkpoint-only mode using the shared stop/replace/start machinery. Persist mode in the installed launch configuration and preserve it during recovery. Checkpoint using durable native identity and the same cursor and archive verification gates. Revalidate pending work before sealing. Ordinary Resume and destination provisioning generate ordinary launch configurations.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Run `node --test scripts/run.test.mjs scripts/install.test.mjs` for wrappers. Run focused worker/controller tests during implementation, with every Cargo test outside the sandbox. Finish with `cargo test` and `cargo clippy --all-targets -- -D warnings`, then commit only changed files on the current branch.

## Validation and Acceptance

Script fixtures must show current native and portable artifacts before launch or install, including debug/release and a preexisting stale native installation. Worker tests must serve a checkpoint and seal it with an unusable harness command, without running queued prompts or shells. Move tests must cover failed source startup, queue reconfirmation, cancellation, and retained state on failure. Existing model-recovery tests must still show a replacement question and persistent choice on ordinary startup. Full tests and clippy must pass.

## Idempotence and Recovery

Stop owning processes before replacing or reading recoverable state. Never remove source storage before verifying the new checkpoint. Missing journal/native data and failed process termination remain explicit errors. Retain installed recovery mode across interrupted Move, and use normal launch settings when resuming normally. Validate with isolated fixtures, not the user's live Move. Restart the controller daemon only after the complete worker set is built because worker sources are pinned at daemon startup.

## Artifacts and Notes

Preexisting unrelated files are `.agents/plans/restore-tui-workspaces-and-status.md` and `mj.sqlite3`; leave them untouched.

## Interfaces and Dependencies

Introduce `WorkerRunMode::{Harness, CheckpointOnly}` with default `Harness` on worker launch configuration. Report checkpoint-only capability in the relay's operational state independently of native-session readiness. `MovePreparation.source_unavailable` reports recovery to TUI/web clients, and `MoveOperation.source_checkpoint_only` persists source recovery across retries. Bump daemon protocol to 18 because older native clients reject unknown Move confirmation fields. Use existing subprocess, lifecycle reservation, checkpoint barrier, archive verification, and destination conversion helpers. No public CLI switch or new dependency is required.

Initial plan recorded 2026-09-12 from the accepted implementation plan.

Updated 2026-09-12: recorded implementation and initial test results. Automatic worker recovery now checks persisted Move intent before refreshing an old launch plan. The user also authorized pushing the completed commits to upstream.

Updated 2026-09-12: recorded successful full validation, protocol compatibility handling, and observed recovery of the affected source worker without executing Move.

Final validation recorded 2026-09-12: `cargo test` and `cargo clippy --all-targets -- -D warnings` both exited successfully after all Rust changes. The completed work is committed on the existing master branch and pushed to its configured upstream as authorized by the user.
