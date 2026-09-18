# Report a prompt the harness ended without answering, instead of calling it finished

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan must be maintained in accordance with `.agents/PLANS.md` from the repository
root. That file is the canonical rules document for ExecPlans in this repository.

This plan answers GitHub issue BrokkAi/mjolnir#970, "A queued prompt dispatched during
agent compaction is lost silently".


## Purpose / Big Picture

Mjolnir (the `mj` command, this repository) drives a coding agent through the Agent
Client Protocol, called ACP: a JSON-RPC protocol in which Mjolnir sends `session/prompt`
and the agent streams back `session/update` notifications and finally a stop reason. A
person can type several prompts in a row; Mjolnir accepts them into a durable queue and
sends them to the agent one at a time.

Today, when the agent accepts a prompt and then ends the turn without doing anything
with it, Mjolnir reports that turn as finished. Automation (`mj wait`) returns
`finished`, the chat shows a user line with no answer under it, and nothing says the
prompt was never acted on. That is what issue #970 observed: a prompt sent while Claude
Code was compacting its context was swallowed, and the person only discovered it because
they re-typed the prompt by hand and got an answer the second time.

After this change, a prompt that the harness ended without producing any output of its
own is reported as not answered, on all three surfaces: `mj wait` returns an error
outcome naming it instead of `finished`, the terminal chat draws a line under the user's
prompt saying the harness ended the turn without answering and offers to put the text
back in the composer, and the web viewer shows the same. You can see it working with the
scripted test harness this plan adds: a fake agent that accepts a prompt and returns
`end_turn` with no updates, after which `mj wait` reports the failure instead of success.

The plan deliberately does **not** add the timer-driven "undelivered" state the issue
proposes. The evidence collected while writing this plan (see `Context and Orientation`)
says the specific window the issue describes has been closed upstream in the Claude
bridge version this repository pins, and that the part of the problem which is still
provably ours needs no clock at all: the turn's own completion is the bound. A
clock-driven hold and a resend workflow remain specified here as Milestone 3, behind an
explicit trigger condition, so they are done only if measurement shows they are needed.


## Progress

- [x] (2026-09-17) Read issue #970 and its two triage comments in full.
- [x] (2026-09-17) Established what is still true on master: the promotion gate, the
      existing empty-response detector and its weakness, and the `mj wait` behaviour.
- [x] (2026-09-17) Established harness behaviour from the pinned bridge sources rather
      than from guesswork, and recorded what remains unknown.
- [x] (2026-09-17) Wrote this plan. Phase 1 (plan only) ended here; the maintainer
      approved it with four decisions, recorded in the `Decision Log`.
- [x] (2026-09-18) Milestone 0: measured Claude and Codex live, on the pinned bridges,
      with a prompt queued behind `/compact`. Neither returned early and neither lost the
      queued prompt. Results and journal evidence are in `Surprises & Discoveries`.
- [x] (2026-09-18) Milestone 1: one shared answer to "did this turn produce anything",
      used by the worker, which now fails such a turn under the stop reason
      `prompt_unanswered` so `mj wait` reports a named error.
- [x] (2026-09-18) Milestone 2: the same answer on both surfaces, with the prompt text
      recoverable into the composer and never resent automatically.
- [x] (2026-09-18) Live test: the scripted agent's swallow mode and the
      `unanswered-prompt` scenario, which fails on the unfixed build and passes on this
      one.
- [ ] Milestone 3 (conditional): hold queue promotion until the session has been quiet
      for a settle window. **Trigger did not fire** — see Milestone 0. Not built.
- [ ] Milestone 4 (conditional): read the ACP compaction updates the Codex bridge's own
      schema defines. **Trigger did not fire** — Codex reports compaction as an ordinary
      tool call, and no `compaction_update` was observed. Not built.


## Surprises & Discoveries

- Observation (Milestone 0, live, Codex on `@brokkai/codex-acp@1.11.4`): `/compact` does
  not return early, the queued prompt is held while it runs, and compaction is visible to
  Mjolnir as an ordinary tool call rather than as a compaction update.
  Evidence: the session's relay journal, ordinals 58-70. 58 queues `/compact`, 59 starts
  it, 63 is `"sessionUpdate": "tool_call", "title": "Compact conversation", "kind":
  "think"` with `_meta.contextCompaction`, 64 queues the next prompt *while the first is
  in flight*, 68 completes the tool call, 69 completes the `/compact` prompt with
  `EndTurn` after 8.1 seconds, and only then does 70 start the queued prompt, which was
  answered. No `compaction_update` or `compaction_summary_chunk` appeared.

- Observation (Milestone 0, live, Claude Code on
  `@agentclientprotocol/claude-agent-acp@0.73.0`): the same, and the banners the bridge
  source predicted arrive exactly as predicted, as ordinary assistant text.
  Evidence: the session's relay journal, ordinals 26-33. 28 is an `agent_message_chunk`
  reading `"Compacting..."`, 29 reads `"\n\nCompacting failed: Not enough messages to
  compact."`, 31 completes the `/compact` prompt with `EndTurn` *after* those, 32 queues
  the next prompt and 33 starts it, and it was answered.

- Observation: the reported loss therefore does not reproduce on either pinned bridge, so
  Milestones 3 and 4 stay unbuilt behind their triggers, as the maintainer directed.
  Not observed: a *successful* Claude compaction. The one measured refused itself with
  "Not enough messages to compact", so the case where a compaction turn produces nothing
  but the two banners was not seen live. It is covered by construction instead: a prompt
  that asks the harness to compact is never judged unanswered
  (`mj_core::acp::prompt_requests_compaction`), with a unit test for each side of it.

- Observation: minimal test fixtures answered a prompt with a bare `end_turn` and no
  output at all, which is exactly the shape this change reports as unanswered. Five had
  to start answering like a working harness.
  Evidence: `mj-worker/src/acp/plan_tests.rs` and `mj-worker/src/acp/session_config_tests.rs`
  failed with `left: "prompt_unanswered", right: "EndTurn"` until their scripted agents
  sent one `agent_message_chunk` before their result.

