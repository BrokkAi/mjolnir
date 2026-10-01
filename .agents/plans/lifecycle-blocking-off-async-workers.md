# Keep lifecycle commands off the daemon's async worker threads

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.


## Purpose / Big Picture

The Mjolnir daemon (`mj daemon-run`, crate `mj-controller`) serves every client request, feed and status update from a Tokio multi-thread runtime. That runtime has a fixed set of "async worker threads", by default one per CPU core, and an async task runs on one of them until it reaches an `.await`. If a task instead waits synchronously, for example for a child process to exit, that thread does nothing else for as long as the wait lasts.

Today the daemon's session lifecycle operations (create, sub-agent start, resume, close with its checkpoint, park, unpark, recovery adopt) run their target commands (podman, ssh, git, npm, worker installs, checkpoint transfers) synchronously inside async tasks. With as many slow commands as there are worker threads, the daemon stops answering anything: the API, the dashboard feeds, status, and even the request that would cancel the slow commands. A terminal test showed it: with `TOKIO_WORKER_THREADS=2` and two session launches held in a fake `npm`, an unrelated third launch request never returned in 2 of 6 runs.

After this change, a lifecycle operation and every target command wait on threads from Tokio's "blocking pool" (a separate, growable set of threads meant for synchronous waits), never on an async worker thread. Any number of hung launches leave the daemon answering other requests, and cancelling an operation (the user's Cancel, or daemon stop) still kills the hung command's process group and lets the operation finish promptly. How a daemon handoff (automatic upgrade) decides what to wait for does not change.

To see it working, run the new daemon test `hung_launch_commands_leave_the_daemon_serving_and_end_when_cancelled` in `mj-controller/src/daemon/tests.rs`: four launches hang in their preflight on a runtime with two worker threads, and the daemon still answers a ping and registers another launch; cancelling one launch and then stopping the daemon kills the hung processes within five seconds. The terminal test `empty_workspace_waits_for_explicit_new_before_creating_a_session` in `mj-cli/tests/termination_pty.rs` again holds both of its launches.


## Progress

- [x] (2026-10-01) Inventory blocking work on async threads (see Context and Orientation).
- [x] (2026-10-01) Add `mj_core::runtime::off_async_worker` with its unit tests (prototype, uncommitted at plan time).
- [x] (2026-10-01) Write the daemon-level test (uncommitted at plan time).
- [ ] Commit this plan.
- [ ] Confirm the daemon-level test fails on the unchanged daemon.
- [ ] Milestone 1: executor seam (`ProcessExecutor`, `CancellableProcessExecutor` wait through `off_async_worker`).
- [ ] Milestone 2: lifecycle seam (`admit_lifecycle` runs each operation on the blocking pool).
- [ ] Milestone 3: restore the terminal test to hold two launches.
- [ ] Validation: controller lib tests, mj-cli integration tests, clippy, fmt, 20 loaded runs of the terminal test.


## Surprises & Discoveries

- Observation: Move already runs its lifecycle work on the blocking pool, through the daemon's `blocking(...)` helper and `mj_core::runtime::block_on`. That helper also takes upgrade admission under the label "database operation" for the whole closure, so a Move holds that label even while its resumable copy has released "session lifecycle" through `begin_resumable_move_work`. This is likely a pre-existing deviation from the rule that minutes-long restartable work does not hold admission. This plan does not change it, because the task forbids changing admission; it is reported for follow-up.
  Evidence: `mj-controller/src/daemon/serve.rs` `blocking` calls `crate::upgrade::activity("database operation")`; `mj-controller/src/daemon/session_move.rs` wraps `move_session_managed_controlled` in it.

- Observation: much of the blocking work inside lifecycles is not a subprocess: archive verification (`verify_archive_streaming`), checkpoint transfer hashing, `fs::copy` of a LocalBare archive, joins on `std::thread::scope` lanes, waits on `on_dedicated_thread` HTTP downloads, the image download gate's sleep-poll loop (`image_pull_gate::hold_image_pull`), and the cross-harness join loop in `resume.rs`. A fix only inside the command executors would leave all of those on async workers.
  Evidence: inventory below.


