# Use AIR for Claude and Codex, and align what each uses

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir talks to Claude Code and Codex through ACP adapters (the Agent Client Protocol bridge processes `@agentclientprotocol/claude-agent-acp` and our fork `@brokkai/codex-acp`). Both adapters implement AIR, an optional set of ACP extensions that JetBrains defined and that a client turns on by listing capability names in `initialize.clientCapabilities._meta.jetbrains.air.capabilities`. AIR is separate from the ACP protocol version: Mjolnir stays on ACP version 1 and adds AIR features on top.

Before this work, Mjolnir used AIR unevenly and kept private workarounds for things AIR already provides. Claude's background-task list came from raw Claude SDK messages, with AIR used only for stop buttons. Codex had no task list at all: Mjolnir guessed running shells from tool cards that lacked an exit code. Claude did not ask for typed failures, so its quota errors were classified differently from Codex's. Codex goals used a private root-level `_meta.goal` copy of the AIR goal data. Claude prompts ended and Claude's self-started turns were tracked through raw SDK `result` messages.

After this work, both harnesses declare the shared AIR capabilities, with Claude additionally declaring `backgroundSubagents`, and Mjolnir uses AIR for each of those features. A user sees the same background-task list and stop controls for Claude and Codex, including Codex shells left running. Turn failures and quota stops are typed records for both harnesses. Goals use the AIR goal metadata for both harnesses. Claude prompts end when the adapter answers them, and Claude's own turns are AIR turn tasks.

## Progress

- [x] (2026-10-27) Mapped the AIR capabilities of both adapters, the fork's private patches, and Mjolnir's extension use (reports under `.mj/agents/`, summarized in Context and Orientation).
- [x] (2026-10-27) Codex adapter: merged upstream 582e545 and moved goal metadata into AIR (`~/Projects/codex-acp` commits 8729423 and 006db4d, version 1.15.0, packed tarball, full adapter suite green).
- [x] (2026-10-27) Mjolnir milestone 1: AIR async tasks become the background-task list for both harnesses (db3c7ab3e; see also `.agents/plans/air-background-tasks.md`).
- [x] (2026-10-27) Mjolnir milestone 2: same AIR capability list for both; typed failures for Claude; AIR-only goals (4a841b2e5).
- [x] (2026-10-27) Milestone 3a implementation: Claude declares `backgroundSubagents`, warns visibly when it is missing, and completes foreground prompts only at real PromptResponse boundaries. Removed ReleasePrompt, detached held replies, result stamps, and prompt-stop heuristics. The relay holds early root AIR spawns until the foreground boundary, discards starts terminated before that boundary, and preserves child work.
- [x] (2026-10-27) Milestone 3a validation and commit: 13 fake-adapter boundary tests pass; full core suite (550 unit + 3 integration), worker suite (714 passes + the known Kimi bridge-reload timeout, which passed alone after the suite), 19 worker integration tests, and doc tests completed. Workspace clippy excluding desktop and fmt pass. Initial plan-fixture capability mismatches and a stale test comment were fixed. Logs and live acceptance cases are in `.mj/agents/83ac380845ed73cb85c5de5febe54bb9/`; the parent owns live acceptance.
- [x] (2026-10-27) Milestone 3a live checks on Claude 0.97.0 / 2.1.308 in an isolated instance driven through the TUI in tmux: no missing-capability warning; a prompt that started a background Agent ended at the adapter's answer (about 9 s) while the child ran, and Claude's later turns opened and closed; Esc during a running foreground command printed Interrupted and killed it; two background commands finishing together produced one follow-up turn and a prompt typed during it ran once afterwards; a steer reached the running prompt; a background child's permission requests arrived after the parent prompt ended and could be approved and denied (the denied write did not happen); an approved plan implemented and settled.
- [x] (2026-10-27) Upstream fix for claude-agent-acp#1453 opened as https://github.com/agentclientprotocol/claude-agent-acp/pull/1455 (prepared here; published by session e8566ee4 with the user's approval, rebased on 0.98.0). Milestone 3b starts once a release contains it; until then Mjolnir keeps raw-result settlement and the silent-turn-only task-end rule, and does not vendor the bridge.
- [ ] Milestone 3b: end output-bearing autonomous turns from AIR terminals and drop the raw SDK subscription, blocked on claude-agent-acp#1453. Preserve current raw autonomous results, placeholder protection, silent-task terminal handling, output inference and stop semantics until then. The older complete milestone 3 experiment remains in the stash as reference only; it has not been applied or dropped.
- [x] (2026-10-27) Live checks in an isolated instance, driven through the TUI in tmux. Codex (managed 1.14.1): a running `exec_command` appeared as an AIR task with a Stop control, and Stop ended the process without opening a turn. Claude 0.94.0: a background Bash task listed from AIR, Stop removed it without a turn, and a task that finished on its own produced Claude's follow-up turn and returned to Idle. Codex 1.15.0 (published, pinned): a `/goal` run left the relay with the AIR goal capability (set, pause, resume, clear) and a `complete` snapshot.
- [x] (2026-10-27) Published `@brokkai/codex-acp` 1.15.0 (tag v1.15.0, npm provenance; release tarball identical to the gate-tested one), pinned it in 14c567dfa, merged origin/master (including the Claude 0.97.0 bump 87a322241 in 3ef8c29cb), pushed master 0a47952e5, and installed it locally with `scripts/install.sh`; the daemon handed off to that build.