- Observation: a permission request is the only thing some real turns produce, so
  counting session updates alone would report them as unanswered. The count now includes
  every request the agent makes of Mjolnir.
  Evidence: `acp::plan_tests::guardian_approval_uses_auto_without_a_followup_prompt`
  passed again as soon as `RequestPermissionRequest` marked the counter.

- Observation: `mj_core::activity::ActivityFacts` has no `compaction_in_flight` field.
  The task description that led to this plan asserted it does.
  Evidence: the whole of `mj-core/src/activity.rs` (545 lines) was read; the fields are
  listed at lines 41-82 and none of them concerns compaction. `grep -rn
  "compaction_in_flight" --include=*.rs` returns nothing.

- Observation: the ACP schema Mjolnir compiles against has no compaction concept at all,
  so no compaction fact can be produced today from protocol traffic.
  Evidence: `Cargo.lock` pins `agent-client-protocol` 2.0.0 and
  `agent-client-protocol-schema` 1.5.0; the `SessionUpdate` enum in that crate
  (`src/v1/client.rs:99-139`) has twelve variants, none about compaction, and the enum is
  `#[non_exhaustive]` with no `serde(other)` fallback.

- Observation: the Codex bridge Mjolnir pins carries a *newer* ACP schema in its own
  bundle, and that newer schema does define compaction on the wire.
  Evidence: `@brokkai/codex-acp@1.11.4`, file `dist/index.js` lines 19324-19345 and
  19385-19388, defines `compaction_update` (with `compactionId` and a status of
  `in_progress`, `completed`, `failed` or `cancelled`) and `compaction_summary_chunk` as
  members of the `sessionUpdate` union. Whether the bridge actually emits them at runtime
  is unknown and is what Milestone 0 measures.

- Observation: on the Claude bridge version this repository pins, the window described in
  issue #970 does not exist for `/compact`.
  Evidence: `mj-core/src/harness_runtime.rs:12` pins
  `@agentclientprotocol/claude-agent-acp@0.73.0`. In that package's `dist/acp-agent.js`,
  `prompt()` (line 1053) pushes a Turn onto `session.turnQueue` and awaits a promise that
  is settled only when the SDK's user-turn `result` for that turn arrives (lines
  1090-1120). `/compact` is not in `LOCAL_ONLY_COMMANDS` (line 366), so it is an ordinary
  turn, and the compaction banners are emitted as assistant text *inside* that turn:
  `status: "compacting"` sends "Compacting..." (lines 2144-2156) and
  `compact_result === "success"` sends "Compacting completed." (lines 2157-2169). So the
  `session/prompt` call for `/compact` does not return until compaction has reported
  completion, and Mjolnir cannot promote the next queued prompt before then because
  promotion requires no prompt to be in flight.

- Observation: the existing empty-response detector cannot see the failure in the shape
  issue #970 reported, because the harness's own prose counts as activity.
  Evidence: `mj-worker/src/acp/drive.rs:1065`, `prompt_returned_without_updates`, is
  `stop_reason != Cancelled && updates_before == updates_after`, where the counter is the
  session-wide `session_update_count` (`mj-worker/src/acp/session.rs:578`). A
  "Compacting..." banner arriving inside the swallowed prompt's turn increments that
  counter, so the check does not fire.

- Observation: even when the detector does fire, nothing durable changes; it is a
  transcript warning only, and automation still sees success.
  Evidence: the detector emits `RuntimeEvent::Warning` with
  `mj_core::acp::PROMPT_EMPTY_RESPONSE_MARKER` (`mj-core/src/acp.rs:41`). The turn's
  durable outcome stays `TurnOutcomeKind::Completed { stop_reason: "EndTurn" }`
  (`mj-core/src/state.rs:172-179`), and `map_stop_reason`
  (`mj-controller/src/server/api/wait_policy.rs:10-19`) maps `end_turn` to
  `WaitOutcome::Finished`.

- Observation: the controller already reads the exact span of a finished turn and already
  extracts the last agent message inside it, so "did this turn produce an answer" costs
  one more column on a query that already runs.
  Evidence: `mj-controller/src/database/materialized.rs:367-427`,
  `load_materialized_turn_summary_from`, bounded by `turn_start_position` and
  `turn_completed_position`, returning `TurnSummary` (`mj-core/src/storage.rs:138-148`)
  whose `final_message` is "the last nonempty agent message the turn produced".


## Decision Log

- Decision: treat the reported incident's mechanism (a prompt dispatched into an ongoing
  compaction) as likely already closed upstream for Claude, and do not build a
  compaction-specific interlock now.
  Rationale: the pinned bridge keeps `/compact` inside one ACP turn (evidence above), and
  Mjolnir never dispatches a second prompt while one is in flight
  (`mj-worker/src/relay/commands.rs:1039-1053`). Building a compaction interlock would
  need a compaction fact that the protocol Mjolnir compiles against cannot supply.
  Date/Author: 2026-09-17, plan author.

- Decision: fix the part that is provably still broken — Mjolnir calling an unanswered
  turn "finished" — and make that one answer serve the worker warning, `mj wait`, and
  both surfaces.
  Rationale: this is the half of issue #970 that is ours by the issue's own framing ("a
  prompt leaves the queue when it is delivered, not when it is acted on, and nothing
  distinguishes those"), it is verifiable with a scripted agent, and it protects against
  any future swallow, whatever causes it.
  Date/Author: 2026-09-17, plan author.

- Decision: take the verdict at turn completion rather than on a timer.
  Rationale: the issue's bound exists to avoid firing during a long think, a long tool
  call or a user shell. Waiting for the turn to complete excludes all three by
  construction, with no bound to tune and no exclusion list to maintain. A turn that never
  completes is a different failure and is already handled by the stall watchdog
  (`mj_core::activity::stall_verdict`, used at `mj-worker/src/acp/session.rs:625-660`).
  Date/Author: 2026-09-17, plan author.