## Decision Log

- Decision: Use two placements of one rule, "synchronous waits leave the async workers": (1) `admit_lifecycle` in `mj-controller/src/daemon/lifecycle.rs` runs every lifecycle operation's future on a blocking-pool thread with `tokio::task::spawn_blocking` and `mj_core::runtime::block_on`; (2) the command executors in `mj-core/src/targets.rs` wait for their child through `mj_core::runtime::off_async_worker`, which uses `tokio::task::block_in_place` when called on a multi-thread runtime worker.
  Rationale: The lifecycle seam is the single owner of lifecycle work and covers the non-command blocking found by the inventory without touching each call site. The executor seam is the single owner of "run a target command" and covers callers outside lifecycles (recovery adopt runs `ProcessExecutor` directly in an async action handler) and any future async caller, so the defect cannot be reintroduced by a new call path. Inside a lifecycle the executor seam is a no-op, because the thread is already a blocking-pool thread. Neither placement adds `spawn_blocking` at individual call sites.
  Date/Author: 2026-10-01, Claude.

- Decision: `admit_lifecycle` calls `tokio::task::spawn_blocking` directly, not the daemon's `blocking(...)` helper.
  Rationale: `blocking(...)` takes upgrade admission as "database operation". Lifecycles already hold their own admission ("session lifecycle", "session destroy", or the draining "failed startup cleanup"), and Move releases it for its resumable copy. Adding a second hold would change what a handoff waits for, which this work must not do.
  Date/Author: 2026-10-01, Claude.

- Decision: Use `mj_core::runtime::block_on`, not `Handle::block_on` directly, inside the lifecycle thread.
  Rationale: `mj_core::runtime` already coordinates runtime shutdown with blocking threads that await futures; polling a Tokio timer after the timer driver has shut down asserts. A lifecycle that is still running when the process shuts its runtime down is cancelled at its next await, which is what happened to a `tokio::spawn`ed lifecycle before.
  Date/Author: 2026-10-01, Claude.

- Decision: Cancellation stays flag-based. No new cancellation path is added.
  Rationale: `CancellableProcessExecutor` already polls its flag every 25 ms while a child runs and, when it is set, kills the child's process group through `mj_core::subprocess::signal_process_group` and returns an error. The defect was not that cancellation could not reach the child but that the request to cancel could not be served while every worker thread waited. Moving the waits off the workers lets the cancel request run.
  Date/Author: 2026-10-01, Claude.


## Outcomes & Retrospective

To be written at completion.


## Context and Orientation

Terms used here:

An "async worker thread" is one of the threads of a Tokio multi-thread runtime that poll async tasks. The daemon's runtime is built in `mj-cli/src/main.rs` (`run`), with the default worker count (one per core, or `TOKIO_WORKER_THREADS`). The "blocking pool" is Tokio's separate set of threads for synchronous work, reached with `tokio::task::spawn_blocking`; it grows on demand up to 512 threads. `tokio::task::block_in_place(f)` runs `f` on the current thread after handing the thread's queued async tasks to a replacement thread, so the current thread effectively becomes a blocking-pool thread for the duration of `f`; it panics on a current-thread runtime.

A "lifecycle operation" is a daemon-owned session operation such as Create, Resume, Suspend (close), Move, Destroy, Park or Unpark. They are admitted by `RuntimeState::admit_lifecycle` in `mj-controller/src/daemon/lifecycle.rs`, which records the operation in the lifecycle map, takes upgrade admission, and spawns the operation's future. The future receives a `cancelled: Arc<AtomicBool>` flag. Cancel (the `CancelLifecycle` daemon action, `RuntimeState::cancel_lifecycle` in `mj-controller/src/daemon/views.rs`) and daemon stop (`RuntimeState::cancel_and_wait_lifecycles`, same file) set that flag.

