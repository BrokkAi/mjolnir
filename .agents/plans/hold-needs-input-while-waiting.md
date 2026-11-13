# Keep needs-input open until answered, and hold it while the agent waits on its own work

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root. It implements GitHub issue #1518 (`gh issue view 1518 --repo BrokkAi/mjolnir --comments`), with the design the user decided on 2026-11-13.


## Purpose / Big Picture

Mjolnir shows a `?` glyph, and sends a desktop, terminal or phone notification, when a coding session needs its user. Today two things go wrong, both measured on real sessions.

First, many `?` alerts fire while the agent is still waiting for work it started itself: a build, a test run, a child session it delegated to. The agent's reply mentions a decision the user will make later ("your call later", "then coordinator review and your approval"), the end-of-turn classifier reads that as a question, and the user is interrupted while the agent is still busy. In a 4,307-turn labeled study, about 83% of the weighted false `?` alerts were turns of this kind.

Second, a question the user has not answered can silently disappear. The `?` is decided from the latest turn only, so when the agent takes another turn on its own (a background job finished, a child reported back) and that turn ends without restating the question, the `?` goes away although nobody answered it.

After this change, a question stays open until the user answers it or dismisses the notification. While the agent is waiting on work it started, and only for the Claude and Codex harnesses, whose reports of running background work are reliable, an open question is held: no `?`, no notification, the session simply shows as working. When the wait ends, the question is shown. There is no check of whether the agent settled the question itself, because labeled data showed agents almost never do (see Outcomes).

To see it working: in an isolated instance, run a Claude or Codex session that starts a background job and ends its turn saying "When this finishes, tell me whether to keep the old format". The session shows as working with no `?`. When the job finishes and the agent's next turn ends, the `?` and a notification appear, with the original question as the notification text, even if that next turn did not mention the question. If the user answers at any point, the question closes.


## Progress

- [x] Review follow-up: managed-child activity uses the shared activity state plus queued prompts; legacy notification fallback uses the same worker capability check as question_message. Focused controller/TUI tests, touched-crate Clippy, formatting and whitespace checks pass.

- [x] Review follow-up: shared legacy snapshot/checkpoint seeding and exact checkpoint envelope implemented. Core snapshot/open_request filters: 11 passed; worker upgrade/restore filters: seven passed; touched-crate Clippy with warnings denied, formatting and whitespace checks passed. No commit.

- [x] (2026-11-13) Milestone 1, study: does an agent settle its own held question before the user replies? Concluded: almost never (about 2%); the resolution check is dropped by user decision. Evidence and numbers in Outcomes.
- [x] (2026-11-13) Parent settled dismissal identity: retain the original InferredQuestion in OpenRequest; implementation resumed.
- [x] (2026-11-13) Milestone 2 (durable frontier and late-update/replay regressions pass): the worker owns the open request (set by a needs-input turn, kept across agent-only turns, cleared only by the user).
- [x] (2026-11-13) Milestone 3 (projector, push and TUI notification regressions pass): the daemon shows the open request as NeedsInput, holds it while a Claude or Codex agent waits on its own work, and notifications and Web Push follow.
- [x] (2026-11-13) Milestone 4: compatibility, full workspace Clippy and formatting complete; full eight-package run complete. Its two failures pass targeted follow-up after correcting a CLI fixture; the unchanged readiness case exceeded 250 ms only in the full run. Live acceptance remains with the parent as instructed.


- [x] (2026-11-13) Review follow-up: restored the shared activity-idle gate; corrected projector assertions and field comment; all requested affected checks pass.


## Surprises & Discoveries

Review follow-up: project_states already uses RelayOperationalState::activity_state directly to distinguish Working from Idle; there is no separate helper to reuse. Child holding now reads that same activity state plus queued prompts. TUI details previously carried only a question target, so CLI presentation now passes the shared supports_open_request capability beside that target for notification fallback selection.

Review follow-up: `DurableRelay::open_with_harness` (`mj-worker/src/relay.rs`) is the shared startup/checkpoint-seed load point. Existing snapshots are decoded there before their private schema marker advances. Legacy harness InputRequired can be recovered from `assessment.completion.stop_reason` plus the assessment turn id/completion ordinal; no inference from a rendered reply is needed. Checkpoint export receives only `RelayOperationalState`; the parent decided to add only `person_input_through` there and export derived `open_request` plus that frontier. Restoring a retained request that was already fenced is unnecessary: the frontier still fences every late update.

