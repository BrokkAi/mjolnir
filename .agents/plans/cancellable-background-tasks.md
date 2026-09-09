# Add targeted background-task cancellation

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir can already identify commands an agent left running, but the task list is observational: a person must ask the agent to stop work or terminate the whole session. After this change, the conversation TUI and web viewer can immediately request that one supported background task stop while leaving its session, foreground turn, and sibling tasks alone. Claude background tasks use its JetBrains AIR extension, while Kimi and other agents whose shells run in client-owned ACP terminals use Mjolnir's terminal process-group supervisor. Codex tasks remain visible but read-only until its adapter exposes a matching control primitive.

## Progress

- [x] (2026-09-09) Audited standard ACP, Claude ACP 0.73.0, Kimi 0.41.0, Codex ACP 1.8.0, and Mjolnir's current task projection.
- [x] (2026-09-09) Chose immediate, per-task controls in both the TUI and web viewer.
- [x] (2026-09-09) Added relay protocol 9, opaque namespaced task identities, capability projection, and the live session control path.
- [x] (2026-09-09) Added Claude AIR lifecycle/stop support and hosted-terminal process-group stopping.
- [x] (2026-09-09) Added TUI and web controls with supervised request state, deduplication, and failure reporting.
- [x] (2026-09-09) Filed `agentclientprotocol/codex-acp#492` and recorded the URL.
- [x] (2026-09-09) Completed validation, committed the integrated change, and pushed `master`.

## Surprises & Discoveries

- Observation: Standard ACP has no client-to-agent request for cancelling one agent-owned background task. `session/cancel` targets the current turn, while `terminal/kill` is an agent-to-client request for a client-owned terminal.
  Evidence: The pinned adapters' method tables and Mjolnir's ACP handlers expose no standard task-stop request.
- Observation: Claude ACP 0.73.0 implements JetBrains AIR async-task notifications and `_session/async_task/stop`, but only for clients advertising the `asyncTasks` capability.
  Evidence: The adapter's extension response is `{ stopped: bool }`, and spawned notifications carry an async task ID and `canStop`.
- Observation: Kimi does not expose its internal TaskStop model tool over ACP, but its Bash runner uses Mjolnir's `terminal/create`; therefore Mjolnir already owns the process group that must be killed.
  Evidence: `TerminalRegistry` keeps every live terminal ID and exposes process-group `kill` without discarding output or exit state.
- Observation: Codex ACP 1.8.0 owns its detached exec processes and provides neither AIR async tasks nor another targeted stop method.
  Evidence: Its `session/cancel` maps only to `turn/interrupt`; the `terminal/kill` reverse request does not apply to Codex-owned processes.
- Observation: Running all six PTY termination fixtures concurrently is timing-sensitive on this host: the repository-wide `cargo test` run reached them after the large parallel suites and they failed before producing their expected dashboard output. A serial batch still had two timeouts, but each of those two cases passed when isolated; the other four passed in the serial batch.
  Evidence: `workspace_manager_terminates_without_leaving_and_reopening_the_dashboard` passed alone in 2.29s and `disabled_startup_waits_for_explicit_new_before_creating_a_session` passed alone in 7.42s. All Rust unit/integration suites before that binary passed.
- Observation: Once relay protocol 9 made the attachment compatibility window span two versions, clippy correctly identified its old pair of comparisons as a manual range check.
  Evidence: Rewriting it to `(8..=RELAY_PROTOCOL_VERSION).contains(...)` preserved the compatibility boundary and made the warnings-as-errors gate pass.

## Decision Log

- Decision: Publish an opaque, namespaced task ID and `can_stop` on every `BackgroundCommand`.
  Rationale: Surfaces need stable row identity and capability without learning provider routing details. Serde defaults keep protocol-8 snapshots readable.
  Date/Author: 2026-09-09 / Codex
- Decision: Make stopping a connection-only relay request rather than a durable `RelayCommand`.
  Rationale: Task IDs exist only in the current process, and callers need the provider acknowledgement or failure rather than command-queue admission.
  Date/Author: 2026-09-09 / Codex
- Decision: Keep the existing Claude `background_tasks_changed` signal authoritative for task existence and use AIR only to attach stop capability.
  Rationale: AIR excludes some Claude work, while the existing level signal already drives truthful activity. Joining the two avoids hiding read-only tasks.
  Date/Author: 2026-09-09 / Codex