## Surprises & Discoveries

- Observation: claude-agent-acp 0.97.0 can end an autonomous AIR turn task early. A late SDK idle from the previous turn completes the next turn's task, because the idle handler ignores the adapter's own owed-idle count. Another session found it in an adversarial review and filed it upstream.
  Evidence: https://github.com/agentclientprotocol/claude-agent-acp/issues/1453 (`src/acp-agent.ts` idle handler at v0.97.0).

- Observation (milestone 3, stashed): the notification handler can deliver an AIR spawn before the preceding prompt loop delivers PromptFinished. The relay must restore adapter order by holding the start, not change persisted projection guards. A task terminal before PromptFinished absorbs that held work into the foreground turn.

- Observation (milestone 3, stashed): RuntimeEvent payloads are read from historical WorkerEvent::Adapter records by transcript and chat readers. The old ClaudeTurnResult variant therefore remains as an opaque JSON payload solely for decoding; its parser and all live production/dispatch are removed. The new optional AIR task classification/state fields default when decoding old records.
- Observation (milestone 3, stashed): a foreground PromptFinished may be processed after the next root AIR spawn. The relay now holds that spawn until the preceding prompt finishes, then journals the autonomous start before claiming queued work.
- Observation: AIR spawn duplicates for ordinary background tasks can refresh their description and stop affordance. Only turn spawns/tombstones are inert duplicates; making all duplicate spawns inert broke the existing inventory boundary regression.

- Observation: no adapter recognizes `nativeSubagentAvailability`, which Mjolnir listed among its AIR capabilities. The availability query it guards is a Mjolnir proposal that neither adapter implements.
  Evidence: no match in `~/Projects/codex-acp` (upstream or fork) or in claude-agent-acp 0.94.0 and 0.97.0 `dist/`.
- Observation: the only fork patch that duplicated AIR was the root-level goal mirror. Goal recovery guarantees, execution state, idle-steering policy and native child cancellation have no AIR equivalent, so the fork keeps them.
  Evidence: `.mj/agents/201fe07edf79510ce4da68aa0eda19eb/fork-classification.md`.
- Observation: Claude's AIR mode does not offer native subagent cancellation; it grants `capabilities: {}`. Only the non-AIR subagent form grants `cancel`. Mjolnir's `_session/subagent/cancel` therefore works only with the Codex fork.
  Evidence: claude-agent-acp 0.97.0 `dist/native-subagents.js:1267-1336`.

## Decision Log

- Decision (milestone 3a): split foreground response alignment from autonomous turn completion. Claude alone adds `backgroundSubagents` to the shared AIR list; its real prompt response owns foreground stop reason, usage and typed failure. Warn with claude-agent-acp 0.95.0 minimum and worker/container upgrade guidance when negotiation is absent, without a raw-result fallback. Keep raw results exclusively for autonomous completion, including the current zero-turn and premature-task-end protections. The relay's existing session/task registry holds a live root spawn received while a prompt is active; prompt terminal handling promotes it before any queue claim under the same owner lock, while an earlier terminal discards it. Restart/close clear the registry; no persisted observation gains fields. Child output and requests remain live after foreground completion.
  Rationale: `backgroundSubagents` solves foreground reply holding independently of #1453. The adapter emits autonomous spawns only after answering its prompt, but separate ACP tasks can deliver those events to the relay in reverse order. Restore that order where classification is owned instead of changing transcript guards or narrowing the race. Milestone 3b remains blocked on #1453; earlier stashed decisions below describe that broader, unapplied experiment.
  Date/Author: 2026-10-27, parent decision.


