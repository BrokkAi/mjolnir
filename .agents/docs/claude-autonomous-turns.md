# Turns Claude Code starts on its own

Claude Code re-invokes itself. When a background command it started finishes,
its adapter opens a new SDK turn and works through it with no `session/prompt`
open on Hel's side. The adapter calls these "autonomous cycles" and reports
five origins for them: `task-notification`, `peer`, `coordinator`, `observer`,
and `observer-activity`. A prompt that arrives during one is queued, not
dropped: the adapter's `prompt` handler pushes it onto its turn queue and its
SDK input stream, and answers it at the next turn boundary.

Hel models such a cycle as a **harness-initiated turn** in the relay state
machine, so everything that already keys on execution state — the UI, the
checkpoint barrier, the recovery boundary — is right without further changes.

## The adapter contract

Checked on `@agentclientprotocol/claude-agent-acp@0.81.0` with Claude Code
2.1.280, the versions pinned in `mj-core/src/harness_runtime.rs`. It is
**observed behaviour, not a documented protocol**, so Hel treats it as a
per-harness contract and degrades to its previous behaviour wherever it is
absent.

- The adapter streams a cycle through ordinary `session/update` notifications.
- Claude Code ends every model cycle with one SDK `result` message. New and
  resumed sessions ask for it through `_meta.claudeCode.emitRawSDKMessages`
  (`{"type":"result"}`, beside the background-task level), and the adapter
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
  opens a turn.
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
4. `Closing` or `Closed`.

Clearing a turn returns execution to `Idle` when no prompt is active.
`SessionRestarted` became a state-changing observation for this reason, so it
is applied through a staged snapshot rather than as a frontier-only append;
`observation_changes_state` and `apply_relay_event` still mirror each other,
and `transcript_observations_move_nothing_but_the_frontier` enforces that.

Both places that record `SessionRestarted` do so with no prompt in flight (the
worker interrupts in-flight commands first), so the projection's restart arm —
which cannot see `active_prompt` — closes streams and goes idle whenever the
session was running.

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
- A queued checkpoint barrier waits for an open harness turn exactly as it waits
  for a prompt. A **prompt** does not wait: `promote_next_queued_command` gates
  on `active_prompt`, not on execution state, so a prompt typed mid-cycle
  dispatches at once and the adapter queues it.
- Stop works during a Claude harness turn. The chat's Esc, the phone's
  interrupt, `CancelTurn`, and the older `Cancel` all reach the adapter as
  `session/cancel`, which interrupts the running cycle; that cycle's result
  then settles the turn. A Codex turn of this kind is a native goal and keeps
  its own controls, so the relay still refuses `Cancel` for it.

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
- `ClaudeTasks` (Claude). Includes hosted terminals plus the SDK's full live
  background-task list. New and resumed Claude sessions opt into the
  `system/background_tasks_changed` raw SDK message (and the `result` message
  described above) through `_meta.claudeCode.emitRawSDKMessages`. The adapter forwards it as
  `_claude/sdkMessage`; Hel excludes tasks marked `ambient` (SDK housekeeping),
  replaces its task map on each notification, and
  preserves the first observation time of IDs still present. An empty list
  clears the tasks. These updates do not open a foreground turn, advance the
  step clock, or enter the transcript. Prompt completion leaves the list
  intact; bridge teardown, restart, and close clear it. The SDK level is
  process-local and emits nothing at startup, so it must not be restored from
  transcript history. Task starts and completion bookends are deliberately
  not correlated with this level: their ordering is unspecified, and starts
  include foreground tasks. This fixes idle being displayed while Claude's
  own background agents are still running.

  A stop Hel requests for one of these tasks is acknowledged by the
  adapter with a plain `agent_message_chunk` whose text is
  `**Task stopped by user:** <name>.` and no origin marker: the SDK injects
  nothing into the model for a user-stopped shell task, so no turn runs.
  Only the prefix is stable. `<name>` is whatever the adapter calls the task
  at that moment, and a later `task_started` or level replaces it (a live
  check saw the level say `Sleep for the live check` and the acknowledgement
  quote the command `sleep 900`). The relay therefore records the task id as
  a pending stop when `background_task_stop_target` resolves a
  `ClaudeAsyncTask`, treats the next prefixed single-text chunk as its
  acknowledgement while any stop is pending, and does not open a harness turn
  for it; the chunk still enters the transcript. A stop that never reaches
  the adapter drops its pending entry (`claude_stop_not_sent`), and restart or
  close clears them all. Checked against claude-agent-acp 0.73.0
  `dist/async-tasks.js` (`taskStopped`, `mergeLevel`, `mergeStarted`) and
  live on 2026-09-14. Tasks that end on their own still re-invoke the model
  and open a turn as before.

  A task list that stays populated for hours is not by itself a relay bug.
  On 2026-09-14 a session reported six tasks alive for over five hours; each
  was a `until ! pgrep -f "<pattern>"; do sleep 10; done` loop whose pattern
  matched the loop's own command line, so it could never exit. The relay
  reported the SDK level accurately. The remedy is agent guidance, not a relay
  change.
