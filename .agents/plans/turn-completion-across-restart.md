# Bound a turn whose harness reply never arrives (#1017)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.
It must be maintained in accordance with `.agents/PLANS.md` at the repository root.

This plan was written as a look at issue #1017, not as an ambitious design. The
maintainer's stated position is that the reported loss may not be fixable without
worse tradeoffs. The most important part of this document is therefore the finding
below, which says what is already guaranteed, what is still broken, and which losses
cannot be recovered on our side at all. The proposed change is deliberately small.

## The finding, first

Issue #1017 says: four sessions finished their turn inside the container (final
message written, commit made) but Mjolnir kept them "running", and after a daemon
restart they showed as idle with no final message. The issue attributes this to the
daemon restart.

On current master (base commit `e3e1d1fc`) the restart attribution is wrong, and the
real remaining hole is somewhere else.

**1. A turn that completes while the daemon is dead is fully recovered. Confirmed,
live, on master.** The whole durability chain the ticket asks for already exists and
works end to end. Evidence is in `Artifacts and Notes` below: a Codex session was
given a prompt, the daemon was `SIGKILL`ed about thirty seconds into the turn, the
worker kept running and kept journaling (relay journal grew from ordinal 11 to 89
with no daemon process in existence), the turn completed with `stop_reason EndTurn`
while no daemon existed, and after the daemon was started again
`mj wait --turn 8` answered:

    "outcome": "finished",
    "stop_reason": "EndTurn",
    "final_message": "FINALA",
    "elapsed_ms": 78568

with the turn's token usage intact, and `mj sessions --json` reported
`chat_phase idle`, `is_idle true`, `has_error false`. Nothing was lost.

**2. "ZCode" no longer exists, and the harness it became is exempt from every
watchdog. Confirmed.** Commit `e581aea0` ("Remove the ZCode harness", 2026-09-15)
deleted the kind; GLM models now run through an ordinary Codex profile pointed at
Z.ai. So #1017's harness class today is `HarnessKind::Codex`
(`mj-core/src/config/harness.rs`). `HarnessKind::marks_own_turn_end`
(`mj-core/src/config/harness.rs:465`) returns true for `Codex` and `Claude` and
false for `Kimi`, `Grok` and `Muse`. At the time of the incident ZCode was a
separate kind and was *not* self-marking, and the watchdog that now covers
non-self-marking harnesses (`be5abcca`, v2.8.0) did not exist yet.

**3. For `Codex` and `Claude` there is no bound on a turn at all. Confirmed by
reading.** In `mj-worker/src/acp/session.rs:472-479`:

    let stall_policy = if turn_ends_only_on_prompt_reply(spec.harness) {
        spec.stall_policy.unwrap_or_else(turn_stall_policy)
    } else {
        mj_core::activity::StallPolicy { silence: None, tool_call: None }
    };

and the watchdog arm at `mj-worker/src/acp/session.rs:667` is guarded by
`stall_policy.enabled()`, which is `silence.is_some() || tool_call.is_some()`
(`mj-core/src/activity.rs:460`). So for a Codex or Claude session neither the
ten-minute silence bound nor the four-hour tool-call bound is armed. The comment
above that code says the exemption is safe because "Claude and Codex mark their own
turn ends, so a lost reply cannot hang them."

**4. Nothing except the `session/prompt` reply ever ends a turn, for any harness.
Confirmed by reading.** The durable turn outcome that `mj wait` and
`last_turn_outcome` read comes only from `RelayObservation::CommandCompleted` with
a `RelayCommandOutcome::Prompt` payload
(`mj-core/src/relay/snapshot/apply.rs:255-270`). `RelayObservation::HarnessTurnSettled`
deliberately does **not** end a prompt: it only moves the execution flag to `Idle`
when `snapshot.active_prompt.is_none()`
(`mj-core/src/relay/snapshot/apply.rs:615-621`), and its own field documentation in
`mj-core/src/relay/snapshot.rs:780-787` says "The relay keeps `Running` for it." So
the premise in point 3 is false in the only sense that matters: whatever the harness
marks, the turn Mjolnir reports ends on the reply and on nothing else.

**5. The Codex adapter does not, in fact, mark its own turn ends. Confirmed, live.**
In the live session used for this investigation the relay journal contains two
`harness_turn_started` observations and **zero** `harness_turn_settled` observations
across two turns, one of which completed normally:

    $ grep -c harness_turn_settled .../relay-journal/active.jsonl
    0
    $ grep -c harness_turn_started .../relay-journal/active.jsonl
    2

`HarnessTurnStarted`/`HarnessTurnSettled` for Codex are produced in
`mj-worker/src/relay.rs:740-795` from transitions of the adapter's native goal state
(`mj_core::goal::GoalState::apply`, `mj-core/src/goal.rs:153`), and the settle is
appended only `if self.snapshot.harness_turn.is_some()` — which `CommandCompleted`
has usually already cleared. Whatever the cause, a completed Codex turn produced no
end marker of its own.

