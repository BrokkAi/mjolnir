# Turns Claude Code starts on its own

Claude Code re-invokes itself. When a background command it started finishes,
its adapter opens a new SDK turn and works through it with no `session/prompt`
open on Hel's side. The adapter calls these "autonomous cycles" and reports
five origins for them: `task-notification`, `peer`, `coordinator`, `observer`,
and `observer-activity`. A prompt that arrives during one is normally queued:
the adapter's `prompt` handler pushes it onto its turn queue and SDK input
stream, then answers it at the next boundary. Version 0.86.0 drops prompts
from that queue when cancellation arrives, so Mjolnir keeps them in its own
durable queue until the turn settles.

Hel models such a cycle as a **harness-initiated turn** in the relay state
machine, so everything that already keys on execution state — the UI, the
checkpoint barrier, the recovery boundary — is right without further changes.

## The adapter contract

The current bridge pin is `@agentclientprotocol/claude-agent-acp@0.99.0`
with Claude Code 2.1.308, using ACP v1. Both Claude and Codex declare AIR
`asyncTasks`, `nativeSubagentSessions`, and `sessionFailure`; Claude also
declares `backgroundSubagents`.

- The adapter streams cycles through ordinary `session/update` notifications.
- A live root `async_task_spawned` with `taskType: "turn"` announces a Claude
  autonomous cycle. Child-session tasks and replayed spawns open no root turn.
- The adapter queues prompts during autonomous work and cancellation can discard
  those prompts. Mjolnir keeps them in its durable queue until the turn settles.
- Claude Code ends every model cycle with a raw SDK `result`. New and resumed
  sessions still subscribe through `_meta.claudeCode.emitRawSDKMessages`.
  These results close autonomous turns, pending claude-agent-acp#1453, and
  never release or change an active foreground prompt.
- A successful zero-turn placeholder (`num_turns == 0`) does not close a turn
  owned by a live autonomous task. Coalesced task notifications can produce
  these placeholders before the real model answer.
- An AIR task terminal closes only a silent autonomous turn. Once output has
  appeared, the raw result owns the end: #1453 lets an idle owed to the previous
  cycle terminate a newer AIR turn task too soon.

## Where a prompt ends

The real `session/prompt` response ends every foreground Claude prompt, using
its stop reason, usage, and typed AIR `sessionFailure`. With
`backgroundSubagents` (claude-agent-acp 0.95.0 or later), the adapter answers
at its model result while native background agents keep running. Steering
and plan-mode transitions resolve inside the adapter before it answers.
Mjolnir awaits cancellation acknowledgement and completes approved-plan mode
restoration and continuation at real response boundaries as well.

If initialize does not advertise `backgroundSubagents`, Mjolnir emits a visible
warning naming the minimum version and asking the operator to upgrade the
container or worker. It continues using real responses; there is no raw-result
fallback. Old adapters can therefore still hold prompts for background work.

Prompt completion preserves child state and pending requests. Child output,
permissions, and elicitations remain serviceable with no active root prompt.
The foreground boundary does not mark children complete or drain their work.

The ACP notification stream can deliver the next autonomous spawn before the
prompt loop delivers the previous response's `PromptFinished`. The adapter only
starts autonomous work after answering its prompt. The relay restores that
order as the single owner of turn classification: its session/task registry
holds a live root spawn seen during an active prompt, then journals the new
harness turn immediately after the prompt terminal boundary, before queue
promotion, under the same serialized decision. A task terminal arriving first
discards the held start and opens nothing; its pending-boundary marker still
prevents steering until the prompt response is processed. Restart and close
clear the registry. Held starts also prevent steering into the prompt whose
response is already in transit. Persisted observation shapes are unchanged.

## What Hel records

