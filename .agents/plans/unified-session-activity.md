# One shared answer to "is this session working, idle, or stalled"

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

The rules for this document are in `.agents/PLANS.md` from the repository root, and this
document must be maintained in accordance with that file.

## Purpose / Big Picture

Mjolnir (the `mj` command, and the background process it calls the daemon) runs coding
agents inside containers or on remote machines. Today it answers the question "is this
session working right now?" in six different places, with six different sets of facts, and
those answers disagree. Two user-visible failures come directly from the disagreement:

Issue #1020: a turn is failed after ten minutes of silence even though the agent is alive
and blocked inside one long tool call (a `cargo nextest` run, a background job poll). The
watchdog that fails the turn looks only at "when did anything last arrive over the agent
protocol", which is silent for the whole of a twenty-minute build. Five of six evaluation
lanes lost their first turn this way. The second half of #1020 is that the failure carries
no reason: `mj wait` prints only "the turn ended as error", the session summary shows no
error text, and the only trace is a sentence in the transcript.

Issue #1025: after the daemon restarts, a session whose turn is still running reports
`chat_phase: idle` with `has_error: true` and no error text, while `mj wait` on the same
session correctly answers "still running". Automation that reads `chat_phase == idle` as
"the turn finished" treats a running turn as finished.

After this change:

* A turn blocked in a long tool call is not failed. The watchdog counts an in-flight tool
  call as activity, and only a much longer, separately configured bound can end it.
* A turn that really is failed for silence says so: `mj wait` prints
  `error (harness_inactive)` followed by a sentence naming how long the silence was and
  what was in flight; `mj sessions --session <id> --json` carries the same text in `error`;
  the worker log carries the same line.
* A session whose turn spans a daemon restart never reports itself idle. While the daemon
  has no live connection to the worker it reports what its durable record last knew
  (`running`), not the default value of an enum.
* There is one implementation of "active / idle / stalled / safe to restart / safe to
  checkpoint" in `mj-core`, produced by the worker, published once in the state the worker
  already sends, and read unchanged by the daemon, the CLI, the terminal UI and the web
  viewer. The other five are deleted or become one-line calls into it.

You can see it working with the live test in `Validation and Acceptance`: with the stall
timeout shortened to thirty seconds, a `sleep 120` tool call must not fail the turn, a
harness that is really silent must fail it with a visible reason, and killing the daemon
mid-turn must keep the session reporting `running`.

## Progress

- [x] (2026-09-17 21:00Z) Milestone 1 (`abfe82af`): `mj-core/src/activity.rs` with
      `ActivityFacts`, `ActivityState`, `classify`, `has_work_in_flight`, `is_quiet`,
      `safe_to_replace`, `checkpoint_blocker`, `stall_verdict`, `while_disconnected`, the
      shared `ToolsInFlight` tracker, and fourteen tests driven directly.
- [x] (2026-09-17 21:20Z) Milestone 2 (`b3ce0f5b`): the worker's private tool map and its
      private idle predicate are gone; `tools_in_flight` and `activity` are published in
      `RelayOperationalState`, with a fallback in both directions for an older peer.
- [x] (2026-09-17 21:35Z) Milestone 3 (`ca210226`): the watchdog reads `stall_verdict`; a
      tool call is a sign of life; `MJ_TURN_TOOL_STALL_TIMEOUT_MS` bounds one tool call at
      four hours by default; the failure carries the `harness_inactive` stop reason and a
      diagnostic.
- [x] (2026-09-17 21:45Z) Milestone 4 (`12441e49`): `mj wait` prints the diagnostic.
      `ApiSession.error` was **not** widened: the daemon deliberately does not publish raw
      runtime error text for a running session, and a turn's reason already travels in
      `last_turn_diagnostic`. See the Decision Log.
- [x] (2026-09-17 22:05Z) Milestone 5 (`381eb93a`): every session gets one activity state;
      a session the daemon cannot see reports what was last known; `activity_state` is
      published; `PROTOCOL_VERSION` is 24.
- [x] (2026-09-17 22:15Z) Milestone 6 (`3da4eb51`): the checkpoint defer, the barrier wait
      and the restart-readiness wait ask the shared predicate.
- [x] (2026-09-17 22:40Z) Milestone 7, added during the work (`b4ada197`): two behaviour
      tests drive the real ACP loop against a scripted silent bridge, and the daemon
      carries the turn bounds to the workers it starts.
- [x] (2026-09-17 22:45Z) Milestone 8, from review (`42bdcbdf`, `a40a8a60`): the watchdog
      wakes at most a second apart, a session nobody can see always holds work, a wait can
      never conclude a turn finished while the session is unaccounted for, and the last
      hand-rolled predicates — the worker's capacity-retry admission, the file-write
      barrier, the close's cancellation, and `SessionActivity::is_idle` — became calls into
      the mechanism.
- [ ] Remaining: the tool-call bound has never been seen to fire in a live Muse session,
      although it fires in the ACP-loop test (completed: the live silence bound, the live
      restart evidence, the end-to-end tests; remaining: one instrumented live run logging
      what `turn_stall_facts` sees, to find out whether Muse's shell cards register as
      in-flight tool calls at all).

## Surprises & Discoveries

- Observation: the worker's idle clock runs on every journal append, so the facts it
  decides with have to be cheap to assemble. Building a whole operational state there
  would clone the session's configuration once per streamed chunk.
  Evidence: `refresh_idle_clock` is called from `mj-worker/src/relay/journal.rs:740`, in
  the append path. `DurableRelay::activity_facts` therefore assembles the facts directly,
  and `worker_facts_match_the_published_state` in `mj-worker/src/relay/tests.rs` pins it
  to `RelayOperationalState::facts`, which is what the daemon reads.

- Observation: a bare `execution == Running` flag was already treated as *not* evidence of
  work by the terminal, deliberately, and two tests said so. Making the shared classifier
  treat it as a running turn broke both.
  Evidence: `a_live_sdk_step_proves_work_but_an_idle_step_clock_does_not` and
  `activity_indicators_ignore_stale_phases_and_questions_without_work` in
  `mj-client/src/usage_format.rs`. The classifier now requires something live to
  corroborate the flag, while `has_work_in_flight` still counts it, so the safety
  predicates did not weaken.

- Observation: publishing the answer alongside the facts means a test that edits the facts
  by hand gets a stale answer.
  Evidence: `a_live_sdk_step_proves_work_but_an_idle_step_clock_does_not` edits a snapshot
  and had to clear `activity` first. In production the two always travel together.

- Observation: `MJ_TURN_STALL_TIMEOUT_MS` could not reach a worker on a bare target at
  all, so the documented way to shorten the watchdog for a test did nothing there.
  Evidence: the worker re-execs with `env_clear()` in `mj-worker/src/main.rs:283`, keeping
  only the login environment and `target_environment`, and `carries_image_variable` in
  `mj-core/src/login_environment.rs` deliberately drops every `MJ_` name. The daemon now
  carries the two turn bounds explicitly.