- Decision (milestone 3, stashed): keep the persisted HarnessTurnStarted shape and all Codex projection guards unchanged. The relay task registry holds a root AIR spawn received while a user prompt remains active. Prompt completion, rejection or interruption promotes that pending start immediately after the prompt boundary in the same serialized operation. A prior task terminal discards the held start; restart/close clear the registry. Stops can capture the held task ID. The same turn-controls decision excludes automatic/explicit steering into the preceding prompt while its response is in transit.
  Rationale: Claude 0.97 starts autonomous turns only when no prompt is active or queued, and activation closes the autonomous task first (acp-agent.js:3193-3199,4252-4263). A spawn seen with an active relay prompt therefore means its response is already in transit between ACP tasks. Restoring adapter order at the relay owner preserves transcript/recovery boundaries without a persisted contract change or changed Codex semantics.
  Date/Author: 2026-10-27, parent decision.

- Decision (milestone 3, stashed): the relay background-task registry owns AIR turn classification and terminal tombstones, rather than a second stateful decoder in ACP ingress. The key is owning session plus task ID, and live root entries retain the matching durable turn-start ordinal until terminal. Captured stop targets use the existing background stop transport through a connection-supervised task.
  Rationale: inventory, turn state, checkpoint admission and prompt claims are serialized at the relay; progress/terminal updates lack taskType, and old-task terminals must not settle a newer turn. Native-session-used evidence is emitted at the live root spawn before forwarding the task event.
  Date/Author: 2026-10-27, Codex.

- Decision: AIR goal metadata is the only goal namespace. The fork-only goal fields (`resumePolicies`, `resumePolicy`, `limitReason`, `expectedGoal`) become extra fields on the AIR goal objects.
  Rationale: the user asked to remove fork patches that duplicate AIR. AIR has no pre-resume pause or limit reason, so these fields stay but move into the namespace that already carries the goal.
  Date/Author: 2026-10-27, Claude.
- Decision: `mj_core::goal::goal_meta` keeps reading root `_meta.goal` after the AIR namespace, only for stored records.
  Rationale: relay journals and materialized sessions written with Codex fork 1.13.x or Claude 0.81 and earlier carry root-only goal snapshots, and Mjolnir rebuilds state from them. No current adapter sends the root spelling, and Mjolnir no longer sends or checks it on the live connection.
  Date/Author: 2026-10-27, Claude.
- Decision: AIR's visible task set is authoritative for both harnesses. Claude Monitor tasks and ambient tasks that the adapter hides are no longer listed.
  Rationale: the user asked to move to AIR. Keeping the raw SDK level only for these would keep the workaround.
  Date/Author: 2026-10-27, Claude.
- Decision: bump Claude to 0.97.0 and replace raw SDK `result` handling with AIR `backgroundSubagents` and `taskType: "turn"` tasks.
  Rationale: the user chose this over staying on 0.94.0.
  Date/Author: 2026-10-27, user.
- Decision (milestone 3, stashed): Claude 0.97 turn model. Pin claude-agent-acp 0.97.0 with Claude Code 2.1.310 and launch it with `CLAUDE_AGENT_ACP_PROTOCOL=v1`. Claude additionally declares `backgroundSubagents`. The real `session/prompt` answer ends a foreground prompt; `ReleasePrompt`, held-reply detaching, result stamps, the `ClaudeTurnResult` parser and the `_meta.claudeCode.emitRawSDKMessages` subscription are removed. A `taskType: "turn"` task's spawn starts the relay's harness-initiated turn and its terminal state ends it. Terminal and progress updates omit `taskType`, so the classification is remembered by owning session and task id from the spawn. Output with no prompt no longer starts a turn, so the stop-acknowledgement chunk pairing and the five-second synthetic idle are removed. A live root turn spawn replaces the raw result as evidence that the native session was used. Stopping an autonomous turn sends `_session/async_task/stop` for its task id; a false answer or an error is reported and never falls back to `session/cancel`. Active user prompts still use `session/cancel`. Automatic steering stays limited to active user prompts, and Mjolnir keeps queued prompts until the autonomous turn ends. A Claude adapter that does not advertise `backgroundSubagents` gets a visible warning naming the required version; Mjolnir does not fall back to raw results.
  Rationale: design study `.agents/docs/claude-097-air-turn-design.md`. In 0.97, `session/load` still waits for replay (`dist/acp-agent.js:1476-1523`), so the load answer stays the history boundary for both harnesses; `loadReplayEnd` would add a Claude-only path without changing what Mjolnir accepts, because session metadata already passes the boundary and no conversation content is live before the answer. AIR terminal updates omit `taskType` (`dist/async-tasks.js:315-360`).
  Date/Author: 2026-10-27, Claude.