- Resolution (2026-11-13): the parent chose a durable `person_input_through` frontier. The snapshot now retains `request`; person prompts, human form answers and DismissQuestion advance the frontier, and `open_request()` derives publication from the completion ordinal comparison. Dismissal no longer invalidates the assessment. Locator writes are now explicit `QuestionTargetUpdated` observations: they only fill a retained request with the same InferredQuestion, and cannot create one. This observation is necessary because a full TurnAssessmentUpdated otherwise conflated a semantic verdict with a locator result; older full assessment observations still replay normally, with the frontier fencing their published effect.
- Validation environment (2026-11-13): an elevated loopback bind succeeded before the new targeted run. All new cargo invocations use escalated execution outside the sandbox. The CLI directory's Cargo package is `brokk-mjolnir`; final validation includes it plus core, worker, controller, tui, client, chat and transcript. Live acceptance is explicitly deferred to the parent by the latest instruction.

- Discovery resolved by the frontier decision (2026-11-13, before full validation): a human `ElicitationResolved` clears `open_request` but leaves the latest assessment current. `store_question_target` still accepts a late locator result for that assessment (`mj-worker/src/relay/verdict.rs:370`), journals another `TurnAssessmentUpdated`, and snapshot apply unconditionally sets the request again for its AwaitInput action (`mj-core/src/relay/snapshot/apply.rs:267`). An assessment already in flight can likewise arrive after the form answer. The planned optional request alone does not distinguish an assessment that has not raised a request yet from one the person already answered. A worker-owned person-input ordinal fence could reject semantic results from completions preceding the answer; alternatively the form answer could invalidate outstanding assessments, with replay handling for already-written journals where such a later update was valid. These change persisted state or assessment transitions and are not chosen in the plan. The parent subsequently chose the frontier; its implementation and validation supersede this earlier stop. Regression recipe and pointers: `.mj/agents/ad0c15bbdaf0aeb6567a40ee8a66a5bf/late-form-answer-decision.md`.
- Observation (implementation, 2026-11-13): the TUI also retained a local latest-turn-derived `awaiting_input` for child navigation and unfinished counts. Removed that duplicate interpretation; those paths now read the daemon's served Question dismissal identity. Removed its obsolete local interpretation test and added a retained-message notification regression. Seven targeted TUI notification tests passed (`tui-targeted.log`).

- Observation (implementation, 2026-11-13): late assessments need the final reply of their own completed turn as a question-target fallback, rather than the last assistant message in the whole retained history. The snapshot now retains `completed_reply_message` at completion, before the assessment context closes that reply. It stores only an ID. Both TUI and Web Push use the shared `materialized_agent_message_text` transcript helper to recover text.
- Review correction (2026-11-13): semantic_question preserves the existing activity-idle gate for every worker protocol and harness. Background, Expecting, Goal, Retry and Tool activity therefore continue to suppress prose questions; the hold adds work not already excluded by that gate. The projector regression checks Kimi NeedsInput with a running managed child, then checks Claude and Kimi Working with Background activity after the child finishes.

- Observation (implementation, 2026-11-13): managed-child ownership existed only as `DurableRelay::managed_subagent`, while `InputRequired` completions are independent of classification. Replay therefore needs this fact to prevent child prose requests. Added `ManagedSubagentConfigured { managed }` and a serde-defaulted snapshot flag under Milestone 2's missing-fact provision; the launch setter journals changes before work. Relay store revision advances 22 to 23 because older writers would discard retained requests and cannot replay the new observation. Relay protocol 64 and daemon protocol 96 are taken.
- Observation (implementation, 2026-11-13): `ElicitationResolved` is also emitted for automatic approval and cancellation. Its optional rendered `reply` is populated only for human answers in the current ACP producers; only a resolution carrying that reply clears the prose request. Automatic resolutions preserve it.
- Observation (implementation, 2026-11-13): `SessionTail.latest_user_position` counted generated user-role messages, which would suppress retained questions following mailbox turns. Publication now supplies a person-authored frontier using the existing `is_user_authored_message` classification; transcript-free attention guards retain their frontier comparison.
- Observation (validation, 2026-11-13): the first targeted build exposed explicit operational-state test initializers needing `open_request: None`; fixed those fixture initializers. The first isolated CLI attempt used installed mj and failed binding its isolated loopback endpoint under the sandbox (EPERM), before any session could start; log in `.mj/agents/ad0c15bbdaf0aeb6567a40ee8a66a5bf/live-check.log`. It did not exercise the new build's live acceptance behavior.