**Therefore the remaining, still-broken behavior on master is this:** for a Codex or
Claude session, if the adapter produces the final message and the commit but the
`session/prompt` reply is lost or never sent, the turn stays `Running` forever. No
watchdog is armed, no harness marker ends it, and no daemon restart helps, because
the event that would end it was never produced. That is exactly the shape #1017
reports, and it is the part that is still true today.

Two failure paths that #1017 could also have been are *not* broken, and both were
checked live:

* **Bridge death mid-turn.** Killing the ACP adapter process while a turn ran ended
  the turn in about fifteen seconds with `outcome error`, `message "Incoming
  transport closed"`, followed by `warning: ACP bridge exited; reloading the native
  session` and `session_restarted` in the journal. Visible, prompt, not silent.
* **Worker restart mid-turn.** `DurableRelay::recover_nonterminal_commands`
  (`mj-worker/src/relay/journal.rs:965-1080`) records an in-flight prompt as
  interrupted with "relay restarted while the ACP command was in flight; it was not
  replayed". Visible, not silent.

## Which losses are recoverable and which are not

The ticket asks for each loss window to be classified as (a) lost before it is
durable anywhere, (b) durable in the worker but not replayed, or (c) replayed but
misprojected. Only (b) and (c) are fixable on our side.

**(a) Lost before durable — not fixable, only boundable.** The harness produced a
turn end and the worker never received it. This covers #1017's Codex/GLM case,
#1029's Muse case, and #1007's older Muse case. The relay journal is the first
durable copy of anything, and it is written by the worker from what arrives over
ACP; an event that never arrives has no durable copy to replay. Mjolnir cannot
recover the *turn*, only detect that the turn stopped answering and say so.

**(b) Durable in the worker but not replayed — does not exist today.** The machinery
the ticket proposes as a fix is already built and already load-bearing:

* The worker makes each event durable before answering the caller.
  `DurableRelay::append_relay_event` (`mj-worker/src/relay/journal.rs:660-730`)
  writes the record and calls `file.sync_data()` before the event becomes visible.
* The controller attaches at an explicit ordinal with a digest chain.
  `RelayRequest::Attach { after_ordinal, after_digest }`
  (`mj-core/src/relay/protocol.rs:54-58`), served by
  `mj-worker/src/relay/replay.rs:127`, verified link by link in
  `RelayClient::attach` (`mj-controller/src/worker_client/relay.rs:35-78`).
* The controller persists each projection page **before** it acknowledges, and
  acknowledges only the exact frontier it reached.
  `StandaloneSession::catch_up_fixed_frontier`
  (`mj-controller/src/session_manager/standalone.rs:126-163`) and
  `apply_event_page` (same file, 309-424).
* The worker keeps unacknowledged history forever. Garbage collection is gated on
  `snapshot.retained_through()`, which only advances with the acknowledgement
  (`DurableRelay::garbage_collect_relay_history`,
  `mj-worker/src/relay/journal.rs:911-946`). There is no age or size bound that can
  drop an unacknowledged event.

So "replay the worker's durable journal from the controller's last acknowledged
ordinal on reattach" and "have the worker hold completions until acknowledged" are
both descriptions of what master already does, and the live test in point 1 proves
they work under `SIGKILL`.

**(c) Replayed but misprojected — was real, is fixed.** #1025 (a session reporting
`chat_phase idle` while its turn ran) was this: `chat_phase` defaulted to the enum's
`Idle` and was overwritten only when the daemon had a live view of the worker. It
was fixed by `52aba5d1` and the shared mechanism in `88fbe7af`
(`mj-core/src/activity.rs`), which gives every session one activity state and reports
`Unknown` rather than `Idle` for a session the daemon cannot see. #1017's reported
"chat_phase idle, has_error true" symptom is that bug, not a lost completion.

## Purpose / Big Picture

After this change, a Codex or Claude session whose harness stops answering can no
longer sit in `running` forever. The turn ends with the stop reason
`harness_inactive`, `mj wait` returns instead of blocking until its own timeout, the
transcript carries a warning line naming what happened and where to look, and the
session becomes idle so it can be closed, checkpointed, exported or re-prompted.
The user still loses the turn — the final message the harness wrote is genuinely not
recoverable through Mjolnir — but they learn that within a bounded time instead of
discovering it hours later, and the message tells them to read the harness's own
session files in the workspace.

This is a visibility fix, not a recovery fix. It is deliberately the smallest change
that removes the unbounded case, and it reuses the watchdog, the stop reason and the
diagnostic that already exist for Muse, Kimi and Grok.

## Progress

- [x] (2026-09-17 23:30Z) Read #1017, #1025, #1029, #1032, #1007, #1009 and their
      closing comments.
- [x] (2026-09-17 23:45Z) Traced the produce/durable/replay path for a turn
      completion across worker, controller and transcript projection.
- [x] (2026-09-18 00:05Z) Live: turn completing during a daemon `SIGKILL` is fully
      recovered on master. #1017's headline scenario does not reproduce.
- [x] (2026-09-18 00:10Z) Live: bridge death mid-turn ends the turn visibly in ~15s.
- [x] (2026-09-18 00:15Z) Confirmed the Codex adapter emits no `harness_turn_settled`
      for a completed turn, so the `marks_own_turn_end` exemption has no basis.
