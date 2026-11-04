# Keep dashboard network requests out of the blocking pool

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Opening a large dashboard must keep rendering and renewing its daemon attachment. The observed client exhausted all 512 Tokio blocking threads with summary requests, each of which synchronously waited for a connection that needed another blocking thread. The upgrade itself finished in 35 seconds; its stale splash notice concealed this later deadlock. Move network-only dashboard jobs to supervised asynchronous tasks so connection metadata reads and rendering preparation can always obtain blocking threads.

## Progress

- [x] (2026-11-04) Verify the upgrade completed and sample the stalled client: 512 threads blocked in `spawn_stored_session_summary`.
- [x] (2026-11-04) Add an isolated regression using the real summary scheduler and one blocking thread.
- [x] (2026-11-04) Confirm the regression fails before the fix with its metadata-starvation deadline.
- [x] (2026-11-04) Convert network-only read adapters and adjacent settings saves to the existing supervised asynchronous job helpers.
- [x] (2026-11-04) Verify the actual summary scheduler completes the burst on a one-thread blocking pool; formatting passes.
- [x] (2026-11-04) Finish the CLI suites: 273 passed, four ignored, two direct-daemon shutdown tests failed; reproduce the owner-exit failure using the pre-fix installed binary in a named isolated instance. Workspace Clippy and formatting pass.
- [x] (2026-11-04) Commit the validated fix and logging-fixture correction separately and deliver them to origin/master.

## Surprises & Discoveries

The old daemon accepted handoff after 25 seconds and exited five seconds later. The replacement answered readiness after another five seconds. The displayed upgrade notice therefore did not identify the current stall. The client log subsequently recorded attachment refresh timeouts every five seconds.

A GDB stack sample of client PID 3250942 found 636 threads, including exactly 512 running `spawn_stored_session_summary` through `mj_core::runtime::block_on`. A fresh external connection could still read the daemon runtime feed.

The live dashboard startup reply measured 30,054,558 JSON bytes. It contained 4,551 sessions and 4,516 subagents; the subagents' initial prompts accounted for 20,666,439 serialized bytes. Session states were 3,737 stopped, 770 parked, 43 running, and one provisioning. Stopped children remain included through a live parent. The payload's historical prompt volume is separate from the scheduling deadlock and is not reduced by this patch.

Full CLI validation exposed an obsolete two-log assertion after node processes gained logs. Replace that count with an assertion that the started daemon has its own log; privacy and command-failure assertions remain. Two direct-daemon tests still exceed their shutdown deadlines: `daemon_exits_when_its_owner_process_exits` and `daemon_survives_compatible_migration_then_exits_on_an_incompatible_one`. The owner-exit failure reproduces with `/home/jonathan/.cargo/bin/mj`, the pre-fix installed build, in the isolated `summary-deadlock-baseline` instance. Its log reaches "daemon shutdown began" but the process remains after ten seconds. That baseline process group was stopped before deleting storage.

## Decision Log

- Decision: Remove synchronous waits for network-only jobs rather than increasing the blocking-pool size or adding spare-thread allowances.
  Rationale: A finite pool can always be exhausted when its occupants wait for more work in that same pool. Async tasks release the thread while waiting.
  Date/Author: 2026-11-04, Codex.
- Decision: Use `spawn_background_async` and `spawn_critical_async`, retaining the existing connection/startup policy and update payloads.
  Rationale: These helpers already supervise task failures and bound acknowledgement waits. Filesystem and rendering preparation remain on blocking tasks.
  Date/Author: 2026-11-04, Codex.

## Outcomes & Retrospective

Network-only dashboard reads and the adjacent setup/container-settings saves now use supervised async jobs. Summary preparation still uses a blocking thread, acquired only after the network response. The starvation regression fails before the change and passes afterward in 0.05 seconds. CLI validation finished with 273 passing tests, four ignored harness-dependent tests, and two direct-daemon shutdown failures unrelated to dashboard scheduling; the owner-exit failure is independently reproduced with the pre-fix installed binary. All 14 PTY terminal tests, including automatic upgrade/re-exec, passed. Workspace Clippy and formatting passed. The live daemon and workers remain running; the already-deadlocked client will need relaunching under the fixed build. Historical subagent initial prompts explain the large startup payload and remain an optimization opportunity.

