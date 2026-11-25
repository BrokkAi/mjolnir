# Give cancellation cleanup a consistent shutdown-only executor (#1678)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository keeps its ExecPlan standard in `.agents/PLANS.md`; maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

When a user cancels a running session lifecycle operation (such as Suspend or Close), Mjolnir intentionally runs cancellation cleanup and rollback so the session returns to a clean state in the current daemon. Today, if a daemon handoff or graceful shutdown arrives while that cancellation cleanup is running a slow Podman or SSH subprocess, the subprocess ignores shutdown because rollback substituted plain `ProcessExecutor` (or a fresh timeout-only `CancellableProcessExecutor`) and because `LifecycleControl` cannot distinguish a later shutdown after user cancellation has already won. As a result, stalled commands can delay daemon handoff while holding admission, or continue mutating state after admission was yielded.

After this change, every cancellation-cleanup and rollback command runs through one executor-owned shutdown-only view derived from the supervising daemon executor. Normal user cancellation still completes rollback in the current daemon, and a sealed `Close` still goes forward, while a subsequent daemon handoff or graceful shutdown immediately interrupts any in-flight Podman or SSH cleanup command, prevents any retired owner from writing controller or store state after admission is released, and leaves the durable accepted operation intact for the next daemon to recover.

## Progress