`HarnessTurnPolicy` (`mj-worker/src/relay.rs`) selects this behaviour. The
runtime sets it to `ClaudeAdapter` for `HarnessKind::Claude` right after
`DurableRelay::open` (`mj-worker/src/worker_runtime/unix.rs`); Codex gets
`CodexAdapter`, whose turns follow its native goal state; every other harness,
and the reviewer's sidecar relay, stays `Disabled` and behaves exactly as it
did before harness turns existed.

Under `ClaudeAdapter`, `DurableRelay::record_session_update` journals two new
observations:

- `harness_turn_started { started_at_ms }`, appended at a live root AIR turn
  spawn (held until an active prompt ends, as above), or **before** an agent-output
  update (`AgentMessageChunk`, `AgentThoughtChunk`, `ToolCall`,
  `ToolCallUpdate`, `Plan`) that arrives with no active prompt and no turn
  already open, so the turn covers that output. Session bookkeeping —
  `usage_update`, `user_message_chunk`, `available_commands_update`,
  `current_mode_update`, `config_option_update`, `session_info_update` — never
  opens a turn. Neither does output that arrives while a `SetConfig` or
  `SetSessionMode` request is waiting for its answer: the adapter publishes
  some notices as agent text while it answers one (a switch to a model without
  Auto mode sends `**Auto mode unavailable:** …`), and no model cycle, so no
  result, follows them (launch finding R4-2). Nor does output that arrives
  while a new bridge sets its session up, from `agent_initialized` until
  `session_opened` (the load or new-session request and the model and mode
  applied after it): a resumed session on a model without Auto mode gets the
  same notice there (launch finding R8-4). Agent text in either window, with
  no prompt running and no turn open, is journaled as a `notice` row instead
  of a session update, so it is never joined to the previous reply (launch
  finding R8-5).
- `harness_turn_settled { origin }`, appended by
  `DurableRelay::claude_turn_result` when a result arrives with no active prompt while a harness turn is open.
  Successful zero-turn placeholders do not settle a live task-owned turn. `origin` is the result's origin (`human` when
  absent) and is kept for diagnostics only.

Both are transcript events on the ordinary journal, so a controller replaying
the journal reaches the same state.

## Clearing rules

A harness turn ends on any of:

1. a Claude Code result with no active prompt, except a successful zero-turn
   placeholder while a live autonomous task owns the turn; or the owning AIR
   task terminal when the turn is silent;
2. a prompt terminal outcome — `CommandCompleted` for a prompt, or the active
   prompt being rejected or interrupted — because a prompt result means the SDK
   reached a turn boundary;
3. `SessionRestarted`, because the control plane behind the cycle is gone;
4. `Closing` or `Closed`;
5. a stop (`Cancel` or `CancelTurn`) that Claude Code has not answered with a
   result within `HARNESS_TURN_STOP_GRACE` (5 s). A running cycle answers its
   interrupt with a result at once; a turn still open after that was opened
   for output no cycle produced, and nothing else would end it. The relay
   coordinator runs this timer and records `harness_turn_settled` with origin
   `stop_unanswered`.

Clearing a turn returns execution to `Idle` when no prompt is active.
`SessionRestarted` became a state-changing observation for this reason, so it
is applied through a staged snapshot rather than as a frontier-only append;
`observation_changes_state` and `apply_relay_event` still mirror each other,
and `transcript_observations_move_nothing_but_the_frontier` enforces that.

Both places that record `SessionRestarted` do so with no prompt in flight (the
worker interrupts in-flight commands first), so the projection's restart arm —
which cannot see `active_prompt` — closes streams and goes idle whenever the
session was running.

Each ending path also releases prompts held in Mjolnir's queue. A result,
prompt terminal outcome, or stop timeout is followed by the coordinator's
command-claim pass, which promotes the queue head once. A bridge exit or crash
records `SessionRestarted`; promotion waits until the replacement bridge sends
`SessionConfigured`, so no prompt reaches a dead or half-open adapter. A worker
restart recovers the journal, records the same restart boundary, and promotes
after session readiness. Close seals the relay behind a checkpoint: it does
not send queued prompts to the closing bridge, and the checkpoint retains them
for promotion after a later resume has configured its bridge.

