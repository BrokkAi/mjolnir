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
- [x] (2026-11-04) Follow-up: trace direct-daemon shutdown failures to background commands waiting for an absent node; share the daemon shutdown token with the node command host. All three direct-daemon regressions pass in 1.22 seconds with unchanged deadlines.
- [x] (2026-11-04) Follow-up: validate the core/controller/CLI suites (3,089 passed, 28 ignored, including doctests), Clippy, and formatting; commit and deliver the shutdown fix to origin/master. One rsync Text-file-busy failure passed on its focused rerun.
- [x] (2026-11-04) Follow-up: trace startup payload membership to complete stopped-child families and full durable launch records; inspect terminal consumers and measure parent-family sizes.

## Surprises & Discoveries

The old daemon accepted handoff after 25 seconds and exited five seconds later. The replacement answered readiness after another five seconds. The displayed upgrade notice therefore did not identify the current stall. The client log subsequently recorded attachment refresh timeouts every five seconds.

A GDB stack sample of client PID 3250942 found 636 threads, including exactly 512 running `spawn_stored_session_summary` through `mj_core::runtime::block_on`. A fresh external connection could still read the daemon runtime feed.

The live dashboard startup reply measured 30,054,558 JSON bytes. It contained 4,551 sessions and 4,516 subagents; the subagents' initial prompts accounted for 20,666,439 serialized bytes. Session states were 3,737 stopped, 770 parked, 43 running, and one provisioning. Stopped children remain included through a live parent. The payload's historical prompt volume is separate from the scheduling deadlock and is not reduced by this patch.

Full CLI validation exposed an obsolete two-log assertion after node processes gained logs. Replace that count with an assertion that the started daemon has its own log; privacy and command-failure assertions remain. Two direct-daemon tests still exceed their shutdown deadlines: `daemon_exits_when_its_owner_process_exits` and `daemon_survives_compatible_migration_then_exits_on_an_incompatible_one`. The owner-exit failure reproduces with `/home/jonathan/.cargo/bin/mj`, the pre-fix installed build, in the isolated `summary-deadlock-baseline` instance. Its log reaches "daemon shutdown began" but the process remains after ten seconds. That baseline process group was stopped before deleting storage.

The user subsequently requested fixing these direct-daemon tests. Isolated stack sampling found three bounded background commands waiting for the missing host node: quota-container cleanup, checkpoint-leftover cleanup, and local storage measurement. The node command host waited up to 30 seconds for a connection independently of daemon shutdown. Share the existing process cancellation token with that host. A connected node remains usable for cleanup, while an absent node is no longer awaited once shutdown starts. Start the termination coordinator before installing the command host, pass its token into `run_daemon_runtime`, and cancel it when that runtime returns, including startup-error paths. Existing direct-daemon tests now pass without extending their ten-second exit bounds. Extend the existing node command-error regression to exercise cleanup on an already connected node after cancellation.

A second payload snapshot had 36 top-level sessions, thousands of children, and 58 archived records still included. One running parent alone contributed 1,186 direct children. `mj-core/src/state.rs::live_session_ids` recursively retains children of a live parent even when those children are stopped. `mj-controller/src/daemon/dashboard_reads.rs::dashboard_startup` then serializes the retained `State`, including complete `SubagentRecord` values. Terminal production code never reads their `initial_prompt` fields. The same full records occur again in the initial runtime snapshot. The measured 30,054,558-byte reply also becomes 108,675,123 JSON bytes on the wire because `ReplyChunk.bytes` is encoded as an integer array. Payload reduction therefore needs compact client-facing records and on-demand historical detail, preserving complete launch intent in the store.

## Decision Log

- Decision: Remove synchronous waits for network-only jobs rather than increasing the blocking-pool size or adding spare-thread allowances.
  Rationale: A finite pool can always be exhausted when its occupants wait for more work in that same pool. Async tasks release the thread while waiting.
  Date/Author: 2026-11-04, Codex.
- Decision: Use `spawn_background_async` and `spawn_critical_async`, retaining the existing connection/startup policy and update payloads.
  Rationale: These helpers already supervise task failures and bound acknowledgement waits. Filesystem and rendering preparation remain on blocking tasks.
  Date/Author: 2026-11-04, Codex.
- Decision: Use the daemon's existing cancellation token to stop missing-node waits, while allowing cleanup to use an already connected node.
  Rationale: Extending test deadlines hides a real shutdown defect. Reusing the owner's shutdown fact fixes every command waiting for an absent node without independently wiring cancellation into each background poller.
  Date/Author: 2026-11-04, Codex.

## Outcomes & Retrospective

Network-only dashboard reads and the adjacent setup/container-settings saves now use supervised async jobs. Summary preparation still uses a blocking thread, acquired only after the network response. The starvation regression fails before the change and passes afterward in 0.05 seconds. CLI validation finished with 273 passing tests, four ignored harness-dependent tests, and two direct-daemon shutdown failures unrelated to dashboard scheduling; the owner-exit failure is independently reproduced with the pre-fix installed binary. All 14 PTY terminal tests, including automatic upgrade/re-exec, passed. Workspace Clippy and formatting passed. The live daemon and workers remain running; the already-deadlocked client will need relaunching under the fixed build. Historical subagent initial prompts explain the large startup payload and remain an optimization opportunity.

The shutdown follow-up resolves both previously failing direct-daemon regressions. All three direct-daemon cases pass with their original deadlines, and cleanup on an already connected node remains covered. Validation across the affected crates completed with 3,089 passing tests and 28 ignored tests including doctests. The full controller run's single rsync Text-file-busy failure passed on focused retry; the other 2,253 controller tests passed in the full run. Core passed 560 tests and CLI passed 275, including all 14 PTY cases. Workspace Clippy and formatting are clean. Payload membership and wire overhead are diagnosed, but this follow-up preserves the payload shape and stored history.

