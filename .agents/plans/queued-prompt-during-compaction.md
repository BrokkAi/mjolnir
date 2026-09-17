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
- [x] (2026-09-17) Wrote this plan. Phase 1 (plan only) ends here.
- [ ] Milestone 0: measure what each harness does with a prompt during compaction.
- [ ] Milestone 1: one shared answer to "did this turn produce anything", used by the
      worker warning and by `mj wait`.
- [ ] Milestone 2: the same answer on both user-facing surfaces, with the prompt text
      recoverable into the composer.
- [ ] Milestone 3 (conditional, see its trigger): hold queue promotion until the session
      has been quiet for a settle window.
- [ ] Milestone 4 (conditional, see its trigger): read the ACP compaction updates that
      the Codex bridge's own schema defines.


## Surprises & Discoveries

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


## Outcomes & Retrospective

To be written at the end of each milestone. At the time of writing, Phase 1 (plan only)
is complete and no product code has changed.


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
the session is ready for something belongs in this module, not in the relay. There is no
compaction fact in it today.

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

### Milestone 0 — Measure what each harness does with a prompt during compaction

Scope: no product code changes. At the end of this milestone the `Surprises &
Discoveries` section states, per harness, whether `session/prompt` can return while the
harness is still working, and whether any compaction signal reaches Mjolnir.

Work: run a private instance (instance name `fix970`, phone port 4170, see `Concrete
Steps`) with `RUST_LOG=debug` for the worker, open a session on the cheap `deepseek`
profile, and record the worker log around a `/compact` submission followed immediately by
a second prompt. Three questions to answer, each recorded as confirmed or unknown:

1. Does the bridge's `session/prompt` for `/compact` return before the compaction output
   appears? If it does not, the issue's window is closed for that harness.
2. Does any `session/update` arrive that Mjolnir's ACP schema cannot decode? The Codex
   bridge's bundled schema defines `compaction_update`; Mjolnir's Rust schema does not.
   If such notifications arrive, note how the worker currently treats them (decode error,
   silently ignored, or surfaced as an adapter payload).
3. With a second prompt queued behind `/compact`, does it get an answer?

Acceptance: three recorded answers, each marked confirmed or unknown, with a quoted log
line for each confirmed one. This milestone may end with "the reported loss does not
reproduce on the pinned bridges", which is a valid and useful result.

### Milestone 1 — One shared answer to "did this turn produce anything"

Scope: the worker's detector becomes specific about what counts as producing something,
the controller learns to answer the same question from a finished turn's span, and
`mj wait` stops calling an unanswered turn finished. At the end of this milestone,
automation can tell a swallowed prompt from an answered one.

Work, in order:

*Commit 1 — the shared classifier.* In `mj-core/src/acp.rs`, beside the existing
`session_update_has_native_history`, add:

    /// Whether this update is the agent doing the work a prompt asked for.
    ///
    /// Catalogues, mode changes, configuration options, session metadata and
    /// usage accounting all arrive without the agent having done anything, so
    /// a turn carrying only those produced no answer.
    pub fn session_update_is_agent_output(update: &SessionUpdate) -> bool

It returns true for `AgentMessageChunk`, `AgentThoughtChunk`, `ToolCall`,
`ToolCallUpdate`, `Plan` and `UserMessageChunk` (a harness echoing the prompt back is
evidence it took it), and false for `AvailableCommandsUpdate`, `ConfigOptionUpdate`,
`CurrentModeUpdate`, `SessionInfoUpdate` and `UsageUpdate`. The enum is
`#[non_exhaustive]`, so the match must have a catch-all arm returning true: an unknown
future variant must not be read as "the agent did nothing".

In `mj-core/src/transcript.rs`, add the same question for a stored transcript item:

    /// Whether this transcript item is output the agent produced.
    pub fn item_is_agent_output(item: &TranscriptItem) -> bool

Read the existing item kinds in that file before writing it; it must count agent
messages, thoughts and tool calls, and must not count the user's own prompt item, harness
turn markers, or system and warning lines Mjolnir itself wrote.