- Decision: keep native child cancellation, execution state, steering idle policy, guardian permissions, token usage, DeepSeek ordering, model labels, MCP approval policy and asynchronous questions as fork patches.
  Rationale: AIR provides none of them.
  Date/Author: 2026-10-27, Claude.

- Decision: defer milestone 3 and leave the Claude 0.97.0 bump to session e8566ee4.
  Rationale: that session already has the bump, a turn-start improvement that opens Claude turns at the AIR turn-task spawn, and live evidence. Its design keeps raw SDK results for turn ends because of claude-agent-acp#1453, which would make this plan's replacement close turns early. The user chose this option.
  Date/Author: 2026-10-27, user.

## Outcomes & Retrospective

Claude and Codex declare the shared AIR capabilities (asyncTasks, nativeSubagentSessions, sessionFailure), and Mjolnir consumes each of them the same way for both harnesses. Claude additionally declares backgroundSubagents for foreground response boundaries. AIR task updates are the only background-task list for both: Claude's raw SDK task level and Codex's exec-card inference are gone, Codex shell tasks became stoppable, and tasks in native child sessions are listed and stopped through their own session. Claude typed failures, including quota stops, go through the shared decoder. Goals use only the AIR goal namespace; the Codex fork's root `_meta.goal` copy was removed in 1.15.0, and its remaining goal extensions travel as extra fields on the AIR goal objects. The spurious `nativeSubagentAvailability` AIR declaration was dropped.

Milestone 3a now ends Claude foreground prompts at PromptResponse with AIR `backgroundSubagents`, preserving background children and restoring spawn/response order in the relay. Remaining: milestone 3b (ending output-bearing autonomous turns from AIR task terminals and dropping raw results) waits for claude-agent-acp#1453. The broader earlier experiment is in this clone's untouched stash and the design in `.agents/docs/claude-097-air-turn-design.md`. The Codex fork keeps patches for things AIR does not cover (goal recovery guarantees, execution state, idle steering policy, native child cancellation, guardian permissions, token usage, DeepSeek ordering, model labels, MCP approval policy, asynchronous questions).

Lessons: a first adapter-level release gate failed only because its harness exposed its own `exec` tool; an A/B run of the previous release with Mjolnir's launch settings separated harness artifacts from regressions. Another session was bumping the same adapter at the same time; checking memory notes and session history before continuing a large change found the overlap and a real upstream bug (#1453) that the plan had not considered.

## Context and Orientation

The ACP bridge code lives in `mj-worker/src/acp/`. `session.rs` sends `initialize` and runs the prompt loop. `launch.rs` builds the `session/new`, `session/load` and `session/resume` requests. `drive.rs` routes adapter notifications. `native_agents.rs` routes native subagent sessions. `claude_tasks.rs` held the Claude task decoder and the stop request. The relay (`mj-worker/src/relay.rs` and `mj-worker/src/relay/`) is the worker's durable state machine; `relay/background.rs` keeps the background-task list and resolves stop targets. `mj-worker/src/worker_runtime/unix.rs` chooses per-harness policies, and `worker_runtime/unix/dispatch.rs` applies runtime events to the relay. Goals are interpreted in `mj-core/src/goal.rs` and `mj-worker/src/acp/goal.rs`. Harness versions are pinned in `mj-core/src/harness_runtime.rs`, `mj-core/assets/harnesses/`, and `containers/Containerfile.agent-dev`.

The AIR features this plan uses:

`asyncTasks`: the adapter sends `session/update` notifications `async_task_spawned` (with `asyncTaskId`, `name`, `taskType`, `canStop`), `async_task_progress`, and `async_task_state_update` (`running`, `paused`, `completed`, `failed`, `stopped`). The client stops a task with the request `_session/async_task/stop {sessionId, asyncTaskId}`, which answers `{stopped}`. Tasks started in a native child session are published on that child's session id. Codex publishes background shells (`taskType: "shell"`). Claude publishes background shells, workflows and other non-agent tasks, and from 0.95 also its own turns as `taskType: "turn"`.