- Observation (implementation reconnaissance, 2026-11-13): the specified `OpenRequest { asked_at_ordinal, question_message }` cannot directly produce today's dismissal payload after the assessment advances. `InferredQuestion` requires both `turn_id` and `completed_ordinal` (`mj-core/src/assessment.rs:344`); the worker validates exact identity against the current assessment (`mj-worker/src/relay/commands.rs:211`), and snapshot apply does the same (`mj-core/src/relay/snapshot/apply.rs:454`). The daemon forwards that payload (`mj-controller/src/daemon/actions.rs:1306`). The plan specifies ordinal identity for the retained request but does not specify whether to change this existing wire shape or retain the original turn ID in OpenRequest. Implementation paused before source edits for the parent's decision: use an ordinal-only dismissal contract for new workers (keeping legacy support), or add the original turn ID to OpenRequest and retain the existing payload shape. Current checkout protocols are relay 63 and daemon 95; no numbers taken.

- Observation: an agent almost never settles its own question in the turns it takes before the user replies.
  Evidence: `/mnt/optane/tmp/held-request-study-20261113/production-frame/` (PARENT-TRACKING.md in the parent directory has the run log). 149 sampled AwaitInput turns (from 596 eligible turns in 65 sessions, at most 3 per session) that were followed by an agent-only turn before the next user message. Blind pass 1: 143 contain a real request to the user, 6 do not (4%). Blind pass 2 on the 143: the two labelers agree on settled versus open for 141; 2 are unanimously settled (answered by the assistant), 2 more are disputed, 1 is unclear (advice), and the rest stay open (restated, new request, or not mentioned). Settled rate 1.4% to 2.8%.