- [x] (2026-09-18 00:38Z) Live: a Codex turn with a suspended bridge, no tool call
      open and total ACP silence was still `running` after ten minutes and thirty-one
      seconds — past the bound every non-self-marking harness runs under. This is the
      still-broken behavior the plan fixes.
- [ ] Maintainer decision on the open questions below.
- [ ] Milestone 1: arm the bounds for self-marking harnesses.
- [ ] Milestone 2: name the loss in the transcript warning.

## Surprises & Discoveries

- Observation: the durability design the ticket asks for is already complete and
  already proven under `SIGKILL`. The daemon-restart half of #1017 is fixed.
  Evidence: `mj wait --turn 8` returned `finished` / `FINALA` after the daemon was
  killed mid-turn and restarted; see `Artifacts and Notes`.

- Observation: `HarnessKind::marks_own_turn_end` is used for exactly one decision —
  whether to arm the stall watchdog — and the fact it asserts is not the fact the
  turn machinery depends on. The turn ends on `CommandCompleted`, which comes only
  from the `session/prompt` reply, for every harness without exception.
  Evidence: `mj-core/src/relay/snapshot/apply.rs:255-270` versus
  `mj-core/src/relay/snapshot/apply.rs:615-621`.

- Observation: the Codex adapter does not emit a turn-end marker at all in practice,
  so even a design that completed the prompt on `HarnessTurnSettled` plus a grace
  period would not help Codex. It would help Claude, whose marker comes from a usage
  update carrying `_meta` origin (`claude_turn_origin`,
  `mj-worker/src/relay/background.rs:29-40`).
  Evidence: zero `harness_turn_settled` records against two `harness_turn_started`
  records in a journal covering one completed and one interrupted turn.

- Observation: an open tool call suppresses the silence bound entirely and leaves
  only the four-hour tool bound. `stall_verdict` (`mj-core/src/activity.rs:515-538`)
  returns early on a non-empty `tools_in_flight`. This is deliberate — it is the
  #1020 fix — and it matters twice here. It means a live test that suspends the
  bridge while a tool card is open does not distinguish "no policy at all" from "a
  Muse policy", so the code, not that test, is the proof of the Codex gap. It also
  means the silence bound will in fact catch the #1017 shape, where the harness had
  finished its tools, written its final message and committed before the reply went
  missing.

- Observation: `LaunchSpec::stall_policy` (`mj-worker/src/acp/launch.rs:49`) is
  `None` at every production call site; it exists only so tests can inject a policy.
  The real knobs are the environment variables `MJ_TURN_STALL_TIMEOUT_MS` and
  `MJ_TURN_TOOL_STALL_TIMEOUT_MS`. That means the change proposed here is entirely
  worker-local: no relay protocol change, no launch-config field, no database
  migration, and no musl-worker re-pin hazard beyond rebuilding the worker.

- Observation: #1029 is a different failure from #1017 and is not addressed here. In
  #1029 the Muse harness keeps executing prompts invisibly after a restart, because
  `muse serve` outlives the adapter. For Codex the adapter owns the model loop and
  dies with the bridge, which the live bridge-kill test confirmed: the whole child
  tree including the sandboxed `codex` process was gone.

## Decision Log

- Decision: treat #1017 as two separate claims and answer them separately.
  Rationale: the restart claim is disproved on master; the "finished but still
  running" claim is real and has a different cause. Merging them would produce a
  design for a bug that no longer exists.
  Date/Author: 2026-09-18, plan author.

- Decision: recommend bounding the turn rather than recovering it.
  Rationale: the lost event has no durable copy anywhere in Mjolnir, so there is
  nothing to replay. Every recovery design has to read the harness's own files, and
  that is a per-harness, per-target, unversioned dependency with no way to map a
  native record onto an mj turn id or its usage. See `Alternatives considered`.
  Date/Author: 2026-09-18, plan author.

- Decision: keep the existing `harness_inactive` stop reason rather than introducing
  a new one.
  Rationale: `mj wait`, the session summary and the transcript already understand it
  (`TURN_STALLED_STOP_REASON`, `mj-worker/src/acp/drive.rs`), and #1032's closing
  comment states it was added precisely to tell this symptom apart from a plain
  stall. Adding a second reason would split the same signal in two.
  Date/Author: 2026-09-18, plan author.

- Decision: give self-marking harnesses a longer silence bound than the ten minutes
  Muse uses, rather than the same one.
  Rationale: a Codex or Claude turn can legitimately spend a long time in a single
  model call with no tool card open, and #1020 was caused by a bound that was too
  aggressive. A bound that is too long only delays a failure the user can already end
  with `mj cancel-turn`; a bound that is too short destroys real work.
  Date/Author: 2026-09-18, plan author.

## Outcomes & Retrospective

Not started. To be written when Milestone 1 lands. The success test is the live test
in `Validation and Acceptance`: on the build before the change the suspended-bridge
Codex session stays `running` indefinitely; on the build after it, the turn ends with
`harness_inactive` within the configured bound.

## Context and Orientation

This section assumes no prior knowledge of the repository.