In `mj-core/src/state.rs`, beside `classify_prompt_completion`, add the interpretation
both consumers share:

    /// What a finished turn delivered.
    pub enum TurnDelivery {
        /// The agent produced output for this prompt.
        Answered,
        /// The agent ended the turn without producing anything. The prompt may
        /// never have been acted on.
        Unanswered,
        /// The turn did not end normally, so this question does not apply.
        NotApplicable,
    }

    pub fn turn_delivery(outcome: &TurnOutcomeKind, produced_agent_output: bool)
        -> TurnDelivery

`NotApplicable` for `Rejected` and `Interrupted`, and for a `Completed` turn whose stop
reason is not `Finished` under `classify_prompt_completion` — a cancelled, quota-limited
or errored turn already reports its own failure and must not be relabelled. Unit tests in
the same files cover each arm.

*Commit 2 — the worker counts only agent output.* In `mj-worker/src/acp/session.rs`
around line 578, the counter compared before and after the prompt must count only updates
for which `session_update_is_agent_output` is true. Find where `session_update_count` is
incremented (`mj-worker/src/acp/drive.rs`, search `session_update_count`) and add a second
counter beside it rather than changing the meaning of the existing one, which other code
reads. `prompt_returned_without_updates` then compares the new counter. Its existing unit
tests (`mj-worker/src/acp/tests.rs:964-966`) stay valid; add one showing that a turn
carrying only a usage update and an available-commands update is still "no output".

*Commit 3 — the controller answers from the turn's span.* Add
`produced_agent_output: bool` to `TurnSummary` (`mj-core/src/storage.rs:138-148`). This
is an in-memory struct returned by a query, not a stored record, so nothing is migrated
and no schema changes. Compute it in `load_materialized_turn_summary_from`
(`mj-controller/src/database/materialized.rs:380-427`) in the same connection: a
`SELECT EXISTS(...)` over `materialized_transcript_items` bounded by the same
`position >= ?2 AND position <= ?3` as the timestamps query, restricted to items that
`item_is_agent_output` accepts. Write the SQL against the columns that query already
uses; read the surrounding functions in that file before choosing the predicate, and put
the interpretation in the shared helper rather than duplicating a list of kinds in SQL if
the stored shape allows it. Update the two fake backends in
`mj-controller/src/server/api/tests.rs` (lines 415 and 1202) and the real one in
`mj-controller/src/server_runtime/api.rs:1228`.

*Commit 4 — `mj wait` stops lying.* In `mj-controller/src/server/api/wait.rs`, after the
summary is loaded (line 199), ask `turn_delivery` with the decision's outcome and the
summary's `produced_agent_output`. On `TurnDelivery::Unanswered`, replace the outcome with
`WaitOutcome::Error` and set the message to exactly:

    the harness ended the turn without answering; the prompt may not have been
    delivered — resend it

Keep the turn id and span in the response so a caller can still read the turn. Add tests
in `mj-controller/src/server/api/tests.rs` driving `resolve_wait` plus the summary step:
one turn with agent output returns `finished`; one without returns `error` with that
message; a cancelled turn without output still returns `cancelled`; a turn that is still
running returns nothing.

### Milestone 2 — Say it on both surfaces, and keep the text

Scope: a person watching either surface sees the same verdict, and can put the prompt
back in the composer without retyping it.

The worker already emits a warning carrying `PROMPT_EMPTY_RESPONSE_MARKER`, and both
surfaces already render worker warnings as system lines, so after Milestone 1 commit 2
the failure is visible in the transcript on both surfaces with no UI work. The text of
that marker is not addressed to a person, so:

*Commit 5 — the message a person can act on.* Change the warning the worker emits at
`mj-worker/src/acp/session.rs:578-592` to carry both the stable marker (which
`mj-core/src/credentials.rs:504` matches on, so it must not be removed) and a sentence
telling the person what happened and what to do:

    ACP prompt returned no session updates: the harness ended the turn without
    answering. The prompt may not have been acted on — resend it.

Update `mj-worker/src/acp/tests.rs:2325`, which asserts on the exact string, to assert
the marker is contained rather than equal.

*Commit 6 — recoverable text in the terminal chat.* When the chat sees a completed turn
it submitted whose transcript span carries this marker, push the prompt text onto the
existing `unsent` list (`mj-chat/src/chat.rs:354-414`) with a new `UnsentKind` headline
"Prompt was not answered", so the existing Ctrl-Alt-R restore path works unchanged. Add a
chat test in `mj-chat/src/chat/tests.rs` following the naming style there, for example
`unanswered_turn_offers_the_prompt_for_restore`, driving the event application directly
rather than through rendering.