`sessionFailure`: a failed turn ends with `stopReason: end_turn` and a typed record at `PromptResponse._meta.jetbrains.air.sessionFailure`; warnings arrive in `session_info_update`.

`nativeSubagentSessions`: native child sessions announced with `subagent_spawned` and `subagent_state_update`.

AIR goal: the adapter advertises `initialize._meta.jetbrains.air.goal`, accepts `_session/goal`, and sends snapshots at `session_info_update._meta.jetbrains.air.goal`. Codex supports set, pause, resume and clear; Claude supports set and clear.

`backgroundSubagents` (Claude 0.95+): the adapter answers `session/prompt` at the model result even while background subagents run, instead of holding the answer.

## Plan of Work

The Codex adapter change is done (see Progress). Mjolnir work is in three milestones.

Milestone 1 makes AIR task updates the single source for agent-owned background work. One relay policy replaces the Claude-specific and Codex-specific policies. Task updates from child sessions are kept, tagged with the child session. Stopping sends `_session/async_task/stop` to the session that owns the task. The raw SDK `background_tasks_changed` subscription and the Codex exec-card inference are deleted. Claude-specific consequences, such as a completed task being followed by a Claude turn, stay tied to the Claude turn policy.

Milestone 2 declares `["asyncTasks", "nativeSubagentSessions", "sessionFailure"]` for both harnesses and drops `nativeSubagentAvailability` from the AIR list. Quota classification of typed failures applies to Claude as well as Codex. Goal reading and the resume policy use only the AIR goal namespace, and goal snapshots at session open are ingested for any adapter that advertises AIR goals.

Milestone 3a uses the merged Claude 0.97.0 pin, declares `backgroundSubagents`, and ends foreground prompts at the adapter's answer. The relay restores notification/response order before claiming queued work. Milestone 3b will replace raw autonomous results with AIR terminals and remove the raw subscription after claude-agent-acp#1453 is fixed. The detailed decisions are recorded above.

## Concrete Steps

Adapter work runs in `~/Projects/codex-acp`: `npm ci`, `npm run typecheck`, `npm test`, `npm run build`, `npm pack`. Mjolnir work runs in the repository root: `cargo test -p brokk-mj-worker <filter>`, `cargo test -p brokk-mj-core <filter>`, then the full suites of touched crates and `cargo clippy --all-targets -- -D warnings` once at the end.

## Validation and Acceptance

Automated: the fake-adapter tests in `mj-worker/src/acp/tests.rs` drive Claude-shaped and Codex-shaped AIR task updates, root and child, through a stop round trip; typed-failure tests cover Claude quota records; goal tests use the AIR shapes. Live: in an isolated instance (`--instance air-alignment`), a Codex session that starts `sleep 600` in the background shows the task and stops it from the task dialog; a Claude session shows its background task and stops it; a Claude prompt that starts a background subagent ends when the model finishes, and Claude's follow-up turn after the subagent finishes shows as a running turn and then settles.

## Idempotence and Recovery

Adapter publication is a one-way step: never move a published tag. If a check fails after tagging, publish a new patch version. Mjolnir changes are ordinary commits.

## Artifacts and Notes

Investigation reports: `.mj/agents/4093ade733c5d0208959cb02e7f1a069/air-matrix.md`, `.mj/agents/201fe07edf79510ce4da68aa0eda19eb/fork-classification.md`, `.mj/agents/5804624f1708f9e5c23d4c6b04c931ab/mj-extension-map.md`, `.mj/agents/814bd66f7ef85ac2b74a94c3cbb229fd/claude-air.md`. These directories are temporary; the facts this plan relies on are restated above.

## Interfaces and Dependencies

`@brokkai/codex-acp` 1.15.0 and `@agentclientprotocol/claude-agent-acp` 0.97.0. The AIR capability list is built once in `mj-worker/src/acp/session.rs`. `mj_core::relay::BackgroundTaskStopTarget` gains a harness-neutral async-task variant carrying the owning session id and task id.

2026-10-27 milestone 3 update: implemented the approved Claude 0.97 AIR lifecycle and converted its regressions. Validation remains in progress; the load response remains the history boundary.