- Decision: do not resend automatically.
  Rationale: the issue reaches the same conclusion, and the risk is concrete — Mjolnir
  cannot distinguish "the harness dropped it" from "the harness ran it and said nothing",
  so an automatic resend can run the same instruction twice.
  Date/Author: 2026-09-17, plan author.

- Decision: add no field to any durable record.
  Rationale: `MaterializedTurnOutcome` and `MaterializedSession`
  (`mj-core/src/state.rs:205-226`) are `#[serde(deny_unknown_fields)]`, so a new field
  would be an incompatible read for an older build and would force a breaking migration
  under the rules in `CLAUDE.md`. The verdict is derivable from records that already
  exist, so it is derived.
  Date/Author: 2026-09-17, plan author.

- Decision (maintainer): keep Milestones 3 and 4 behind their triggers and do not build
  them now, but run Milestone 0 live for Claude and Codex rather than treating the bridge
  sources as sufficient. If a harness were measured returning early, Milestone 3 would be
  built in the same phase.
  Rationale: source reading says what a bridge intends; only a live run says what it does.
  Date/Author: 2026-09-18, maintainer. Outcome: neither harness returned early, so
  Milestone 3 stays unbuilt.

- Decision (maintainer): an unanswered turn is reported by `mj wait` as an error with a
  stop reason and diagnostic code of its own, `prompt_unanswered`, following the
  `harness_inactive` pattern from #1020.
  Rationale: automation must be able to tell this failure from every other error without
  matching on prose.
  Date/Author: 2026-09-18, maintainer.

- Decision (maintainer): the false positive is acceptable only if "output" is defined
  widely — a turn that made tool calls, wrote files or ran commands has answered even
  with no text.
  Rationale: silence is not the same as inaction, and flagging a working turn would be a
  worse failure than the one being fixed.
  Date/Author: 2026-09-18, maintainer. Implemented as
  `mj_core::acp::session_update_is_agent_output`, which counts tool calls and tool call
  updates, with a test for a text-free tool-only turn; the worker additionally counts
  every request the agent makes of Mjolnir, because a permission request alone can be
  the only thing a real turn produces.

- Decision (maintainer): restore the text into the composer; never resend automatically,
  because of the duplicate-execution risk.
  Date/Author: 2026-09-18, maintainer.

- Decision: derive the verdict in the worker at prompt completion rather than in the
  controller from the turn's transcript span, as the approved plan had proposed.
  Rationale: the worker already holds a per-turn count taken at dispatch, so the answer
  needs no new query, no `TurnSummary` field and no controller change; and recording it
  as the turn's stop reason makes `mj wait`, the session summary, the chat and the web
  viewer all report it without any of them learning a new rule. `stop_reason` is an
  existing `String` field, so this still adds no field to any durable record and there is
  still no migration.
  Date/Author: 2026-09-18, implementer.

- Decision: exclude a bridge's compaction banners from what counts as an answer, and
  never judge a prompt that itself asked the harness to compact.
  Rationale: the banners are the one thing a harness emits *instead of* acting on the
  prompt, and they are ordinary assistant text, so without naming them the shape issue
  #970 reported stays invisible. The paired exception keeps a real `/compact` turn, whose
  honest answer is those banners, from being reported as a failure.
  Date/Author: 2026-09-18, implementer.


## Outcomes & Retrospective

The work is complete and the reported failure mode is now visible wherever it can be
seen: `mj wait` returns `error (prompt_unanswered)` with the reason, both surfaces show
the same explanation, and the prompt text can be put back in the composer on either one.

The measurement changed the shape of the fix. Neither pinned bridge loses a prompt queued
behind `/compact` any more — both hold it while the compaction turn runs, and both end
that turn only when compaction has finished — so the interlock the issue proposed would
have guarded a window that is already closed. What remained, and is now fixed, is that
Mjolnir called a turn that produced nothing a success, which is the part that was ours
and the part that protects against any future swallow, whatever causes it.

What is not covered: a harness that accepts a prompt and produces something unrelated to
it still looks like an answer, because ACP gives a client no way to attribute output to a
prompt. A harness that emits its own prose during a swallowed turn is caught only when
that prose is a compaction banner Mjolnir names. If a loss is seen again in a shape this
misses, Milestone 3's settle window is the next step and its specification is unchanged
below.


## Context and Orientation

Read this section even if you know the repository; it fixes the vocabulary the rest of
the plan uses.

**Mjolnir's processes.** `mj` is a Rust workspace. A long-lived *daemon* (crate
`mj-controller`, launched from `mj-cli`) owns the database and serves the terminal UI
(`mj-tui`, `mj-chat`) and the web viewer (`mj-controller/src/web/viewer.js`). Each agent
session is run by a separate *worker* process (crate `mj-worker`) which speaks ACP to the
*bridge*, a third-party program that adapts one vendor's coding agent to ACP. This plan
touches the worker, the controller, and both surfaces.

**Harness.** The vendor agent behind the bridge: Claude Code, Codex, Kimi, Grok, Muse and
others. `mj-core/src/harness_runtime.rs` pins the bridge version per harness; line 12
pins `@agentclientprotocol/claude-agent-acp@0.73.0` and line 10 pins
`@brokkai/codex-acp@1.11.4`.

**The relay.** Inside the worker, `mj-worker/src/relay.rs` and its submodules keep a
durable, append-only journal of everything that happens in a session, plus a snapshot
projected from it. Commands a person submits (a prompt, a shell command, a configuration
change) are appended to that journal, then promoted one at a time.

**Promotion.** `promote_next_queued_command` in `mj-worker/src/relay/commands.rs:1039`
starts the head of the queue. It refuses only when: the process is checkpoint-only, a
prompt is already in flight (`snapshot.active_prompt`), a promoted configuration change
is still settling, a checkpoint barrier is held or pending, the session is closing or
closed, a close is requested, or a user shell accepted earlier than this prompt is still
running. There is no gate on how long ago the harness last did anything, and no gate on
compaction. Confirmed by reading the function in full.

**Active prompt.** `snapshot.active_prompt` is set when the relay dispatches a prompt and
cleared when the worker records `PromptFinished`, which happens when the bridge's
`session/prompt` call returns. So the next queued prompt is dispatched as soon as the
previous ACP call returns — this part of the issue's diagnosis is confirmed on master.

