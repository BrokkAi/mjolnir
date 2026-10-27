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

The adapter contract below was observed on
`@agentclientprotocol/claude-agent-acp@0.81.0` with Claude Code 2.1.280. The
current bridge pin is 0.86.0; that version drops prompts already queued inside
the adapter when `session/cancel` arrives. Hel therefore keeps mid-cycle
prompts in its own durable queue until the turn settles. The remaining details
are **observed behaviour, not a documented protocol**, so Hel treats them as a
per-harness contract.

- The adapter streams a cycle through ordinary `session/update` notifications.
- It queues a `session/prompt` that arrives during an autonomous cycle and
  would ordinarily answer it at the next boundary. Since 0.86.0, cancelling
  that cycle discards prompts in this adapter queue; Hel must not dispatch a
  queued user prompt there before the cycle ends.
- Claude Code ends every model cycle with one SDK `result` message. New and
  resumed sessions ask for it through `_meta.claudeCode.emitRawSDKMessages`
  (`{"type":"result"}`), and the adapter
  forwards it as `_claude/sdkMessage` before it does anything else with it.
  The result names its origin: `human` (or no origin, on older Claude Code)
  for a cycle that answered the user, and `task-notification`, `peer`,
  `coordinator`, `observer`, or `observer-activity` for a cycle Claude Code
  started itself.
- The adapter also sends a `usage_update` carrying `cost` and
  `_meta["_claude/origin"] = { kind: ... }` after most results, but it leaves
  that marker out when the cycle produced no assistant usage. Hel no longer
  reads the marker as a turn boundary.
- The adapter answers `session/prompt` when it decides the prompt is over, and
  it holds that reply while background subagents the prompt started are still
  running, and after a steer until Claude Code reports `idle`, which it does
  only once all background work has drained. There is no option to turn the
  hold off.

## Where a prompt ends

`mj-worker/src/acp/drive.rs` parses each result into
`mj_core::acp::ClaudeTurnResult` (`mj-core/src/acp/claude_result.rs`), stamps
it with its arrival position on the connection, and sends it as
`RuntimeEvent::ClaudeTurnResult` on the same ordered stream as the session
updates. The relay coordinator (`mj-worker/src/worker_runtime/unix/dispatch.rs`)
therefore sees a result only after everything the adapter sent before it.

When a prompt is in flight and `ClaudeTurnResult::prompt_stop_reason` gives a
stop reason, the coordinator sends `CommandRequest::ReleasePrompt` to the
prompt loop in `mj-worker/src/acp/session.rs`. The loop ends the prompt with
that stop reason and the result's usage, unless a cancel is in flight, an
approved plan is being handed to its continuation prompt, or the result
arrived before the current `session/prompt` was sent. It then keeps the
adapter's reply alive in a task owned by the connection and discards it when
it comes: dropping the reply future would make the ACP crate send
`$/cancel_request`, which the adapter treats as a cancel of the live turn. The
loop, not the coordinator, emits `PromptFinished`, so a prompt completes once
whichever of the result and the reply reaches it first.

A result gives no stop reason, and the adapter's reply still ends the prompt,
when:

- its origin is not `human` (a background cycle);
- `queued_turn_count` is above zero, meaning another user cycle follows, which
  is how the cycle a steer interrupted ends;
- it is an interruption report: `is_error` with an `[ede_diagnostic]` text
  containing `result_type=user`, which Claude Code sends for a cycle that a
  steer, a plan answer, or a cancel interrupted;
- the cycle made no model call (`num_turns == 0`, a local command such as
  `/context`) or, on a success, produced no output tokens (a replayed answer).
  The adapter sends the text of such a cycle after the result and before its
  reply, and never holds that reply;
- it is a failure (`is_error`, other than `max_tokens`) or a sign-in failure.
  The adapter fails the prompt at once, and its error carries what the
  worker's failure handling and credential recovery read;
- it is a refusal (`stop_reason: "refusal"`, the safety classifier blocked
  the reply). The adapter sends the classifier's explanation as agent text
  after the result and before its reply, so the reply ends the prompt with
  that text inside the turn.

The stop reason follows the adapter's own mapping (`max_tokens`, `end_turn`,
`max_turn_requests`), spelled as the reply's is recorded (`MaxTokens`,
`EndTurn`, `MaxTurnRequests`).

## What Hel records

`HarnessTurnPolicy` (`mj-worker/src/relay.rs`) selects this behaviour. The
runtime sets it to `ClaudeAdapter` for `HarnessKind::Claude` right after
`DurableRelay::open` (`mj-worker/src/worker_runtime/unix.rs`); Codex gets
`CodexAdapter`, whose turns follow its native goal state; every other harness,
and the reviewer's sidecar relay, stays `Disabled` and behaves exactly as it
did before harness turns existed.

Under `ClaudeAdapter`, `DurableRelay::record_session_update` journals two new
observations:

- `harness_turn_started { started_at_ms }`, appended **before** an agent-output
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
  `DurableRelay::claude_turn_result` when a result that the prompt loop was not
  asked to take arrives while a turn is open. Any result settles the turn,
  whatever started its cycle. `origin` is the result's origin (`human` when
  absent) and is kept for diagnostics only.

Both are transcript events on the ordinary journal, so a controller replaying
the journal reaches the same state.

## Clearing rules

A harness turn ends on any of:

1. a Claude Code result that does not end a prompt;
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
- **Replies are still awaited in a few cases.** Where Hel leaves the end of a
  prompt to the adapter's reply and the adapter holds that reply because
  background subagents run, the prompt still waits for them: a "keep planning"
  answer to a plan review, the planning part of a plan hand-off (the loop sends
  the continuation prompt only after the planning reply), and any cycle on a
  backend that omits token counts.
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