*Commit 7 — the same in the web viewer.* In `mj-controller/src/web/viewer.js`, where the
transcript renders a warning item, recognize the marker and render the turn's user prompt
with a "Put back in composer" action, reusing the take-back path already written for
queued prompts (line 3060-3075). Add a case to the DOM-level tests in
`mj-controller/src/web/test-dom.js` if that file covers transcript rendering; otherwise
add a Playwright case under `tests/e2e/web/` following the existing fixture-driven
specs, run with `MJ_BROWSER_SPEC=<spec> npx playwright test` from `tests/e2e/web`.

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

**Unit and behaviour tests.** `cargo test` must pass on the dev profile, and
`cargo clippy --all-targets -- -D warnings` must be clean. The new tests, and what each
proves:

- `mj-core/src/acp.rs`: a usage update and an available-commands update are not agent
  output; an agent message and a tool call are; an unrecognized variant is treated as
  output.
- `mj-core/src/state.rs`: `turn_delivery` returns `Unanswered` only for a completed turn
  whose stop reason means finished and which produced nothing.
- `mj-worker/src/acp/tests.rs`: a prompt whose turn carried only non-output updates is
  reported as producing nothing. This test fails before commit 2 and passes after.
- `mj-controller/src/server/api/tests.rs`: `mj wait` returns `error` with the resend
  message for an unanswered turn, `finished` for an answered one, and is unchanged for
  cancelled, quota-limited and still-running turns. These fail before commit 4.
- `mj-chat/src/chat/tests.rs`: an unanswered turn puts its text where Ctrl-Alt-R restores
  it.

**Live test that fails on the unfixed build.** The repository already has a scripted ACP
agent used by the end-to-end lab: `tests/e2e/reliability_lab.py:408-470` writes a Python
bridge into the runtime root and configures a `codex`-kind profile named `fake` against a
`local-bare` target, with behaviour switched by environment variables such as
`MJ_FAKE_ACP_DELAY_MS` and `MJ_FAKE_ACP_PROMPT_DELAY_MS` (declared in the generated
config at line 617). Add one more switch, `MJ_FAKE_ACP_SWALLOW_PROMPT`, whose value is the
one-based index of the prompt the fake agent swallows. For that prompt the fake agent
returns `{"stopReason": "end_turn"}` and sends no `session/update` at all; for the variant
that reproduces issue #970's exact shape, it first sends one `agent_message_chunk`
containing the text `Compacting...` and then returns `end_turn` with no answer.

Then, in a new script under `tests/e2e/` following the style of the existing ones:

1. Start the lab, open a session on the `fake` profile.
2. Send a first prompt and let it be answered normally.
3. Send a second prompt with the swallow switch set to 2, and run `mj wait` on it.

Expected before the change, on both variants: `mj wait` exits reporting `finished`, and
the transcript shows the user line with nothing under it. Expected after the change:
`mj wait` exits reporting `error` with the message "the harness ended the turn without
answering; the prompt may not have been delivered — resend it", and the transcript carries
the warning line. The "Compacting..." variant is the one that fails on the unfixed build
*even with the existing detector in place*, because that detector is defeated by the
banner; state this explicitly in the test's comment, because it is the whole point.

**Acceptance, as behaviour a person can check.** With the lab running and the swallow
switch set, `mj -i fix970 wait <session>` prints an error naming the unanswered prompt
rather than success; the terminal chat shows a line under the prompt saying the harness
ended the turn without answering; pressing Ctrl-Alt-R with an empty composer restores the
prompt text; and the web viewer shows the same line with a control that puts the text
back in the composer.


## Idempotence and Recovery

Every step is repeatable. The commits are additive: commits 1 and 3 add a function and a
struct field with no behaviour change, commit 2 narrows a counter, and commit 4 changes
one mapping in the wait path. Any single commit can be reverted without breaking the
ones before it, except that commit 4 depends on commit 3's field and commit 6 depends on
commit 5's message text.