- Observation: the first calibration attempt did not test the resolution check (the held text lacked the question; its "negatives" were labels given after the question had left the labelers' view), and text-mined "questions" were mostly plans and status reports. The production frame (AwaitInput turns plus an identification pass) fixed both.
  Evidence: advisor review 2026-11-13; `/mnt/optane/tmp/held-request-calibration/` (discarded) and the superseded corpus under `/mnt/optane/tmp/held-request-study-20261113/`.
- Observation: the worker does not track whether its managed child sessions are still running; only the daemon does.
  Evidence: `mj-core/src/continuation.rs` documents `awaiting_children_since_ms` as a legacy protocol-39 field that "no longer affects continuation"; managed children live in the daemon's `RuntimeProjection.subagents` and child `RuntimeSessionView`s (`mj-client/src/runtime_feed.rs`).
- Observation: a background command's existence is deterministic, but whether the agent will act on its result is a classifier judgment.
  Evidence: `mj-core/src/relay/snapshot.rs` defines `BackgroundCommand { id, started_at_ms, command, can_stop }` with no dependency field; `ActivityFacts::background_needed` in `mj-core/src/activity.rs` is the classifier's judgment.


## Decision Log

- Decision: no resolution check. An open request is released (shown) when the wait ends, whatever the agent's later replies say; only the user closes it.
  Rationale: the user's decision (2026-11-13), from the Milestone 1 data. With about 2% of held requests settled by the agent, the agreed pass rule (at least 60% of settled requests closed, at most 5% of open ones wrongly closed) cannot be measured, and even a check that met it would make about four wrong closes for every right one. Showing every held request is right about 98% of the time. This removes the GLiDE `held_request` question, the request evidence field, the resolution state, the settled-becomes-Important rule and the proxy deploy from the earlier plan.
  Date/Author: 2026-11-13 / user decision, recorded by parent.
- Decision: the worker owns the open request; the daemon owns only what to display, as a pure function of current facts.
  Rationale: the worker produces every end-of-turn verdict, sees every prompt, and journals its state, so the open request survives a daemon restart with no new daemon storage. The daemon is the only component that knows all three kinds of running work (background commands and native sub-agents arrive in the worker's published operational state; managed child sessions are daemon records), and it already owns attention. Each fact has one owner and the hold is a projection, not a second state machine.
  Date/Author: 2026-11-13 / parent session.
- Decision: the open request replaces the latest-turn rule as the single source of semantic needs-input. The daemon no longer derives NeedsInput from "the latest turn's assessment is AwaitInput"; it reads the worker's open request.
  Rationale: two predicates for one question ("does this session have an unanswered question") are a bug even while they agree (AGENTS.md, Concurrency). Pending forms, approvals, reviewer questions and promoted child forms are separate facts and keep their own paths.
  Date/Author: 2026-11-13 / parent session.
- Decision: only the person closes an open request: a prompt the user sent, a form answer, or dismissing the notification. Generated prompts (mailbox wakes, task notifications, quota resumes, peer-session messages) do not close it. A later turn that itself ends needing input replaces it with the new question. A failed, cancelled or empty later turn leaves it as it is.
  Rationale: the user's rule that needs-input resolves only by input. The study shows later agent-only turns almost never settle the question.
  Date/Author: 2026-11-13 / user rule, recorded by parent.
- Decision: a durable, worker-owned person-input frontier fences every question source. The snapshot keeps `person_input_through` (the journal ordinal of the latest person input: a prompt from the person, a human form answer, or DismissQuestion), advanced in snapshot apply from those events, so replaying an old journal derives it with no special case. The snapshot also keeps the latest request raised by an input-requiring completion (set or replaced by an AwaitInput assessment or a harness InputRequired completion, with the question locator only filling `question_message` on a request of the same identity). The published open request is a function of the two: that request when its `completed_ordinal` is greater than `person_input_through`, otherwise none. Person input only advances the frontier; nothing "clears" or "re-sets" the request.
  Rationale: the implementation child found that a late assessment or question-locator update for a completion the person had already answered reopened the request (a human form answer cleared it; a later TurnAssessmentUpdated for the same completion set it again). The same happens when a person types before the slow end-of-turn verdict arrives. Assessment invalidation would need ordering rules for every late update and for old journals; a monotone frontier makes the reopening unrepresentable, because no update about a completion at or before the frontier can produce an open request in any order. This is a race found while implementing, before the code was believed done, so its test is a tricky-condition test without a Hard-won marker.
  Date/Author: 2026-11-13 / parent session.
- Decision: existing state carries the open request across upgrades and checkpoints. A relay snapshot stored before state version 23, and a checkpoint without the new field, are seeded once by one shared helper from the legacy facts: an undismissed current AwaitInput assessment (or a harness InputRequired stop recorded in its completion) becomes the request, and the frontier is set to `latest_prompt_accepted_ordinal` when that prompt came after the completion, reproducing the old latest-turn rule. Checkpoints carry `open_request: Option<CheckpointOpenRequest { request, person_input_through }>`; absent means older (seed), present is exact (including empty). The worker publishes `person_input_through` only for export; restore sets the request to the exported derived open request and the frontier to the exported frontier, which is behaviorally lossless because the frontier alone fences every late update for an earlier completion.
  Rationale: both pre-push reviewers found that an upgraded worker or a restored checkpoint would otherwise load `request: None` and drop a pending `?`, against the upgrade and checkpoint-fidelity rules. A single optional field keeps `Checkpoint.version` at 1, so an older worker ignores it and keeps today's behavior.
  Date/Author: 2026-11-13 / parent session, from review.
- Decision: the open request keeps the full `InferredQuestion` identity (turn id plus completed ordinal), not the ordinal alone; dismissal is validated against the open request instead of the latest assessment.
  Rationale: the existing dismissal payload, daemon action and worker validation already use `InferredQuestion`; keeping it avoids a wire-shape change and a legacy branch. Only the validation's reference point moves, which the latch requires anyway (the implementation child found this, 2026-11-13).
  Date/Author: 2026-11-13 / parent session.
- Decision: hold only when the harness has reliable background inventory: Claude and Codex.
  Rationale: the user's direction ("mj works amazing with Claude and Codex, best effort for the rest"). Claude and Codex report background tasks through the pinned AIR bridge; Kimi reconciles a journal with provisional tasks, and other ACP harnesses only expose terminals Mjolnir hosts. Other harnesses still get the open request (so their questions no longer disappear); they just never hold.
  Date/Author: 2026-11-13 / user decision, recorded by parent.
- Decision: while held, show neither `?` nor a notification.
  Rationale: the user's explicit choice between that and "show ? but skip the notification".
  Date/Author: 2026-11-13 / user decision.
- Decision: "waiting on work it started" means at least one of: a running managed child session owned by this session; a running native sub-agent; a background command while the latest assessment judged background work needed (`background_needed == Some(true)`); or a harness turn imminent because a task just settled (`turn_imminent`, 60-second window).
  Rationale: children are deterministic facts. A bare background command is not: a server or watcher left running would otherwise hold a question forever, and an indefinite hold is a lost alert. The classifier's existing judgment is the only dependency signal available. The imminent window covers the one to three seconds between a task finishing and Claude's notification turn starting, so the question is not released and then immediately re-held.
  Date/Author: 2026-11-13 / parent session.


## Outcomes & Retrospective

Child/notification review follow-up: parent holds now include a managed child with Background activity and no foreground turn. Legacy workers retain last-reply notifications; modern missing targets remain generic. Existing projector and notification tests were extended. Targeted controller attention/TUI notification checks and Clippy (controller, TUI, core, CLI) passed outside the sandbox; fmt and whitespace checks passed. Evidence: `review-child-notify-{tests,clippy,fmt}.log` in the report directory. No commit.

Persistence review follow-up: pre-23 worker snapshots and legacy checkpoint seeds preserve unanswered legacy questions through one load-boundary helper. New checkpoint export/restore preserves derived open request and frontier exactly. The real-load regression covers dismissed, later-prompt fenced, harness InputRequired, legacy checkpoints and current exact-empty snapshots. Focused checks pass (11 core, seven worker); Clippy passes for core, worker, controller, chat and CLI. Logs are `review-upgrade-{core,worker,clippy,fmt}.log` in the report directory.

Activity-gate review follow-up (2026-11-13): restored the existing Idle activity gate for both worker protocol paths while retaining header and person-position guards. Kimi's no-hold case now uses a running managed child; with the child finished, explicit Background activity remains Working for Claude and Kimi. The person-authored frontier field comment is accurate without a rename. All requested follow-up checks pass outside the sandbox: 13 controller attention/push/dismiss tests, controller Clippy with warnings denied, formatting and whitespace. Logs and details: `.mj/agents/ad0c15bbdaf0aeb6567a40ee8a66a5bf/review-activity-result.md`. No commit was made.

Final implementation checkpoint (2026-11-13): Milestones 2–4 are implemented and uncommitted on master. The frontier ordering/replay regression passes, and no design question remains. Full workspace Clippy and final formatting pass. The full eight-package TZ=UTC run completed with --no-fail-fast: an unchanged controller readiness test exceeded its 250 ms assertion, and one CLI fixture still supplied only the assessment; all other cases passed. The CLI case now supplies the published request and verifies persistence across a later assessment, holding and closure. Both affected tests pass their targeted follow-up, and the follow-up CLI Clippy check passes. The full run was not repeated after the fixture-only fix. Live acceptance is owned by the parent. Detailed results and logs are in `.mj/agents/ad0c15bbdaf0aeb6567a40ee8a66a5bf/implementation-status.md` and `full-suite-results.txt`.

Frontier validation checkpoint (2026-11-13): the ordering/replay regression passes for late locator/assessment updates after a human form answer and for a late AwaitInput callback after a person prompt. The old dismissal test now checks that late classification may finish while publication remains closed; its targeted rerun passes. Workspace Clippy with warnings denied and formatting pass. Full dev-profile tests of all eight touched packages are running with TZ=UTC and --no-fail-fast; the controller readiness cancellation test has failed and its failure detail will be examined after the suite finishes. Current logs: `frontier-targeted.log`, `frontier-dismiss-rerun.log`, `final-clippy.log`, `final-fmt.log`, `final-tests.log` under the report directory.

Implementation checkpoint (2026-11-13): the worker latch, daemon projection/hold, notifications and compatibility code are present, uncommitted. Three targeted controller tests, two core tests, the worker replay/dismissal/person-input regression, the extended child InputRequired/replay case and seven TUI notification cases pass. Formatting and whitespace checks pass. Details and resolved fixture failures are in `.mj/agents/ad0c15bbdaf0aeb6567a40ee8a66a5bf/implementation-status.md`. A late form-answer/question-locator race requires a parent design decision before the implementation is complete. The full clippy/three-crate validation and actual new-code live acceptance remain pending; the isolated installed-CLI attempt failed its sandbox loopback bind.

Milestone 1 (2026-11-13). The question was whether a "maybe it's resolved" check is worth running when a held question is released. Two blind labelers per case, with a third adjudicating disagreements, labeled 149 production AwaitInput turns that were followed by an agent-only turn. 143 contained a real request. Of those, the agent settled 2 to 4 in its next agent-only turn (1.4% to 2.8%); about 98% stayed open. GLiDE with a frozen five-label question wrongly closed none of 97 agreed open tuning cases at a 0.5 to 0.9 cut and closed the one agreed settled tuning case, but with so few settled cases its benefit cannot be measured, and the expected value is small. The user chose to ship the hold and always show the question when the wait ends. The study also confirms the latch: a needs-input question kept open across agent-only turns is still open about 98% of the time, so keeping it until the user answers is correct. One limit: each case covers only the first agent-only turn after the question; over a longer wait a few more might be settled.


## Context and Orientation

Mjolnir is a Rust workspace. Its pieces relevant here:

The daemon (`mj daemon-run`, crate `mj-controller`) is the long-lived control process. It owns the database and publishes a runtime projection (`mj_client::runtime_feed::RuntimeProjection`, in `mj-client/src/runtime_feed.rs`) to every client: the TUI (crate `mj-tui`), the browser viewer (`mj-controller/src/web/viewer.js`) and `mj sessions` (crate `mj-cli`). A worker (crate `mj-worker`) runs one coding session: it drives the harness process (Claude Code, Codex, Kimi and others, through the Agent Client Protocol, called ACP), keeps a journal of everything that happened, and publishes an operational snapshot (`RelayOperationalState`, in `mj-core/src/relay/snapshot.rs`) that the daemon copies into `RuntimeSessionView.operational` (`mj-client/src/daemon.rs`). A worker keeps running when no daemon is attached, and a new daemon reattaches and replays from the worker's journal; therefore any state that must survive a daemon restart belongs in the worker's journaled snapshot or in the database.

Session attention is the daemon's single answer to "does this session need the person", an enum `mj_core::attention::SessionAttention` (`mj-core/src/attention.rs`) with values such as NeedsInput (shown as `?`), Important (`!`), Unread, Working, Idle, Failed, Unreachable. It is computed by one function, `mj-controller/src/daemon/attention.rs::project_states`, called incrementally by `update` (same file) for the sessions in a dirty set whenever records, sessions, transcripts or native agents change. Every alert path reads only this served value: TUI notifications (`mj-tui/src/notify.rs`), the browser (`viewer.js`), and phone Web Push (`mj-controller/src/web_push.rs`, function `episodes`, which only considers NeedsInput and Important). Next to attention the daemon publishes a notification dismissal target per session (`attention.rs`, the dismissal section; `NotificationDismissal::Question`).

Today semantic NeedsInput comes from `MaterializedSession::unanswered_input_outcome` (`mj-core/src/state.rs`): no active or queued turn, the latest turn outcome requires input (`MaterializedTurnOutcome::requires_input` in `mj-core/src/event_outcome.rs`: the current assessment's action is AwaitInput and the question was not dismissed, or the harness itself reported InputRequired), and no user transcript item came after it. The assessment (`mj_core::assessment::TurnAssessment`, `mj-core/src/assessment.rs`) only ever describes the latest completed turn, which is why a question disappears after a later agent-only turn. `attention.rs` calls `unanswered_input_outcome` both in `project_states` (to decide NeedsInput) and in the dismissal section (to build the question dismissal target and the inferred-question identity).

The end-of-turn assessment comes from GLiDE through Mjolnir's proxy. The worker applies it in `mj-worker/src/relay/verdict.rs` (`apply_glide_assessment`, `store_assessment`, which journals `RelayObservation::TurnAssessmentUpdated`); replay applies that observation in `mj-core/src/relay/snapshot/apply.rs`. The question locator (`mj-core/src/activity/question_locator.rs`, stored by `verdict.rs::store_question_target`) records which transcript message holds the question as `TurnAssessment.question_message` (a transcript stable id). Web Push already recovers question text from that id (`web_push.rs`, around the content builder). Dismissal arrives as `RelayCommand::DismissQuestion`.

Running work, as the daemon sees it: `RelayOperationalState.background_commands` (`Vec<BackgroundCommand>`), `RelayOperationalState.native_agents` and `native_agent_count`, and managed child sessions (`RuntimeProjection.subagents`, a map from child id to `SubagentMetadata` with `parent_session_id`, plus each child's own `RuntimeSessionView`). A managed child counts as running when its own session view shows an active turn or queued work, not merely because its metadata exists. Whether a background command matters comes from the latest assessment's activity facts, `ActivityFacts::background_needed` (`mj-core/src/activity.rs`). When a background task finishes, the worker sets `task_settled_at_ms`, and `mj_core::activity::turn_imminent` reports a harness turn as imminent for up to `IMMINENT_TURN_WINDOW_MS` (60 seconds) after the settle.

Generated prompts: `mj_core::continuation::is_generated_prompt(command_id)` (`mj-core/src/continuation.rs`) identifies prompts Mjolnir generated rather than the person. Harness capabilities are methods on `mj_core::config::HarnessKind` (`mj-core/src/config/harness.rs`, for example `has_native_subagents`).

Terms used below. An open request is a question from a turn that needed input, which the person has not answered or dismissed. Held means an open request exists but the daemon shows the session as Working because a Claude or Codex agent is waiting on work it started. Released means the daemon shows the open request as NeedsInput.


## Plan of Work

Milestone 2 makes the worker own the open request. In `mj-core/src/assessment.rs`, next to `TurnAssessment`, define:

    pub struct OpenRequest {
        /// The turn that raised it (turn id and completed ordinal): the
        /// identity used by the dismissal target, DismissQuestion validation,
        /// notifications and Web Push for this request.
        pub question: InferredQuestion,
        /// Transcript stable id of the message holding the question, when the
        /// question locator found one; otherwise the turn's final reply.
        pub question_message: Option<String>,
    }

The identity reuses the existing `InferredQuestion { turn_id, completed_ordinal }`, so the dismissal payload keeps its shape. What changes is the check: the worker's command validation (`mj-worker/src/relay/commands.rs`, DismissQuestion) and snapshot apply validate a dismissal against the open request's identity instead of the latest assessment's, because the open request outlives that assessment. Below, "asked_at_ordinal" means `question.completed_ordinal`.

Store the latest request as `request: Option<OpenRequest>` and the latest person-input ordinal as `person_input_through: u64` in `RelaySnapshot` (`mj-core/src/relay/snapshot.rs`), both serde-defaulted. `RelaySnapshot::open_request()` derives the open request when its completion exceeds that frontier; publish this derived value in `RelayOperationalState::open_request`. Do not copy question text into it; readers recover text by id as Web Push does now (keep one place that turns an id into display text; search before adding a helper).

All transitions happen in snapshot apply (`mj-core/src/relay/snapshot/apply.rs`) from journaled observations and commands, so replay reproduces them and no new observation kind is needed unless an existing one lacks a fact (if so, add it and record why here):

- A completed top-level turn whose assessment action is AwaitInput sets the retained request (replacing any older one) with that turn's completed ordinal and question message. A completed turn the harness reported as InputRequired does the same. Apply both when the facts arrive (the assessment can arrive after the completion; the question locator after the assessment, which then fills in `question_message` for the same ordinal).
- A prompt from the person, a human form answer and `RelayCommand::DismissQuestion` advance `person_input_through` to their journal ordinal. They leave the retained request intact. Use the existing classification of generated prompts; mailbox wakes, task notifications, quota resumes and peer-session messages do not advance the frontier. Locator updates use `QuestionTargetUpdated` and fill only the retained request with matching identity; they cannot create a request.
- A later completed turn that is not AwaitInput, including a failed, cancelled or empty turn, leaves it unchanged.
- Child sessions are never classified (existing rule), so they never get one.

Bump the relay protocol in `mj-core/src/relay.rs` (check master first), because the published operational state gains a field the daemon reads.

Milestone 3 makes the daemon display it. Replace the semantic part of `unanswered_input_outcome` with the open request so there is one predicate: the session has semantic needs-input when its worker publishes an open request, its activity is Idle, there is no active or queued header turn, and no person's prompt is known at a position after `asked_at_ordinal` (the existing guard against a transcript that is newer than the published snapshot; keep its semantics, not a second rule). Update both call sites in `attention.rs` (the NeedsInput decision and the question dismissal target, whose identity becomes `asked_at_ordinal`). The harness-reported InputRequired case is covered because the worker sets the open request for it.

A worker that predates the field publishes no `open_request` at all. Represent that as unknown, distinct from "no open request", and keep today's latest-turn predicate only for that case, with a comment naming the relay protocol after which it can be removed. Do not let an old worker lose its `?`.

Add `HarnessKind::has_reliable_background_inventory()` to `mj-core/src/config/harness.rs`, true for Claude and Codex only, with a doc comment naming the AIR bridge as the reason. In `attention.rs`, compute for each top-level session a `waiting_on_own_work` fact as defined in the Decision Log. Then, where `project_states` decides NeedsInput from the open request, apply the hold: if the harness has reliable inventory and `waiting_on_own_work` is true, the session is Working instead. Pending forms and approvals, reviewer questions and promoted child forms are never held. The dirty-set logic in `update` must reproject a parent when a managed child's running state changes (it already expands managed ancestors; confirm child state changes reach it, and add the missing path if not).

Web Push (`web_push.rs::episodes`): when NeedsInput comes from an open request, use `asked_at_ordinal` as the episode source identity, so a release after later turns is one new episode for that question, and a held request never produces one. Notification text, in the TUI, the browser and Web Push, comes from the open request's question message, not from the latest turn's final reply. Log each hold and release at INFO with the session id, `asked_at_ordinal` and which running work held it, so behavior can be audited from production logs. Bump the daemon protocol in `mj-client/src/daemon.rs` (check master first; it was 95 on 2026-11-13) and keep the normal exact-protocol upgrade.

Milestone 4 validates: compatibility with an older worker, the end-to-end behavior below in an isolated instance, and the full checks.


## Concrete Steps

All commands run from the repository root, `/home/jonathan/Projects/mjolnir/.mj/clones/4f86b847ae014721aac99daaffd3ae50`, outside the restricted sandbox (tests use loopback sockets). Never touch the default Mjolnir instance; any binary run uses `--instance hold-needs-input`.

Targeted tests while iterating:

    cargo test -p brokk-mj-core -- open_request assessment snapshot
    cargo test -p brokk-mj-worker -- verdict open_request
    cargo test -p brokk-mj-controller -- attention web_push dismiss

At the end, once:

    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    TZ=UTC cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-transcript


## Validation and Acceptance

Latest parent instruction (2026-11-13): skip live acceptance in this worker; the parent runs it. Run the full suites of all touched crates, including tui, CLI (`brokk-mjolnir`), client, chat and transcript, in addition to core, worker and controller.

Add tests of these kinds only (the repository's rules allow golden, end-to-end, integration and tricky-condition tests; extend existing tests where one fits):

Snapshot apply and replay (tricky condition: ordering, persisted format): an AwaitInput assessment creates the open request even when it arrives after the completion; a later agent-only turn that ends Finished keeps it; a generated prompt keeps it; a person's prompt clears it; DismissQuestion clears it; a later AwaitInput turn replaces it; replaying the journal into a fresh snapshot reproduces the same open request; a pre-23 snapshot seeds its undismissed legacy AwaitInput or harness InputRequired request once, fenced by any later accepted prompt; a current snapshot preserves exact empty publication.

Daemon projector (`attention.rs` tests): a Claude session with an open request and a running managed child is Working, and becomes NeedsInput when the child finishes and the imminent window passes; the same with a Kimi session is NeedsInput immediately; a session whose later turn ended Finished still shows NeedsInput for the earlier open request; a pending form is NeedsInput even while children run; a change in a child's state alone reprojects the parent; a worker that publishes no field (older protocol) gets today's behavior.

Web Push: a held open request produces no episode, and its release after two later turns produces exactly one episode keyed by `asked_at_ordinal`, whose body is the original question.

Acceptance by behavior, in an isolated instance with a Claude session: start a background job and end the turn with a question deferred to its result; the dashboard shows the session as working with no `?`; when the job ends and the next turn ends, `?` and a notification with the original question appear; the daemon log shows the hold and release lines.


## Idempotence and Recovery

The new snapshot fields are serde-defaulted. At the single durable load boundary, pre-23 snapshots seed their legacy current input-requiring completion once. New checkpoint envelopes preserve derived open_request and person_input_through exactly, including empty; older checkpoints use the same seeding helper. The daemon keeps today's behavior for workers that predate the field. If the hold misbehaves in production, making `has_reliable_background_inventory` return false for a harness disables holding for it without other changes; the open request still keeps questions from disappearing.


## Artifacts and Notes

Study evidence: `/mnt/optane/tmp/eot-continue-study-20261112/false-alert-breakdown/results.json` (227 of 347 non-needs-user alerts are waits) and `/mnt/optane/tmp/held-request-study-20261113/` (Milestone 1: `production-frame/pass1-final.jsonl`, `labels-pass2-*.jsonl`, `results-resolution-*-tuning/`, `PARENT-TRACKING.md`, scripts under `scripts/`).


## Interfaces and Dependencies

No new crates. New public items: `mj_core::assessment::OpenRequest`, `RelaySnapshot::request`, `RelaySnapshot::person_input_through`, derived `RelaySnapshot::open_request()`, its operational-state copy, and `HarnessKind::has_reliable_background_inventory`. Relay and daemon protocol numbers advance by one each.


Revision note (2026-11-13): initial plan written by the parent session from a read-only source map, after the user decided the design on issue #1518.

Revision note (2026-11-13, later): after the first calibration was found not to test the check, Milestone 1 was rebuilt around production AwaitInput turns with a request-identification pass.

Revision note (2026-11-13, final): Milestone 1 found agents settle about 2% of held questions on their own; the user chose to drop the resolution check. Removed the GLiDE question, resolution state, settled-becomes-Important rule and proxy work; made the open request the single source of semantic needs-input, with a compatibility path for older workers; condensed the superseded first-attempt material into Surprises and Outcomes.

Revision note (2026-11-13, implementation): recorded child-ownership replay, automatic form resolutions, person-only publication frontier, selected protocol/store revisions, and initial validation findings.

Revision note (2026-11-13, frontier): aligned the implementation steps with the parent’s derived-open-request decision and expanded final validation to every touched crate; live acceptance is delegated to the parent.

Revision note (2026-11-13, final validation): recorded the full eight-package result, the corrected CLI fixture and passing targeted follow-up, the unchanged readiness timing observation, and passing workspace/follow-up lint and formatting checks. No commit or installation was performed.

Revision note (2026-11-13, review): restored the shared activity-idle gate, corrected the harness/background assertions and documented SessionTail.latest_user_position as person-authored. Only the requested affected controller checks are run for this follow-up.

Revision note (review upgrade/checkpoint follow-up): recorded the shared loader and the missing exporter transport facts; implementation paused for the parent’s contract decision.

Revision note (review persistence decision): implemented shared pre-23/legacy-checkpoint seeding, and the parent-selected CheckpointOpenRequest envelope (absent is legacy; present is exact). RelayOperationalState exports only the person frontier in addition to the existing derived request; protocol remains unshipped 64.

Revision note (child/notification review): shared operational activity now covers a managed child’s background work and other non-idle activity; pre-64 notifications retain their historical last-reply fallback through the common question capability check.