- Observation: the durable `execution_state` column is **not** stale. The earlier reading
  that suggested it was came from a session whose worker was being killed repeatedly,
  where each restart records `SessionRestarted` and returns the projection to `Idle`.
  Evidence: sampling the column and the daemon's live answer together through a Muse turn
  gives `live=running durable=running@11` on every sample for the whole turn. The column
  moves to `running` at `CommandStarted`
  (`mj-transcript/src/projection/observation.rs:166`), which is turn start.

- Observation: the watchdog computed when to look again from the facts it had at that
  moment and then slept for that whole interval, so a tool call that opened or ended in
  the meantime was invisible until the sleep ended. With the default ten-minute silence
  bound the tool-call bound could never trip at all.
  Evidence: `a_tool_call_that_outlives_its_bound_ends_the_turn` timed out against the real
  ACP loop before the wake interval was capped, and passes after.

- Observation: `Unknown { last_known: Idle }` reported that the session held no work,
  because the answer recursed into what was last known. A session nobody can see may have
  started a turn since, so a checkpoint or a worker replacement could have run against it.
  Evidence: the assertion in
  `a_wait_never_concludes_finished_while_the_session_is_unaccounted_for` failed on exactly
  that state before `has_work_in_flight` stopped recursing.

- Observation: a live Muse session on a local target cannot demonstrate the tool-call rule,
  because Muse keeps the activity clock fresh right through a blocking shell command.
  Evidence: with the silence bound cut to five seconds, a turn spending ninety seconds
  inside one `sleep 90` finished normally — and so did the same run on a build patched back
  to the pre-change rule. Only a genuinely silent harness discriminates, which is what the
  scripted-bridge tests provide and what the issue reporters saw in containers.

- Observation: the worker already tracks in-flight tool calls with their start times, in
  two places at once, and the stall watchdog can reach neither.
  Evidence: `mj-worker/src/relay.rs:131` holds
  `foreground_tools: BTreeMap<String, (ToolCallStatus, i64)>`, filled by
  `track_foreground_tool` in `mj-worker/src/relay/background.rs:450`; and
  `mj-core/src/acp/step_clock.rs:48` holds `tool_statuses: BTreeMap<String, ToolCallStatus>`
  for the same tool calls. The watchdog in `mj-worker/src/acp/session.rs:619-660` sees only
  `spec.acp_activity`, a single timestamp of the last inbound protocol message.

- Observation: nothing new has to be added to the event journal or the database to carry a
  turn's failure reason. The channel exists and is passed `None`.
  Evidence: `RuntimeEvent::PromptFinished` already has a `diagnostic` field; the stall
  branch at `mj-worker/src/acp/session.rs:648-657` sets `diagnostic: None`, and the same
  field is carried all the way to `WaitResponse.diagnostic`
  (`mj-controller/src/server/api/wait.rs:203`).

- Observation: `mj wait` never prints the diagnostic even when one is present.
  Evidence: `report_wait` in `mj-cli/src/api_commands.rs:429-482` prints outcome,
  stop reason, turn number, elapsed, message, capacity retry, relay state and final
  message — but not `response.diagnostic`.

- Observation: `chat_phase: idle` on a running session is not a stale value. It is the
  default value of an enum, printed when the daemon has no live view at all.
  Evidence: `mj-controller/src/server/viewer_types.rs:133` builds every session with
  `chat_phase: ViewerChatPhase::default()`, and `ViewerChatPhase`'s default is `Idle`
  (`mj-controller/src/server/viewer_types.rs:859-867`). Only the `if let Some(state) = live`
  branch at `mj-controller/src/server_runtime/snapshot.rs:539` ever overwrites it. The
  sibling field `is_idle` is computed in that same branch and defaults to `false`, which is
  exactly the `chat_phase idle` + `is_idle false` pair #1025 reports.

- Observation: `has_error: true` with `error: null` is structural, not a lost value.
  Evidence: `has_error` is `session.last_error.is_some() || configuration_issue`
  (`mj-controller/src/server/viewer_types.rs:99`), but the API's `error` field is filled
  only from `launch_error`, which is only set when the session state is `Error`
  (`mj-controller/src/server/api/types.rs:73` and `viewer_types.rs:104-108`).

## Decision Log

- Decision: the tool-call bound defaults to four hours, not sixty minutes, and its knob is
  `MJ_TURN_TOOL_STALL_TIMEOUT_MS` with `0` removing it.
  Rationale: failing a healthy long tool call loses work silently — the run in #1020 was
  ninety-seven minutes in — while a bound that is too long only delays a failure the user
  can already end with a cancel. Confirmed in code that a bridge *process* that exits is
  detected separately and at once, by the `child.wait()` arm of the select in
  `mj-worker/src/acp.rs:335-359`, so the long bound delays nothing about a dead bridge.
  Date/Author: 2026-09-17, reviewer's decision, recorded by the implementer.

- Decision: `chat_phase` gains no new variant; `Unknown { last_known: Turn }` maps to
  `running` and the richer state travels in the optional `activity_state` field.
  Date/Author: 2026-09-17, reviewer's decision.

- Decision: `SessionActivity` stays in `mj-client` for rendering; its predicates are gone.
  It reads the worker's published state, and classifies its own facts with the same
  `mj-core` function when a snapshot carried no answer.
  Date/Author: 2026-09-17, reviewer's decision.

- Decision: the published `activity` is the truth; `classify` is called only when an older
  worker sent no answer, and it is the same function. A test asserts the two agree.
  Date/Author: 2026-09-17, reviewer's decision.

- Decision: `ActivityState` deserializes an unrecognized variant to a cautious
  `Unrecognized` rather than failing.
  Rationale: it is an internally tagged enum inside the snapshot a worker sends. A future
  variant would otherwise fail deserialization in an older reader and take the whole
  snapshot with it. `Unrecognized` is never idle and always work in flight.
  Date/Author: 2026-09-17, reviewer's addition.

- Decision: `Unknown` counts as work in flight, but a session whose last known state is
  `Closed` is reported as `Closed`, not as unknown.
  Rationale: a worker that is gone must still be recoverable; reporting it as unknown
  would make the restart and checkpoint paths wait for work that no longer exists.
  `a_lost_worker_is_still_recovered` covers it.
  Date/Author: 2026-09-17, reviewer's addition.

- Decision: `ApiSession.error` is **not** widened to carry `last_error`.
  Rationale: the first attempt did widen it and broke three tests that exist to keep raw
  runtime error text out of a running session's projection
  (`api_session_exposes_a_launch_failure_reason_only_when_the_session_errored`,
  `public_snapshot_omits_homes_environment_locators_and_raw_errors`,
  `snapshot_endpoint_returns_only_public_projection`). A failed turn's reason reaches the
  user through `mj wait`'s diagnostic and through `last_turn_diagnostic` on a
  single-session query, which is what #1020 asked for, without publishing session error
  text the daemon deliberately withholds.
  Date/Author: 2026-09-17, implementer.