Mjolnir runs coding agents. A **harness** is a coding agent program such as Codex,
Claude Code or Muse. Mjolnir does not talk to a harness directly; it talks to an
**ACP bridge** (also called the adapter), a small process that speaks the Agent
Client Protocol on its standard input and output and drives the harness. The
**worker** (`mj-worker`, binary `hel`) is a process that lives beside the workspace,
starts the bridge as its child, and records everything the bridge reports into a
durable append-only file called the **relay journal**. The **daemon** (the
controller, `mj-controller`) is the long-lived process the `mj` command line talks
to; it connects to each worker over a Unix socket, reads the journal forward, and
writes a **projection** of it into a SQLite database. The projection is what
`mj sessions`, `mj wait` and `mj transcript` read.

The relevant files:

* `mj-core/src/config/harness.rs` — `HarnessKind` and its per-harness facts,
  including `marks_own_turn_end` at line 465.
* `mj-core/src/relay/snapshot.rs` — `RelayObservation`, the event type stored in the
  journal, including `CommandCompleted` and `HarnessTurnSettled`.
* `mj-core/src/relay/snapshot/apply.rs` — the state machine that folds an observation
  into the relay snapshot. This is where a turn is decided to be over.
* `mj-core/src/activity.rs` — the single activity model added on 2026-09-17 by the
  `unified-session-activity.md` plan. It holds `StallPolicy`, `stall_verdict` and
  `StallVerdict`.
* `mj-worker/src/acp/session.rs` — the loop that sends one `session/prompt` and waits
  for its reply. The stall watchdog is the `verdict = async { ... }` arm of the
  `tokio::select!` near line 625.
* `mj-worker/src/acp/drive.rs` — `turn_ends_only_on_prompt_reply`,
  `turn_stall_policy`, `turn_stall_timeout`, `DEFAULT_TURN_STALL_TIMEOUT_MS`
  (600000, ten minutes), `DEFAULT_TOOL_CALL_STALL_TIMEOUT_MS` (four hours),
  `TURN_STALLED_STOP_REASON` (`"harness_inactive"`), and `turn_stall_message`.
* `mj-worker/src/relay/journal.rs` — the durable journal: append with `sync_data`,
  attach, acknowledge, garbage collection, and restart recovery of non-terminal
  commands.
* `mj-controller/src/session_manager/standalone.rs` — the controller side of the
  catch-up: attach, apply pages durably, then acknowledge one frontier.

A **turn** is one `session/prompt` request and everything until its reply. The turn's
durable outcome is stored as `last_turn_outcome` on the projected session and is what
`mj wait --turn <n>` answers with.

## Plan of Work

### Milestone 1 — arm the bounds for Codex and Claude

**Scope.** Remove the blanket exemption in `mj-worker/src/acp/session.rs` so that
every harness runs under a stall policy, and give self-marking harnesses their own,
longer silence default. Nothing else changes: the same watchdog code, the same
`harness_inactive` stop reason, the same diagnostic, the same environment overrides.

**What exists at the end that did not before.** A Codex or Claude session whose
bridge stops answering ends its turn within a bounded time with a stated reason,
instead of staying `Running` forever.

The edits:

1. In `mj-core/src/config/harness.rs`, replace `marks_own_turn_end` with a function
   that returns the silence bound the harness should run under, because that is the
   only decision the flag was ever used for and its name asserts something that is
   not true of the Codex adapter. Suggested shape:

       /// How long a turn of this harness may go completely silent — no ACP
       /// traffic at all and no tool call open — before the worker gives up on
       /// it and records the turn as `harness_inactive`.
       ///
       /// Every harness needs one. The turn Mjolnir reports ends only when the
       /// `session/prompt` reply arrives (see
       /// `mj-core/src/relay/snapshot/apply.rs`), so a lost reply hangs any
       /// harness, including the ones that publish a turn-end marker of their
       /// own. Codex and Claude get a longer bound because they can spend a long
       /// time inside one model call with no tool card open; ten minutes there
       /// would fail healthy turns, which is what #1020 was.
       pub const fn turn_silence_bound(self) -> Duration {
           match self {
               Self::Codex | Self::Claude => Duration::from_secs(30 * 60),
               Self::Kimi | Self::Grok | Self::Muse => Duration::from_secs(10 * 60),
           }
       }

   Delete `marks_own_turn_end` and `turn_ends_only_on_prompt_reply`
   (`mj-worker/src/acp/drive.rs:976`), and update the test
   `the_stall_watchdog_covers_only_harnesses_whose_turn_ends_on_the_reply`
   (`mj-worker/src/acp/tests.rs:4234`) to assert the new per-harness bounds instead.

2. In `mj-worker/src/acp/drive.rs`, make `turn_stall_policy` take the harness and use
   `harness.turn_silence_bound()` as the default that `MJ_TURN_STALL_TIMEOUT_MS`
   overrides. Keep `timeout_from_environment`'s existing rule that `0` removes the
   bound, so an operator can still opt out per session through the target's container
   environment. Keep the four-hour `MJ_TURN_TOOL_STALL_TIMEOUT_MS` default unchanged
   and now apply it to every harness.

