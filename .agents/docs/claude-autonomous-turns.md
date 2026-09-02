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

Observed on `@agentclientprotocol/claude-agent-acp@0.73.0`, the version pinned
at `src/hel_controller/worker_binary.rs` (`CLAUDE_AGENT_ACP_FALLBACK_VERSION`).
It is **observed behaviour, not a documented protocol**, so Hel treats it as a
per-harness contract and degrades to its previous behaviour wherever it is
absent.

- The adapter streams a cycle through ordinary `session/update` notifications.
- It settles every SDK turn with a `usage_update` carrying `cost` and
  `_meta["_claude/origin"] = { kind: ... }`. The observed kinds are `human` on
  a turn a prompt drove and `task-notification` on a cycle the harness started
  itself.
- Hel treats **any** origin kind as a settle marker: the marker means an SDK
  turn ended, whatever began it.

## What Hel records

`HarnessTurnPolicy` (`src/hel_worker.rs`) selects this behaviour. The runtime
sets it to `ClaudeAdapter` for `HarnessKind::Claude` right after
`DurableRelay::open` (`src/hel_worker_runtime/unix.rs`); every other harness,
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
- `harness_turn_settled { origin }`, appended **after** a `usage_update`
  carrying the origin marker while a turn is open. `origin` is kept for
  diagnostics only.

Both are transcript events on the ordinary journal, so a controller replaying
the journal reaches the same state.

## Clearing rules

A harness turn ends on any of:

1. the settle marker;
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
  stable id `harness-turn:{ordinal}` (`src/hel_transcript.rs`).
- That item is a **turn start**, alongside a user message
  (`TranscriptItem::is_turn_start`). The recovery boundary,
  `latest_completed_turn_ordinal`, and the scope of a plan update all key on the
  newest turn start, so autonomous work is covered by the next recovery copy
  and a plan produced in a cycle does not overwrite the previous turn's plan.
  Three implementations must agree: `ProjectionWindow::of`, the position-only
  query `last_materialized_turn_start` in `src/hel_database.rs`, and the free
  function `latest_completed_turn_ordinal` in `src/hel_state.rs`.
- `RelayOperationalState` exposes `harness_turn` (open turns only) and
  `last_harness_turn_started_ordinal` (monotonic), so a checkpoint can tell
  whether a cycle began during its capture window.
- A queued checkpoint barrier waits for an open harness turn exactly as it waits
  for a prompt. A **prompt** does not wait: `promote_next_queued_command` gates
  on `active_prompt`, not on execution state, so a prompt typed mid-cycle
  dispatches at once and the adapter queues it.

## Background work the agent leaves running

`RelayOperationalState.background_commands` reports commands the agent started
and then stopped waiting on, oldest first. Two harness families produce that
evidence differently, and `BackgroundWorkPolicy` picks which one a relay reads
(set beside the harness-turn policy in `src/hel_worker_runtime/unix.rs`):

- `HostedTerminals` (Claude, Kimi, and every harness that is not Codex). Hel
  spawned the process, so `active_agent_terminals` is exact: an entry leaves the
  list the moment the child exits. A terminal only counts as background work
  while no prompt and no harness turn is open; until then it is the turn's own
  work.
- `CodexExecCards` (Codex). codex-acp runs its own shells and never calls
  `terminal/create`, so the only evidence is the tool card. An `exec_command`
  card carries its result under `rawOutput`, with `exit_code` parsed from
  Codex's structured result. A card whose `rawOutput` has no exit code (null or
  absent) is a process Codex's unified exec left running; the relay tracks it by
  tool call id and clears it when a later card for the same call reports an exit
  code, when the harness restarts, and when the session closes.

**The Codex card shape is unvalidated.** It is implemented from the description
above, not from a recorded session. Before trusting it, run one live Codex
session: ask it to start a long command and yield, confirm the `exec_command`
card's `rawOutput.exit_code` is null, check that the row reads `BG`, then ask it
to poll the process and confirm the row clears on the card that reports the
exit. Adjust `DurableRelay::track_codex_exec_card` if the shape differs. If
Codex's five-minute reap of unpolled processes is confirmed, entries should also
age out at that bound.

Every surface renders the same three states from one pair of helpers in
`src/usage_format.rs`: `format_activity_columns` for wide rows, the chat pane
title and the phone, and `format_activity_clock` for the minimized grid.
Running clocks read `43m36s`, not `00:43:36` (`format_clock`).

## Known limitations

- **A stray chunk with no marker leaves the session Running.** An agent chunk
  that arrives at idle and is never followed by a settling `usage_update` holds
  the turn open until a prompt result or a restart clears it. That is visible in
  the UI as a running session, and no recovery copies happen meanwhile — the
  same exposure as a prompt that never returns.
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
- **Cancelling a self-started turn is not supported.** ACP `session/cancel` is
  prompt-scoped, so the relay rejects `Cancel` while only a harness turn is
  open, and says so.
- **A periodic re-evaluation in the recovery coordinator is still missing.** It
  is event-driven, so a real failure on a session that then goes quiet is only
  retried at that session's next event.
- **Autonomous cycles are not reviewed.** The turn-review host starts a review
  on a prompt-driven turn only; reviewing self-started turns is a separate
  decision.