- Decision: a running execution flag with nothing to corroborate it is not the agent
  working, but it is still work in flight.
  Rationale: the durable projection can lag behind a turn that has ended, and the terminal
  already refused to animate on the flag alone. Killing a worker under it is a different
  question with a different cost, so the safety predicate keeps counting it.
  Date/Author: 2026-09-17, implementer.

- Decision: the daemon carries `MJ_TURN_STALL_TIMEOUT_MS` and
  `MJ_TURN_TOOL_STALL_TIMEOUT_MS` to the workers it starts.
  Rationale: the worker re-execs with a cleared environment, so without this the knobs
  reached only container targets that can name them in configuration, and the live test
  the plan calls for was impossible on a local target. Six lines, alongside the existing
  `MBX_*` passthrough, and no configuration schema change.
  Date/Author: 2026-09-17, implementer.

- Decision: put the mechanism in `mj-core`, not in `mj-client`.
  Rationale: `mj-worker`, `mj-controller`, `mj-client` and `mj-cli` all depend on `mj-core`
  (checked in their `Cargo.toml` files); `mj-worker` does not depend on `mj-client`, so the
  existing `SessionActivity` in `mj-client/src/usage_format.rs` cannot be the one
  implementation while the worker needs it too.
  Date/Author: 2026-09-17, plan author.

- Decision: the worker computes the answer and publishes it; the daemon only adds what the
  worker cannot know, which is whether the daemon can currently see the worker at all.
  Rationale: #1032 asks for one source of truth. Every fact except the connection state
  lives in the worker. Publishing the classified state next to the facts means the CLI, the
  terminal and the browser never re-derive it, and a test can assert that re-deriving it
  from the published facts gives the same answer.
  Date/Author: 2026-09-17, plan author.

- Decision: do not add a new variant to `ViewerChatPhase`.
  Rationale: `ViewerChatPhase` is serialized to older clients that would fail to
  deserialize an unknown variant. Instead `chat_phase` keeps its four values and becomes
  truthful (it falls back to the durable record instead of to a default), and the richer
  state travels in a new, optional, additive field.
  Date/Author: 2026-09-17, plan author.

- Decision: an in-flight tool call gets its own bound, defaulting to sixty minutes, set by
  `MJ_TURN_TOOL_STALL_TIMEOUT_MS`, where `0` means no bound.
  Rationale: #1020 asks for an in-flight tool call to count as activity. But a bridge that
  dies leaving a tool card open (the shape in #1029) would then hang the turn forever, so
  some bound is required. Sixty minutes is six times the silence bound and clears every
  duration named in #1020 (twenty to fifty-five minutes); the environment variable lets an
  evaluation driver raise or remove it.
  Date/Author: 2026-09-17, plan author. Marked as an open question for the reviewer.

## Context and Orientation

Read this section even if you know the repository; it names every file the plan touches.

Mjolnir runs a long-lived background process called the **daemon** (crate
`mj-controller`), which the CLI (`mj-cli`, the `mj` binary) talks to over a local socket.
For each session the daemon starts a **worker** process (crate `mj-worker`) on the target
machine or inside the target container. The worker starts the **harness** — the vendor's
own coding agent, such as Claude Code, Codex, Muse, ZCode or DSH — and speaks to it over
**ACP**, the Agent Client Protocol, a JSON-RPC protocol in the `agent_client_protocol`
crate. Types shared by all of these live in crate `mj-core`.

Terms used below, each defined once here:

* **Turn**: one prompt sent to the harness and everything it does until it answers. A turn
  can also be started by the harness itself with no prompt from Mjolnir; the code calls
  that a *harness turn*.
* **Relay**: the worker's durable, append-only record of everything that happened in a
  session, in `mj-worker/src/relay.rs` (type `DurableRelay`). It writes a journal of
  `RelayEvent` records and keeps a snapshot folded from them.
* **Operational state**: `mj_core::relay::RelayOperationalState`, defined in
  `mj-core/src/relay/snapshot.rs:391`. This is the live picture of a session the worker
  sends to the daemon on every sync. It is a plain serde struct with no
  `deny_unknown_fields`, so adding a field to it is backward and forward compatible: an
  older daemon ignores a field it does not know, and a newer daemon defaults a field an
  older worker does not send.
* **Materialized state**: `mj_core::state::MaterializedExecutionState` (`Idle`,
  `Running { started_at_ms }`, `Closing`, `Closed`) plus `active_turn` and
  `last_turn_outcome`, projected from the journal and stored in the daemon's SQLite
  database. This is what the daemon still knows when it cannot reach the worker.
* **Stall watchdog**: the loop in `mj-worker/src/acp/session.rs:619-660` that fails a turn
  that has produced no ACP traffic for `turn_stall_timeout()`
  (`mj-worker/src/acp/drive.rs:924`, default ten minutes, overridable with
  `MJ_TURN_STALL_TIMEOUT_MS`, `0` disables).
* **Checkpoint**: a copy of the session's state taken while the session is quiet, in
  `mj-controller/src/controller/checkpoint/`.

### The six overlapping predicates, and where they disagree

This is the inventory #1032 asks for. Each entry names the file and symbol, what it reads,
and who it can contradict.

**(1) `RelayOperationalState::is_quiet`** — `mj-core/src/relay/snapshot.rs:497`. Reads
`goal.pending_resume`, `goal.decision`, `goal.active()`, `goal.running()`, `execution`,
`acp_ready`, `background_work_known`, `active_prompt`, `harness_turn`, `queued_prompts`,
`active_user_shells`, `active_agent_terminals`, `foreground_tool_started_at_ms`,
`background_commands`, `checkpoint_barrier`. Its two wrappers are
`safe_to_replace(harness)` (line 531, adds Codex goal synchronization and Kimi background
knowledge) and, separately, `safe_for_checkpoint(harness)` /
`checkpoint_background_blocker(harness)` (lines 540 and 545), which read *only* the
provider-owned work facts and deliberately ignore whether a turn is running.

**(2) `DurableRelay::activity_is_idle`** — `mj-worker/src/relay.rs:490`, private to the
worker. Reads `execution`, `background_work_known`, `active_prompt`, `harness_turn`,
`goal.active()`, `foreground_tools` (its own live map), `active_user_shells`,
`background_commands()`. It drives `idle_since_ms` and `activity_was_idle`, which are
published in the operational state. It is (1) minus the goal-decision, queued-prompt,
terminal, `acp_ready` and barrier terms — so the worker's published `idle_since_ms` can say
"idle since 12:04" about a session `is_quiet` calls busy, for instance one with a queued
prompt or an open agent terminal.

**(3) `SessionActivity`** — `mj-client/src/usage_format.rs:34-320`, with `is_idle`,
`is_working`, `kind` and `details`. Built from the operational state by
`SessionActivity::of`. It is the UI classification, and it is the only one of the six that
treats an in-flight tool call, or a current step while execution is `Running`, as work in
its own right (`foreground_tool_started_at_ms`, `usage_format.rs:135-146`). The daemon uses
it for the viewer's `is_idle` and activity columns
(`mj-controller/src/server_runtime/snapshot.rs:570-590`).

**(4) `chat_phase`** — `mj-controller/src/server_runtime/snapshot.rs:540`. A direct
translation of the single `execution` field, computed only when a live operational snapshot
exists, and otherwise left at the enum default `Idle`
(`mj-controller/src/server/viewer_types.rs:133`). **This is the disagreement that produces
#1025** (confirmed by reading the code: `chat_phase: idle` with `is_idle: false` is exactly
what the default plus the computed `is_idle` produce when `live` is `None`). It contradicts
(3) in the same JSON object, and it contradicts `mj wait`, which falls back to the durable
materialized record when no live snapshot exists
(`mj-controller/src/server/api/wait.rs:158-165`).