**Activity.** `mj-core/src/activity.rs` is the single place that answers what a session
is doing. It holds `ActivityFacts` (the facts), `classify` (what state they mean),
`has_work_in_flight`, `is_quiet`, `safe_to_replace`, `checkpoint_blocker`, `chat_phase`,
`while_disconnected`, `StallPolicy` and `stall_verdict`. Any new predicate about whether
the session is ready for something belongs in this module, not in the relay.

State this plainly, because the brief that led to this work assumed the opposite:
**`ActivityFacts` has no `compaction_in_flight` field and no other compaction fact.**
Read the struct at `mj-core/src/activity.rs:41-82`; every field is listed there and none
concerns compaction, and `grep -rn "compaction_in_flight" --include=*.rs` returns
nothing. The reason is not an oversight. A fact has to come from something Mjolnir can
observe, and the ACP schema this build compiles against —
`agent-client-protocol-schema` 1.5.0, whose `SessionUpdate` enum is at
`src/v1/client.rs:99-139` — has no compaction variant at all, so no compaction signal can
reach the worker in a form it can decode. What the harnesses actually send instead was
measured rather than assumed, and is recorded under `Surprises & Discoveries`: Codex
reports compaction as an ordinary tool call, and Claude Code as two lines of assistant
text. Neither is a fact about compaction; both are ordinary output that happens to be
about compacting. That is why this change names the Claude banners explicitly instead of
adding a compaction fact, and why adding one is Milestone 4, behind a trigger that has
not fired.

**The turn record.** When a prompt completes, the controller projects a
`MaterializedTurnOutcome` (`mj-core/src/state.rs:205-222`) carrying the command id, the
accepted ordinal, the turn's start position, the completion ordinal and a
`TurnOutcomeKind` which is `Completed { stop_reason }`, `Rejected { message }` or
`Interrupted { message }`. A *position* is a relay ordinal, and transcript items carry
the same ordinals, so a finished turn is the span from `turn_start_position` to
`completed_ordinal`.

**`mj wait`.** `mj wait` asks the daemon to block until a session reaches an ending.
`mj-controller/src/server/api/wait_policy.rs` decides; `resolve_wait` (line 139) returns
a `WaitDecision`, and `mj-controller/src/server/api/wait.rs:199` then loads the finished
turn's summary over the turn's span. `map_stop_reason` turns the harness's `end_turn`
into `WaitOutcome::Finished`. This is the automation-visible form of the bug: a swallowed
prompt's turn ends with `end_turn`, so automation is told the work succeeded.

**The existing detector.** `prompt_returned_without_updates`
(`mj-worker/src/acp/drive.rs:1065`) compares a session-wide update counter taken before
the prompt and after it, and on equality the worker emits a warning carrying
`PROMPT_EMPTY_RESPONSE_MARKER`. Two weaknesses, both confirmed by reading: any update at
all defeats it, including updates that have nothing to do with answering the prompt
(available-commands catalogues, mode changes, usage updates, and a harness's own
compaction banners); and its only effect is a transcript warning.

**What the surfaces show today.** The terminal chat (`mj-chat`) renders worker warnings
as system lines and already keeps a client-side record of submissions the relay refused,
`UnsentPrompt` (`mj-chat/src/chat.rs:354-414`), drawn at the end of the transcript and
restorable into the composer with Ctrl-Alt-R. The web viewer renders queued prompts and
can take the newest queued prompt back into the composer
(`mj-controller/src/web/viewer.js:3060-3075`). Neither has any notion of a prompt that was
sent and not answered.

**What the issue asked for, restated.** A prompt that leaves the queue and produces
nothing must not look finished; a merely slow prompt must never be reported that way; the
text must be recoverable without retyping; and the mechanism must be testable directly.
This plan meets all four, and meets the second one by construction rather than by tuning
a bound.


## Plan of Work

The work is one measurement milestone, two implementation milestones that are always
done, and two conditional milestones that are done only if the measurement says so.

### Milestone 0 — Measure what each harness does with a prompt during compaction (done)

Scope: no product code changes. The question is whether the window the issue describes is
still open on the bridges this repository pins, because everything else depends on it.

Work: a private instance (name `fix970`, phone port 4170; see `Concrete Steps`) with a
local-bare target, one session per harness, and in each: a warm-up prompt, then `/compact`
followed immediately by a second prompt, so the second is queued behind the first. The
evidence is the session's own relay journal at
`~/.local/share/mjolnir/instances/fix970/workers/<session>/relay-journal/active.jsonl`,
which records every command and every session update in order with ordinals. That is
better evidence than a log: it is the same record the projection is built from.

Three questions, answered per harness:

1. Does the bridge's `session/prompt` for `/compact` return before the compaction output
   appears? **No, on both.** Codex completed the prompt at ordinal 69, after the
   compaction tool call completed at 68, 8.1 seconds after starting it. Claude Code
   completed the prompt at ordinal 31, after both banners at 28 and 29.
2. Does any compaction signal arrive that Mjolnir's ACP schema cannot decode? **No.**
   Codex reports compaction as an ordinary `tool_call` titled "Compact conversation" with
   `_meta.contextCompaction`; Claude Code reports it as two `agent_message_chunk` lines.
   No `compaction_update` or `compaction_summary_chunk` appeared, and no decode failure
   was recorded.
3. With a second prompt queued behind `/compact`, does it get an answer? **Yes, on both.**
   Codex answered it at ordinals 78-109, Claude Code at 35-37.

Result: the reported loss does not reproduce on the pinned bridges, so the triggers for
Milestones 3 and 4 did not fire and neither was built. Full quotations are in
`Surprises & Discoveries`.

### Milestone 1 — One shared answer to "did this turn produce anything" (done)

Scope: the worker becomes specific about what counts as producing something, and a turn
the harness ended without producing anything is failed under a stop reason of its own, so
`mj wait` reports a named error instead of success.