- [x] (2026-11-25 00:08Z) Mapped `LifecycleControl`, `CommandExecutor`, `DaemonStageReportingExecutor`, all plain-executor substitution sites in `mj-controller/src/controller/lifecycle.rs`, `mj-controller/src/controller/lifecycle/origin_delta.rs`, and `mj-controller/src/controller/move_session.rs`, and authored this ExecPlan.
- [x] (2026-11-25) Added the orthogonal monotonic shutdown `CancellationToken` to `LifecycleControl` (`mj-daemon/src/daemon.rs`); `stopped_for_shutdown()`/`shutdown_stopped()` read it, `ActiveLifecycle::interrupt_for_shutdown()` cancels it for a person-cancelled non-Create lifecycle, and `LifecycleControl::executor()` builds the lifecycle executor with it.
- [x] Added `ShutdownOnly` (and `CommandExecutor::shutdown_only()`) in `mj-core/src/targets.rs`; `CancellableProcessExecutor` gained `with_shutdown_cancellation`; wrappers forward `shutdown_interrupted()`.
- [x] Replaced every cancellation-cleanup substitution in `lifecycle.rs`, `origin_delta.rs` (single `wait_for_stop`), `move_session.rs` and `move_session/transport.rs` (`ProcessStreamPipe::new(executor)`); lifecycle admission closures now receive the `LifecycleControl`.
- [x] Review follow-up: `LifecycleControl` publishes shutdown before the `cancelled` flag; added `CommandExecutor::with_cleanup_timeout` (`CleanupTimeout` view) and restored the 15 s bound at `move_session.rs` (retained-source stop keeps a person's cancel; `recover_move_source_stop` runs on `shutdown_only().with_cleanup_timeout`).
- [x] Merged master 45bc071e0: `4b09deda8` batched the stop observation (`request_and_restart`, `poll_worker`, `restart_and_receipt`), and its cancelled-path reads and the remote wait now run on the shutdown-only view; its two-executor `wait_for_stop_with_uncancelled` tests use one executor. `3fe00362e` added no cancellation-cleanup executor substitutions.
- [x] Added tests (see Outcomes) and ran fmt, strict Clippy and the targeted suites.

## Surprises & Discoveries

- Observation: `CommandExecutor` cannot return a `+ Sync` boxed view from a defaulted method (an unsized `Self` cannot coerce, and `Self: Sync` cannot be assumed), so the view is a thin `ShutdownOnly<'a, E: ?Sized>` borrow plus a per-command marker.
- Observation: `RemoteGitExecutor::remote` rebuilds the command from scratch and would silently drop the marker; it now carries it over.

- Observation: `LifecycleControl` in `mj-daemon/src/daemon.rs:106-228` encodes `OPEN`, `CANCELLED`, `COMMITTED`, and `CANCELLED_FOR_SHUTDOWN` in one atomic byte. Once user cancellation wins (`CANCELLED`), `ActiveLifecycle::interrupt_for_shutdown()` skips the lifecycle and `stopped_for_shutdown()` permanently returns `false`.
  Evidence: `mj-daemon/src/daemon.rs:216-223` and `mj-daemon/src/daemon.rs:748-774`. Merely switching cleanup call sites to an executor that polls `stopped_for_shutdown()` would still miss shutdown after user cancel unless `LifecycleControl` carries an orthogonal persistent shutdown signal.
- Observation: Replacing `DaemonStageReportingExecutor` with plain `ProcessExecutor` does not merely drop cancellation; it silently converts `begin_resumable_work()`, `yield_admission_if_resumable()`, `end_resumable_work()`, and `shutdown_interrupted()` to default trait no-ops (`mj-core/src/targets.rs:1078-1113`), breaking nested worker startup admission callbacks in `origin_delta.rs:760-818`.
  Evidence: `mj-daemon/src/daemon/support.rs:56-353` and `mj-controller/src/controller/lifecycle/origin_delta.rs:760-818`.

## Decision Log

- Decision: The shutdown-only view marks each command (`CommandSpec::is_shutdown_only`, executor-local, not serialized) instead of adding per-method trait hooks or returning a boxed executor.
  Rationale: `CommandExecutor` has ~230 implementers and callers require `+ Sync`, so a trait method returning a new executor type cannot be required or boxed. A marker lets every wrapper (`DaemonStageReportingExecutor`, `StagedExecutor`, `SshStageReporter`, `CrossHarnessProvisionExecutor`) keep its reporting and `upgrade_work` callbacks untouched, while the one leaf (`CancellableProcessExecutor`) runs a marked command on a twin that ignores the person's flag and the deadline. Wrappers must forward `shutdown_interrupted()` (added to `StagedExecutor`, `CrossHarnessProvisionExecutor`, `MoveCleanupExecutor`, `RemoteGitExecutor`, which also keeps the marker when it rewrites a command).
  Date/Author: 2026-11-25 / Implementer.
- Decision: Shutdown is cancelled before `cancelled` is stored in `cancel_as` and `interrupt`; `CleanupTimeout` is a separate view over `execute_cleanup_with_timeout`, composed with `ShutdownOnly`, rather than a deadline on the shutdown-only twin.
  Rationale: an operation that observes `cancelled` for a shutdown must also observe `stopped_for_shutdown()`, or it records a person's cancel. The twin ignores the operation's deadline but keeps an explicit per-call cleanup bound (`execute_cleanup_with_timeout` sets it after routing), so the 15 s bounds survive. The bound is per command; the old fresh executors shared one 15 s deadline across a whole cleanup.
  Date/Author: 2026-11-25 / Implementer.
- Decision: Lifecycle admission closures receive the `LifecycleControl` instead of its `Arc<AtomicBool>` flag.
  Rationale: the executor needs the shutdown token, and the control is the one owner of both.
  Date/Author: 2026-11-25 / Implementer.

- Decision: Add an orthogonal monotonic shutdown `CancellationToken` to `LifecycleControl` rather than adding a fifth state to the commit/cancel enum or replacing `LifecycleControl` on cleanup.
  Rationale: `BeforeClose` captures `LifecycleControl` to enforce that `grant_commit()` loses forever once user cancellation wins. Keeping the commit/cancel CAS state untouched while tracking shutdown on a monotonic `CancellationToken` makes shutdown-after-user-cancel representable without risking commit resets or a second settlement discipline.
  Date/Author: 2026-11-25 / Coordinator.
- Decision: Expose a first-class shutdown-only view on `CommandExecutor` that recursively wraps/transforms the inner executor while retaining `DaemonStageReportingExecutor` identity, reporting, and `upgrade_work` admission callbacks, instead of adding per-call gate watchers, timeout guesses, or retry loops.
  Rationale: Matches `#1677` (`fe9cc00b5`) and `#1678`'s root cause: one owner (`LifecycleControl` + `DaemonStageReportingExecutor`) owns both interruption and admission across the entire cleanup pipeline, and all repeated `if cancelled { ProcessExecutor } else { executor }` branches disappear.
  Date/Author: 2026-11-25 / Coordinator.
- Decision: Keep `move_session/recovery.rs:317` (background granted-key cleanup) and `diagnose_suspension_worker` (`lifecycle.rs:2353`, read-only 5s diagnostic) outside this cancellation-cleanup view, while updating the two rollback timeout executors in `move_session.rs:1360` and `:2841-2860` and the SSH lease setup in `move_session/transport.rs:549-552` to use the supervising executor's shutdown-only view.
  Rationale: Background key cleanup has no active lifecycle control and belongs to daemon background-task lifetime; the rollback and lease paths run directly inside an active Move lifecycle and share the exact same supervision gap.
  Date/Author: 2026-11-25 / Coordinator.

## Outcomes & Retrospective

Implemented as planned, with the marker-based view recorded in the Decision Log. A person-cancelled non-Create lifecycle now stops its cleanup commands on daemon shutdown while `grant_commit()` stays lost; a create's rollback is deliberately unchanged. The 30-second `failed_launch_cleanup_executor` use in parked rollback was dropped in favour of the supervising view, as the plan specified; shutdown now ends a stalled command there. The two Move rollback sites keep a 15-second per-command bound through `with_cleanup_timeout`. Left out of scope: `move_session/recovery.rs` background key cleanup, `diagnose_suspension_worker`, and `finish_move_result`'s separate gate check. Tests: `shutdown_only_view_survives_a_person_cancel_and_stops_for_shutdown` (core; captured, no-deadline, to-file and 300 KiB streamed stdin), `only_a_shutdown_cancel_reports_that_the_daemon_stopped` (daemon; shutdown after person cancel), and the extended `wait_for_stop_runs_shutdown_only_commands_after_cancellation_on_non_local_target` and `cancelled_container_suspend_probe_records_rollback_for_recovery` (controller), `cleanup_timeout_bounds_the_shutdown_only_view_without_losing_its_stops` (core; also covers the unrestricted executor keeping a person's cancel, which is the retained-source stop's composition), `retained_source_stop::stop_keeps_the_cleanup_bound_and_the_executors_cancellation` (controller; the extracted `stop_retained_source_worker` runs every command with the 15 s bound, unmarked, and fails on the executor's cancel) and `a_shutdown_stop_is_visible_before_the_cancelled_flag_is` (daemon).

## Context and Orientation

Mjolnir's daemon (`mj-daemon`) is the control plane that owns session lifecycle operations (Create, Suspend, Close, Resume, Move) and coordinates daemon upgrades/handoffs through a single admission gate (`mj-core/src/upgrade.rs:295-653`). While a lifecycle runs, `mj-daemon/src/daemon.rs` tracks it in `ActiveLifecycle`, which holds an `upgrade_work` admission guard and a `LifecycleControl` (`mj-daemon/src/daemon.rs:106-228`). Subprocess execution for Podman, Docker, and SSH runs through `CommandExecutor` (`mj-core/src/targets.rs:1020`), implemented at the bottom by `ProcessExecutor` (`targets.rs:1342`) and `CancellableProcessExecutor` (`targets.rs:1994`), and wrapped by the daemon in `DaemonStageReportingExecutor<E>` (`mj-daemon/src/daemon/support.rs:56-353`).

`DaemonStageReportingExecutor` owns both lifecycle progress reporting and upgrade admission handoff callbacks (`begin_resumable_work`, `yield_admission_if_resumable`, `end_resumable_work`, `shutdown_interrupted`). When a lifecycle operation is user-cancelled, `CancellableProcessExecutor` aborts normal work because its user-cancel flag is set. Today, rollback and cancellation cleanup in `mj-controller/src/controller/lifecycle.rs` and `mj-controller/src/controller/lifecycle/origin_delta.rs` work around that by switching to `ProcessExecutor` (or a fresh deadline-only `CancellableProcessExecutor`), which discards both shutdown interruption and `DaemonStageReportingExecutor`'s admission callbacks.

## Plan of Work

First, update `LifecycleControl` in `mj-daemon/src/daemon.rs` to hold a dedicated `shutdown: CancellationToken` (from `tokio_util::sync::CancellationToken`). In `cancel_for_shutdown()` and `interrupt()`, cancel this shutdown token whenever the daemon interrupts the lifecycle for shutdown. In `ActiveLifecycle::interrupt_for_shutdown()` (`mj-daemon/src/daemon.rs:748-774`), keep the existing policy that committed Create operations are not interrupted, while ensuring that an already user-cancelled (`CANCELLED`) non-Create lifecycle still cancels the shutdown token (and notifies waiters) without altering the atomic commit/cancel state (`OPEN / CANCELLED / COMMITTED / CANCELLED_FOR_SHUTDOWN`) or first-cancel cause. Update `stopped_for_shutdown()` to check the shutdown token directly (and update `shutdown_stopped()` to await `shutdown.cancelled()`), so shutdown arriving after user cancellation is always observable and monotonic while `grant_commit()` continues to return `false` forever after user cancellation.

Second, extend `CancellableProcessExecutor` and `CommandExecutor` in `mj-core/src/targets.rs` so shutdown cancellation is distinct from user cancellation and deadlines. Give `CancellableProcessExecutor` an optional shutdown `CancellationToken` (for example `shutdown_cancellation: Option<CancellationToken>`, set via a builder method such as `with_shutdown_cancellation(token)` when `mj-daemon` builds the lifecycle executor). Ensure normal execution (`is_cancelled` and `is_cancelled_without_deadline`) observes user cancellation OR the general token OR the shutdown token (plus deadline where applicable), and ensure `execute_to_file`, streamed stdin (`execute_with_stdin*`), and no-deadline methods all honor the active cancellation policy rather than constructing an uncancellable helper. Add a shutdown-only view method on `CommandExecutor` — use a clean borrowed or boxed/owned view shape that fits generic `E: CommandExecutor + Sync` callers without forcing every caller to allocate unnecessarily, such as returning a wrapper or associated `ShutdownOnly<'a>` / `ShutdownOnly` type (or `fn shutdown_only(&self) -> ShutdownOnlyExecutor<'_, Self>` if `CommandExecutor` gains a small `fn execute_shutdown_only...` or `with_shutdown_only` hook; choose the cleanest idiomatic Rust trait design so `DaemonStageReportingExecutor`, `SshStageReporter`, `StagedExecutor`, `CrossHarnessProvisionExecutor`, and `CancellableProcessExecutor` all forward accurately and cannot silently inherit a no-op default!). Verify that `SshStageReporter` (`mj-core/src/targets.rs:1153-1310`), `StagedExecutor` (`mj-controller/src/controller/provisioning.rs:1991`), `CrossHarnessProvisionExecutor` (`mj-controller/src/controller/resume.rs:3516-3590`, ensuring its destination-provisioning abort token is NOT treated as daemon shutdown), and `DaemonStageReportingExecutor` (`mj-daemon/src/daemon/support.rs:56-353`) all forward the view while preserving stage reporting, borrowed-worker hooks, destination reservations, and the exact same `upgrade_work` admission callbacks (`begin_resumable_work`, `yield_admission_if_resumable`, `end_resumable_work`, `shutdown_interrupted`). In the shutdown-only view, `cancellation_requested()` and `cancellation_requested_without_deadline()` must return `true` ONLY when daemon shutdown is signaled (never for user cancellation or operation deadline), and callers must continue consulting the original executor or `LifecycleControl` to classify user cancellation and `BeforeClose`.

Third, thread the daemon executor and its shutdown-only view across every cancellation-cleanup site and remove the ad-hoc fallbacks:

In `mj-controller/src/controller/lifecycle.rs`, add an executor parameter to `record_origin_delta_suspend_acceptance` (`:715-720`, called from `mj-daemon/src/daemon/close.rs:294`) so accepted-operation reads and writes run under the supervising executor instead of plain `ProcessExecutor`. Replace the plain `ProcessExecutor` branches at `ensure_suspend_target_running` (`:800-827`, specifically `:814`), user-cancel classification / `load_origin_delta` (`:1066-1077`), `restart_reason` / `record_suspend_cancellation` (`:1168-1175`), receipt observation (`:1308-1313`), `live_worker_mode` (`:1331-1339`), cancelled `start_worker` (`:1350-1360`), already-quiescent parked rollback (`:1864-1885`, replacing `failed_launch_cleanup_executor` with the owner's shutdown-only view and checking readmission before writing `Parked` or clearing wire records), rollback `record_restart` (`:1891-1897`), rollback Harness start (`:1899-1930`, collapsing the duplicate call branches), rollback `clear_after_restart` (`:1938-1947`, ensuring a shutdown error does not degrade to a warning followed by stale-owner settlement), `recover_interrupted_close` (`:2102-2128`), and `finish_observed_sealed_suspend` (`:2725-2742`, keeping sealed `Close` proceeding to cleanup without replaying `BeforeClose`). Keep `lifecycle.rs:91` (LocalBare local-file classification), `:113` (`cfg(test)` helper), `:1860` (semantic shutdown guard), and `:2353` (read-only 5s diagnostic) distinct.

In `mj-controller/src/controller/lifecycle/origin_delta.rs`, collapse `wait_for_stop_with_uncancelled(executor, ProcessExecutor)` (`:578-675`) back into one `wait_for_stop` function that derives one shutdown-only view from `executor` for REQUEST, RESTART, worker identity probe, and PROGRESS I/O while keeping the original `executor` for deciding whether user cancellation requests a `RESTART` intent. Keep the between-poll shutdown check so sleep intervals exit promptly too, and preserve the rule that worker identity observation continues until the worker actually exits even if a receipt already arrived.

In `mj-controller/src/controller/move_session.rs` and `mj-controller/src/controller/move_session/transport.rs`, replace the fresh 15-second rollback executors at `move_session.rs:1360` and `move_session.rs:2841-2860` with the supervising executor's shutdown-only view, and pass the supervising executor into `pipe_capture_stdout` SSH session lease acquisition (`transport.rs:549,552`). Preserve `begin_resumable_work()` / `end_resumable_work()` pairing everywhere so readmission is always checked before mutating controller or store state after resumable I/O.

Fourth, add targeted tricky-condition and integration tests covering all acceptance cases from `#1678`. Extend `mj-core/src/targets.rs` tests to verify shutdown-only cancellation of stdout capture, streamed stdin (>64KB to test pipe backpressure boundaries), output-to-file, and no-deadline execution while user cancellation is already set. Extend `mj-daemon/src/daemon/tests.rs` (around `:3340-3364`) to cover shutdown arriving after user cancellation without resetting `grant_commit()` to `true`. Extend `mj-controller/src/controller/lifecycle/tests.rs` (around `:4490-4661` and `:4758+`) and isolated daemon lifecycle tests to stall target recovery, accepted-operation record I/O, original-worker stop waiting, and rollback worker startup after user cancellation, initiate handoff/shutdown, and assert each unwinds promptly without losing the durable accepted operation or writing `Parked`/cleared records through a retired owner, while normal user cancellation without shutdown still finishes rollback and a sealed `Close` still completes cleanup.

## Concrete Steps

Work exclusively inside the detached worktree `/home/jonathan/Projects/mjolnir/.mj/clones/62f67a3f25ad58150b0b60eb2a982e94/.mj/worktrees/exec-1678`.

1. Inspect the exact current lines in `mj-daemon/src/daemon.rs`, `mj-daemon/src/daemon/support.rs`, `mj-daemon/src/daemon/close.rs`, `mj-core/src/targets.rs`, `mj-controller/src/controller/lifecycle.rs`, `mj-controller/src/controller/lifecycle/origin_delta.rs`, `mj-controller/src/controller/move_session.rs`, and `mj-controller/src/controller/move_session/transport.rs`.
2. Implement the `LifecycleControl` shutdown token, `CommandExecutor` shutdown-only view + forwarding across all wrappers, and cleanup call-site consolidation.
3. Run strict Clippy and targeted tests in the foreground (waiting for each command to finish before handing back):

       cargo fmt --all -- --check
       cargo clippy -p brokk-mj-core -p brokk-mj-targets -p brokk-mj-controller -p brokk-mj-daemon --all-targets -- -D warnings
       cargo test -p brokk-mj-core --lib targets::
       cargo test -p brokk-mj-controller --lib lifecycle::
       cargo test -p brokk-mj-daemon --lib daemon::tests::

4. Commit all changes cleanly on the worktree's detached HEAD (staging only changed files, including updating this ExecPlan's `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective`). Do not push and do not create a branch.

## Validation and Acceptance

Acceptance requires every targeted test filter to match >0 tests and pass with zero warnings under strict Clippy:
- User cancellation wins, cleanup blocks in a fake Podman/SSH command (including >64KB streamed stdin and output-to-file), then daemon shutdown arrives: the blocked command terminates promptly, `grant_commit()` remains `false`, and no retired owner writes `Parked` or clears wire records.
- Shutdown arrives while cleanup yielded admission (`begin_resumable_work`): the command terminates and failed `end_resumable_work` readmission prevents stale controller/store writes while leaving the durable accepted operation intact for recovery.
- Normal user cancellation without daemon shutdown still completes rollback in the current daemon, and an observed sealed `Closing`/`Closed` relay still finishes verified cleanup without replaying `BeforeClose`.

## Idempotence and Recovery

All changes are pure Rust runtime refactoring with no database schema or wire-protocol changes. Because work runs in a detached worktree, it is isolated from `master` and can be cleanly cherry-picked or discarded without affecting other workstreams.

## Artifacts and Notes

Reference analysis from reconnaissance is saved at `.mj/agents/22e6cb9c0015337e4fcc61640f0643ed/1678_analysis.md`.

## Interfaces and Dependencies

In `mj-daemon/src/daemon.rs`, `LifecycleControl` retains its existing atomic commit/cancel state and gains an orthogonal monotonic shutdown `CancellationToken`, signaled by `cancel_for_shutdown()` and `ActiveLifecycle::interrupt_for_shutdown()` (including when `CANCELLED` has already won for non-Create lifecycles) and read by `stopped_for_shutdown()` and `shutdown_stopped()`.

In `mj-core/src/targets.rs`, `CancellableProcessExecutor` gains `with_shutdown_cancellation(self, token: CancellationToken) -> Self`, and `CommandExecutor` provides a shutdown-only view mechanism forwarded by `DaemonStageReportingExecutor`, `SshStageReporter`, `StagedExecutor`, and `CrossHarnessProvisionExecutor` so every wrapper preserves its admission and reporting semantics while ignoring user cancellation and operation deadlines.