3. In `mj-worker/src/acp/session.rs:472-479`, delete the `if`/`else` and always take
   `spec.stall_policy.unwrap_or_else(|| turn_stall_policy(spec.harness))`. The
   watchdog arm's `stall_policy.enabled()` guard stays as it is, so setting both
   variables to `0` still disables the watchdog completely.

4. In `mj-worker/src/acp/drive.rs`, extend `turn_stall_message` so the text for a
   silence verdict tells the reader that the harness may have finished work that
   Mjolnir did not see, and where to look. The message already travels with the
   outcome as a `TurnDiagnostic`, so `mj wait --json` and the session summary both
   carry it. Proposed text for the silence case:

       the harness sent nothing for 30m and the turn was recorded as
       harness_inactive; if it had already finished, its own session files in the
       workspace hold the final message and any commit it made

**Tests.** Add behavior tests next to the code, not lists that mirror it:

* In `mj-worker/src/acp/tests.rs`, a test that `turn_stall_policy(HarnessKind::Codex)`
  and `turn_stall_policy(HarnessKind::Claude)` are `enabled()` and carry a silence
  bound, and that `MJ_TURN_STALL_TIMEOUT_MS=0` disables it. This is the test that
  fails on the unfixed build: today the Codex policy is `{silence: None, tool_call:
  None}`.
* In `mj-core/src/activity.rs` tests, `stall_verdict` already has coverage; add one
  case only if the per-harness bound introduces a new path, which it should not.
* A worker-level behavior test that a Codex session whose bridge goes silent loses
  its turn. `mj-worker/src/acp/tests.rs` already has the Muse version of exactly this
  — `silent_bridge_spec` (line 2512, which hard-codes `HarnessKind::Muse` with the
  comment "Muse marks no turn end of its own, so the watchdog covers it") feeding
  `a_tool_call_that_outlives_its_bound_ends_the_turn` (line 2605) and the silence
  test after it. Give `silent_bridge_spec` a harness parameter, keep the existing
  Muse callers, and add a Codex case that asserts `PromptFinished` with
  `stop_reason == TURN_STALLED_STOP_REASON`. Injecting the short policy through
  `LaunchSpec::stall_policy` is enough to make this fail on the unfixed build,
  because today the `else` branch at `mj-worker/src/acp/session.rs:475-478` throws
  the injected policy away for a self-marking harness and substitutes
  `{silence: None, tool_call: None}`. After the change the injected policy is used
  and the turn ends.

**Commit.** One commit. Message states that the turn Mjolnir reports ends only on the
prompt reply, that the Codex adapter emits no end marker of its own, and that the
exemption therefore left Codex and Claude turns unbounded. Reference #1017.

### Milestone 2 — make the remaining loss legible (optional, small)

**Scope.** The turn now ends, but the session summary does not say "this turn's
result may exist in the workspace and not in Mjolnir". Milestone 1 puts that in the
diagnostic; this milestone puts it where a person scanning `mj sessions` will see it.

**The edit.** `mj-transcript/src/projection/observation.rs` already projects
`RelayObservation::Warning` as a system transcript line. Nothing new is needed there.
The only addition is in the session summary that `mj sessions --json` returns: when
`last_turn_outcome` is a completion whose stop reason is `harness_inactive`, surface
a short `last_turn_diagnostic` note rather than leaving the caller to parse the stop
reason. #1032's closing comment says `last_turn_diagnostic` already exists, so this
is a presentation change in `mj-controller/src/server/api` and the viewer, not new
state.

**Whether to do it at all** is open question Q4 below. It is genuinely optional: the
information is already in `mj wait --json` after Milestone 1.

## Alternatives considered, and why they are rejected

**Do nothing.** Serious, and cheaper than anything else. The argument for it: #1017's
headline scenario is fixed, #1007 is fixed, #1025 is fixed, and the remaining case
requires a harness adapter to lose a reply, which is rare. The argument against it:
when it does happen there is no bound at all, the session sits in `running` for
hours, an evaluation lane's observer blocks until its own timeout, and the operator
finds out by hand. The Muse-class harnesses already have this bound; the Codex class
has it withheld on a premise that the live journal disproves. Cost of the fix is
roughly eighty lines and one new default constant. Recommend fixing.