This differs from the approved plan in where the answer is computed. The plan proposed
deriving it in the controller from the finished turn's transcript span and a new
`TurnSummary` field. The worker already holds a per-turn count taken at dispatch, so the
answer needs no new query and no controller change, and recording it as the turn's stop
reason makes every reader report it without learning a new rule. The `Decision Log` has
the reasoning. Nothing durable gained a field either way, so there is still no migration.

*Commit 1 — the shared classifier* (`c83e6f6a`). In `mj-core/src/acp.rs`, beside
`session_update_has_native_history`:

    /// Whether this update is the agent doing the work a prompt asked for.
    pub fn session_update_is_agent_output(update: &SessionUpdate) -> bool

Answering is defined widely, as the maintainer directed: agent messages, thoughts, plans,
tool calls and tool call updates all count, so a turn that only edited files and said
nothing has answered. Excluded is traffic a harness emits on its own schedule —
`AvailableCommandsUpdate`, `ConfigOptionUpdate`, `CurrentModeUpdate`, `SessionInfoUpdate`
and `UsageUpdate`. The match is written as a negation with a catch-all, so an unknown
future variant of the `#[non_exhaustive]` enum counts as output: reporting a working turn
as unanswered would be worse than missing a swallow. Tests cover both sides, including a
text-free tool-only turn.

*Commit 2 — the worker counts and reports it* (`f9fb1e22`, with the follow-up `6d1255ac`).
Three parts, all in `mj-worker/src/acp/`:

Two more pieces of the classifier went into `mj-core/src/acp.rs` with this commit, because
the Milestone 0 measurement showed they were needed. `session_update_is_compaction_banner`
recognizes the progress text a bridge streams while compacting ("Compacting...",
"Compacting completed.", "Compacting failed…"), and `session_update_is_agent_output`
excludes it: those banners are the one thing a harness emits *instead of* acting on the
prompt, and they are ordinary assistant text, so without naming them the shape issue #970
reported stays invisible. `prompt_requests_compaction` recognizes Mjolnir's own outgoing
`/compact` text, and a turn answering such a prompt is never judged, because banners are
the honest answer to it. Only Mjolnir's outgoing text is inspected, never the harness's.

`drive.rs` gains `AgentOutputCount`, a shared counter marked wherever the agent acts: in
the session-update handler when `session_update_is_agent_output` accepts the update, and
in every handler that serves a request the agent makes — a permission, a terminal
(create, output, wait, kill, release), an elicitation or any other ext request, and
Grok's own turn-completion notification. The measurement's lesson is in that list: a real
turn can produce a permission request and nothing else, and counting only session updates
would report it as unanswered.

`prompt_returned_without_updates` now fires only for `StopReason::EndTurn`. A cancelled,
refused or token-limited turn already reports its own ending, and relabelling it would
hide why it really ended.

`session.rs` then fails such a turn rather than only warning about it: the stop reason is
`mj_core::acp::PROMPT_UNANSWERED_STOP_REASON` ("prompt_unanswered") and the diagnostic
carries the same code, following the `harness_inactive` pattern from #1020. Because any
stop reason that is not a known completion classifies as an error
(`mj_core::state::classify_prompt_completion`), `mj wait` reports
`error (prompt_unanswered)` with the explanation and exits non-zero, and nothing in the
controller had to change. A test in `mj-controller/src/server/api/tests.rs` pins that
mapping.

The warning text keeps `PROMPT_EMPTY_RESPONSE_MARKER` as its prefix, because
`mj-core/src/credentials.rs:504` matches on it, and adds the sentence a person can act
on: which harness ended the turn, that the prompt may never have been acted on, and to
check the workspace before resending in case the work was done without being reported.

Five scripted test agents that answered a prompt with a bare `end_turn` and no output now
send one line first, which is what a working harness does.

### Milestone 2 — Say it on both surfaces, and keep the text (done)

Scope: a person watching either surface sees the same verdict and can put the prompt back
in the composer without retyping it. Neither surface resends anything: Mjolnir cannot
tell a prompt the harness dropped from one it acted on silently, so resending would risk
running the same instruction twice and stays the person's decision.

The warning already reaches both surfaces as a system line in the transcript
(`mj-transcript/src/transcript.rs:1220`), so the work here is the recovery affordance.

*Commit 3 — both surfaces* (`57203c2e`). In `mj-chat/src/chat.rs`, `apply_materialized`
calls a new `keep_unanswered_prompt`: when the session's `last_turn_outcome` is a
completed turn whose stop reason is `prompt_unanswered`, and that turn has not already
been recorded, the prompt is read from the turn's own first transcript item and pushed
onto the existing unsent list under a new `UnsentKind::Unanswered`, headed "Prompt was not
answered", so the existing Ctrl-Alt-R restore path works unchanged. Reading the text from
the transcript rather than from anything the client remembers is what makes a prompt
submitted from the web viewer, or promoted from the queue, recoverable in the terminal
too. Two tests in `mj-chat/src/chat/tests.rs` cover it: an unanswered turn is recorded
once however often the projection arrives and restores into the composer, and a finished
turn leaves nothing to restore.

In `mj-controller/src/web/viewer.js`, a warning row carrying the marker gains a "Put back
in composer" button holding the prompt that turn was running, which is the newest user row
before it. It reuses `setComposerText`, the same path the queue's Edit button uses. A case
in `tests/e2e/web/viewer.unit.test.mjs` runs the new function under Node.

### Milestone 3 — Conditional: hold promotion until the session settles

**Trigger condition.** Do this milestone only if Milestone 0 answers question 1 with
"yes, `session/prompt` returned while the harness was still working" for any harness, or
if a loss is observed again after Milestones 1 and 2 are in place. Otherwise leave it
unbuilt and record in `Outcomes & Retrospective` that the trigger did not fire.

Scope: the relay stops dispatching the next queued command the instant the previous ACP
call returns, and instead waits until the session has shown no sign of life for a settle
window. This is vendor-neutral: it needs no knowledge of compaction or of any harness's
command list, which is why it is preferred over the "hold after a forwarded slash
command" alternative the issue also rejected.