## Context and Orientation

`mj-cli/src/dashboard/io/spawn.rs` schedules one-shot dashboard work. `spawn_io` uses Tokio's blocking pool, a finite group of threads for synchronous work. `spawn_background_async` and `spawn_critical_async` instead await network work on ordinary tasks, report failures, and enforce a timeout. Critical tasks hold a quit guard until the acknowledgement completes. `mj-cli/src/dashboard/io.rs` defines update messages and the stored-metadata adapter. `mj-client/src/daemon.rs::connect_existing` itself needs a blocking metadata-file read; `crate::daemon::connect_or_start` also needs blocking startup-lock and readiness operations.

## Plan of Work

First pin the defect in `mj-cli/src/dashboard/io.rs` beside the existing asynchronous-save starvation regression. Re-run the new test in an isolated child using `crate::test_support::rerun_in_isolated_child`, with its own `MJ_DATA_DIR` and `MJ_CONFIG_DIR`. Publish metadata advertising an unsupported newer protocol so every read must report refusal without launching a daemon. Schedule 64 actual summary jobs on a runtime with one blocking thread. Require all results and a subsequent blocking operation within five seconds; shut down with a bounded wait before asserting to avoid hanging the test on the old defect.

Convert stored-summary and stopped-transcript jobs to async network work. Prepare summaries on a blocking task only after the network reply arrives. Convert the same synchronous network wrappers for stored workspace layouts and lifecycle metadata reloads. Make the metadata adapter async and update its settings-save caller; convert network-only setup and container-settings saves to the existing critical async helper. Preserve filesystem-only uses of `spawn_io` and operations that combine local lifecycle work with network calls.

For the requested shutdown follow-up, add the process shutdown token to `mj-core/src/node.rs::NodeCommandHost::new`. In `connected`, return an already connected node immediately, but select the token alongside changes to the registry while no node is present. Keep the existing connection deadline for ordinary operation. Pass the token through `mj-controller/src/node_registry.rs::install_command_host`. Create the termination coordinator in `mj-controller/src/daemon/process.rs::run_daemon_process` before installing the host, pass its token to `run_daemon_runtime`, and cancel it after that runtime returns. Update constructor callers and extend the existing command-error test to run its streamed case after shutdown cancellation. Retain the existing direct-daemon owner and migration regression deadlines.

## Concrete Steps

Run from `/home/jonathan/Projects/mjolnir`, preserving the configured mbx build cache and target layout:

    cargo test -p brokk-mjolnir stored_summary_bursts_leave_the_blocking_pool_available
    cargo test -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

For the shutdown follow-up, first run the existing direct-daemon cases, then the full suites of all affected crates and the workspace checks:

    cargo test -p brokk-mjolnir --test store_divergence
    cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

The full controller suite had one `Text file busy` failure in the rsync namespace adapter; all other 2,253 tests passed and 19 were ignored. Re-run only that failure; it passed in 0.18 seconds. Then run the core and CLI suites that Cargo had not reached. Do not rerun the already completed controller suite merely to confirm the isolated retry.

Run every Cargo test outside the restricted sandbox. After validation, commit the logging assertion correction separately, then stage only this plan and the two edited CLI source files, commit on the current branch, and push `HEAD:master` to origin.

## Validation and Acceptance

The new test must fail before the fix with its five-second starvation deadline and pass afterward. The real summary scheduler must return all 64 refusals without exhausting the single blocking thread. Existing save timeout and quit-guard tests must remain passing. Full CLI tests and workspace Clippy must pass on the dev profile. Do not run a development binary against the live default instance.

## Idempotence and Recovery

The regression owns temporary directories in an isolated child and never contacts the host default daemon. Re-running it is safe. Do not terminate the live daemon or workers to recover the terminal. The user can relaunch the terminal after obtaining the fixed build; workers keep their accepted turns independently.

## Artifacts and Notes

The diagnostic stack sample is `/tmp/mj-stalled-client-stacks.txt`. The initial regression log is `/tmp/mj-summary-pool-regression.log`. These local diagnostics are not committed.

## Interfaces and Dependencies

Use the existing Tokio runtime, `spawn_background_async`, `spawn_critical_async`, `SAVE_ACK_TIMEOUT`, and unchanged `DashboardIoUpdate` messages. Change `stored_session_metadata` to `async fn ... -> Result<DashboardMetadata>` and update every caller. The initial summary fix requires no schema, protocol, dependency, or worker-lifecycle changes. The shutdown follow-up adds the existing workspace `tokio-util` dependency to `mj-core` for `CancellationToken`, with its corresponding Cargo.lock dependency entry. `NodeCommandHost::new` takes that token as its third argument, `node_registry::install_command_host` receives it, and `run_daemon_runtime` accepts the same process-owned token. No schema or wire-protocol changes are needed.

Revision note: Created after live stack sampling established the blocking-pool dependency cycle.

Revision note: Updated after confirming the failing regression and completing the async conversion; full validation is in progress.

Revision note: Recorded completed validation, the pre-existing direct-daemon shutdown failure, the corrected logging assertion, and measured startup payload composition.

Revision note: The user requested the direct-daemon shutdown fix next. Recorded its root cause, cancellation ownership, passing focused regressions, and the remaining broad validation. Expanded the payload diagnosis with parent-family counts, unused launch fields, duplicate snapshots, and chunk wire overhead; payload redesign is not implemented in this follow-up.