**(5) `wait_for_idle_projection`** — `mj-controller/src/controller/worker_restart.rs:343`.
Polls `relay.sync()` every 200 ms and requires `native_session_is_ready()` plus
`execution == Idle` (or a synchronized active goal), with the journal ordinal unchanged for
three consecutive polls. It reads the bare `execution` flag, so a restarted worker whose
harness has already started a turn of its own, or which is running a tool, can be declared
ready underneath that work. It contradicts (1), (2) and (3), all of which would call that
session busy.

**(6) Ad-hoc `execution == Running` / `execution == Idle` checks** — the checkpoint path:
`mj-controller/src/controller/checkpoint/latched.rs:213` (routine checkpoints refuse to
open a barrier while `execution == Running`),
`mj-controller/src/controller/checkpoint/barrier.rs:203` and `:222`
(`checkpoint_barrier_wait_ended` classifies a session as *wedged* — and therefore restarts
its worker — from the same bare flag). Because the flag can lag a live harness turn or tool
call, a routine checkpoint can restart a worker that is doing work; that is the "overly
aggressive automatic worker restart" #1032 opens with.

Related call sites that will become thin calls: `mj-controller/src/daemon/snapshot.rs:108`
and `:121`, `mj-controller/src/daemon/session_move.rs:77-83`,
`mj-controller/src/controller/move_session.rs:558-565`,
`mj-controller/src/controller/worker_restart.rs:191`,
`mj-controller/src/controller/checkpoint/workspace_lease.rs:18` and `:64`,
`mj-controller/src/controller/checkpoint/validate.rs:69`,
`mj-controller/src/controller/checkpoint/latched.rs:221` and `:271`.

**(7) The stall watchdog**, which is not in #1032's list of six but is the same question
asked a seventh time. `mj-worker/src/acp/session.rs:619-660` with
`acp_idle_millis(&spec.acp_activity)` from `mj-worker/src/acp/drive.rs:944`. It reads one
fact — the timestamp of the last inbound ACP message — and none of the facts (1) to (3)
read. **This is the disagreement that produces #1020** (confirmed): during a twenty-minute
tool call the operational state has `foreground_tool_started_at_ms` set and `(3)` calls the
session "Step", working; the watchdog sees only silence and fails the turn.

### Why an earlier narrow fix is not enough

Branch `origin/fix/restart-readiness-shared-inflight-predicate` (commits `b9bae252`,
`1e9c79bd`) extracts `has_work_in_flight` out of `is_quiet` and uses it in the checkpoint
defer decision and in `wait_for_idle_projection`. That is the right direction for (1), (5)
and (6), and this plan keeps that extraction. It does not touch (2), (3), (4) or (7), so it
fixes neither #1020 nor the `chat_phase` half of #1025. Note that the branch was written
before `mj-controller/src/controller/checkpoint.rs` was split into the
`mj-controller/src/controller/checkpoint/` directory, so its hunks do not apply as-is; the
equivalent lines are named in the inventory above.

## Plan of Work

### The single mechanism

Create `mj-core/src/activity.rs` and declare `pub mod activity;` in `mj-core/src/lib.rs`.
It holds one input type, one output type, and one function. Everything else in the
repository calls into it.

The input type carries every fact the inventory showed somebody reading, so no consumer
ever needs a second source:

    /// Every fact that bears on whether a session is working. Built by the
    /// worker from its relay; reconstructed by nobody else.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct ActivityFacts {
        pub execution: RelayExecutionState,
        pub prompt_in_flight: Option<i64>,        // start, epoch ms
        pub harness_turn_started_at_ms: Option<i64>,
        pub turn_started_at_ms: Option<i64>,      // retained through background work
        pub queued_commands: usize,
        pub tools_in_flight: Vec<InFlightToolCall>,
        pub background_commands: Vec<BackgroundCommand>,
        pub active_user_shells: Vec<ActiveUserShell>,
        pub active_agent_terminals: Vec<ActiveAgentTerminal>,
        pub goal: GoalState,
        pub background_work_known: Option<bool>,
        pub acp_ready: Option<bool>,
        pub checkpoint_barrier: bool,
        pub compaction_in_flight: bool,
        pub last_acp_activity_at_ms: Option<i64>,
        pub current_step_started_at_ms: Option<i64>,
        pub idle_since_ms: Option<i64>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct InFlightToolCall {
        pub tool_call_id: String,
        pub status: ToolCallStatus,   // Pending or InProgress only
        pub started_at_ms: i64,
    }

The output is one enum plus the evidence behind it:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case", tag = "state")]
    pub enum ActivityState {
        /// The daemon cannot see the worker, so this is the last thing it knew.
        Unknown { last_known: Box<ActivityState>, since_ms: Option<i64> },
        Closed,
        Closing,
        /// A turn is in flight: a prompt, or a turn the harness started.
        Turn { started_at_ms: Option<i64> },
        /// No turn boundary, but a tool call is running.
        Tool { tool_call_id: String, started_at_ms: i64 },
        /// Only work the agent left running behind it.
        Background { started_at_ms: Option<i64> },
        /// A native goal owns the session.
        Goal,
        Idle { since_ms: Option<i64> },
    }

    /// The one classifier. Pure: same facts in, same state out, no clock and
    /// no I/O. `now_ms` is passed in so tests drive it directly.
    pub fn classify(facts: &ActivityFacts) -> ActivityState;

with a small set of question-shaped methods on `ActivityState`, each one line, so no caller
re-implements a predicate:

    impl ActivityState {
        pub fn is_idle(&self) -> bool;            // Idle only; Unknown is never idle
        pub fn is_working(&self) -> bool;
        pub fn has_work_in_flight(&self) -> bool; // everything except Idle/Closed
        pub fn chat_phase(&self) -> RelayExecutionState; // for the viewer's four values
    }

and the harness-specific wrappers that today hang off `RelayOperationalState`, moved here
unchanged in meaning and given the facts rather than the snapshot:

    pub fn safe_to_replace(facts: &ActivityFacts, harness: HarnessKind) -> bool;
    pub fn checkpoint_blocker(facts: &ActivityFacts, harness: HarnessKind) -> Option<&'static str>;