Work: in `mj-core/src/activity.rs`, beside `StallPolicy`, add

    /// How long the session must have been silent before the next queued
    /// command is dispatched, and the longest that hold may last.
    pub struct PromotionPolicy {
        pub settle: Duration,
        pub cap: Duration,
    }

    /// When the next queued command may be dispatched, or `None` when it may
    /// be dispatched now.
    pub fn promotion_hold_until_ms(
        facts: &ActivityFacts,
        policy: PromotionPolicy,
        now_ms: i64,
    ) -> Option<i64>

It returns `None` when there is nothing to hold for (no queued commands, or the session
has been silent for at least `settle`), and otherwise the epoch-milliseconds instant to
try again, never later than `cap` after the queue head was accepted. The cap exists so a
harness that chatters forever cannot hold a prompt forever. Defaults: `settle` two
seconds, `cap` thirty seconds, overridable through the dev environment variables
documented in `.agents/docs/internal-environment-variables.md`, following the pattern of
`MJ_TURN_STALL_TIMEOUT_MS`.

Wire it exactly like the existing capacity retry, which is the precedent for a
timer-driven relay action: `promote_next_queued_command`
(`mj-worker/src/relay/commands.rs:1039`) consults `promotion_hold_until_ms` from
`self.activity_facts()` and returns `Ok(None)` while held; the relay exposes a
`promotion_hold_deadline()` next to `capacity_retry_deadline()`
(`mj-worker/src/relay/commands.rs:810`); and the coordinator loop in
`mj-worker/src/worker_runtime/unix/dispatch.rs:26-108` adds a second deadline branch
beside the capacity timer that wakes and calls `dispatch_pending` again.

Tests: pure state-transition tests on `promotion_hold_until_ms` in
`mj-core/src/activity/tests.rs`, driven with explicit `now_ms` values rather than a real
clock — a queue head with activity one second ago is held; the same facts two seconds
later are not; a tool call in flight holds regardless; a queue head older than the cap is
released even while activity continues. Then a relay-level test in
`mj-worker/src/relay/tests.rs` stepping the same clock values, asserting the queued
command is promoted only after the settle window.

### Milestone 4 — Conditional: read the compaction updates the newer schema defines

**Trigger condition.** Do this milestone only if Milestone 0 answers question 2 with
"yes, `compaction_update` notifications arrive". Otherwise record that they do not.

Scope: Mjolnir gains a real compaction fact instead of the text sniffing in
`mj-chat/src/chat.rs:1283` (`is_compaction_artifact`, which matches `sessionUpdate`
values `compaction`, `context_compaction` and `compaction_summary` — none of which is
what the Codex bridge's bundled schema actually defines; it defines `compaction_update`
and `compaction_summary_chunk`). Either upgrade `agent-client-protocol` to a version
whose Rust schema carries the variants, or decode them at the notification boundary in
the worker before the typed schema sees them. Then add `compaction_in_flight` to
`ActivityFacts`, make `classify` treat it as work in flight, and let
`promotion_hold_until_ms` refuse while it is set. Note in the plan at that point whether
this changes the worker-to-daemon operational state on the wire; adding a field to
`ActivityFacts` does, so it needs a `#[serde(default)]` and a note in the commit message,
and the daemon protocol version in `mj-client/src/daemon.rs` must be bumped by one.


## Concrete Steps

All commands run from the repository root of your worktree. Substitute your own worktree
path for `<worktree>`.

Build and check, outside any sandbox, on the dev profile:

    cd <worktree>
    cargo build
    cargo test
    cargo clippy --all-targets -- -D warnings

Live instance for Milestone 0, following the repository's private-instance rules. The
instance name is `fix970` and the phone port is 4170; never touch another instance:

    mkdir -p ~/.config/mjolnir/instances/fix970
    cp ~/.config/mjolnir/instances/campaign0916/config.toml \
       ~/.config/mjolnir/instances/fix970/config.toml
    # edit [phone] bind to 127.0.0.1:4170 and add, if absent:
    #   [targets.localhost]
    #   kind = "local-bare"
    # the file contains an API key: never print, commit or paste it
    tmux new-session -d -s fix970 -x 140 -y 40
    tmux send-keys -t fix970 'MJ_WORKER_BINARY=<worktree>/target/debug/mj-worker \
      RUST_LOG=debug <worktree>/target/debug/mj -i fix970 go' Enter

