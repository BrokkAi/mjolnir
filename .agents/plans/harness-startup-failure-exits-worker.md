# A worker whose harness cannot start exits with a typed reason

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md`.

Tracking issue: BrokkAi/mjolnir#1617. It supersedes one decision of `.agents/plans/worker-serves-before-harness-preparation.md` (#1192): "a worker whose preparation fails keeps serving its relay in the `Failed` state". Related and already fixed: #1619 (an interrupted Suspend whose harness failed to open).

## Purpose / Big Picture

A Mjolnir session runs its agent through a worker process (`hel worker run`, crate `mj-worker`). The worker prepares the agent's harness (resolves and installs the Codex, Claude or other bridge, sets up ACP, starts the bridge) and then waits up to 300 seconds for the bridge to open its ACP session. Today two kinds of startup failure leave the worker alive in a mode where it serves its relay with no coordinator and no harness: a failure in any preparation step, and expiry of the 300-second open deadline. The worker publishes `HarnessPreparation::Failed` and keeps answering the daemon. The session looks alive but cannot run a turn, and that mode produced a set of lifecycle defects: user shells accepted into the relay that never run, a Close that never completes, a Close racing the moment the harness starts, a pending Close that suppresses the open deadline, a timed-out preparation thread that keeps writing after teardown, and a Restart that stops at the native-readiness wait.

A third kind of startup failure already behaves differently. When the bridge reports a fault before its session first opens, the worker exits and writes a typed marker, `harness_startup_failure`, into its exit record `worker-exit.json` (commit 3c01134c4). The daemon reads that marker: relay recovery stops restarting the worker and records the failure on the session, and a Close finishes from a checkpoint-only worker that needs no harness (#1619).

After this change every startup failure takes that path. A worker whose harness cannot start always exits, and its exit record carries the typed marker with the reason: "harness preparation failed at <step>: <error>" for a preparation failure, and "the harness did not open its session within 300s; use Restart agent to retry in the existing environment" for the open deadline. No worker built from this tree publishes `HarnessPreparation::Failed`. The daemon keeps reading `Failed` from older workers that are still running.

To see it working: in an isolated instance, start a session whose harness preparation fails (for example a harness command that does not exist). The worker process is gone, `worker-exit.json` in its root holds `harness_startup_failure` with the step and message, and the session shows that message as its error instead of a session that looks alive.

## Progress

- [x] (2026-11-20) Research: worker preparation supervisor, relay coordinator startup deadline, request-side lifecycle dispatch, daemon consumers of `HarnessPreparation::Failed` and of the exit-record marker. Findings are in Context below.
- [x] (2026-11-20) Milestone 1: preparation failures exit the worker with the typed marker; Close during preparation is serialized with the Started publication (`publish_started`); cancelled preparation waits for its blocking threads.
- [x] (2026-11-20) Milestone 2: the 300-second open deadline exits the worker with the typed marker, and only a held checkpoint cut defers it.
- [x] (2026-11-20) Milestone 3: `Failed` documented as older-workers-only in `mj-core`, superseded decision noted in the older plan; worker suite (938 of 939; the remaining one is the host's French-locale Git quirk and passes with `LANGUAGE=en`), clippy on `mj-worker` and `mj-core`, workspace check.
- [x] (2026-11-20) End-to-end: `tests/e2e/startup_failure_marker.py`, extended with a preparation-failure phase, passes in the isolated instance `e2e-1617`.
- [x] (2026-11-20) Independent review fixes: `publish_started` cancels preparation itself when it finds the relay sealed; a cancelled preparation no longer waits for its threads. Worker suite (938 pass with `LANGUAGE=en`) and clippy rerun.

## Surprises & Discoveries

- Observation: the daemon already handles a worker that exits with the typed marker on every path that matters. Relay recovery reads the marker before restarting a dead worker and moves a `Running` or `Disconnected` session to `Error` (`mj-session-runtime/src/session_manager/recovery.rs`, `dead_worker_startup_failure`). A Close whose worker is proven dead with the marker restarts the worker checkpoint-only and finishes (`reopen_closing_relay_without_harness` in `mj-controller/src/controller/lifecycle.rs`). Fresh launches and resumes add the exit reason to their error through `worker_probe_diagnosis` (`mj-worker-staging/src/worker_binary/process.rs`). So the worker change needs no new daemon behaviour.
  Evidence: commits 3c01134c4, a2de94486, d908daae9.
- Observation: a Close can only be accepted at a ready checkpoint cut (`close_cut_refusal` in `mj-worker/src/relay/checkpoint_hold.rs`), and the coordinator claims neither `BeginCheckpoint` nor `Close` until the ACP session is configured (`claim_pending_commands_up_to` in `mj-worker/src/relay/commands.rs`). A Close accepted while the session was ready, followed by a bridge restart that never reconfigures, therefore waits for a configuration that never comes, and the `close_pending()` term of the coordinator's `held` guard kept the open deadline from firing. That is the "Close can suppress the open deadline" and "Close can strand" pair from the issue.

- Observation (review): the request handler accepts a Close and cancels preparation under two separate relay-lock acquisitions. `publish_started` could run between them, refuse because of the seal, and leave the token uncancelled, so the refusal read as a startup failure and the worker exited mid-Close. A worker that boots with a durable Closing relay reached the same refusal with no handler to cancel at all.
  Evidence: `a_close_during_preparation_and_started_have_one_order`, case `SealedBeforeHandlerCancels`, fails with "harness preparation failed at acp-setup: preparation cancelled before the bridge started" when `publish_started` does not cancel.

## Decision Log

- Decision: every startup failure exits the worker with `mj_core::relay::WorkerHarnessStartupFailure` as the error the worker returns, so `mj-worker-bin` writes it into the exit record. This covers a failed, timed-out or panicking preparation step and the 300-second open deadline, in addition to the existing bridge fault before the first open.
  Rationale: one carrier for "this harness cannot start" in current workers. Live `Failed` was a second state with its own lifecycle rules, and each of the issue's defects came from those rules. Process exit also ends any preparation thread that could not be stopped.
  Date/Author: 2026-11-20, Claude.
- Decision: the marker message for a preparation failure is "harness preparation failed at <step>: <error>".
  Rationale: it is the text the daemon already stores for a live `Failed` report from older workers (`HarnessPreparationFailure`'s `Display` in `mj-session-types/src/lib.rs`), so a session's error reads the same whichever worker generation produced it. The struct keeps its single `message` field; the record format does not change.
  Date/Author: 2026-11-20, Claude.
- Decision: no daemon behaviour change and no relay protocol bump. The daemon keeps all of its `HarnessPreparation::Failed` handling for older workers, and the variant stays in `mj-core`.
  Rationale: the daemon already settles a dead worker with the marker (see Surprises). A new worker under an older daemon is not a supported pairing: the daemon installs the worker, and the marker reader (3c01134c4) ships in the same release as this change.
  Date/Author: 2026-11-20, Claude.
- Decision: a Close accepted while the harness is preparing still cancels preparation, and the worker then serves its sealed relay without a harness, as a worker whose relay was closed at boot does. Preparation never publishes `Failed` for it. The relay's `execution` (Closing, then Closed) is the fact the daemon reads, and its readiness probe already checks Closed before Preparing.
  Rationale: a sealed relay is not a startup failure. It must stay attachable so the daemon can finish its checkpoint.
  Date/Author: 2026-11-20, Claude.
- Decision: the relay lock is the single decision point between a Close during preparation and the Started publication. The request handler reads the preparation state, dispatches relay-local lifecycle work and cancels preparation inside one relay lock. `start_bridge` publishes Started inside the relay lock, and only when preparation is not cancelled and the relay is neither Closing nor Closed. Lock order is always relay lock, then the preparation watch.
  Rationale: before, the handler read the watch outside the lock and could cancel preparation after Started was published. That aborted a coordinator that had already claimed the Close, and the Close was stranded InFlight. With one decision point, the race cannot be expressed: either the handler sees Preparing and the bridge never starts, or it sees Started and the coordinator owns the Close.
  Date/Author: 2026-11-20, Claude.
- Decision (revised after review): a cancelled preparation returns at once. Only a failed one waits for its threads, within the failed step's own bound, before the worker exits.
  Rationale: a cancelled preparation either belongs to a worker that is stopping, where the exit ends every thread and a wait only delays an orderly exit past the stop script's kill, or to a worker that keeps serving a sealed relay, where waiting changes nothing observable. There the cancelled threads finish under their own step bound with their commands stopped, and the controller stops the worker process before it removes any of its files. A failed preparation's wait lets threads whose commands were just cancelled stop those commands before the exit ends them.
  Date/Author: 2026-11-20, Claude.
- Decision: `publish_started` cancels preparation itself, under the relay lock, when it finds the relay sealed.
  Rationale: the cancelled token is then the single fact that tells a cancellation from a startup failure, whichever side sees the seal first. See the review observation in Surprises.
  Date/Author: 2026-11-20, Claude.
- Decision: the open deadline is deferred only while a checkpoint barrier holds its cut (`worker_writes_held()`), not while a Close is pending. At expiry the coordinator interrupts in-flight commands with the cause, clears open elicitations, and returns the typed failure; it no longer hands lifecycle work back to the request handler.
  Rationale: a held cut is daemon-owned and ends on a relay request or a disconnect, so deferring is bounded, and the expiry's own journal writes would otherwise move the cut. A pending Close is not bounded while the session is unconfigured, because the coordinator never claims it. After the exit, the daemon's close finishes from a checkpoint-only worker (#1619).
  Date/Author: 2026-11-20, Claude.

- Decision: every blocking thread a preparation starts is counted, through a task-local `TaskTracker` that `mj_core::runtime::spawn_blocking` consults (`mj_core::runtime::track_blocking_work`). The preparation also runs inside `mj_core::subprocess::with_preparation_cancellation`, so commands that harness installs run through the shared helpers stop with it. Threads are waited for up to the current step's own deadline, recorded in `PreparationSnapshot::step_deadline`.
  Rationale: harness-resolve awaits blocking work deep inside harness installation (tool cache, node entrypoints, archive extraction, `npm ci`), which a tracker passed by hand would not reach. Bounding the wait by the step's deadline means a step that already timed out exits at once. Limitation: tokio's own blocking pool, used by `tokio::fs`, is not counted.
  Date/Author: 2026-11-20, Claude.

## Outcomes & Retrospective

A worker whose harness cannot start now always exits, and its exit record carries the typed marker. No current worker publishes `HarnessPreparation::Failed`. End to end, a replacement worker whose preparation failed at `command-permissions` exited with "harness preparation failed at command-permissions: …". Relay recovery then moved the session to Error with that cause, did not restart the worker during a 65-second observation, and Resume restored the session. The daemon needed no change.

Lessons. Process exit is the only complete way to stop a preparation whose threads cannot be stopped, so failure exits; only a sealed relay keeps the worker alive without a harness. A decision split across two lock acquisitions is still two decisions: the review found a Close sealed in one and cancelled in the next, which the Started publication could split. A wait needs a reason someone can observe; the first draft waited for a cancelled preparation's threads on paths where the wait changed nothing but stop latency. Boxing matters: the extra wrappers around the preparation future overflowed the test thread's stack until the future was boxed before wrapping. In the end-to-end lab the Codex launch uses the managed runtime, which ignores `bridge_command`, so a missing bridge is not a preparation failure there; a missing harness home is.

## Context and Orientation

Terms. The worker is the per-session process that runs the agent's harness through an ACP bridge (ACP is the Agent Client Protocol). The relay is the worker's durable journal of commands and events, plus the control socket the daemon talks to. `RelayOperationalState` (`mj-core/src/relay/snapshot.rs`) is its live summary, and its field `harness_preparation: Option<HarnessPreparation>` reports `Preparing { step, since_ms }`, `Started`, or (older workers only, after this change) `Failed { step, error, at_ms }`. The coordinator is the task that records ACP runtime events into the relay and dispatches queued commands to ACP (`run_relay_coordinator` in `mj-worker/src/worker_runtime/unix/dispatch.rs`). A checkpoint barrier is a `BeginCheckpoint` command whose ready cut freezes the journal so the daemon can archive it. A Close is accepted only at that cut and seals the relay. The exit record is `worker-exit.json` in the worker root, written by `mj-worker-bin/src/main.rs` from the error `run_daemon_owned` returns. `mj-worker/src/exit_record.rs` writes the record; a typed `WorkerHarnessStartupFailure` anywhere in the error chain becomes its `harness_startup_failure` field.

Worker startup today (`mj-worker/src/worker_runtime/unix.rs`, `run_daemon_owned`). The worker recovers the durable relay, publishes the control socket, and spawns the accept loop. It then spawns `supervise_harness_preparation`, which runs `prepare_and_start_harness` under `supervise_preparation`. That function selects on the cancellation token, catches panics, and on any error publishes `Failed` and records the failure in `worker-startup.json`. `start_bridge` spawns the ACP runtime, publishes `Started` with the prepared services, and spawns the coordinator. If preparation fails, `run_daemon_owned` logs "harness preparation failed; keeping relay available" and serves the relay until a signal. Blocking steps run through `bounded_blocking_preparation_step`, which detaches the thread on timeout or cancellation.

Request side (`serve_client_with_memory` in the same file). After `handle_request`, the handler reads the preparation watch. While preparation is pending (anything but `Started`), it calls `DurableRelay::dispatch_preparation_lifecycle` (`mj-worker/src/relay/commands.rs`), which completes `BeginCheckpoint` (checkpoint ready) and `Close` (Closed) locally. If the request was a Close and the relay is Closing or Closed, it cancels preparation.

Open deadline (`HarnessStartup` in `dispatch.rs`). The coordinator arms a 300-second deadline (`mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT`) and rearms it on a bridge restart. On expiry, unless `held`, it interrupts in-flight commands, drains lifecycle work locally, publishes `Failed` with step `session-open`, hands its dispatch-wake receiver to the runtime owner (`idle_dispatch_wakes`), and returns `Ok`. A bridge fault before the first configuration already returns `Err(WorkerHarnessStartupFailure)` from `record_runtime_event_with_startup`, after clearing elicitations.

Daemon side, for reference only (no change). `mj-session-runtime/src/readiness.rs` (`native_session_readiness`, `wait_for_startup`) and `mj-session-runtime/src/session_manager/readiness.rs` (`HarnessStartupWatch`) turn a live `Failed` into `HarnessPreparationFailure`. Its consumers persist the session's failure: `checkpoint/latched.rs`, `worker_restart.rs`, `resume/repair.rs`, and `mj-daemon/src/worker_upgrade.rs`. Recovery reads the exit marker in `session_manager/recovery.rs`.

## Plan of Work

Milestone 1, preparation failure. In `mj-worker/src/worker_runtime/unix.rs`, change `supervise_preparation` so that on a non-cancellation error it records the failure in `worker-startup.json` (as today) and returns an error built from `WorkerHarnessStartupFailure { message: format!("harness preparation failed at {step}: {error:#}") }`, without publishing any preparation state. On cancellation it returns a cancellation outcome and publishes nothing. Run the preparation inside `mj_core::runtime::track_blocking_work` (a new task-local tracker that `mj_core::runtime::spawn_blocking` consults) and `mj_core::subprocess::with_preparation_cancellation`. After a failure, wait for the tracker until the failed step's deadline before returning; after a cancellation, return at once. In `run_daemon_owned`, a failed preparation returns that error, so the worker exits. A cancelled preparation keeps serving the sealed relay and keeps the dispatch-wake receiver alive while it does. Make `start_bridge` publish Started through `publish_started`, inside the relay lock, refusing when cancelled or when the relay is Closing or Closed; a seal found there cancels the preparation itself. In `serve_client_with_memory`, read the preparation state, dispatch relay-local lifecycle work and cancel preparation inside one relay lock. Remove the live-`Failed` branches: the adoption wait's `Failed` check, and the `preparation_failed` serving branch at the end of `run_daemon_owned`. `preparation_error` loses its `Failed` arm.

Milestone 2, open deadline. In `dispatch.rs`, the expiry branch takes the relay lock. If `worker_writes_held()`, it continues (the hold's end wakes the loop). Otherwise it sets `acp_ready` false, interrupts in-flight commands with the cause, and records `ElicitationsCleared`. It then shuts down the verdict tasks and returns `Err(WorkerHarnessStartupFailure { message: cause })`. The sleep branch's guard uses `worker_writes_held()` instead of `held`. `HarnessStartup` loses `status`, `idle_dispatch_wakes` and `fail`. `RunningHarness` and `StartedBridgeTasks` lose `idle_dispatch_wakes`. `run_daemon_owned` already returns the coordinator's error through `stop_peer_and_return`, which stops the ACP runtime first.

Milestone 3. Update the `HarnessPreparation::Failed` doc comment in `mj-core/src/relay/snapshot.rs` to say that only older workers send it. Note the superseded decision in `.agents/plans/worker-serves-before-harness-preparation.md`. Validate, commit and push.

## Concrete Steps

Work from the repository root. Run the focused worker tests after each milestone, outside the sandbox:

    cargo test -p brokk-mj-worker preparation
    cargo test -p brokk-mj-worker harness_startup
    cargo clippy -p brokk-mj-worker --all-targets -- -D warnings

At the end, also run `cargo test -p brokk-mj-core relay` and `cargo check --workspace --all-targets`. `mj-core`'s doc comment is the only change outside the worker, and the daemon crates compile against the unchanged type.

## Validation and Acceptance

The existing worker tests change with the behaviour. `failed_preparation_stays_attachable_and_checkpointable` is replaced by `a_harness_that_cannot_be_prepared_ends_the_worker_with_a_typed_failure` in `relay_tests.rs`, which runs the real `run_daemon` with a bridge that does not exist and expects it to return the typed failure for harness-resolve and remove its control socket. `panicking_preparation_ends_with_a_typed_startup_failure` and `blocking_preparation_deadline_fails_without_waiting_for_its_thread` assert the typed failure instead of a published `Failed`; the second also asserts that an expired step does not wait for its blocked thread. `close_during_preparation_kills_the_fake_harness_process_group` asserts that preparation ends `Cancelled` without publishing `Failed`, that the relay is Closed and that no process survives. `adoption_wait_reports_preparation_failure_with_a_stranded_prompt` is removed, because a failing worker now drops the connection. In `dispatch.rs`, `harness_startup_ready_and_expiry_have_one_outcome` asserts that expiry returns the typed failure and still interrupts a dispatched clear exactly once, and its new `CloseAtReleasedCut` arrival shows that a pending Close no longer holds the deadline and that nothing is journaled behind the seal. New tricky-condition test: `a_close_during_preparation_and_started_have_one_order`, which covers a Close through the real request handler before Started, Started before the Close, and a seal that `publish_started` finds before the handler cancels. End to end, `tests/e2e/startup_failure_marker.py` gains a phase in which a replacement worker whose launch names no harness home fails preparation, exits with the marker, and leaves the session in Error without restarts until Resume.

Acceptance: no code in `mj-worker` constructs `HarnessPreparation::Failed`, and the focused suites and clippy pass.

## Idempotence and Recovery

Code-only change, no migration. Tests use temporary directories. Re-running any step is safe.

## Interfaces and Dependencies

`mj_core::relay::WorkerHarnessStartupFailure { message: String }` is unchanged and remains the only typed startup failure a current worker emits. `HarnessPreparation` keeps all three variants for wire compatibility.
