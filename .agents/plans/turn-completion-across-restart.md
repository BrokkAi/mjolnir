# Report how long a turn has been quiet, and stop guessing from it (#1017)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.
It must be maintained in accordance with `.agents/PLANS.md` at the repository root.

This plan began as a look at issue #1017, not as an ambitious design. The most
important part of it is still the finding below, which says what is already
guaranteed, what is still broken, and which losses cannot be recovered on our
side at all. Its first version then recommended a new silence bound for the two
harnesses that had none; the maintainer rejected that, on the ground that a
silence bound is a heuristic, and the design that replaces it publishes the
silence as a fact and makes every automatic ending opt-in. See `Revision notes`
at the end and the `Decision Log`.

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

After this change, a running turn tells you how long the harness has been quiet,
and Mjolnir stops pretending it can tell a dead turn from a slow one.

`mj sessions --session <id>` prints `running, no harness activity for about 11
minute(s)`. `mj wait` says the same in its timeout message, so an orchestrator
that times out has the one fact that bears on what to do next and can decide to
call `mj cancel-turn`. The terminal and web session rows show a `Quiet` clock
beside the turn and step clocks. None of this ends a turn.

Mjolnir ends a turn on its own only when something deterministic says it cannot
finish: the harness bridge process exited, its connection closed, or the worker
restarted. Those were already correct and are unchanged. The automatic ending
for silence becomes opt-in for every harness, through
`MJ_TURN_STALL_TIMEOUT_MS` and `MJ_TURN_TOOL_STALL_TIMEOUT_MS`, and is off
unless set. That is a behavior change for Muse, Kimi and Grok, which had a
ten-minute silence default; it is deliberate, and the reason is below.

What the user still loses is the turn in the original report: an adapter that
wrote its final message and made its commit and then failed to send the
`session/prompt` reply. That work exists in the workspace and in the harness's
own session files, and Mjolnir has no durable copy of the reply to replay. It
cannot be recovered. What changes is that the session says so within a minute
instead of looking like ordinary work for hours.

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
      seconds — past the bound every non-self-marking harness ran under.
- [x] (2026-09-18 01:05Z) Maintainer decision: do not add a silence bound for Codex
      and Claude. Publish the silence age instead, and make the automatic ending
      opt-in for every harness. Plan revised; see the `Decision Log`.
- [x] (2026-09-18 02:10Z) Milestone 1: publish the silence age
      (`mj-core/src/activity.rs`, the session summary, `mj wait`, the API details,
      the terminal rows and the web viewer). Commit `0e40e2bd`, with `1eab18e3`
      moving the quiet clock into the always-shown row columns after the first
      attempt put it only in the detailed clock, which is off by default.
- [x] (2026-09-18 02:15Z) Milestone 2: both stall bounds opt-in, no harness
      special case, docs. Commit `1f064ef9`.
- [x] (2026-09-18 02:40Z) Live validation in instance `fix1017`, all four checks
      below. Evidence in `Artifacts and Notes`.
- [x] (2026-09-18 02:45Z) `cargo test` 3618 passed, 0 failed, and
      `cargo clippy --all-targets -- -D warnings` clean, both on the dev profile
      outside the sandbox.
- [ ] Remaining: the Muse behavior change is argued rather than demonstrated end
      to end; see the limitation recorded in `Outcomes & Retrospective`.

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

- Decision: **superseded.** The first version of this plan recommended giving
  Codex and Claude a silence bound of their own, thirty minutes rather than the
  ten Muse used. The maintainer rejected it. See the next entry, which replaces
  it; the reasoning is kept here because the rest of the document was written
  around it.
  Date/Author: 2026-09-18, plan author.

