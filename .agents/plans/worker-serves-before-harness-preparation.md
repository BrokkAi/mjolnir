# Serve the worker relay before harness preparation

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md`.

Tracking issue: BrokkAi/mjolnir#1192. Related: #1200 (fixed by 2fd45e3c, the misleading "no startup step" message) and #1209 (asks for a readiness timeline).

## Purpose / Big Picture

A Mjolnir session runs its agent through a worker process (`hel worker`, crate `mj-worker`). The daemon (`mj daemon-run`, crate `mj-controller`) starts the worker and connects to the worker's control socket. Today the worker opens that socket only after it has prepared the agent's harness (resolved and checked the Codex or Claude bridge, captured the review baseline, and started the bridge). The daemon therefore cannot tell a worker that is slowly preparing from one that is not running at all, and it guesses with a timer: 30 seconds, extended by 60 seconds whenever the worker records a new startup step, up to 300 seconds. On a loaded host, a worker that was alive and making progress was declared failed (issue #1192), and the user only saw a generic "Start" stage.

After this change the worker answers the daemon as soon as it has recovered its durable state, then prepares the harness in the background and reports, through its relay state, which preparation step it is on and since when, or that preparation failed and why. The daemon waits only a short time for the worker to answer, then shows "Preparing <harness>: <step>" while the worker prepares, and reports the worker's own failure message if preparation fails. A slow host makes launches slower, not failed.

To see it working: start a session in an isolated instance with a harness whose preparation is deliberately slow (see Validation). The session's launch stage reads "Preparing ..." with the worker's current step for as long as preparation takes, and the launch succeeds even when preparation takes longer than 60 seconds in one step.

## Progress

- [x] (2026-10-05) Research: current worker startup order, daemon readiness wait, cleanup of failed launches. Findings are in Context below.
- [x] (2026-10-05) Shared type `HarnessPreparation` and field `RelayOperationalState::harness_preparation` added in `mj-core/src/relay/snapshot.rs`.
- [x] (2026-10-05) Milestone 1: worker serves before preparation, reports `harness_preparation`. Worker and core suites pass; clippy clean.
- [x] (2026-10-05) Milestone 2: daemon waits for the worker to answer, shows the preparation step, and fails on the worker's reported failure. Controller suite passes (2,239 passed, 10 ignored); clippy clean.
- [x] (2026-10-05) Integration: daemon half applied onto the worker half; client `PROTOCOL_VERSION` raised from 54 to 55.
- [x] (2026-10-05) Review fixes: preparation always ends in `Started` or `Failed` (cancellation and panics publish `Failed`); worker-owned 3-minute deadlines for the synchronous steps; the daemon's probe reads `Failed`, then `Closed`, then `Preparing`.
- [x] (2026-10-05) Milestone 3: full validation passed (core, worker, controller, client, TUI, CLI suites; workspace clippy; fmt), including the 14 isolated startup and upgrade regressions in `mj-cli/tests/daemon_startup.rs`.
- [ ] Isolated end-to-end launch with slowed and failing preparation (completed: attempted in `--instance prep-1192`; remaining: needs a signed-in harness profile, which the agent container did not have).

## Surprises & Discoveries

- Observation: most of #1192's stopgaps already landed before this plan. The readiness wait extends on new startup steps (da519db5, 5578e1b4), the "no startup step" message was a parser bug (2fd45e3c), the per-launch npm tree hash that made preparation slow was reverted (1bedf09e), and a failed fresh launch removes its target (`rollback_failed_new_session`, `mj-controller/src/controller/provisioning.rs`).
  Evidence: commit history; v2.24.0 and v2.25.0 tags contain them.
- Observation: 2c6df023 moved the control socket to just before the accept loop. That stopped connections hanging in the socket backlog, but it means the socket is absent for all of harness preparation, so readiness still measures preparation.

## Decision Log

- Decision: the worker is the only owner of the preparation state, published as `RelayOperationalState::harness_preparation` with three states: `Preparing { step, since_ms }`, `Started`, `Failed { step, error, at_ms }`.
  Rationale: the daemon's step-extended timer is a second opinion on a fact only the worker knows. One owner removes the guess.
  Date/Author: 2026-10-05, Claude (approved by jbellis).
- Decision: a worker whose preparation fails keeps serving its relay in the `Failed` state instead of exiting.
  Rationale: the daemon then receives the failure as a fact with the step and message, and the session can still be checkpointed and closed through the normal relay. The daemon's failed-launch rollback then removes the target.
  Date/Author: 2026-10-05, Claude.
- Decision: while a worker reports `Preparing`, the daemon applies no deadline of its own. The worker bounds each preparation step and reports `Failed` when a bound is exceeded. The daemon's existing 300-second native-session deadline applies from `Started` until the ACP session opens. User cancellation still ends the wait at any time.
  Rationale: a deadline on a worker that reports progress is the guess this plan removes. The worker knows which step is slow and can say so.
  Date/Author: 2026-10-05, Claude.
- Decision: keep the daemon's existing connect wait (30 seconds, extended by new startup steps up to 300 seconds) unchanged.
  Rationale: older workers still accept only after preparation, and new workers still run login-environment discovery and durable-relay recovery before they accept.
  Date/Author: 2026-10-05, Claude.
- Decision: the field is optional and additive; no relay protocol version change.
  Rationale: `RelayOperationalState` has no `deny_unknown_fields`, so older daemons ignore it. A new daemon reading an older worker sees `None` and behaves as today. An older daemon reading a new worker connects earlier and waits on `acp_ready` as it does today; if preparation fails it times out after 300 seconds instead of seeing an exit. Daemons are not downgraded automatically, so that pairing is rare.
  Date/Author: 2026-10-05, Claude.

- Decision: raise the client protocol (`PROTOCOL_VERSION` in `mj-client/src/daemon.rs`) from 54 to 55.
  Rationale: `ProvisionStage` gained the struct variant `PreparingHarness { harness, step, since_ms }`, and clients receive stages in `RuntimeLifecycleView::active_stages`. Clients require an exact protocol match, and that file says every non-frozen action changes behind a version bump. An older client would otherwise fail to decode the new stage.
  Date/Author: 2026-10-05, Claude.
- Decision: keep the activity reason "harness session still opening" for a failed preparation for now.
  Rationale: it is an internal reason string from `mj-core/src/activity.rs`. The user-visible result of a failure is the session error and notice the daemon records with the worker's step and message. A separate activity fact can follow if the reason string proves confusing.
  Date/Author: 2026-10-05, Claude.
- Decision (revised after review): the synchronous local steps (subagent-profile, project-memory, acp-setup, bridge-start) run on blocking threads under worker-owned 3-minute deadlines, and preparation always ends in `Started` or `Failed`. Close and shutdown publish `Failed` ("cancelled because the session is closing or the worker is shutting down"), and so does a panic.
  Rationale: the daemon applies no deadline while it reads `Preparing`, so a `Preparing` state that never changes would be a wait that never ends. Review found exactly that for a Close during preparation. The worker owns the deadline because it knows which step is slow. The steps that call external programs keep their own bounds: review-baseline 130 s (Git commands 120 s), login-resolve 10 s, harness-resolve 20 minutes, managed install lock and `npm ci` 15 minutes, other installer commands 10 minutes; the shared subprocess helper kills and reaps the process group on timeout or cancellation.
  Date/Author: 2026-10-05, Claude.

- Decision: accept that a timed-out blocking step's thread is abandoned rather than stopped.
  Rationale: a blocked system call cannot be stopped safely from another task. The abandoned thread cannot publish preparation state or start the bridge, because only the supervisor publishes and its continuation is dropped. It can still finish a side effect it had already begun (for example, a project-memory prompt-context write) after `Failed` is published. That needs a step blocked for more than 3 minutes, a failed fresh launch is torn down anyway, and a worker in `Failed` never starts its harness. Making late side effects impossible would need a cancellation-safe commit design for each step; that is out of scope here.
  Date/Author: 2026-10-05, Claude.

## Outcomes & Retrospective

The worker now answers the daemon right after durable-relay recovery and reports harness preparation as its own state. The daemon shows "Preparing <harness>: <step>" with the worker's start time, applies no deadline while the worker reports progress, and reports the worker's failure message when preparation fails. A slow host now makes launches slower instead of failing them. Remaining: an end-to-end launch on a real target with a slowed harness, and a distinct activity reason for a failed preparation (`mj-core/src/activity.rs`). Lesson: a "no deadline while the owner reports progress" rule is only safe if every path, including cancellation and panics, ends in a terminal state; the first version missed cancellation.

## Context and Orientation

Terms. The worker is the per-session process that runs the agent's harness (the Codex or Claude command-line agent, driven through an ACP bridge; ACP is the Agent Client Protocol). The relay is the worker's durable journal of commands and events plus the control socket the daemon talks to; its live summary is `RelayOperationalState` in `mj-core/src/relay/snapshot.rs`. `worker-startup.json` is a file in the worker root where the worker appends named startup steps with timestamps; the daemon reads it through `probe_worker` in `mj-controller/src/controller/worker_binary/process.rs`.

Worker startup today (`mj-worker/src/main.rs` around lines 373-428 and `mj-worker/src/worker_runtime/unix.rs` lines 269-693) runs these steps in order: start, login-environment (a login shell, bounded at 5 seconds), re-exec, runtime, policy, optional review-baseline (Git work, command timeout 120 seconds, `unix.rs` 300-347), durable-relay (open and replay the journal, `unix.rs` 349-356), login-resolve, harness-resolve (`prepare_harness_launch`, `mj-worker/src/harness_launch.rs`; for an ambient Codex bridge it runs `codex-acp --version` with a 10-second bound; for a managed bridge it may run `npm ci` under a per-harness lock, `mj-worker/src/harness.rs`), ACP channels and configuration, reviewer and sub-agent sockets, the launch spec, bridge-start, then bind-socket and serving just before the accept loop (`unix.rs` 670-693). Checkpoint-only or closed relays skip harness preparation and publish immediately (`unix.rs` 441-475).

The daemon's first request is Hello (`mj-core/src/relay/protocol.rs` 50-58 and 430-440; worker side `mj-worker/src/relay/requests.rs` 55-77), which needs only the recovered relay identity. Attach (next) reads relay state and journal pages and already works without ACP; the state reports `acp_ready: Some(false)` until the ACP session is configured (`mj-worker/src/relay.rs` 440-445, 959-989), and activity shows "harness session still opening" (`mj-core/src/activity.rs` 564-566). Prompts are journaled and dispatched only after ACP reports `SessionConfigured` (`unix.rs` 1229-1279, `unix/dispatch.rs` 398-415 and 849-870). Other parts of the client runtime are wired to objects built during preparation: the ACP command sender, reviewer sidecar, sub-agent endpoint, credentials, project memory and CPU state (`unix.rs` 700-723 and 893-899). Checkpoint barriers are owned per connection (`unix.rs` 980, 1255-1285).

Daemon side: `connect_and_start_worker` in `mj-controller/src/controller/provisioning.rs` (around 868-928) starts the worker, calls `connect_started_worker` (`mj-controller/src/controller/readiness.rs`, connect wait described above), then `wait_for_native_session_in_stage` with the stage from `bridge_readiness_stage` (`mj-controller/src/controller/worker_binary/harness.rs` 54, label "Installing <harness>"). `wait_for_native_session` (`readiness.rs` 77-130) polls `NativeSessionProbe::native_session_readiness` (Ready, Closed, Waiting) under `NATIVE_SESSION_STARTUP_TIMEOUT` of 300 seconds. Stages are `ProvisionStage` in `mj-core/src/targets.rs`; clients see the active stage label and its start time (`mj-controller/src/server_runtime/snapshot.rs` 131-167 and 672-692).

## Plan of Work

Milestone 1, worker. In `mj-worker/src/worker_runtime/unix.rs`, publish the control socket and enter the accept loop right after durable-relay recovery. Move login-resolve, review-baseline, harness preparation, ACP setup, socket and launch-spec construction, and bridge start into one supervised background task that owns `harness_preparation`: it sets `Preparing { step, since_ms }` as it enters each step (the same step names it writes to `worker-startup.json`, which it keeps writing), `Started` when the bridge is started, and `Failed { step, error, at_ms }` on any error, recording the failure in `worker-startup.json` too. Review-baseline must still finish before the bridge starts. Make the objects built during preparation reachable by the connection handlers once they exist (for example through a `tokio::sync::watch` of the prepared runtime), and give every request that needs them a defined behaviour before preparation completes and after it fails: wait for preparation, answer "not ready", or answer with the failure. List each request and its behaviour in this plan. Prompt submission keeps its existing journal gate. Checkpoint barriers must work during preparation, since no turn can be running. Suspend, close and worker shutdown must cancel the preparation task and stop any processes it started (process group first, files second). Every preparation step must have a bound; where one is missing (check the managed `npm ci` path), add one through the shared subprocess helpers, and report exceeding it as `Failed`. If the activity classification in `mj-core/src/activity.rs` should describe `Preparing` or `Failed` differently from "harness session still opening", change it there, in the one classification function.

Milestone 2, daemon. In `mj-controller/src/controller/readiness.rs`, extend the native-session readiness result with a failure carrying the worker's step and message, and end the wait with an error like "harness preparation failed at harness-resolve: <message>" when the worker reports `Failed`. While the worker reports `Preparing`, publish a stage that names the harness and the step (add a variant or detail to `ProvisionStage` in `mj-core/src/targets.rs`) and its start time, and do not count the 300-second deadline; start that deadline when the worker reports `Started` (or immediately for workers that omit the field). Check every caller of `connect_started_worker` and `wait_for_native_session` (fresh create, resume, worker restart, sub-agent start, checkpoint bounce) and make the failure reach the session as a visible error. Leave the connect wait unchanged.

Milestone 3, integration. Merge both halves, run the isolated end-to-end check below, run the upgrade regressions, run full validation, commit with a body that explains the ownership change, and push to `origin master`.

## Concrete Steps

From the repository root `/workspace/1872507e4d7eeafd43b66594471ab0a3/hel`: focused tests per round with `cargo test -p brokk-mj-worker <filter>`, `cargo test -p brokk-mj-controller <filter>`, `cargo test -p brokk-mj-core <filter>`. Final validation once: `cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings`, both outside the sandbox, on the dev profile.

## Validation and Acceptance

Worker behaviour tests (colocated `#[cfg(test)]`), using a hand-written fake harness whose preparation blocks until the test releases it: Hello and Attach are answered while preparation is blocked and `harness_preparation` reports `Preparing` with the step; a prompt submitted during preparation stays queued and is dispatched after the ACP session opens; a preparation error yields `Failed` with step and message while Attach and checkpoint still work; close during preparation cancels it and leaves no child process.