**Complete the prompt when `HarnessTurnSettled` arrives with `prompt_in_flight`, after
a grace period.** This is the most principled-looking option: use the harness's own
turn-end marker instead of a timeout. It is rejected because the marker is not there.
The live Codex journal has zero `harness_turn_settled` records across two turns,
including one that completed normally, and the settle is appended only when
`snapshot.harness_turn.is_some()`, which `CommandCompleted` has usually cleared
first. It would work for Claude, whose marker comes from a usage update's `_meta`
origin, but a fix that covers one of the two exempt harnesses and not the one in the
ticket is not worth its own mechanism. It also adds a new way to end a turn early if
an adapter settles mid-turn, which Codex's own comment in
`mj-transcript/src/projection/observation.rs:546` warns about ("Codex reports native
execution starts for ordinary replies too").

**Reconcile against the harness's native session file after a restart.** Read
`~/.codex/sessions/*.jsonl` or Muse's `session.jsonl` and recover the final message
and the turn's fate from there. What it would guarantee: the final message text is
recovered in the cases the ticket's reporter recovered by hand. What gets worse:
a per-harness, per-adapter-version parser for an undocumented file format, which is
exactly the dependency that `mj-checkpoint` already carries and that #1009 showed
breaking on an adapter point release; reading it requires executing inside the
container for container targets, on a path that must not block the event loop; there
is no field in those files that maps a native record onto an mj turn id, so the
reconciliation has to guess, and a wrong guess writes a fabricated completion into
the durable projection; and the recovered text cannot carry the turn's token usage or
its transcript span. It is a large amount of code for a partial answer that can be
wrong. Reject.

**Have the worker hold completions until acknowledged; replay from the controller's
last acknowledged ordinal.** Already implemented, and proven live. See "(b)" in the
classification above. No work.

**Make the loss visible instead of fixing it.** This is what Milestone 1 actually is.
The naming matters: a bound plus a stated reason is the visibility option, not a
lesser version of a recovery option. There is no recovery option that does not read
harness files.

## Open questions for the maintainer

**Q1. Should Codex and Claude turns get a silence bound at all?** The evidence says
the exemption rests on a fact that is not true of the shipped Codex adapter, and that
the turn machinery does not depend on that fact anyway. The risk is failing a healthy
long model call. Note what the change does *not* touch: the watchdog arm is guarded
by `prompt_running`, so a turn the harness started on its own — Claude's autonomous
work outside a user prompt — is unaffected either way. *Recommendation: yes.*

**Q2. What silence bound for Codex and Claude?** Ten minutes matches Muse but is what
caused #1020. *Recommendation: thirty minutes, overridable with the existing
`MJ_TURN_STALL_TIMEOUT_MS`, and `0` still disables it.*

**Q3. Should the four-hour tool-call bound also apply to Codex and Claude?** It exists
to catch a bridge that leaves a tool card open without the process dying. A bridge
process that exits is already detected at once by the `child.wait()` arm in
`mj-worker/src/acp.rs`, which the live test confirmed takes about fifteen seconds.
*Recommendation: yes — it is free once the policy is no longer nulled out, and four
hours cannot plausibly fail healthy work.*

**Q4. Is Milestone 2 worth doing?** After Milestone 1 the reason is in
`mj wait --json` and in the transcript. Milestone 2 only moves it into the session
summary. *Recommendation: skip it for now; reopen if an operator reports missing it.*

**Q5. What should happen to #1017 itself?** Its headline claim is disproved on
master and two of its three symptoms were separate, since-fixed bugs. *Recommendation:
comment with the live evidence, retitle it to the unbounded-Codex-turn problem, and
close it when Milestone 1 lands. Leave #1029 open; it is a different failure and this
plan does not touch it.*

## Concrete Steps

All commands run from the repository root, outside the restricted sandbox, on the dev
profile. Substitute your own worktree path.

Build and run the checks:

    cd /path/to/mjolnir
    cargo build
    cargo test
    cargo clippy --all-targets -- -D warnings

`cargo test` exercises loopback TCP and Unix sockets and must run with elevated
permissions, or it fails with `EPERM` or hangs. A known flaky test (#1036,
`codex_usage`, "Text file busy") can fail when other agents build in parallel; rerun
it alone before treating it as yours.

Note the stale-worker trap: a `local-bare` session runs `target/debug/mj-worker`, and
neither `cargo build --bin mj` nor `cargo test` rebuilds it. Run a full `cargo build`
before any live test of worker code, and prove the running worker is current, for
example:

    strings ~/.local/share/mjolnir/instances/<instance>/workers/<session>/hel \
      | grep harness_inactive

## Validation and Acceptance

### Unit level

`cargo test` must pass.

The fail-before/pass-after pair is the worker-level behavior test, not the policy
test. The policy test cannot fail on the unfixed build, because `turn_stall_policy`
does not take a harness there and the test would not compile; it is a guard against
regression, not a demonstration. The behavior test can be written against today's
API: build a `LaunchSpec` with `harness: HarnessKind::Codex` and
`stall_policy: Some(StallPolicy { silence: Some(200ms), tool_call: Some(1h) })`,
drive it with the existing `silent_after_prompt_bridge`, and assert a
`RuntimeEvent::PromptFinished` with `stop_reason == TURN_STALLED_STOP_REASON`
arrives. Write it first and watch it hang, then make the change and watch it pass.

### Live test

This is the test that proves the change, and it fails on the unfixed build. It uses a
private instance so it cannot touch anything else. Use instance name `plan1017`,
tmux session `plan1017` and port 4117, or your own equivalents.

Set up:

    mkdir -p ~/.config/mjolnir/instances/plan1017
    cp ~/.config/mjolnir/instances/campaign0916/config.toml \
       ~/.config/mjolnir/instances/plan1017/config.toml

Edit that copy: set `[phone] bind` to `127.0.0.1:4117` and append

    [targets.localhost]
    kind = "local-bare"

The copied file contains an API key. Never print, paste or commit it.

In a tmux session, with `MJ_WORKER_BINARY` pointing at your freshly built
`target/debug/mj-worker`, run `mj -i plan1017 go` once from a small git repository to
create a workspace (a fresh instance has none and `mj new` then fails with a generic
500, which is #1080). Then create a session:

    mj -i plan1017 new --profile deepseek --target localhost --workspace proj \
       --project-directory /path/to/small/repo --title plan1017a

Two details matter, and getting either wrong makes the test prove nothing.

First, **no tool call may be open when the bridge goes silent.** `stall_verdict`
(`mj-core/src/activity.rs:515-538`) returns early when `tools_in_flight` is
non-empty and then judges only against the four-hour tool bound; the silence bound
applies only when nothing is in flight. That is the #1020 fix and it is correct. It
also matches the #1017 shape, where the harness had finished its tools, written its
final message and made its commit before the reply went missing. So the prompt for
this test must be one the harness answers without running a tool, for example
"Reply with exactly the text FINALD and nothing else. Do not run any command."

Second, **set the bound short so the test finishes.** `MJ_TURN_STALL_TIMEOUT_MS` is
read by the worker at prompt time, so put it in the session's environment before the
session is created — for a `local-bare` target, export it in the shell that starts
the daemon; for a container target, use the target's `[targets.<id>.container]
environment`. `60000` (one minute) is a good value and differs from every default.

The reliable way to arrange both conditions is to suspend the bridge **before**
sending the prompt. The prompt then never reaches the harness, no tool call can open,
and the silence clock starts at send time, which is where
`mj-worker/src/acp/session.rs:469` marks it. Find the ACP bridge — it is the `node`
child of the worker's `acp-supervisor` process — and suspend the whole chain:

    pstree -p <acp-supervisor pid>
    kill -STOP <node pid> <node child pid> <codex pid>
    mj -i plan1017 prompt --session <id> --json \
       "Reply with exactly the text FINALD and nothing else. Do not run any command."

**On the unfixed build**, the session stays `running` indefinitely. After the bound
has long passed:

    $ mj -i plan1017 sessions --session <id> --json
    "state": "running", "chat_phase": "running", "is_idle": false

    $ mj -i plan1017 wait --session <id> --turn <n> --timeout 20
    timeout
    the turn was still running after 20 seconds
    Error: the turn ended as timeout

and it answers that at every later attempt, for as long as the bridge stays silent.

**On the fixed build**, within the bound the turn ends:

    $ mj -i plan1017 wait --session <id> --turn <n> --timeout 30 --json
    "outcome": "error",
    "stop_reason": "harness_inactive",
    "message": "the harness sent nothing for 1m and the turn was recorded as
                harness_inactive; ..."

and `mj sessions --session <id> --json` reports `chat_phase idle`, `is_idle true`.

Afterwards, resume the suspended processes (`kill -CONT`) or let teardown remove
them, then clean up completely:

    mj -i plan1017 daemon stop
    pgrep -af instances/plan1017     # terminate any survivor first
    rm -rf ~/.config/mjolnir/instances/plan1017
    rm -rf ~/.local/share/mjolnir/instances/plan1017

Do not run another `mj -i plan1017` command after `daemon stop`; it starts the daemon
again.

### Regression test to keep

Re-run the daemon-kill test from `Artifacts and Notes` after the change, to prove the
new bound does not fire on a healthy turn that outlives its daemon. The turn must
still come back as `finished` with its final message.

## Idempotence and Recovery

Every step is repeatable. The code change is additive and local to the worker; there
is no schema change, no relay protocol change, and no launch-config field, so
`PROTOCOL_VERSION` in `mj-client/src/daemon.rs` does not move and no migration needs
classifying. Rolling back is deleting the commit; a worker built from the previous
commit and a daemon built from this one interoperate, because nothing on the wire
changed.

The live test creates only the `plan1017` instance directories and one session. The
cleanup block above removes all of it. If the daemon is stopped while a worker
survives, terminate the worker by pid before removing the directories: deleting a
running process's working files is never a substitute for stopping it, and a
surviving writer recreates whatever was removed under it.

## Artifacts and Notes

**Live evidence that a completion survives a daemon `SIGKILL` (current master,
`e3e1d1fc`, Codex profile `deepseek`, `local-bare` target).**

The prompt was "Run this exact shell command and wait for it to finish: sleep 75.
Then reply with exactly the text FINALA and nothing else." `mj prompt --json`
returned `turn_id 8`. About thirty seconds in, with the session reporting
`chat_phase running`, the daemon was killed with `kill -9`. The worker survived:

    2738041 .../workers/185df.../hel worker run --root ... --config .../launch.json
    2738357 .../workers/185df.../hel worker acp-supervisor --spec .../acp-supervisor.json

With no daemon process in existence the journal kept growing, and the turn completed:

    {"recorded_at_ms":1789689289515,
     "command_id":"api-5d3195bc01ba76f09c57ca6bada0b6c7",
     "observation":{"type":"command_completed",
       "data":{"command_id":"api-5d3195bc...",
         "outcome":{"type":"prompt","data":{"stop_reason":"EndTurn",
           "usage":{"scope":"turn","total_tokens":30875,...}}}}}}

with the final message journaled as three chunks at ordinals 78, 79 and 80 spelling
`F` / `INAL` / `A`. After starting the daemon again:

    $ mj -i plan1017 wait --session 185df... --turn 8 --timeout 30 --json
    {
      "usage": { "scope": "turn", "total_tokens": 30875, ... },
      "outcome": "finished",
      "stop_reason": "EndTurn",
      "final_message": "FINALA",
      "turn_id": 8,
      "turn_number": 1,
      "elapsed_ms": 78568,
      "relay": { "state": "connected" }
    }

    $ mj -i plan1017 sessions --session 185df... --json
    state = running
    lifecycle = live
    chat_phase = idle
    is_idle = True
    has_error = False
    activity_state = {'state': 'idle', 'since_ms': 1789689289515}

**Live evidence that a bridge death mid-turn is visible and prompt.** A second turn
(`turn_id 96`) was started and the bridge `node` process was killed with `kill -9`
about fifteen seconds in:

    $ mj -i plan1017 wait --session 185df... --turn 96 --timeout 60 --json
    {
      "diagnostic": { "message": "Incoming transport closed", "code": "Internal error" },
      "outcome": "error",
      "stop_reason": "error",
      "message": "Incoming transport closed",
      "turn_id": 96,
      "elapsed_ms": 14957
    }

and the journal recorded, in order, `warning: prompt failed: Incoming transport
closed`, `command_completed` with `stop_reason error`, `warning: ACP bridge exited;
reloading the native session`, `session_restarted`, `agent_initialized`,
`session_opened` with `resumed: true`. The whole bridge child tree, including the
sandboxed `codex` process running `sleep 90`, was gone.

**Live evidence that the Codex adapter emits no turn-end marker.** Over the same
journal, covering one completed turn and one interrupted turn:

    $ grep -c harness_turn_started .../relay-journal/active.jsonl
    2
    $ grep -c harness_turn_settled .../relay-journal/active.jsonl
    0

**Live evidence that a silent Codex turn is unbounded.** This is the discriminating
test. The ACP bridge was suspended with `kill -STOP` *before* the prompt was sent, so
the prompt never reached the harness, no tool call ever opened, and the session was
completely silent from the moment the ACP activity clock was marked at send time
(`mj-worker/src/acp/session.rs:469`). The turn started at epoch-ms 1789690077968 and
the session was read at epoch second 1789690709 — **ten minutes and thirty-one
seconds of total silence**, past the ten-minute bound a Muse, Kimi or Grok session
runs under:

    state = running
    chat_phase = running
    is_idle = False
    has_error = False
    activity_state = {'state': 'turn', 'started_at_ms': 1789690077968}

and throughout:

    $ mj -i plan1017 wait --session 185df... --turn 175 --timeout 20
    timeout
    the turn was still running after 20 seconds
    Error: the turn ended as timeout

Nothing in Mjolnir will ever end that turn.

An earlier variant of the same test, with a `sleep 240` tool card left open, is
weaker evidence and is recorded here only to be honest about it: with a tool in
flight `stall_verdict` judges against the four-hour tool bound, so a Muse session
would have survived it too. That turn stayed `running` through eleven minutes of a
fully suspended bridge and then completed normally once the processes were resumed:

    $ mj -i plan1017 wait --session 185df... --turn 150 --timeout 30 --json
    finished EndTurn 'FINALC' 678118

which is itself a useful result: the relay reattaches cleanly after a long harness
freeze and does not lose the turn.

## Interfaces and Dependencies

In `mj-core/src/config/harness.rs`, replacing `marks_own_turn_end`:

    impl HarnessKind {
        pub const fn turn_silence_bound(self) -> std::time::Duration;
    }

In `mj-worker/src/acp/drive.rs`, `turn_stall_policy` gains the harness:

    pub(super) fn turn_stall_policy(
        harness: mj_core::config::HarnessKind,
    ) -> mj_core::activity::StallPolicy;

and `turn_ends_only_on_prompt_reply` is deleted.

In `mj-worker/src/acp/session.rs`, the policy selection becomes unconditional:

    let stall_policy = spec
        .stall_policy
        .unwrap_or_else(|| turn_stall_policy(spec.harness));

Nothing else changes. `mj_core::activity::StallPolicy`,
`mj_core::activity::stall_verdict`, `TURN_STALLED_STOP_REASON` and the
`RelayObservation`/`RelayCommandOutcome` wire types are used exactly as they are
today, so there is no relay protocol change and `PROTOCOL_VERSION` in
`mj-client/src/daemon.rs` does not move.

## Revision note

2026-09-18, first version. Written as an investigation of #1017 rather than as a
design for its literal claim, because the literal claim does not reproduce on master:
the daemon-restart recovery path the ticket asks for already exists and was proven
live under `SIGKILL`. The plan therefore leads with what is still broken — that
`HarnessKind::marks_own_turn_end` withholds every turn bound from Codex and Claude on
a premise the live Codex journal disproves — and proposes the smallest change that
removes the unbounded case, while stating plainly that the lost turn itself is not
recoverable on our side.