- Decision: do not add a silence bound for Codex and Claude. Publish the silence
  age as a fact, and make the automatic ending opt-in for every harness, which
  turns the existing ten-minute default off for Muse, Kimi and Grok too.
  Rationale (the maintainer's): a silence bound is a heuristic. It cannot tell a
  turn that has stopped from a turn that is merely slow, because the evidence
  for both is identical — nothing arriving. Adding a heuristic to two more
  harnesses spreads a guess rather than fixing anything, and this plan's own
  finding argues against it: #1020 was a healthy turn destroyed by exactly this
  bound. What Mjolnir can do honestly is report what it knows. It already knows
  when the harness last spoke; it simply never said so. Publishing that turns an
  unanswerable question ("is this turn dead?") into an answerable one ("has
  anything arrived lately?") and leaves the irreversible act — ending the turn —
  with the person or the orchestrator who has context Mjolnir does not. An
  operator who wants the old automatic behavior keeps it by setting one
  environment variable, and it then applies uniformly.
  Date/Author: 2026-09-18, maintainer, recorded by the plan author.

- Decision: turning the ten-minute default off for Muse, Kimi and Grok is a
  deliberate behavior change, not collateral damage.
  Rationale: the alternative is a default that ends turns on a guess for three
  harnesses and not the other two, which is the inconsistency this ticket
  started from. Uniformly off is defensible and explainable; uniformly on is the
  #1020 failure by default; split is what we had. The loss is real and is stated
  in `Outcomes & Retrospective`: a Muse session whose relay dies mid-turn
  (#1029) will now sit visibly quiet instead of failing after ten minutes.
  Visibly quiet is the honest report, and #1029 remains open.
  Date/Author: 2026-09-18, plan author, following the maintainer's decision.

- Decision: report the silence age only once it exceeds one minute, and only
  while something is running.
  Rationale: every surface has to agree on when a turn "reads as quiet", or the
  terminal and `mj sessions` will disagree about the same session. One shared
  constant, `mj_core::activity::SILENCE_WORTH_REPORTING`, with the web viewer
  carrying a matching constant because it cannot read the Rust one. Below a
  minute, quiet is ordinary and reporting it is noise. For an idle session
  silence means nothing at all, so there is nothing to report.
  Date/Author: 2026-09-18, plan author.

- Decision: `while_disconnected` reports no silence age.
  Rationale: the daemon losing sight of a worker is not harness silence. Filling
  the field from the projection would make every daemon restart look like a
  stalled harness and send readers to cancel healthy turns — the same class of
  mistake as #1025, where a missing fact was filled in with a default.
  Date/Author: 2026-09-18, plan author.

## Outcomes & Retrospective

Both milestones landed and were validated live on 2026-09-18 in instance
`fix1017` (commits `0e40e2bd`, `1f064ef9`, `1eab18e3`). `cargo test` reports
3618 passed and 0 failed, and `cargo clippy --all-targets -- -D warnings` is
clean, both on the dev profile outside the sandbox.

The four live checks all passed, and the transcripts are in `Artifacts and
Notes`: a quiet Codex turn reports its silence in `mj sessions --session` and in
`mj wait`'s timeout message; that turn stays cancellable; with
`MJ_TURN_STALL_TIMEOUT_MS=30000` the same suspension fails the turn with
`harness_inactive` at 30011 ms, on a Codex session that the old build would never
have armed a watchdog for at all; and a healthy seventy-second turn on a worker
with no knob set survives a daemon `SIGKILL` and comes back `finished` with its
final message, so the new reporting ends nothing.

**One limitation, stated plainly.** The behavior change for Muse, Kimi and Grok —
a ten-minute silence no longer failing a turn when the knob is unset — was not
demonstrated end to end against a real Muse container. Doing so needs the
`morannon-podman` target and ten minutes of wall clock per build, and the
harness was not available in this environment. It is instead argued from two
things: `turn_stall_policy` no longer takes a harness at all, so every harness
now follows the one code path that the live Codex runs exercised in both
directions (unset, quiet for over seven minutes, not failed; set to 30 s, failed
at 30 s); and `a_stall_bound_is_off_unless_a_positive_timeout_is_configured`
pins the unset case to `None`. No build-time constant was shortened to fake the
ten-minute wait. Someone with a Muse lane should confirm it once.

What this work achieved, measured against the original purpose: the
turn described in #1017 is still not recoverable — that is settled and is not a
gap to close later — but it stops being invisible. A session whose harness has
gone quiet says so within a minute, in the three places someone looks, and an
orchestrator that times out gets the fact it needs to decide whether to cancel.

What it gave up: the automatic ending Muse, Kimi and Grok had by
default. A turn whose relay dies without the process dying (#1029) will now stay
running and quiet until someone ends it. That is the accepted cost of not
guessing, and the mitigation is the visibility this plan adds plus the opt-in
knob for an operator who wants the old behavior back.

What remains open regardless: #1029, which is a different failure — the Muse
harness continuing to execute prompts invisibly after a restart — and is not
touched here.

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

* `mj-core/src/config/harness.rs` — `HarnessKind` and its per-harness facts.
  Before this work it also held `marks_own_turn_end`, the flag that decided
  which harnesses got a stall watchdog; this plan deletes it.
* `mj-core/src/relay/snapshot.rs` — `RelayObservation`, the event type stored in the
  journal, including `CommandCompleted` and `HarnessTurnSettled`.
* `mj-core/src/relay/snapshot/apply.rs` — the state machine that folds an observation
  into the relay snapshot. This is where a turn is decided to be over.
* `mj-core/src/activity.rs` — the single activity model added on 2026-09-17 by the
  `unified-session-activity.md` plan. It holds `ActivityFacts`, `classify`,
  `ActivityState`, `StallPolicy`, `stall_verdict` and `StallVerdict`, and this
  plan adds the silence reporting to it. "Facts in, one classified state out,
  one function per question" is the rule; do not add a predicate that
  re-derives an answer somewhere else.
* `mj-worker/src/acp/session.rs` — the loop that sends one `session/prompt` and waits
  for its reply. The stall watchdog is the `verdict = async { ... }` arm of the
  `tokio::select!` near line 625.
* `mj-worker/src/acp/drive.rs` — `turn_stall_policy`, `turn_stall_timeout`,
  `TURN_STALLED_STOP_REASON` (`"harness_inactive"`) and `turn_stall_message`.
  Before this work it also held the two default bounds (ten minutes of silence,
  four hours for one tool call) and `turn_ends_only_on_prompt_reply`; this plan
  deletes all three.
* `mj-worker/src/relay/journal.rs` — the durable journal: append with `sync_data`,
  attach, acknowledge, garbage collection, and restart recovery of non-terminal
  commands.
* `mj-controller/src/session_manager/standalone.rs` — the controller side of the
  catch-up: attach, apply pages durably, then acknowledge one frontier.

A **turn** is one `session/prompt` request and everything until its reply. The turn's
durable outcome is stored as `last_turn_outcome` on the projected session and is what
`mj wait --turn <n>` answers with.

## Plan of Work

Two milestones, in this order. The first is the whole user-visible gain and is
additive: nothing behaves differently, a running session simply says more about
itself. The second changes behavior, and doing it second means the first can be
validated on its own.

### Milestone 1 — publish how long the harness has been quiet

**Scope.** The worker already knows when anything last arrived over ACP, and
already sends it: `RelayOperationalState.last_acp_activity_at_ms`
(`mj-core/src/relay/snapshot.rs`). Nothing surfaces it. This milestone carries
it through the one shared activity mechanism to the places a person or an
orchestrator reads, and adds no new predicate and no new decision.

**What exists at the end that did not before.** A running session reports its
silence age in `mj sessions --session`, in `mj wait`'s timeout message, in the
API's `activity_details`, and as a `Quiet` clock in the terminal and web rows.

The edits:

1. `mj-core/src/activity.rs` — add `last_activity_at_ms: Option<i64>` to the
   `ActivityState::Turn` and `ActivityState::Tool` variants, filled by
   `classify` from `ActivityFacts::last_acp_activity_at_ms`. Both fields are
   `#[serde(default, skip_serializing_if = "Option::is_none")]`, so the
   published state stays readable by an older controller and a newer controller
   stays readable by an older worker.

   `while_disconnected` leaves it `None` on purpose. The daemon losing sight of
   a worker says nothing about whether the harness is talking, and presenting a
   disconnection as harness silence would send a reader to cancel a healthy
   turn.

2. `mj-core/src/activity.rs` — four small public items beside them:
   `ActivityState::last_activity_at_ms`, `ActivityState::silent_for_ms(now_ms)`,
   the free `silent_for_ms(facts, now_ms)` for a caller that holds facts, and
   `silence_note(state, now_ms)`, which is the one phrase every surface prints.
   `describe_duration` moves here from `mj-worker/src/acp/drive.rs`, where it
   was `stall_duration`, so the worker's stall message and the new note agree on
   wording. `SILENCE_WORTH_REPORTING` is the shared one-minute threshold below
   which quiet is ordinary and saying so is noise.

3. Carry the timestamp to the surfaces. `SessionActivityDetails`
   (`mj-client/src/usage_format.rs`) and `ApiActivityDetails`
   (`mj-core/src/storage.rs`) each gain `last_activity_at_ms`, and
   `viewer_activity_details` (`mj-controller/src/server_runtime/snapshot.rs`)
   copies it across. The API field is optional and skipped when absent.

4. Render it.
   - `mj sessions --session <id>` (`mj-cli/src/api_commands.rs`) prints
     `running, no harness activity for about N minute(s)` under the header line.
   - `mj wait`'s timeout branch (`mj-controller/src/server/api/wait.rs`) appends
     `, with no harness activity for ...` to its message. This is the whole of
     the coordinator's point 4: an orchestrator that times out can now decide
     whether to cancel.
   - `SessionActivity::display_clock(detailed = true)`
     (`mj-client/src/usage_format.rs`) appends ` Q <clock>`, which reaches the
     terminal session rows and the chat pane.
   - `mj-controller/src/web/viewer.js` appends ` . Quiet <clock>` to the turn
     and step labels, using a `SILENCE_WORTH_REPORTING_SECONDS` constant that
     matches the Rust threshold.

**Tests.** `mj-core/src/activity/tests.rs` gains two behavior tests driven
directly from facts, with no relay and no worker:
`a_running_session_reports_how_long_the_harness_has_been_quiet` covers a quiet
turn, a quiet tool call, a session that just spoke, an idle session, and a
worker too old to report the clock; `a_disconnected_session_reports_no_silence_age`
covers the `Unknown` case. Neither duplicates an implementation list.

### Milestone 2 — both stall bounds opt-in, for every harness

**Scope.** Make the automatic ending something an operator asks for, and delete
the harness special case that decided who got one.

The edits:

1. `mj-worker/src/acp/drive.rs` — delete `DEFAULT_TURN_STALL_TIMEOUT_MS` and
   `DEFAULT_TOOL_CALL_STALL_TIMEOUT_MS`. Replace the environment reader with a
   pure `parse_stall_timeout(value: Option<&str>) -> Option<Duration>` in which
   unset, empty, `0` and anything unparseable all mean "no bound" and only a
   positive number of milliseconds arms one. `turn_stall_policy` then returns
   `{silence: None, tool_call: None}` unless the operator set something. Add
   `TURN_STALL_TIMEOUT_VARIABLE` beside the existing
   `TOOL_CALL_STALL_TIMEOUT_VARIABLE` so both names live in one place.

   The permissive parse is deliberate. A typo must not silently arm a watchdog
   that ends turns; the safe direction for a misconfigured value is off.

2. `mj-worker/src/acp/drive.rs` — delete `turn_ends_only_on_prompt_reply`, and
   `mj-core/src/config/harness.rs` — delete `HarnessKind::marks_own_turn_end`.
   With the defaults off there is nothing left to special-case, and the fact the
   flag asserted was not true of the Codex adapter anyway.

3. `mj-worker/src/acp/session.rs` — the policy selection becomes
   `spec.stall_policy.unwrap_or_else(turn_stall_policy)`, with no `if`. The
   watchdog arm keeps its `stall_policy.enabled()` guard, so an unset knob
   leaves the arm unarmed and costs nothing. When a knob *is* set, the
   `harness_inactive` path from #1020 runs exactly as it does today, for every
   harness.

4. Documentation. `.agents/docs/internal-environment-variables.md` states both
   knobs are off by default, gives the parsing rule, and records why the
   ten-minute default and the Codex/Claude exemption are both gone.
   `docs/src/content/docs/configuration.md` lists both knobs in its environment
   table. `docs/src/content/docs/api-reference.md` documents
   `details.last_activity_at_ms` and says plainly that Mjolnir publishes it and
   does not act on it. `docs/src/content/docs/sessions.md` gains a "When a turn
   goes quiet" section that tells a user what Mjolnir will and will not end, what
   the quiet clock means, and how to opt in to an automatic ending.

**Tests.** `a_stall_bound_is_off_unless_a_positive_timeout_is_configured` drives
`parse_stall_timeout` over unset, empty, blank, `0`, `off` and a negative value,
and over one positive value. It is pure, so it does not race other tests over
process-wide environment variables. `the_daemon_carries_both_stall_knobs_to_its_workers`
replaces the old default-values test: a worker re-execs with a cleared
environment, so if the daemon's carry list stops naming a knob there is no way
to arm the opt-in bound on any target. The two existing watchdog behavior tests
in `mj-worker/src/acp/tests.rs` keep working unchanged, because they inject a
policy through `LaunchSpec::stall_policy` and that path still arms the arm.

## Alternatives considered, and why they are rejected

**Add a silence bound for Codex and Claude.** This was the first version of this
plan's recommendation, and it is the option the maintainer rejected. The
argument for it was that the exemption rested on a fact the live Codex journal
disproves, so the two harnesses were left with nothing at all bounding a turn.
The argument against it, which wins: a silence bound is a heuristic, and it
cannot distinguish the case it is meant to catch from the case it destroys.
Nothing arriving is exactly what a finished-but-unreported turn and a slow
model call both look like. #1020 is the record of what a wrong guess costs, and
extending the guess to two more harnesses adds failure modes rather than
removing one. Rejected.

**Keep the ten-minute default for Muse, Kimi and Grok and only publish the
silence age.** Tempting, because it changes nothing for anyone and adds the new
visibility on top. Rejected because it keeps the split that made this ticket
confusing: the same silence ends a turn on three harnesses and not on the other
two, for a reason — "these mark their own turn ends" — that is not true of the
turn Mjolnir actually reports. A default that fires on a guess is not made
better by being inconsistent as well. The mitigation for the harnesses that lose
it is the visibility this plan adds and a one-variable opt-in.

**Complete the prompt when `HarnessTurnSettled` arrives with `prompt_in_flight`,
after a grace period.** The most principled-looking option: use the harness's own
turn-end marker instead of a timeout. Rejected because the marker is not there.
The live Codex journal has zero `harness_turn_settled` records across two turns,
including one that completed normally, and the settle is appended only when
`snapshot.harness_turn.is_some()`, which `CommandCompleted` has usually cleared
first. It would work for Claude, whose marker comes from a usage update's `_meta`
origin, but a mechanism that covers one harness and not the one in the ticket is
not worth its own code. It also adds a new way to end a turn early if an adapter
settles mid-turn, which Codex's own comment in
`mj-transcript/src/projection/observation.rs:546` warns about ("Codex reports
native execution starts for ordinary replies too").

**Reconcile against the harness's native session file after a restart.** Read
`~/.codex/sessions/*.jsonl` or Muse's `session.jsonl` and recover the final
message and the turn's fate from there. What it would guarantee: the final
message text is recovered in the cases the ticket's reporter recovered by hand.
What gets worse: a per-harness, per-adapter-version parser for an undocumented
file format, which is exactly the dependency that `mj-checkpoint` already
carries and that #1009 showed breaking on an adapter point release; reading it
requires executing inside the container for container targets, on a path that
must not block the event loop; there is no field in those files that maps a
native record onto an mj turn id, so the reconciliation has to guess, and a
wrong guess writes a fabricated completion into the durable projection; and the
recovered text cannot carry the turn's token usage or its transcript span. A
large amount of code for a partial answer that can be wrong. Rejected.

**Have the worker hold completions until acknowledged; replay from the
controller's last acknowledged ordinal.** Already implemented, and proven live.
See "(b)" in the classification above. No work.

**Do nothing at all.** Serious, and the cheapest option. #1017's headline
scenario is fixed, #1007 is fixed, #1025 is fixed, and the remaining case needs
an adapter to lose a reply, which is rare. Rejected because the cost of the
remaining case is paid entirely by the user: the session looks like ordinary
work for hours, an evaluation lane's observer blocks until its own timeout, and
the only way to find out is by hand. Publishing a timestamp the worker already
sends is a small change that removes that, without Mjolnir deciding anything it
is not entitled to decide.

## Open questions for the maintainer

All five questions from the first version of this plan have been answered. They
are kept, with their answers, because the answers are the design.

**Q1. Should Codex and Claude turns get a silence bound at all?**
*Answered: no.* A silence bound is a heuristic; see the `Decision Log`. Publish
the silence age instead and leave the decision to end a turn with the person or
the orchestrator.

**Q2. What silence bound for Codex and Claude?**
*Answered: none, and none for anyone else either by default.* Both bounds become
opt-in through `MJ_TURN_STALL_TIMEOUT_MS` and `MJ_TURN_TOOL_STALL_TIMEOUT_MS`,
and a set knob applies to every harness.

**Q3. Should the four-hour tool-call bound also apply to Codex and Claude?**
*Answered: the question dissolves.* With no default there is no bound to extend
and no exemption to justify. When an operator sets the tool knob it applies to
every harness.

**Q4. Is the extra session-summary visibility worth doing?**
*Answered: yes, and it became the main milestone rather than an optional one.*
It is the only thing here that helps the user in the case Mjolnir cannot fix.

**Q5. What should happen to #1017 itself?**
*Recommendation unchanged, and still the plan author's rather than a decision:*
comment with the live evidence that the daemon-restart claim does not reproduce,
retitle it to the unreported-completion problem, and close it when both
milestones land. Leave #1029 open; it is a different failure and this plan does
not touch it.

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

`cargo test` and `cargo clippy --all-targets -- -D warnings`, both on the dev
profile and both outside the restricted sandbox. The suite exercises loopback
TCP and Unix sockets; a sandboxed run fails with `EPERM` or hangs and is not a
result. A known flaky test (#1036, `codex_usage`, "Text file busy") can fail when
other agents build in parallel; rerun it alone before treating it as yours.

Three tests carry the behavior, and each fails on the build before its
milestone:

* `a_running_session_reports_how_long_the_harness_has_been_quiet`
  (`mj-core/src/activity/tests.rs`) — fails to compile before Milestone 1,
  because `silent_for_ms` and `silence_note` do not exist.
* `a_disconnected_session_reports_no_silence_age` (same file) — the same.
* `a_stall_bound_is_off_unless_a_positive_timeout_is_configured`
  (`mj-worker/src/acp/tests.rs`) — before Milestone 2 the equivalent assertion
  is false: an unset variable yields a ten-minute silence bound and a four-hour
  tool bound, not `None`.

The two existing watchdog tests, `a_tool_call_that_outlives_its_bound_ends_the_turn`
and `a_silent_harness_fails_the_turn_with_a_reason`, must keep passing
unchanged. They prove the `harness_inactive` path from #1020 still works when an
operator opts in, which is the half of the old behavior this plan keeps.

### Live test

Two scenarios and one regression, in a private instance. Instance name
`fix1017`, tmux session `fix1017`, port 4117. The instance configuration is
prepared in advance at `~/.config/mjolnir/instances/fix1017/config.toml` with a
`localhost` `local-bare` target; do not open, print or grep that file — it holds
an API key. Use `mj -i fix1017 profiles` and `mj -i fix1017 targets` if you need
to know what is in it.

Before anything else, deal with the stale-worker trap: a `local-bare` session
runs `target/debug/mj-worker`, which neither `cargo build --bin mj` nor
`cargo test` rebuilds. Run a full `cargo build`, point `MJ_WORKER_BINARY` at the
worktree's `target/debug/mj-worker`, and prove the running worker is the new one
rather than assuming it:

    strings ~/.local/share/mjolnir/instances/fix1017/workers/<session>/hel \
      | grep 'no harness activity for'

A fresh instance has no workspace and `mj new` then fails with a generic 500
(#1080), so run `mj -i fix1017 go` once in tmux to create one.

**Scenario A — a silent Codex turn is visible and cancellable.** Suspend the ACP
bridge *before* sending the prompt, so the prompt never reaches the harness, no
tool call can open, and the silence clock starts at send time where
`mj-worker/src/acp/session.rs` marks it. This ordering matters: `stall_verdict`
(`mj-core/src/activity.rs`) returns early while a tool call is open, so a test
that suspends the bridge mid-build measures nothing about silence.

    pstree -p <acp-supervisor pid>          # the bridge is the node child
    kill -STOP <node pid> <node child pid> <codex pid>
    mj -i fix1017 prompt --session <id> --json \
       "Reply with exactly the text FINALD and nothing else. Do not run any command."

After a minute or more of silence:

    $ mj -i fix1017 sessions --session <id>
    <id>  running  <title>
    running, no harness activity for about N minute(s)

    $ mj -i fix1017 wait --session <id> --turn <n> --timeout 20
    timeout
    the turn was still running after 20 seconds, with no harness activity for about N minute(s)

On the unfixed build both lines lack the silence, and the session row shows only
`Turn ... · Step ...`. Then prove the turn is still cancellable, which is the
whole point of reporting rather than acting: resume the bridge with `kill -CONT`
and run `mj -i fix1017 cancel-turn --session <id>`, and the turn must end.

**Scenario B — the opt-in bound still ends a turn.** Restart the `fix1017`
daemon with `MJ_TURN_STALL_TIMEOUT_MS=30000` exported, which the daemon carries
to the workers it starts. Thirty seconds differs from every former default, so
the value proves itself. Repeat scenario A's suspension; within about thirty
seconds:

    $ mj -i fix1017 wait --session <id> --turn <n> --timeout 60 --json
    "outcome": "error",
    "stop_reason": "harness_inactive",

**Regression — a healthy long turn is not ended.** With the knob unset, the
daemon-kill test from `Artifacts and Notes` must still come back `finished` with
its final message. This is the check that the new `Quiet` clock is a report and
nothing more.

**The Muse difference.** The behavior change for Muse, Kimi and Grok is that a
ten-minute silence no longer fails the turn with the knob unset. Demonstrating
it end to end against a real Muse container costs ten minutes of wall clock per
build and needs the `morannon-podman` target. If that is impractical, say so
explicitly in the report and demonstrate the same difference the cheap way: on
the unfixed build `turn_stall_policy()` with no environment set returns a
ten-minute silence bound for `HarnessKind::Muse` and the watchdog arms; on the
fixed build it returns `None` and the arm stays unarmed. Do not shorten a
build-time constant to fake the ten-minute wait without saying that is what was
done.

Clean up completely afterwards: close or destroy the sessions, stop the
`fix1017` daemon, confirm with `pgrep -af instances/fix1017` that no worker
survived and terminate any that did, kill the tmux session, and remove
`~/.config/mjolnir/instances/fix1017` and
`~/.local/share/mjolnir/instances/fix1017`. Do not run another `mj -i fix1017`
command after `daemon stop`; it starts the daemon again.

## Idempotence and Recovery

Every step is repeatable. There is no schema change, because nothing here is
stored, and no migration to classify. There is no relay protocol change either,
so `PROTOCOL_VERSION` in `mj-client/src/daemon.rs` does not move: the two new
`ActivityState` fields carry serde defaults and are skipped when empty, so an
older controller ignores them and a newer controller reading an older worker
sees `None` and reports no silence age. A mixed fleet needs no upgrade
ordering. Rolling back is deleting the commits.

Milestone 2 is the one that changes behavior, and its rollback is a
configuration change rather than a code change: an operator who wants the former
ten-minute default back sets `MJ_TURN_STALL_TIMEOUT_MS=600000` on the daemon,
which carries it to the workers it starts.

The live test creates only the `fix1017` instance directories and its sessions.
The cleanup block above removes all of it. If the daemon is stopped while a
worker survives, terminate the worker by pid before removing the directories:
deleting a running process's working files is never a substitute for stopping
it, and a surviving writer recreates whatever was removed under it.

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

## Artifacts from the implementation's live validation

Instance `fix1017`, Codex profile `deepseek`, `local-bare` target, worker proven
current by digest rather than assumption:

    $ sha256sum target/debug/mj-worker .../instances/fix1017/workers/<session>/hel
    3ec5396087506d72763eb5c7cde4ba89149c75b8342feb00f49ad1e11a8b1c70  target/debug/mj-worker
    3ec5396087506d72763eb5c7cde4ba89149c75b8342feb00f49ad1e11a8b1c70  .../workers/<session>/hel

**Check 1 - a quiet turn reports itself.** The ACP bridge was suspended with
`kill -STOP` before the prompt was sent, so no tool call could open and the
session was silent from send time. After about ninety seconds:

    $ mj -i fix1017 sessions --session cc4d7b97...
    cc4d7b97cd68b35944bf296f0ab56351  running  fix1017a
    running, no harness activity for about 1 minute(s)

    $ mj -i fix1017 wait --session cc4d7b97... --turn 8 --timeout 20
    timeout
    the turn was still running after 20 seconds, with no harness activity for about 1 minute(s)
    Error: the turn ended as timeout

Neither line exists on the build before this work: `strings` on the previously
installed `mj` finds no occurrence of "no harness activity for", and both new
`mj-core` tests fail to compile against that build because `silent_for_ms` and
`silence_note` do not exist.

The published state carries the timestamp the surfaces render from, and it
survives a daemon restart, which also exercises the reconnect path:

    $ mj -i fix1017 sessions --session cc4d7b97... --json
    activity_state {'state': 'turn', 'started_at_ms': 1789696596195,
                    'last_activity_at_ms': 1789696596202}

    $ mj -i fix1017 daemon restart && mj -i fix1017 sessions --session cc4d7b97...
    Mjolnir daemon restarted as PID 1120766.
    cc4d7b97cd68b35944bf296f0ab56351  running  fix1017a
    running, no harness activity for about 5 minute(s)

**Check 2 - a quiet turn stays cancellable.** This is why reporting rather than
acting is enough. After resuming the bridge:

    $ mj -i fix1017 cancel-turn --session cc4d7b97...
    cancelling the turn on cc4d7b97cd68b35944bf296f0ab56351
    $ mj -i fix1017 wait --session cc4d7b97... --turn 8 --timeout 30
    cancelled (Cancelled) turn 1 in 457.1s

**Check 3 - the opt-in bound still ends a turn, on a Codex session.** The daemon
was restarted with `MJ_TURN_STALL_TIMEOUT_MS=30000`, which it carries to the
workers it starts; the worker's own environment was read back to confirm it
arrived rather than assumed:

    $ tr '\0' '\n' < /proc/<worker pid>/environ | grep MJ_TURN_STALL
    MJ_TURN_STALL_TIMEOUT_MS=30000

A new session was suspended and prompted the same way:

    $ mj -i fix1017 wait --session b354b9c1... --turn 8 --timeout 90 --json
    "outcome": "error",
    "stop_reason": "harness_inactive",
    "elapsed_ms": 30011,
    "message": "The Codex turn stopped responding: mj received no activity from
                the harness for 30 second(s) while a turn was running and no tool
                call was open, so it failed the turn. ..."

Thirty seconds differs from every former default, so the value proves itself.
This is also the fail-before evidence for the other half of Milestone 2: on the
old build a Codex session's stall policy was nulled out, so no bound could be
armed for it at any setting.

**Check 4 - nothing healthy was ended.** On the worker with no knob set, a
seventy-second turn was started, the daemon was `SIGKILL`ed twenty-five seconds
in, the worker kept journaling with no daemon alive, and after the daemon came
back:

    $ mj -i fix1017 wait --session cc4d7b97... --turn 38 --timeout 60 --json
    "outcome": "finished",
    "stop_reason": "EndTurn",
    "final_message": "FINALF",

The same worker had already sat quiet for more than seven minutes during check 1
without being failed, which is the unset-knob behavior every harness now shares.

## Interfaces and Dependencies

In `mj-core/src/activity.rs`, two variants gain a field and four items appear
beside them:

    pub enum ActivityState {
        Turn {
            started_at_ms: Option<i64>,
            last_activity_at_ms: Option<i64>,
        },
        Tool {
            tool_call_id: String,
            started_at_ms: i64,
            last_activity_at_ms: Option<i64>,
        },
        // ... unchanged variants
    }

    impl ActivityState {
        pub fn last_activity_at_ms(&self) -> Option<i64>;
        pub fn silent_for_ms(&self, now_ms: i64) -> Option<u64>;
    }

    pub fn silent_for_ms(facts: &ActivityFacts, now_ms: i64) -> Option<u64>;
    pub fn silence_note(state: &ActivityState, now_ms: i64) -> Option<String>;
    pub fn describe_duration(millis: u64) -> String;
    pub const SILENCE_WORTH_REPORTING: std::time::Duration;

In `mj-core/src/storage.rs` and `mj-client/src/usage_format.rs`, the two details
structs each gain `pub last_activity_at_ms: Option<i64>`, optional and skipped
when absent on the wire.

In `mj-worker/src/acp/drive.rs`:

    pub(super) const TURN_STALL_TIMEOUT_VARIABLE: &str = "MJ_TURN_STALL_TIMEOUT_MS";
    pub(super) fn parse_stall_timeout(value: Option<&str>) -> Option<Duration>;

and `DEFAULT_TURN_STALL_TIMEOUT_MS`, `DEFAULT_TOOL_CALL_STALL_TIMEOUT_MS`,
`stall_duration` and `turn_ends_only_on_prompt_reply` are gone.
`HarnessKind::marks_own_turn_end` is gone from `mj-core/src/config/harness.rs`.

Nothing else changes. `mj_core::activity::StallPolicy`, `stall_verdict`,
`TURN_STALLED_STOP_REASON` and the `RelayObservation`/`RelayCommandOutcome` wire
types are used exactly as they are today.

**Compatibility.** No database migration: nothing here is stored. No relay
protocol change, so `PROTOCOL_VERSION` in `mj-client/src/daemon.rs` does not
move; the two new `ActivityState` fields are optional with serde defaults and
skipped when empty, so an older controller reading a newer worker's published
state ignores them and a newer controller reading an older worker's state sees
`None` and reports no silence age, which is the correct answer for a worker that
cannot measure it. The new API field on `activity_details` is additive and
optional. A mixed fleet therefore needs no upgrade ordering.

## Revision notes

**2026-09-18, first version.** Written as an investigation of #1017 rather than
a design for its literal claim, because the literal claim does not reproduce on
master: the daemon-restart recovery path the ticket asks for already exists and
was proven live under `SIGKILL`. The plan led with what is still broken — that
`HarnessKind::marks_own_turn_end` withheld every turn bound from Codex and
Claude on a premise the live Codex journal disproves — and recommended giving
those two harnesses a thirty-minute silence bound.

**2026-09-18, second version.** The maintainer rejected that recommendation: a
silence bound is a heuristic, and it cannot tell a turn that has stopped from a
turn that is merely slow, because the evidence is identical in both cases.
Adding it to two more harnesses would spread a guess that this plan's own
finding already argued against (#1020 was a healthy turn destroyed by exactly
that bound).

The design that replaces it keeps the finding and inverts the response. The
deterministic endings — bridge process exit, transport closed, worker restarted
— stay exactly as they are, because they are facts rather than guesses. The
silence age, which the worker already measures and already sends and which
nothing surfaced, is published through the shared activity mechanism to
`mj wait`, `mj sessions --session`, the API's `activity_details`, and the
terminal and web session rows. The automatic ending for silence becomes opt-in
for every harness through `MJ_TURN_STALL_TIMEOUT_MS` and
`MJ_TURN_TOOL_STALL_TIMEOUT_MS`, both off unless set, which also turns off the
ten-minute default Muse, Kimi and Grok had. With no defaults there is nothing
left to special-case, so `marks_own_turn_end` and `turn_ends_only_on_prompt_reply`
are deleted rather than corrected.

Sections rewritten for this: `Purpose / Big Picture`, `Progress`,
`Plan of Work`, `Alternatives considered`, `Open questions` (all five now
answered), `Validation and Acceptance`, `Interfaces and Dependencies`, and new
`Decision Log` and `Outcomes & Retrospective` entries recording the decision,
its rationale, and what is deliberately given up. `The finding, first`,
`Which losses are recoverable and which are not`, `Surprises & Discoveries`,
`Context and Orientation` and `Artifacts and Notes` are unchanged: the evidence
did not change, only what to do about it.