## What downstream reads

- The transcript gets one system line, `Agent continued on its own`, with the
  stable id `harness-turn:{ordinal}` (`mj-core/src/transcript.rs`).
- That item is a **turn start**, alongside a user message
  (`TranscriptItem::is_turn_start`). The recovery boundary,
  `latest_completed_turn_ordinal`, and the scope of a plan update all key on the
  newest turn start, so autonomous work is covered by the next recovery copy
  and a plan produced in a cycle does not overwrite the previous turn's plan.
  Three implementations must agree: `ProjectionWindow::of`, the position-only
  query `last_materialized_turn_start` in
  `mj-controller/src/database/materialized.rs`, and the free function
  `latest_completed_turn_ordinal` in `mj-core/src/state.rs`.
- `RelayOperationalState` exposes `harness_turn` (open turns only) and
  `last_harness_turn_started_ordinal` (monotonic), so a checkpoint can tell
  whether a cycle began during its capture window.
- Prompt promotion and checkpoint admission use the same relay-owned turn
  boundary (`active_prompt` or `harness_turn`). Prompts and queued
  configuration changes stay in Mjolnir's FIFO queue during that turn. The
  adapter queues prompts internally and can discard them on cancel; it rejects
  config, mode, and clear-context requests while a prompt loop is active.
  Mjolnir starts each only after the turn ends. Native goal controls and turn
  controls have independent live-turn handlers; user shells run outside the
  ACP queue.
- Automatic steering remains limited to `active_prompt` turns. A steer cannot
  reach the model while a harness-initiated turn is blocked in a long tool
  call, so Esc cancels that cycle and the relay sends its queued prompt once
  the cycle settles. With no `active_prompt`, Esc uses non-targeted
  `CancelTurn`; targeted `CancelTurnFor` is only valid for a live prompt.
- Stop works during a Claude harness turn. The chat's Esc, the phone's
  interrupt, `CancelTurn`, and the older `Cancel` all reach the adapter as
  `session/cancel`, which interrupts the running cycle; that cycle's result
  then settles the turn, or clearing rule 5 does when no result comes. A
  Codex turn of this kind is a native goal and keeps its own controls, so the
  relay still refuses `Cancel` for it.

## Background work the agent leaves running

`RelayOperationalState.background_commands` reports commands the agent started
and then stopped waiting on, oldest first. Harness families produce that
evidence differently, and `BackgroundWorkPolicy` picks which one a relay reads
(set beside the harness-turn policy in `mj-worker/src/worker_runtime/unix.rs`):

- `HostedTerminals` (Kimi and other harnesses without a dedicated policy). Hel
  spawned the process, so `active_agent_terminals` is exact: an entry leaves the
  list the moment the child exits. A terminal only counts as background work
  while no prompt and no harness turn is open; until then it is the turn's own
  work.
- `AirTasks` (Claude and Codex). Includes hosted terminals plus the AIR
  adapter's visible background tasks. Both clients declare `asyncTasks` under
  initialize `_meta.jetbrains.air.capabilities`. `async_task_spawned` adds a
  session-owned task, progress can rename it, and completed, failed or stopped
  state updates remove it. Running and paused states retain it. `taskType: turn`
  represents an autonomous turn rather than a background command and is excluded.
  Adapter-hidden Monitor and ambient work does not enter this inventory.

  Tasks are keyed by owning ACP session and task id, so native child sessions
  share the inventory without colliding with root task ids. A task advertised
  with `canStop` resolves to `AirAsyncTask { session_id, task_id }`; stopping
  sends `_session/async_task/stop` to that exact session. Codex shell tasks are
  consequently stoppable. Tool cards remain transcript/output events and never
  establish background process existence.

  Prompt completion preserves this live inventory; harness teardown, restart,
  and close clear it. During session/load, Claude can replay historical spawn
  and terminal pairs, while Codex reannounces live commands. The relay processes
  those inventory updates inside the native-agent replay bracket without
  anticipating notification turns from historical completions.

  Only Claude's harness turn policy anticipates a task-notification turn after
  a live task completes or fails. Stopped tasks get no such turn. Claude still
  opts into raw SDK `result` notifications and `perTaskStopAffordance`; neither
  is replaced by AIR task inventory. Claude acknowledges requested stops with
  a plain `agent_message_chunk` starting `**Task stopped by user:** `, with no
  model cycle or result. The relay pairs those chunks with pending Claude
  stops so they cannot open a foreground turn. Codex uses no such pairing.