## Context and Orientation

`mj-cli/src/dashboard/io/spawn.rs` schedules one-shot dashboard work. `spawn_io` uses Tokio's blocking pool, a finite group of threads for synchronous work. `spawn_background_async` and `spawn_critical_async` instead await network work on ordinary tasks, report failures, and enforce a timeout. Critical tasks hold a quit guard until the acknowledgement completes. `mj-cli/src/dashboard/io.rs` defines update messages and the stored-metadata adapter. `mj-client/src/daemon.rs::connect_existing` itself needs a blocking metadata-file read; `crate::daemon::connect_or_start` also needs blocking startup-lock and readiness operations.

## Plan of Work

First pin the defect in `mj-cli/src/dashboard/io.rs` beside the existing asynchronous-save starvation regression. Re-run the new test in an isolated child using `crate::test_support::rerun_in_isolated_child`, with its own `MJ_DATA_DIR` and `MJ_CONFIG_DIR`. Publish metadata advertising an unsupported newer protocol so every read must report refusal without launching a daemon. Schedule 64 actual summary jobs on a runtime with one blocking thread. Require all results and a subsequent blocking operation within five seconds; shut down with a bounded wait before asserting to avoid hanging the test on the old defect.

Convert stored-summary and stopped-transcript jobs to async network work. Prepare summaries on a blocking task only after the network reply arrives. Convert the same synchronous network wrappers for stored workspace layouts and lifecycle metadata reloads. Make the metadata adapter async and update its settings-save caller; convert network-only setup and container-settings saves to the existing critical async helper. Preserve filesystem-only uses of `spawn_io` and operations that combine local lifecycle work with network calls.

## Concrete Steps

Run from `/home/jonathan/Projects/mjolnir`, preserving the configured mbx build cache and target layout:

    cargo test -p brokk-mjolnir stored_summary_bursts_leave_the_blocking_pool_available
    cargo test -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Run every Cargo test outside the restricted sandbox. After validation, commit the logging assertion correction separately, then stage only this plan and the two edited CLI source files, commit on the current branch, and push `HEAD:master` to origin.

## Validation and Acceptance

The new test must fail before the fix with its five-second starvation deadline and pass afterward. The real summary scheduler must return all 64 refusals without exhausting the single blocking thread. Existing save timeout and quit-guard tests must remain passing. Full CLI tests and workspace Clippy must pass on the dev profile. Do not run a development binary against the live default instance.

## Idempotence and Recovery

The regression owns temporary directories in an isolated child and never contacts the host default daemon. Re-running it is safe. Do not terminate the live daemon or workers to recover the terminal. The user can relaunch the terminal after obtaining the fixed build; workers keep their accepted turns independently.

## Artifacts and Notes

The diagnostic stack sample is `/tmp/mj-stalled-client-stacks.txt`. The initial regression log is `/tmp/mj-summary-pool-regression.log`. These local diagnostics are not committed.

## Interfaces and Dependencies

Use the existing Tokio runtime, `spawn_background_async`, `spawn_critical_async`, `SAVE_ACK_TIMEOUT`, and unchanged `DashboardIoUpdate` messages. Change `stored_session_metadata` to `async fn ... -> Result<DashboardMetadata>` and update every caller. No schema, protocol, dependency, or worker-lifecycle changes are required.

Revision note: Created after live stack sampling established the blocking-pool dependency cycle.

Revision note: Updated after confirming the failing regression and completing the async conversion; full validation is in progress.

Revision note: Recorded completed validation, the pre-existing direct-daemon shutdown failure, the corrected logging assertion, and measured startup payload composition.