Finally, the stall question, which is the same facts asked with a clock and a policy:

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StallPolicy {
        /// Silence with nothing in flight. None disables.
        pub silence: Option<Duration>,
        /// Silence while a tool call is in flight. None disables.
        pub tool_call: Option<Duration>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum StallVerdict {
        Live,
        /// Nothing arrived and nothing is in flight.
        Silent { silent_ms: u64 },
        /// A tool call outlived its own bound.
        ToolCall { tool_call_id: String, running_ms: u64, silent_ms: u64 },
    }

    pub fn stall_verdict(facts: &ActivityFacts, policy: StallPolicy, now_ms: i64) -> StallVerdict;

`stall_verdict` is the whole of #1020's first half: if `facts.tools_in_flight` is not empty
the silence bound does not apply at all; only the tool-call bound can end the turn, and the
verdict names the tool call so the message can say which one.

### Where the truth is produced and how it reaches everyone

The worker produces it. `DurableRelay::operational_state`
(`mj-worker/src/relay.rs:461`) already assembles the live picture; it gains two published
fields on `RelayOperationalState`:

    /// Tool calls the agent has open, newest status first seen, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_in_flight: Vec<InFlightToolCall>,
    /// The worker's own answer, so no consumer re-derives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityState>,

Both are additive `serde` fields on a struct without `deny_unknown_fields`, so an older
daemon reading a newer worker ignores them and a newer daemon reading an older worker gets
`None` and falls back to classifying from the facts it does have. The existing
`foreground_tool_started_at_ms` stays for one release as the maximum of
`tools_in_flight[..].started_at_ms`, so an older daemon keeps working, and is removed in a
later, separate change — not in this plan.

The daemon does not re-derive. `mj-controller/src/server_runtime/snapshot.rs` reads
`operational.activity` when a live snapshot exists. When it does not, it calls the one
function in `mj-core` that adds the only fact the worker cannot know:

    /// What the daemon should report when it cannot see the worker: not
    /// "idle", but the last state it recorded, marked Unknown.
    pub fn while_disconnected(durable: MaterializedExecutionState, since_ms: Option<i64>) -> ActivityState;

The CLI and the viewer read the published `ActivityState`. `mj-client`'s `SessionActivity`
(`mj-client/src/usage_format.rs`) keeps its name and its rendering functions —
`display_clock`, `details`, `format_activity_columns` — but its `is_idle`, `is_working` and
`kind` are deleted and replaced by reads of `ActivityState`. Rendering stays in `mj-client`;
deciding does not.

### The watchdog

`mj-worker/src/acp/session.rs:619-660` no longer computes idleness from
`acp_idle_millis`. The `LaunchSpec` (`mj-worker/src/acp/launch.rs:19`) gains a
`activity: ActivityHandle` — a cheap `Arc`-backed handle, in the same spirit as the
existing `AcpActivityClock` (`mj-core/src/relay.rs:135`) and `StepClock`
(`mj-core/src/acp/step_clock.rs:56`), cloned into both the ACP client handlers and the
`DurableRelay`. It owns the last-activity timestamp and the in-flight tool calls, so there
is one tracker instead of the two the `Surprises` section found. `DurableRelay` drops its
own `foreground_tools` map and `track_foreground_tool`, reading the handle instead; the
`StepClock` keeps its own `tool_statuses`, which is a different question (which step is the
agent on) and stays where it is.

The watchdog branch then becomes: wake at the earliest bound, call `stall_verdict`, and act
on the answer. `Live` keeps waiting. `Silent` and `ToolCall` fail the turn with a message
built from the verdict.

The two bounds come from `mj-worker/src/acp/drive.rs`:
`turn_stall_timeout()` keeps `MJ_TURN_STALL_TIMEOUT_MS` and its ten-minute default; a new
`tool_call_stall_timeout()` reads `MJ_TURN_TOOL_STALL_TIMEOUT_MS`, defaults to
3_600_000 ms, and treats `0` as no bound. Both are documented in
`.agents/docs/internal-environment-variables.md`. Per-target configuration already works
today for container targets, because `[targets.<id>.container] environment` is copied into
`target_environment` (`mj-controller/src/controller/worker_binary/launch.rs:507-514`) and
applied to the worker's own process environment
(`mj-worker/src/main.rs:249`); a profile-level setting does not reach the worker process and
is a deliberate scope cut, recorded below.

### Carrying the reason (the second half of #1020)

Nothing new is needed on the wire. Three edits:

1. `mj-worker/src/acp/session.rs`, the stall branch: emit
   `RuntimeEvent::PromptFinished { stop_reason: TURN_STALLED_STOP_REASON, diagnostic: Some(...) }`
   where `TURN_STALLED_STOP_REASON` is a new constant `"harness_inactive"` in
   `mj-worker/src/acp/drive.rs` beside `PROMPT_ERROR_STOP_REASON`, and the diagnostic is a
   `mj_core::diagnostic::TurnDiagnostic` whose `message` is the sentence the verdict
   produces and whose `code` is `"harness_inactive"` or `"tool_call_timeout"`.
   `classify_prompt_completion` (`mj-core/src/state.rs:190`) maps any unrecognized stop
   reason to `PromptCompletion::Error`, so the outcome stays `error` and no existing caller
   changes behavior; the stop reason is simply no longer the bare word `"error"`.
2. `mj-cli/src/api_commands.rs`, `report_wait`: print `response.diagnostic`'s message when
   there is one, on its own line after the summary line. Today the field is fetched and
   discarded.
3. `mj-controller/src/server/api/types.rs`: `ApiSession.error` is filled from the session's
   `last_error` whenever `has_error` is true, not only from `launch_error` for a session in
   the `Error` state, so `has_error: true, error: null` becomes impossible.

The transcript warning stays; it is the only signal a person reading the conversation sees.

### Surviving a restart (the rest of #1025)

`mj-controller/src/server_runtime/snapshot.rs` stops leaving `chat_phase` at its default.
Every session gets its `ActivityState` from one of two sources: `operational.activity` when
the daemon has a live connection, or `activity::while_disconnected(durable.execution, ...)`
when it does not. `chat_phase` is then `state.chat_phase()`, which maps
`Unknown { last_known: Turn { .. } }` to `running`, so a session whose durable record holds
a running turn reports `running` while the daemon is reattaching, and `is_idle` stays
`false` as it already correctly does. The richer state travels in a new optional field
`activity_state: Option<ActivityState>` on `ViewerSession` and `ApiSession`, additive and
skipped when absent, so older clients are unaffected.

The worker side already survives: the relay's snapshot is on disk and the active prompt,
harness turn and execution state are folded back on reopen
(`mj-worker/src/relay.rs:228-240`). What does not survive a *worker* restart is the live
in-flight tool set, and that is correct — the tools died with the harness. The plan adds no
persistence for it; it adds the rule that an unknown live state is reported as unknown.

## Concrete Steps

Work from the repository root of this worktree. Every command below assumes that working
directory.

### Milestone 1 — the mechanism, alone

Scope: `mj-core/src/activity.rs` exists with `ActivityFacts`, `InFlightToolCall`,
`ActivityState`, `StallPolicy`, `StallVerdict`, `classify`, `stall_verdict`,
`while_disconnected`, `safe_to_replace`, `checkpoint_blocker`, and a `#[cfg(test)] mod
tests` driving them directly. No other file changes behavior. At the end of this milestone
nothing user-visible has changed and the whole suite still passes; what exists that did not
before is a single, testable answer to the question.

Add `pub mod activity;` to `mj-core/src/lib.rs`. Add
`RelayOperationalState::facts(&self) -> ActivityFacts` in
`mj-core/src/relay/snapshot.rs` so existing callers have a bridge.

Run:

    cargo test -p brokk-mj-core activity

Expect the new tests to pass and nothing else to change.

### Milestone 2 — the worker produces the truth

Add `ActivityHandle` (in `mj-core/src/activity.rs`, beside the types it publishes) and
thread it through `mj-worker/src/acp/launch.rs`'s `LaunchSpec`, the client handlers in
`mj-worker/src/acp/drive.rs` that today call `spec.acp_activity.mark()`, and
`DurableRelay`. Delete `DurableRelay::foreground_tools` and
`DurableRelay::track_foreground_tool` (`mj-worker/src/relay/background.rs:450`), reading the
handle in `operational_state` instead. Replace `DurableRelay::activity_is_idle`
(`mj-worker/src/relay.rs:490`) with `classify(&facts).is_idle()`; `refresh_idle_clock` keeps
its shape. Publish `tools_in_flight` and `activity` on `RelayOperationalState`.

Run:

    cargo test -p brokk-mj-worker
    cargo test -p brokk-mj-core

### Milestone 3 — the watchdog

Rewrite the stall branch in `mj-worker/src/acp/session.rs` around `stall_verdict`. Add
`tool_call_stall_timeout()` and `TURN_STALLED_STOP_REASON` in `mj-worker/src/acp/drive.rs`,
and rewrite `turn_stall_message` to take a `StallVerdict` so the sentence names the tool
call and its age when that is what ended the turn. Document
`MJ_TURN_TOOL_STALL_TIMEOUT_MS` in `.agents/docs/internal-environment-variables.md`.

### Milestone 4 — the reason reaches the user

The three edits listed under "Carrying the reason". Add a behavior test in
`mj-cli` or `mj-controller` covering `report_wait` printing a diagnostic, and one covering
`ApiSession.error` being present whenever `has_error` is true.

### Milestone 5 — the consumers

`mj-controller/src/server_runtime/snapshot.rs`: read `operational.activity`, fall back to
`while_disconnected`, set `chat_phase`, `is_idle` and the new `activity_state` from it.
`mj-client/src/usage_format.rs`: delete `SessionActivity::is_idle`, `is_working` and
`kind`; keep the rendering, driven by `ActivityState`. Bump `PROTOCOL_VERSION` in
`mj-client/src/daemon.rs` by one from whatever the base has, and say so in the final
report, because the daemon's session shape gains a field.

### Milestone 6 — delete the duplicates

Rewrite `RelayOperationalState::is_quiet` as
`!self.checkpoint_barrier.is_some() && !classify(&self.facts()).has_work_in_flight()`, and
`safe_to_replace` / `safe_for_checkpoint` / `checkpoint_background_blocker` as one-line
calls into `mj_core::activity`. Replace the bare `execution ==` checks at
`mj-controller/src/controller/checkpoint/latched.rs:213`,
`mj-controller/src/controller/checkpoint/barrier.rs:203` and `:222`, and
`mj-controller/src/controller/worker_restart.rs:350`, with `has_work_in_flight()`, keeping
the deliberate exceptions the earlier branch documented (a held barrier is not itself work;
a synchronized active goal is a restart's expected continuation). Grep afterwards to prove
nothing is left:

    grep -rn "execution == RelayExecutionState::\(Running\|Idle\)" --include=*.rs .

Every remaining hit must be inside `mj-core/src/activity.rs` or be a lifecycle check about
`Closed` / `Closing`.

## Migration list

Each old predicate, and what becomes of it:

* `RelayOperationalState::is_quiet` — kept as a name, body deleted, becomes a call into
  `mj_core::activity`. Callers unchanged.
* `RelayOperationalState::has_work_in_flight` — introduced (as on the earlier branch), body
  delegates to `ActivityState::has_work_in_flight`.
* `RelayOperationalState::safe_to_replace` / `safe_for_checkpoint` /
  `checkpoint_background_blocker` — kept as names, bodies become calls into
  `mj_core::activity::safe_to_replace` / `checkpoint_blocker`.
* `DurableRelay::activity_is_idle` — **deleted**. Its callers use `classify(...).is_idle()`.
* `DurableRelay::foreground_tools` and `track_foreground_tool` — **deleted**, replaced by
  the shared `ActivityHandle`.
* `SessionActivity::is_idle` / `is_working` / `kind` in `mj-client` — **deleted**. The
  struct keeps only rendering.
* `chat_phase`'s `match state.execution` in `server_runtime/snapshot.rs` — **deleted**,
  replaced by `ActivityState::chat_phase()`.
* `wait_for_idle_projection`'s inline idle test — becomes `!has_work_in_flight()` with the
  documented goal exception. Its 200 ms / three-stable-polls loop is **kept as is**; see
  the scope cuts.
* The bare `execution ==` checks in `checkpoint/latched.rs` and `checkpoint/barrier.rs` —
  become `has_work_in_flight()`.
* `acp_idle_millis` — kept, but only as an input to `stall_verdict`; no caller reads it to
  make a decision.

**Protocol and database changes.** Two additive `serde` fields on
`RelayOperationalState` (`tools_in_flight`, `activity`) and one on the daemon's session
shape (`activity_state`), all `#[serde(default)]` and skipped when empty, on structs that do
not use `deny_unknown_fields`. A new stop-reason string value, `"harness_inactive"`, stored
in the existing `last_turn_outcome_json` column, which classifies as `Error` under the
existing rules. **No database migration is required**, and no column, constraint or enum
storage changes; the change is **compatible** under the CLAUDE.md classification, because an
older daemon reading a newer worker ignores the new fields, a newer daemon reading an older
worker defaults them, an older reader of a stored `harness_inactive` stop reason classifies
it as an error exactly as it classified `error`, and no older writer can destroy a value it
does not know. `PROTOCOL_VERSION` in `mj-client/src/daemon.rs` is bumped by one because the
daemon's session shape gains a field.

## Validation and Acceptance

### Unit tests, driven directly

In `mj-core/src/activity.rs`, a `#[cfg(test)] mod tests` that never touches a relay:

* `a_running_tool_call_is_not_a_stall` — facts with `last_acp_activity_at_ms` 30 minutes
  old and one `InFlightToolCall` started 30 minutes ago, `StallPolicy { silence: 10 min,
  tool_call: 60 min }`, expect `StallVerdict::Live`. This test fails before the change,
  because before the change there is nothing to call.
* `silence_with_nothing_in_flight_is_a_stall` — same facts with no tool call, expect
  `Silent { silent_ms: 1_800_000 }`.
* `a_tool_call_that_outlives_its_bound_is_a_stall` — tool call started 61 minutes ago,
  expect `ToolCall { tool_call_id, .. }` naming it.
* `a_zero_bound_disables_that_bound` — `StallPolicy { tool_call: None }` with a tool call
  running for a day, expect `Live`.
* `a_disconnected_daemon_never_reports_idle` — `while_disconnected(Running { .. })` is
  `Unknown { last_known: Turn { .. } }`, `is_idle()` is false, `chat_phase()` is `Running`.
* `every_way_of_being_busy_is_busy` — a table-driven test, one row per fact in
  `ActivityFacts` that means work, asserting `has_work_in_flight()`. This replaces and
  extends the equivalent table in `mj-worker/src/relay/snapshot_tests.rs:107`.
* `the_published_state_agrees_with_the_facts` — for each row, `classify(&facts)` equals what
  the worker published, proving no consumer needs to re-derive.

Run the suite outside the sandbox, on the dev profile:

    cargo test
    cargo clippy --all-targets -- -D warnings

Expect a clean run. A known flaky test (#1036, `codex_usage`, "Text file busy") can fail
when other agents build in parallel; rerun that test alone before treating it as yours.

### Live test

Instance name `fix1020`, port 4020, everything inside a tmux session named `fix1020`.
Set it up exactly as follows.

    mkdir -p ~/.config/mjolnir/instances/fix1020
    cp ~/.config/mjolnir/instances/campaign0916/config.toml ~/.config/mjolnir/instances/fix1020/config.toml

Edit that copy: set `[phone] bind` to `127.0.0.1:4020`, and add a local target, because the
copied config has none:

    [targets.localhost]
    kind = "local-bare"

The file contains an API key. Never print it, never commit it, never paste it.

Build and use the worktree binary by absolute path:

    cargo build --bin mj
    tmux new-session -d -s fix1020 -x 140 -y 40

Shorten the watchdog for the test so a two-minute tool call outlives it, and make every
tested value differ from its default so the test proves the new code ran:

    MJ_TURN_STALL_TIMEOUT_MS=30000 MJ_TURN_TOOL_STALL_TIMEOUT_MS=90000

Three scenarios, each of which must fail on the unfixed build and pass on the fixed one.
Show the unfixed failure first where practical.

1. **A long tool call must not fail the turn.** Start a session on `localhost` with the
   cheap `deepseek` profile and prompt it to run `sleep 120` and then report the exit code.
   With a 30-second silence bound, the unfixed build fails the turn about 30 seconds in with
   "mj received no activity from the harness for about 1 minute(s)". The fixed build keeps
   the turn running for the whole sleep and `mj wait` returns `finished`. Capture the pane
   and quote the lines.
2. **A truly silent harness must still fail, with a reason.** Suspend the harness process
   inside the session (`pkill -STOP -f <harness>` scoped to the `fix1020` instance) so
   nothing arrives and no tool call is open, and wait. Expect `mj wait` to print
   `error (harness_inactive)` and, on the next line, the sentence naming the silence, and
   expect `mj sessions --session <id> --json` to carry that text in `error` with
   `has_error: true`. On the unfixed build the same line reads `error` with no reason and
   `error: null`.
3. **A daemon restart mid-turn must keep reporting the turn as running.** Start a long turn,
   then stop and restart only the `fix1020` daemon while it runs. Immediately after the
   restart, `mj -i fix1020 sessions --session <id> --json` must report `chat_phase` as
   `running` (or `unknown` in the additive `activity_state` field with `last_known` a turn),
   never `idle`, and `mj wait --timeout 5` must agree by reporting the turn still running.
   On the unfixed build this reports `chat_phase: idle` with `is_idle: false`, which is
   #1025 exactly.

Cleanup, which is part of the test:

    tmux kill-session -t fix1020

then close or destroy the sessions created, stop the `fix1020` daemon, confirm no worker of
that instance survives (`pgrep -af instances/fix1020`, terminate any that do), and remove
`~/.config/mjolnir/instances/fix1020` and `~/.local/share/mjolnir/instances/fix1020`.
Never touch the default instance or any other instance, and never stop or restart another
instance's daemon.

## Idempotence and Recovery

Every step is a source edit plus a test run and can be repeated. The live test creates only
the `fix1020` instance and its sessions, both removed by the cleanup above; rerunning it
after a partial failure is safe once that cleanup has run. No step migrates stored data, so
there is nothing to roll back beyond `git checkout` of the files named. If the additive
fields turn out to break an older client in practice, removing them is a pure deletion: no
stored value depends on them.

## Scope cuts

Taken from #1032 and deliberately left out, each with its reason:

* **Replacing the 200 ms / three-stable-polls loop in `wait_for_idle_projection` with a
  worker-published "journal replay settled" signal.** That is a durability question, not an
  activity question, and it needs a new worker-published fact with its own compatibility
  story. Leaving it means the restart path still samples rather than waits on a fact, but it
  now samples the right predicate.
* **Separating the workspace latch from the session latch, so `mj put-file` no longer waits
  for a fully idle session.** #1032 itself marks the underlying question — whether a file
  write may race a running turn — as a product decision needing owner input. It is not
  required by #1020 or #1025.
* **A per-profile or per-target setting for the stall timeouts, in the configuration file.**
  Per-target already works through `[targets.<id>.container] environment` for container
  targets, which is where every lane in #1020 runs, and the environment variables cover the
  rest. A first-class setting is a separate, small change.
* **#1029 (a restarted Muse session whose harness-to-mj relay carries nothing) and #1017
  (lost turn completions across a daemon restart).** Both are relay-delivery bugs, not
  activity-classification bugs. This plan improves their symptom — the failure now says
  which of "nothing arrived at all" and "a tool call never ended" happened — but does not
  attempt the reattachment fix.
* **Removing `foreground_tool_started_at_ms` from `RelayOperationalState`.** Kept for one
  release so an older daemon reading a newer worker still sees tool activity.

### Ordered commits

Each is independently buildable and testable, and each is small enough to validate alone.

1. `mj-core`: the activity module, with its unit tests. No behavior change anywhere.
2. `mj-worker`: one in-flight tool tracker; `activity_is_idle` and `foreground_tools`
   deleted; `tools_in_flight` and `activity` published.
3. `mj-worker`: the watchdog reads `stall_verdict`; the tool-call bound and its environment
   variable; the documentation entry.
4. The failure reason: new stop reason and diagnostic, `mj wait` prints it, `ApiSession.error`
   carries `last_error`.
5. The consumers: `chat_phase` and `is_idle` from the published state, the disconnected
   fallback, `activity_state`, `PROTOCOL_VERSION` bump.
6. The deletions: `is_quiet` and its wrappers, the checkpoint and restart call sites, the
   `mj-client` predicates.

## Artifacts and Notes

The three lines that carry the two bugs, for the reader who wants to see them before
changing anything:

    mj-worker/src/acp/session.rs:625
        let idle = acp_idle_millis(&spec.acp_activity);

    mj-controller/src/server/viewer_types.rs:133
        chat_phase: ViewerChatPhase::default(),

    mj-controller/src/server_runtime/snapshot.rs:539
        if let Some(state) = live {

The first is #1020: one timestamp, no tool calls. The second and third are #1025: the
default is `Idle`, and only a live snapshot ever overwrites it.

## Interfaces and Dependencies

In `mj-core/src/activity.rs`, at the end of Milestone 1, these must exist and be public:

    pub struct ActivityFacts { /* fields listed in Plan of Work */ }
    pub struct InFlightToolCall { pub tool_call_id: String, pub status: ToolCallStatus, pub started_at_ms: i64 }
    pub enum ActivityState { Unknown { .. }, Closed, Closing, Turn { .. }, Tool { .. }, Background { .. }, Goal, Idle { .. } }
    pub struct StallPolicy { pub silence: Option<std::time::Duration>, pub tool_call: Option<std::time::Duration> }
    pub enum StallVerdict { Live, Silent { silent_ms: u64 }, ToolCall { tool_call_id: String, running_ms: u64, silent_ms: u64 } }

    pub fn classify(facts: &ActivityFacts) -> ActivityState;
    pub fn stall_verdict(facts: &ActivityFacts, policy: StallPolicy, now_ms: i64) -> StallVerdict;
    pub fn while_disconnected(durable: crate::state::MaterializedExecutionState, since_ms: Option<i64>) -> ActivityState;
    pub fn safe_to_replace(facts: &ActivityFacts, harness: crate::config::HarnessKind) -> bool;
    pub fn checkpoint_blocker(facts: &ActivityFacts, harness: crate::config::HarnessKind) -> Option<&'static str>;

    impl ActivityState {
        pub fn is_idle(&self) -> bool;
        pub fn is_working(&self) -> bool;
        pub fn has_work_in_flight(&self) -> bool;
        pub fn chat_phase(&self) -> crate::relay::RelayExecutionState;
    }

In `mj-core/src/relay/snapshot.rs`:

    impl RelayOperationalState {
        pub fn facts(&self) -> crate::activity::ActivityFacts;
    }

No new crate is added. The mechanism lives in `mj-core` because `mj-worker`,
`mj-controller`, `mj-client` and `mj-cli` all already depend on it, and `mj-worker` does not
depend on `mj-client`. No new external dependency is required: `serde`,
`agent_client_protocol` and `std::time` are already in `mj-core`.

## Outcomes & Retrospective

All seven milestones are implemented and committed on the working branch. Measured against
the purpose:

A turn blocked in a long tool call survives. `stall_verdict` does not apply the silence
bound while a tool call is open, and `a_turn_blocked_in_a_long_tool_call_is_not_failed`
drives the real ACP loop against a bridge that opens a tool call and then sends nothing.
With the tool-call rule removed that test fails with the exact warning from the issue.

A turn that really stalled says why. The stop reason is `harness_inactive` rather than the
bare word "error", the outcome carries a diagnostic naming the silence or the tool call
and the knob that raises the limit, and `mj wait` prints it.

A turn that spans a daemon restart is never called idle. Every session gets one activity
state; a session the daemon cannot see reports what was last known, which is never
confirmed idle. Live evidence: with the worker gone, `mj sessions --session --json`
reported `"chat_phase":"idle","is_idle":false,"activity_state":{"state":"unknown",
"last_known":{"state":"idle"}}` — the daemon saying what it last knew and refusing to
confirm idleness, where before the field was simply the default value of an enum.

One implementation answers the question. `is_quiet`, `has_work_in_flight`,
`safe_to_replace`, `checkpoint_background_blocker`, the worker's idle clock, the viewer's
`chat_phase` and `is_idle`, the checkpoint defer, the barrier wait, the restart-readiness
wait and the stall watchdog all go through `mj_core::activity`. The worker's private
`activity_is_idle` and `foreground_tools`, and `mj-client`'s `is_idle`/`is_working`/`kind`
bodies, are deleted.

Live evidence after review, with a Muse profile copied into the private instance:

A silent harness loses its turn with a visible reason. With the bridge suspended and
nothing in flight, `mj wait` printed `error (harness_inactive) turn 4 in 30.3s` followed by
the sentence naming the silence — 30.3 seconds against a 30-second bound.

A turn that outlives a daemon that was killed outright is never reported idle. With the
turn running, the worker suspended so the replacement daemon could not reattach, and the
durable record saying `running`, eight consecutive probes reported
`"chat_phase":"running","is_idle":false,"activity_state":{"state":"unknown","last_known":
{"state":"turn",...}}`. The same window on a build patched back to the old fallback
reported `"chat_phase":"idle"` — the ticket's symptom, side by side.

What remains: the tool-call bound has not been seen to fire in a live Muse session. It
fires in the ACP-loop test, the handle the watchdog reads is pinned by test to the tracker
the relay fills, and Muse never goes silent during a tool call on a local target, so the
live runs never reach either bound. Whether Muse's shell cards register as in-flight tool
calls at all is unresolved and is the one thing worth an instrumented live run before this
is relied on for a harness that does go silent.

---

Revision note (2026-09-17): first version of this plan, written during the investigation
phase of #1020 and #1025 and before any code change. It records the inventory of the six
(in fact seven) overlapping predicates, ties #1020 to the stall watchdog reading a single
timestamp and #1025 to `chat_phase` defaulting to `Idle` when no live snapshot exists, and
proposes one mechanism in `mj-core` that every consumer calls. The scope cuts are listed
explicitly so a later contributor can tell what was deliberately left undone.


Revision note (2026-09-17, after implementation): the living sections above were brought up
to date at the end of the work. The plan's shape survived contact with the code; the four
substantive departures from it are recorded in the Decision Log — the four-hour tool-call
bound, `ApiSession.error` deliberately not widened, a running flag needing corroboration
before it counts as the agent working, and the daemon carrying the turn bounds to its
workers so the knobs work on a target that is not a container.


Revision note (2026-09-17, after review): three gaps the reviewer named are closed. The
durable-column worry turned out to be a measurement artefact and the Surprises section now
records what the column actually does. Closing it uncovered two real defects — a watchdog
that slept through changes in what was in flight, so the tool-call bound never tripped, and
an unknown session that reported it held no work — both fixed with tests. The migration is
finished: every predicate in the list below is deleted or a call into `mj_core::activity`.