Turn-boundary regression coverage also checks successful, rejected, and
interrupted prompts: stale foreground tool statuses clear, while a previously
tracked AIR task survives until its terminal lifecycle update.

Validation on 2026-09-05 passed: `cargo fmt --all -- --check`, the full
`cargo test --quiet` suite, and `cargo clippy --all-targets -- -D warnings`.

Every surface renders the same three states from one pair of helpers in
`mj-client/src/usage_format.rs`: `format_activity_columns` for wide rows, the chat pane
title and the phone, and `format_activity_clock` for the minimized grid.
Running clocks read `43m36s`, not `00:43:36` (`format_clock`).

## Known limitations

- **Agent output with no result after it leaves the session Running.** Every
  model cycle ends with a result, so this now needs output that belongs to no
  cycle. The one such chunk Hel itself provokes, the `**Task stopped by user:**
  <name>.` acknowledgement of a stop it requested, is paired with the pending
  stop and handled (see `AirTasks` above).
- **A steer that lands just as a cycle ends can run as a harness turn.** If
  Claude Code finishes the cycle before it reads the steered message, the
  result reports no queued user message and ends the prompt, and the steered
  message then runs as a cycle of its own. It is recorded as a harness turn
  rather than as part of the prompt. The echo that would identify it is not
  forwarded by the adapter.
- **AIR task ends can arrive early (#1453).** Autonomous turns still use raw
  SDK results for output-bearing cycles. Replacing those result boundaries and
  dropping the raw subscription is milestone 3b and remains blocked upstream.
- **Grok goal mode has no marker.** It streams a whole autonomous turn as
  trailing chunks after a prompt completes and never settles it, so it keeps
  today's behaviour: the policy stays `Disabled` and the projection's idle-time
  coalescing path handles those chunks.
- **Kimi's follow-up turn is invisible to Hel.** Kimi injects a notification and
  starts a new agent turn when a background task finishes, but its ACP server
  forwards events only for a turn owned by an open prompt, and skips the settle
  `usage_update` for driverless turns. Enabling this policy for Kimi needs a
  fix in `kimi-code` first: forward driverless turns and emit the settle marker
  with an origin.
- **Codex does not re-invoke itself** when a background process finishes
  (openai/codex#29865), so it needs no harness turn.
- **A periodic re-evaluation in the recovery coordinator is still missing.** It
  is event-driven, so a real failure on a session that then goes quiet is only
  retried at that session's next event.
- **Autonomous cycles are not reviewed.** The turn-review host starts a review
  on a prompt-driven turn only; reviewing self-started turns is a separate
  decision.

On 2026-09-07, the completed `Merge master into 2771-pre-merge` session
showed six completed project-memory MCP calls with `kind: execute` and
`rawOutput` containing only `result` and `error`. Every shell execution had
a non-null exit code. Treating a missing exit field as null falsely kept BG
active after the turn. The relay now requires an explicit null field, with
regressions for both complete cards and partial updates through turn completion.

Validation for foreground response alignment (2026-10-27): all 13 Claude fake-adapter
boundary tests pass, including late child permission and both spawn/response
orderings. Full worker/core coverage, worker integrations, doc tests, workspace
clippy excluding desktop, and fmt completed. The known Kimi bridge-reload
runtime-event timeout failed under parallel load and passed alone after the
suite. Live acceptance remains with the parent session.