No migration is required, and none is permitted by this plan: nothing durable changes
shape. `TurnSummary` is a query result, not a stored record. If a later revision does add
a durable field, `MaterializedTurnOutcome` and `MaterializedSession` are
`#[serde(deny_unknown_fields)]`, so that change is **breaking** and must follow the
migration rules in `CLAUDE.md` — new revision, raised minimum compatible read/write
revision in the same transaction, tested with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`.

Milestones 1 and 2 make no protocol change: no new field crosses the worker-to-daemon
boundary and the HTTP wait response keeps its existing shape, only choosing a different
existing outcome value. Milestone 4, if it is ever triggered, does change the worker
state on the wire and requires bumping `PROTOCOL_VERSION` in `mj-client/src/daemon.rs`
by one.

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

The detector as it stands, from `mj-worker/src/acp/drive.rs:1065-1071`:

    pub(super) fn prompt_returned_without_updates(
        stop_reason: &StopReason,
        updates_before: u64,
        updates_after: u64,
    ) -> bool {
        *stop_reason != StopReason::Cancelled && updates_before == updates_after
    }

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

No new dependency is added by Milestones 0 to 3. Milestone 4, if triggered, may require a
newer `agent-client-protocol` crate than the 2.0.0 pinned in `Cargo.lock`.

In `mj-core/src/acp.rs`, define:

    pub fn session_update_is_agent_output(
        update: &agent_client_protocol::schema::v1::SessionUpdate,
    ) -> bool;

In `mj-core/src/transcript.rs`, define:

    pub fn item_is_agent_output(item: &TranscriptItem) -> bool;

In `mj-core/src/state.rs`, define:

    pub enum TurnDelivery { Answered, Unanswered, NotApplicable }

    pub fn turn_delivery(
        outcome: &TurnOutcomeKind,
        produced_agent_output: bool,
    ) -> TurnDelivery;

In `mj-core/src/storage.rs`, `TurnSummary` gains:

    pub produced_agent_output: bool,

In `mj-core/src/activity.rs`, only if Milestone 3 is triggered, define:

    pub struct PromotionPolicy { pub settle: Duration, pub cap: Duration }

    pub fn promotion_hold_until_ms(
        facts: &ActivityFacts,
        policy: PromotionPolicy,
        now_ms: i64,
    ) -> Option<i64>;

Nothing outside `mj_core::activity` may answer "may the next queued command be
dispatched": `promote_next_queued_command` calls this function and adds no predicate of
its own.


## Open Questions for the Maintainer

These are decisions the plan author recommends but does not own.

1. Is the bridge-source evidence enough to treat the reported compaction window as closed
   upstream, so Milestone 3 stays behind its trigger rather than being built now?
   Recommendation: yes. Building a settle window costs a new timer in the relay
   coordinator and a bound nobody can justify from evidence, to guard a window the pinned
   bridge no longer opens.

2. Should an unanswered turn make `mj wait` return `error`, or should it get its own
   `WaitOutcome` value? Recommendation: `error` with the specific message. A new outcome
   is an API change every caller must learn, and every current caller already treats
   `error` as "do not proceed", which is the right behaviour here.

3. How much weight should the false positive carry — a harness that genuinely answers
   with nothing at all would now be reported as unanswered? Recommendation: accept it.
   Local-only commands forward their result text as assistant output on the pinned
   bridges, so the observed shape is rare, and "the harness ended the turn without
   answering" is a true statement about that turn even when nothing was dropped.

4. Is Milestone 2's restore path (reusing the unsent-prompt list and Ctrl-Alt-R) the
   affordance you want, or should an unanswered prompt be offered as a one-key resend?
   Recommendation: restore, not resend. Restore puts the person in control, and the
   duplicate-execution risk of resending is real and unmeasurable from the client side.


## Revision Note

2026-09-17: first revision, written as phase 1 (plan only) for issue #970. The plan
departs from the issue's proposed design in one respect, and the reason is recorded in
`Decision Log`: the issue proposes a clock-driven undelivered state with slow-tool and
user-shell exclusions, and this plan instead takes the verdict when the turn completes,
which needs no bound and excludes those cases by construction. The clock-driven hold is
retained as Milestone 3 behind an explicit trigger, so the issue's design is not
discarded, only deferred until measurement justifies it.