- Decision: Do not remove a row when Stop is clicked.
  Rationale: A successful response means the stop request was accepted; the existing terminal-close or provider-level signal remains authoritative for actual exit.
  Date/Author: 2026-09-09 / Codex

## Outcomes & Retrospective

The implementation now supports targeted Stop controls for Claude AIR tasks and Mjolnir-hosted ACP terminals, which covers Kimi's managed shell path. Both the conversation TUI and authenticated web viewer retain a `Stopping…` state until the authoritative task snapshot removes the row, restore the control on failure, and keep Codex rows visibly read-only.

The protocol/state, ACP metadata and wire shapes, TUI interactions, web projection/API, and browser state have automated coverage. `cargo clippy --all-targets -- -D warnings` and all 24 web unit tests pass. The repository-wide Rust run passed the chat, client, controller, core, TUI, CLI unit, import, logging, and store-divergence suites; its final PTY binary was flaky under concurrent fixture load, and every failed PTY case subsequently passed alone. Live credentialed Claude/Kimi sessions were not exercised in this workspace, so provider-package compatibility is based on the pinned adapter source and the exact serialized wire contract rather than a paid live session.

Codex remains the one provider limitation: its rows have stable IDs but `can_stop: false` because the adapter has no ownership-safe targeted control. The upstream request is https://github.com/agentclientprotocol/codex-acp/issues/492.

## Context and Orientation

`src/hel_worker/snapshot.rs` defines the relay wire shapes, including `BackgroundCommand` and `RelayOperationalState`. `src/hel_worker.rs` owns the deterministic relay and reduces live terminals, Claude task levels, and Codex exec cards into that common task list. `src/hel_worker/protocol.rs` defines connection-only controller-to-worker requests. `mj-controller/src/hel_worker_client.rs` and `mj-client/src/session.rs` carry those requests to UI-independent session handles.

`src/hel_acp.rs` owns the ACP bridge and `TerminalRegistry`. It currently consumes standard typed `session/update` notifications and Claude's raw SDK background-task level. `mj-worker/src/hel_worker_runtime/unix.rs` folds bridge events into the relay and supplies the live connection-only services.

The conversation TUI task dialog is in `mj-chat/src/hel_chat/active.rs`, with state and remote operations in adjacent `hel_chat` modules. The authenticated web API and public snapshot DTOs are in `mj-controller/src/hel_server.rs`; `mj-cli/src/server.rs` connects that API to live session handles; the browser is implemented under `mj-controller/src/web/`.

## Plan of Work

Raise `RELAY_PROTOCOL_VERSION` to 9. Extend `BackgroundCommand` with an opaque `id` and `can_stop`, defaulting both for compatibility. Give hosted terminals, Claude tasks, and Codex exec cards stable namespaced IDs. Hosted terminals are stoppable whenever they are projected as background work. Claude task rows become stoppable only while the adapter has announced the same ID with `canStop: true`. Codex rows remain read-only.

Add `RelayRequest::StopBackgroundTask { background_task_id }` with protocol minimum 9 and a matching `BackgroundTaskStopRequested` response. The worker must look up the ID in its current background projection, reject missing or read-only rows, resolve the private target, then send a connection-only `CommandRequest` carrying a oneshot response. Extend the session client interfaces with `stop_background_task`, including test fakes.

For Claude, advertise `_meta.jetbrains.air = { version: 1, capabilities: ["asyncTasks"] }` and merge `perTaskStopAffordance: true` into the existing Claude session options. Replace the strongly typed `session/update` notification boundary with a raw envelope. Deserialize ordinary updates into the existing ACP `SessionUpdate` path; consume `async_task_spawned` and terminal `async_task_state_update` variants as process-local control events; ignore progress for relay state. Define the private `_session/async_task/stop` request and bound it to five seconds. A false response, unsupported request, or timeout must reach the caller as a failure. Hosted-terminal targets call `TerminalRegistry::kill`; normal terminal reaping remains responsible for output and removal. Stop requests must also be serviced while a foreground prompt is selected so one background task does not cancel or block the turn.