- `CodexExecCards` (Codex). codex-acp runs its own shells and never calls
  `terminal/create`, so the only evidence is the tool card. An `exec_command`
  card carries its result under `rawOutput`, with `exit_code` parsed from
  Codex's structured result. A card whose `rawOutput` has an explicitly null
  `exit_code` is a process Codex's unified exec left running; the relay tracks it by
  tool call id and clears it when a later card for the same call reports an exit
  code, when the harness restarts, and when the session closes. ACP tool-call
  updates are partial, so the relay first remembers that the card was explicitly
  introduced with `kind: execute`. A later `rawOutput` without `kind` inherits
  that remembered identity. Raw output by itself is not execution evidence:
  Codex Guardian reviews and searches also return it. MCP calls can themselves
  have `kind: execute`; their `result`/`error` envelopes omit `exit_code` and
  must not be classified as background processes.

The Claude level contract was checked on 2026-09-06 against the installed
claude-agent-acp 0.73.0 `dist/acp-agent.js` (raw SDK forwarding and
`shouldEmitRawMessage`) and its SDK's `SDKBackgroundTasksChangedMessage` in
`sdk.d.ts`. The latter specifies replacement semantics, process-local reset,
unspecified ordering relative to edge events, and exclusion of `ambient`
tasks from activity indicators.

The completed-command shape and partial-update behavior were validated against
codex-acp 1.8.0 on 2026-09-04. Ordinary commands ended with
`rawOutput.exit_code: 0`. Guardian cards began as `kind: think`, then ended in a
partial update with raw output and no repeated kind; treating that final update
as an execute result produced dozens of false `BG` entries. The relay regression
tests cover both the correlated execute update and the non-execute negative
control. A live detached-process check is still needed to confirm Codex's null
exit-code and five-minute reap behavior; if the reap is confirmed, entries
should also age out at that bound.

On 2026-09-05, source review of the installed codex-acp 1.8.0
`dist/index.js` (`createCommandExecutionUpdate` and
`createCommandExecutionCompleteUpdate`) confirmed that command starts carry
their kind and command, while completion updates reuse `item.id`, omit kind,
and carry `exit_code: item.exitCode`. In-progress items produce no completion
update. This confirms partial-update correlation, but does not establish that
a null exit code proves detachment or that polling reuses the original item ID.
Those remain assumptions of the existing background-process tracking; no
timeout-based cleanup is justified by this source review.

Turn-boundary regression coverage also checks successful, rejected, and
interrupted prompts: stale foreground tool statuses clear, while a previously
tracked detached command survives and can still be cleared by its correlated
exit update.

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
  stop and handled (see `ClaudeTasks` above).
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