A fresh instance has no workspace, and `mj new` then fails with a generic 500 (issue
#1080), so run `mj go` once first, as above, to create one. Because a `local-bare`
session runs `target/debug/mj-worker`, which `cargo build --bin mj` and `cargo test` do
not rebuild, run a full `cargo build` before any live test and prove the running worker
is the one you built, for example:

    strings <worker root>/hel | grep 'without answering'

Cleanup, in this order — stopping the daemon first, then removing files, never the
reverse:

    <worktree>/target/debug/mj -i fix970 daemon stop
    pgrep -af instances/fix970        # terminate any survivor before continuing
    tmux kill-session -t fix970
    rm -rf ~/.config/mjolnir/instances/fix970 ~/.local/share/mjolnir/instances/fix970

After `daemon stop`, do not run another `mj -i fix970` command: it would start the daemon
again.


## Validation and Acceptance

**Unit and behaviour tests.** `cargo test` passes on the dev profile and
`cargo clippy --all-targets -- -D warnings` is clean. The new tests, and what each proves:

- `mj-core/src/acp.rs`, `agent_output_tests`: a usage update, a command catalogue, a mode
  change, a config-option update and session metadata are not an answer; a message, a
  thought, a tool call and a tool call update are, so a text-free tool-only turn is never
  flagged; a compaction banner is not, while ordinary prose that merely mentions
  compacting is; and only a prompt beginning `/compact` counts as asking to compact.
- `mj-worker/src/acp/tests.rs`,
  `only_a_finished_turn_that_produced_nothing_counts_as_unanswered`: the verdict is taken
  only for `end_turn`, and `max_tokens`, `refusal` and `cancelled` keep their own reasons.
- `mj-worker/src/acp/tests.rs`, the scripted-bridge case that drives a real ACP
  connection: a turn the bridge ends successfully with no output finishes as
  `prompt_unanswered` and warns with the marker and the explanation. It failed before the
  change, which reported `EndTurn`.
- `mj-controller/src/server/api/tests.rs`,
  `an_unanswered_turn_reaches_wait_as_a_named_error` and the stop-reason mapping: `mj wait`
  returns `error` carrying both `prompt_unanswered` and the explanation, and every other
  ending is unchanged.
- `mj-chat/src/chat/tests.rs`: an unanswered turn is recorded once and restores into the
  composer; a finished turn leaves nothing to restore.
- `tests/e2e/web/viewer.unit.test.mjs`: the warning row offers the prompt that turn was
  running, and no other row offers anything.

**Live test that fails on the unfixed build.** The scripted ACP agent in
`tests/e2e/reliability_lab.py` gained a swallow mode: `MJ_FAKE_ACP_SWALLOW_PROMPT` is the
one-based index of the prompt it accepts and then ends as a successful turn without doing
anything, and `MJ_FAKE_ACP_SWALLOW_BANNER` makes it first stream "Compacting..." and
"Compacting completed." into that turn. The banner variant is the shape issue #970
reported, and it is the one a session-wide update counter cannot see.

The `unanswered-prompt` scenario opens a session on the fake profile against a local-bare
target, has one prompt answered normally, then has the second swallowed with banners, and
requires `mj wait` to report the failure. Run it from the repository root:

    cargo build
    python3 tests/e2e/reliability_lab.py --scenario unanswered-prompt \
        --seed 970 --hel target/debug/mj

On this build it passes:

    reliability: passed scenario=unanswered-prompt seed=970 leaks=0

and the run's `trace.json` records what `mj wait` printed for each turn:

    answered-turn   -> finished (EndTurn) turn 1 in 0.0s
                       reliability reply: first seed=970
    unanswered-turn -> error (prompt_unanswered) turn 2 in 0.0s
                       ACP prompt returned no session updates: Codex ended the turn
                       without producing any message, thought or tool call, so this
                       prompt may never have been acted on. Check the workspace before
                       resending it, in case the work was done without being reported.

The same scenario was run against the product code of the base commit, with the scenario
itself unchanged, and failed:

    reliability: failed: a swallowed prompt was reported as success:
    'finished (EndTurn) turn 2 in 0.0s\n\nCompacting...Compacting completed.'

That is the proof the test is not self-fulfilling: the unfixed build calls the swallowed
turn a success *even though the pre-existing empty-response check is in it*, because the
banners defeat that check.

Because a `local-bare` session runs `target/debug/mj-worker`, which `cargo build --bin mj`
and `cargo test` do not rebuild, run a full `cargo build` before the live test and prove
the running worker is the one you built:

    strings target/debug/mj-worker | grep -c 'may never have been acted on'   # 1

**Acceptance, as behaviour a person can check.** With the swallow switch set, `mj wait`
prints `error (prompt_unanswered)` and exits non-zero rather than reporting success; the
terminal chat shows the warning under the prompt and, with an empty composer, Ctrl-Alt-R
restores the prompt text under the heading "Prompt was not answered"; and the web viewer
shows the same warning row with a "Put back in composer" button.

## Idempotence and Recovery

Every step is repeatable, and the live scenario cleans up after itself: it closes the
session, stops the daemon and fails if any process it owned survived.

The commits are ordered so each stands alone. `c83e6f6a` adds the classifier and changes
no behaviour. `f9fb1e22` makes the worker count agent output and fail an unanswered turn;
`6d1255ac` removes a duplicate constant it left behind. `57203c2e` adds the two surface
affordances, and depends on `f9fb1e22` only for the stop reason it keys on. `de01ae6f`
adds the live test. Reverting any later commit leaves the earlier ones working.

No migration is required and none was made: nothing durable changed shape. The verdict
travels in `TurnOutcomeKind::Completed`'s existing `stop_reason` string, which already
carries `harness_inactive` and other worker-chosen reasons. This matters because
`MaterializedTurnOutcome` and `MaterializedSession` (`mj-core/src/state.rs:205-226`) are
`#[serde(deny_unknown_fields)]`, so a new field there would be an incompatible read for an
older build and a **breaking** migration under the rules in `CLAUDE.md`. An older reader
sees an unfamiliar stop reason instead, which it already classifies as an error — the
cautious answer.

There is no protocol change: no new field crosses the worker-to-daemon boundary and the
HTTP wait response keeps its shape, choosing a different existing outcome value.
`PROTOCOL_VERSION` in `mj-client/src/daemon.rs` is untouched. Milestone 4, if it is ever
triggered, would change the published worker state and would need that bump.

One behaviour changes for existing sessions: a turn a harness ends with `end_turn` and no
output at all is now an error rather than a success. Automation that treated such a turn
as finished will start seeing `error (prompt_unanswered)`, which is the intent.

Live-test recovery: if a `fix970` worker survives a daemon stop, terminate it by process
group before removing the instance directories; removing files under a running writer
recreates them.

## Artifacts and Notes

The promotion gate as it stands on master, from
`mj-worker/src/relay/commands.rs:1039-1053`:

    if self.checkpoint_only
        || self.snapshot.active_prompt.is_some()
        || self.promoted_config_in_progress()
        || self.snapshot.checkpoint_barrier.is_some()
        || matches!(self.snapshot.execution, Closing | Closed)
        || self.pending_checkpoint_barrier()
        || self.close_requested()
    {
        return Ok(None);
    }

The detector as it was before this work, from `mj-worker/src/acp/drive.rs`. The counters
it compared counted every session update, and it fired for every stop reason but
`Cancelled`:

    pub(super) fn prompt_returned_without_updates(
        stop_reason: &StopReason,
        updates_before: u64,
        updates_after: u64,
    ) -> bool {
        *stop_reason != StopReason::Cancelled && updates_before == updates_after
    }

and as it is now, reading counters that count only agent output, and judging only a turn
the harness called finished:

    pub(super) fn prompt_returned_without_updates(
        stop_reason: &StopReason,
        updates_before: u64,
        updates_after: u64,
    ) -> bool {
        *stop_reason == StopReason::EndTurn && updates_before == updates_after
    }

The measured Codex compaction turn, from the `fix970` relay journal, abridged to the
`sessionUpdate` values and the command boundaries:

    58 command_queued    prompt "/compact"
    59 command_started
    63 session_update    tool_call "Compact conversation" (kind think)
    64 command_queued    prompt "Reply with exactly: SECOND-PROMPT-ANSWERED"
    68 session_update    tool_call_update completed
    69 command_completed stop_reason EndTurn
    70 command_started   the queued prompt, which was answered

The measured Claude compaction turn, from the same instance:

    26 command_queued    prompt "/compact"
    27 command_started
    28 session_update    agent_message_chunk "Compacting..."
    29 session_update    agent_message_chunk "\n\nCompacting failed: Not enough messages to compact."
    31 command_completed stop_reason EndTurn
    32 command_queued    prompt "Reply with exactly: SECOND-PROMPT-ANSWERED"
    33 command_started   which was answered

The compaction banners inside the pinned Claude bridge's `/compact` turn, from
`@agentclientprotocol/claude-agent-acp@0.73.0`, `dist/acp-agent.js:2147-2168`, abridged:

    if (message.status === "compacting") {
        compactionInProgress = true;
        ... content: { type: "text", text: "Compacting..." }
    } else if (message.compact_result === "success" && compactionInProgress) {
        compactionInProgress = false;
        ... content: { type: "text", text: "\n\nCompacting completed." }
    }

The compaction updates the Codex bridge's bundled ACP schema defines but Mjolnir's Rust
schema does not, from `@brokkai/codex-acp@1.11.4`, `dist/index.js:19324-19345`, abridged:

    var zCompactionStatus = union([
      literal("in_progress"), literal("completed"),
      literal("failed"), literal("cancelled"), string2(),
    ]);
    var zCompactionUpdate = object({ compactionId, status, summary, error });
    var zCompactionSummaryChunk = object({ compactionId, content });


## Interfaces and Dependencies

No dependency was added or changed. Milestone 4, if it is ever triggered, may require a
newer `agent-client-protocol` than the 2.0.0 in `Cargo.lock`, whose schema crate 1.5.0 has
no compaction variant.

In `mj-core/src/acp.rs`:

    /// The stop reason recorded for a turn the harness ended without answering.
    pub const PROMPT_UNANSWERED_STOP_REASON: &str = "prompt_unanswered";

    pub fn session_update_is_agent_output(update: &SessionUpdate) -> bool;
    pub fn session_update_is_compaction_banner(update: &SessionUpdate) -> bool;
    pub fn prompt_requests_compaction(prompt: &[ContentBlock]) -> bool;

In `mj-worker/src/acp/drive.rs`, crate-private:

    #[derive(Clone, Default)]
    pub(super) struct AgentOutputCount(Arc<AtomicU64>);
    impl AgentOutputCount { fn mark(&self); fn get(&self) -> u64 }

    pub(super) fn prompt_unanswered_message(harness: HarnessKind) -> String;

In `mj-chat/src/chat.rs`, private: `UnsentKind::Unanswered` and
`ChatState::keep_unanswered_prompt(&MaterializedSession)`.

In `mj-controller/src/web/viewer.js`: `PROMPT_UNANSWERED_MARKER` and
`unansweredPromptFor(entry, lastUserText)`.

Nothing outside `mj_core::acp` decides what counts as the harness answering, and nothing
outside `mj_core::activity` may decide whether the next queued command may be dispatched
if Milestone 3 is ever built.

## Open Questions for the Maintainer (all resolved)

The four questions this plan raised at review were answered when it was approved; the
answers are in the `Decision Log` as maintainer decisions. In short: keep Milestones 3 and
4 behind their triggers but measure live rather than trusting the bridge sources; report
an unanswered turn through `mj wait` as an error with its own stop reason and diagnostic
code; accept the residual false positive only because "output" is defined widely enough
that a tool-only turn counts as an answer; and restore the prompt into the composer rather
than resending it.

One thing worth a later look, noticed while validating and deliberately not changed here:
`mj wait` prints the failure message twice, once as the outcome message and once as the
diagnostic, because `WaitDecision::from_outcome` falls back to the diagnostic for the
message and `wait_report_lines` then prints the diagnostic as well. That is pre-existing —
`harness_inactive` reads the same way — and fixing it belongs with the wait reporting,
not with this issue.

## Revision Note

2026-09-17: first revision, written as phase 1 (plan only) for issue #970.

2026-09-18: second revision, written as the work was implemented. The changes, and why:

The milestones are rewritten to describe what was built rather than what was proposed,
because a plan that disagrees with the code it produced is worse than no plan. Milestone 0
is recorded as done, with its live measurements quoted in `Surprises & Discoveries` and
abridged in `Artifacts and Notes`; both harnesses hold a queued prompt correctly, so the
triggers for Milestones 3 and 4 did not fire and those milestones stay unbuilt with their
specifications intact.

Milestone 1 now computes the verdict in the worker at prompt completion and records it as
the turn's stop reason, instead of deriving it in the controller from the turn's
transcript span. The reason is in the `Decision Log`: the worker already holds a per-turn
count, and a stop reason reaches `mj wait`, the session summary and both surfaces without
any of them learning a new rule. Two pieces the approved plan did not foresee were added
because the measurement showed they were needed: compaction banners are excluded from what
counts as an answer, and a prompt that asked the harness to compact is never judged.

The `Context and Orientation` section now states plainly that `ActivityFacts` holds no
compaction fact, why it cannot hold one today, and what the harnesses send instead. The
brief that commissioned this work assumed such a fact existed, and an unmarked false
premise in a plan is how the next reader inherits it.

`Validation and Acceptance` is rewritten around the tests that now exist, including the
live scenario's output on this build and its failure on the base commit's product code,
which is what makes it evidence rather than decoration.