Add immediate Stop controls to the TUI task dialog. Only stoppable tasks get a button; mouse and Tab/Shift-Tab plus Enter activate it, while existing scrolling remains intact. The remote supervisor performs the request. The invoking row reads `Stopping…` until the operational snapshot removes it; a failure restores Stop and is shown to the user.

Project background tasks into `ViewerSession` and add an authenticated `POST /api/sessions/{session_id}/background-tasks/stop` endpoint accepting `{ "background_task_id": "…" }`. Route it through a dedicated bounded request path that awaits the live stop result off the HTTP and controller event loops. Return 202 on provider acknowledgement and safe 409, 503, or 500 errors for stale/read-only, unavailable, or failed requests. In the existing conversation work panel, render Background tasks beside queued prompts and user shells. A click acts without confirmation, remains deduplicated through rerenders, and reports failures in an inline alert. Read-only Codex rows say `Stop unavailable`.

Open an issue in `agentclientprotocol/codex-acp` titled “Expose targeted cancellation for background Codex tasks over ACP.” Report the pinned versions, demonstrate why `session/cancel` and `terminal/kill` cannot stop a Codex-owned detached exec, and request stable async task IDs, lifecycle notifications, `canStop`, and a targeted stop request compatible with AIR asyncTasks. Ask for an upstream Codex app-server primitive if the adapter lacks one. Record the URL here and in the final handoff.

## Concrete Steps

Run commands from `/home/jonathan/Projects/hel`. Implement and validate the relay/ACP/client foundation first, then integrate the independently owned TUI and web changes. Use focused package tests while iterating. Format and run the final checks outside the restricted test sandbox:

    cargo fmt
    cargo fmt --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    npm --prefix tests/e2e/web test

Create the upstream issue with `gh issue create --repo agentclientprotocol/codex-acp`, record the returned URL, stage only files changed for this work, commit coherent checkpoints on `master`, and push its configured upstream. Never stage `.agents/plans/restore-tui-workspaces-and-status.md`.

## Validation and Acceptance

Relay tests must prove stable IDs and stop capability for hosted terminals, AIR-announced Claude tasks, and read-only Codex cards; AIR/task-level arrival in either order; teardown cleanup; protocol-8 compatibility; and protocol-9 request validation. ACP tests must prove the initialize/session metadata, ordinary session updates after raw-envelope decoding, exact Claude stop request, false response, timeout, and terminal process-group stop. A terminal streaming more than 64 KiB must stop without pipe deadlock and report one close with readable output. A stop processed during a prompt must leave the prompt alive.

TUI tests must cover keyboard and mouse activation, read-only rows, scrolling, request deduplication, and failure recovery. Web unit and Playwright tests must cover the new projection, panel visibility, immediate no-confirm request, pending state across refreshes, safe stale-task errors, and snapshot-driven row removal.

In disposable Claude and Kimi sessions, start a long-running background shell, stop only that row from each surface, and observe the row disappear while the session and any foreground work remain alive. In a Codex session, observe the background row with `Stop unavailable`. The implementation is complete only when Rust tests, clippy, and web tests pass and the commits are pushed.

## Idempotence and Recovery

The source edits and tests are repeatable. A stop request races safely with natural task exit by returning a stale-task error and leaving existing close/state events authoritative. A timed-out provider request may still settle later; the next snapshot remains the source of truth. If the ACP bridge disappears, drop the oneshot with a reported unavailable error and clear process-local Claude stop capability. Do not delete terminal working files; process groups are always terminated before normal cleanup.

If upstream `master` advances after local commits, merge it without rebasing, resolve only genuine overlaps, rerun affected validation, and push. Preserve all unrelated working-tree files.

## Artifacts and Notes

Codex issue URL: https://github.com/agentclientprotocol/codex-acp/issues/492

## Interfaces and Dependencies

The final public shapes are `BackgroundCommand { id, started_at_ms, command, can_stop }`, `RelayRequest::StopBackgroundTask`, `RelayResponsePayload::BackgroundTaskStopRequested`, `SessionHandle::stop_background_task`, `ViewerBackgroundTask`, and `ViewerSession.background_tasks`. Provider routing uses a private enum distinguishing hosted terminal and Claude async-task targets. No new crate or external dependency is required.