Daemon behaviour tests with a fake `NativeSessionProbe`: `Preparing` beyond 300 seconds (paused tokio time) does not fail; `Failed` ends the wait with the worker's message; a worker that omits the field keeps today's deadline; the published stage carries the step.

End to end in an isolated instance (`--instance prep-1192`): launch a session whose preparation is slowed beyond 60 seconds in one step and observe the "Preparing" stage and a successful launch; launch one whose preparation fails and observe the worker's message as the session error and the target removed.

## Idempotence and Recovery

All changes are code; no database migration. Tests use isolated data directories. Re-running steps is safe.

## Artifacts and Notes

Preparation steps, in order, as the worker reports them: review-baseline (only when review capture is on), login-resolve, subagent-profile (when sub-agents are configured), project-memory (when configured), harness-resolve, acp-setup, bridge-start.

Request behaviour while the worker prepares and after preparation fails:

    Hello, Attach, Status          answered at once; Attach and Status carry harness_preparation
    Prompt and other ACP commands  accepted into the journal; dispatched after SessionConfigured;
    (including SetConfig)          after a failure they stay journaled
    BeginCheckpoint, Close         work during and after preparation; Close cancels preparation
    Reviewer, configured sub-agents,
    elicitation, StopBackgroundTask  answer "not ready" or the failure
    CPU, credentials, skills, token,
    project memory, history,
    attachments                    handled directly; independent of preparation

Behaviour tests: worker `hello_attach_and_prompt_queue_work_while_fake_preparation_is_blocked`, `failed_preparation_stays_attachable_and_checkpointable`, `close_during_preparation_kills_the_fake_harness_process_group`, `a_reviewed_session_serves_its_relay_while_preparing_its_baseline`; daemon `native_session_preparing_does_not_count_timeout_and_started_gets_full_budget`, `native_session_preparation_failure_names_worker_step_and_message`, `native_session_without_preparation_field_keeps_300_second_timeout`, `native_session_preparation_stage_carries_worker_step_and_start_time`.

## Interfaces and Dependencies

In `mj-core/src/relay/snapshot.rs` (already added):

    #[serde(tag = "state", rename_all = "snake_case")]
    pub enum HarnessPreparation {
        Preparing { step: String, since_ms: i64 },
        Started,
        Failed { step: String, error: String, at_ms: i64 },
    }

    // in RelayOperationalState, beside acp_ready:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_preparation: Option<HarnessPreparation>,

Durable snapshots never carry it (`operational_state` sets it to `None`, like `acp_ready`); the live worker fills it.