"Upgrade admission" (`mj-controller/src/upgrade.rs`) is how a daemon handoff knows what daemon-owned work is in flight. Each admitted operation holds a `Work` value under a label; the handoff waits until nothing holds one. Lifecycles hold "session lifecycle" (Destroy holds "session destroy", failed startup cleanup holds a draining hold). This plan must not change these holds.

A "target command" is a `CommandSpec` (program, arguments, environment, purpose) run through the `CommandExecutor` trait in `mj-core/src/targets.rs`. The production executors are `ProcessExecutor` (no cancellation), `CancellableProcessExecutor` (flag and optional deadline; kills the child's process group on cancellation), and `BoundedProcessExecutor` (a per-command deadline that delegates to `CancellableProcessExecutor`). The daemon wraps executors in `DaemonStageReportingExecutor` (`mj-controller/src/daemon/support.rs`) to report stages.

Inventory of blocking work on async workers before this change (paths relative to `mj-controller/src`):

Create (`daemon/create.rs`, the `start_or_join_lifecycle_controlled` closure, then `Controller::provision_session_controlled_with_commit` in `controller/provisioning.rs`) runs with `CancellableProcessExecutor::new(cancelled)` and no deadline. On the async worker it runs `controller_github_token` (a raw `gh auth token`), `preflight_worker_binary` (may download a worker over HTTP), `preflight_harness` (the `npm` preflight), managed worktree preparation, `podman_image_user` and target creation behind the image download gate (minutes), `git_cache::prepare`, `execute_concurrent_lanes` (repository clone inline, worker payload install on a scoped thread that the worker joins; minutes), `install_inherited_git_settings`, `start_worker`, the readiness probes inside `connect_started_worker`, and rollback cleanup. Sub-agent start (`provision_subagent_session_controlled`) runs the same worker installation and start.

Resume (`daemon/resume.rs`, then `controller/resume.rs` `resume_session_with_origin`) runs repository preflights, archive verification, worktree restore, conversion checkpoints, the cross-harness provision with a 10 ms `std::thread::sleep` join loop, the whole Create pipeline, `restore_into_target` (archive upload or `fs::copy`, worker restore; minutes) and worker start, on the async worker.

Suspend/close (`daemon/close.rs` `suspend_admitted`, then `controller/lifecycle.rs`) runs the checkpoint (`controller/checkpoint/latched.rs`: capture, worker restart, export, `CheckpointTransfer::execute` with download and hashing; minutes), publication checks, and target teardown on the async worker. Park and Unpark (`daemon/subagent_park.rs`) run worker stop and start. Recovery adopt (`daemon/actions.rs`, `AdoptRecovery`) runs `adopt_orphan_worker` with `ProcessExecutor` directly in the action handler.

Already off the async workers: Move (prepare, run, recover), Destroy and ForceDestroy, discard and force-stop, deferred and failed-startup cleanup, sub-agent report-root creation, session registration, "checkpoint now", recovery copies and worker upgrades (`recovery.rs`, `worker_upgrade.rs`), relay connects, capacity and image pollers, profile discovery, API exports, new-session and resume preflights, project discovery, reviewer staging, credential sync, recovery scan and destroy, and daemon startup steps.


## Plan of Work

Milestone 1, the executor seam. In `mj-core/src/runtime.rs`, add `pub fn off_async_worker<R>(work: impl FnOnce() -> R) -> R`. It reads `tokio::runtime::Handle::try_current()`; on a multi-thread runtime it returns `tokio::task::block_in_place(work)`, and otherwise (no runtime, current-thread runtime) it returns `work()`. On a blocking-pool thread, or inside `Handle::block_on`, `block_in_place` simply runs `work`. In `mj-core/src/targets.rs`, make the `execute` and `execute_with_stdin` methods of `ProcessExecutor` and `CancellableProcessExecutor` run their whole body (SSH session lease, admission permit, child run) inside `off_async_worker`. `BoundedProcessExecutor` delegates to `CancellableProcessExecutor` and needs no change. At the end of this milestone the unit test `runtime::tests::blocking_waits_leave_the_async_workers_serving` passes, and the daemon test already passes for Create because each hung `npm` waits on a blocking-pool thread.

Milestone 2, the lifecycle seam. In `RuntimeState::admit_lifecycle` (`mj-controller/src/daemon/lifecycle.rs`), replace the inner `tokio::spawn(async move { operation(...).await })` with `tokio::task::spawn_blocking(move || mj_core::runtime::block_on(operation(...)))`. A join error (panic) is reported as today ("daemon lifecycle task failed"); a `block_on` error means the runtime is shutting down and is reported as an internal lifecycle failure. Everything after the operation (reload, failure recording, completion publication, deferred cleanup) stays on the outer async task, unchanged, and the outer task keeps holding the lifecycle's admission. Move's own inner `blocking(...)` stays, so Move's admission does not change. At the end of this milestone, all lifecycle work, including non-command blocking such as archive verification and the image download gate, runs on blocking-pool threads.

Milestone 3, the terminal test. In `mj-cli/tests/termination_pty.rs`, the fake `npm` installed by `spawn_dashboard_pty_with_setup` holds every launch until the release file exists (remove the `mkdir` that limited it to the first launch) and the comment says why two launches can be held.


## Concrete Steps

From the worktree root:

    cargo test -p brokk-mj-core --lib runtime::
    cargo test -p brokk-mj-controller --lib hung_launch_commands
    cargo test -p brokk-mj-controller --lib
    cargo test -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Every `cargo test` runs outside the restricted sandbox (it uses loopback sockets). For the loaded terminal runs, start two busy loops pinned to CPUs 0 and 1 and run the test binary pinned there 20 times:

    taskset -c 0,1 sh -c 'while :; do :; done' &   (twice; note the two PIDs)
    for i in $(seq 1 20); do taskset -c 0,1 cargo test -p brokk-mjolnir --test termination_pty empty_workspace_waits_for_explicit_new_before_creating_a_session -- --exact; done
    kill <the two busy-loop PIDs>


## Validation and Acceptance

The daemon test `hung_launch_commands_leave_the_daemon_serving_and_end_when_cancelled` fails on the unchanged daemon (the watchdog has to release the hung commands, and the timing assertion reports the stall) and passes after Milestone 1 and 2. It checks that with four launches hung and two async workers, a ping and a fourth launch registration are answered within five seconds; that `CancelLifecycle` on one launch kills exactly that launch's process and the launch reports failure within five seconds; and that `cancel_and_wait_lifecycles` (the daemon-stop path) returns within five seconds with every hung process gone.

The unit test `runtime::tests::blocking_waits_leave_the_async_workers_serving` fails when `off_async_worker` calls `work()` directly and passes with `block_in_place`.

The terminal test holds two launches and passes 20 of 20 runs under the two pinned busy loops. The existing upgrade regressions (`automatic_upgrade_drains_a_lifecycle_without_cancelling_it` and the `daemon_startup` integration tests) still pass, which shows the handoff still waits for lifecycles and does not cancel them.


## Idempotence and Recovery

All steps are code edits and test runs. Tests use isolated data directories (`IsolatedTest::isolated_store`) and named instances; nothing touches the default instance. If a test leaves a hung fake `npm`, it exits by itself once its release file exists; the tests create the release file when they drop their guard.


## Artifacts and Notes

To be filled with test transcripts.


## Interfaces and Dependencies

In `mj-core/src/runtime.rs`:

    pub fn off_async_worker<R>(work: impl FnOnce() -> R) -> R

No new crates. Tokio 1.52 with the `rt-multi-thread` feature (already enabled through `full`) provides `block_in_place` and `RuntimeFlavor`.
