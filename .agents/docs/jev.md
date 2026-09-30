# Jev in Mjolnir: design and history

This is the one design document for Mjolnir's use of Jev. It replaces the twelve ExecPlans and four evaluation notes written between 2026-09-19 and 2026-09-27, which are listed at the end with the Git commands to read them. The current plan of work is `.agents/plans/jev-quiet-and-scenario-suite.md`.

## What Jev is

Jev is TypeSafe's "System One" classifier. A request carries a JSON `state` and a fixed set of typed questions; the answer is a choice with a confidence, or a probability, and never generated text. Mjolnir sends bounded evidence about one session turn and asks three independent questions: did the turn fail and how, does the user need to respond, and what authorized work remains. The answers are advisory. Deterministic code in the worker and daemon decides what to do with them, and every automatic action has a confidence threshold, a stable command identity, and a fallback to today's behavior when Jev is unavailable, uncertain, or wrong.

Requests go through the public Cloudflare proxy in `services/jev-proxy/` unless a local TypeSafe key exists (`TYPESAFE_API_KEY` or `~/.secrets/typesafe_api_key`), in which case the worker calls TypeSafe directly and the daemon forwards the key to container and SSH workers. `[jev] enabled = false` disables everything. The proxy runbook, routes, and deployment history are in `.agents/docs/jev-proxy.md`.

## Principles

These are the rules the plans converged on, plus Jonathan's corrections. They are the criteria for any change.

Jev answers questions; code takes actions. A verdict is evidence about the conversation. It becomes an action only through a deterministic policy with explicit thresholds, and the action is admitted by the worker under its serialized relay owner with a stable command identity, so a replay cannot double-execute.

Fail toward today's behavior. An unreachable proxy, a malformed reply, or a low-confidence answer leaves the session exactly as it would be without Jev. Automatic actions need 0.90; activity inferences need 0.85.

The worker owns classification and admission; the daemon consumes. Every physical completion, prompted or harness-started, gets one durable assessment in the worker's relay journal. The daemon reads that record for continuation, quota recovery, `mj wait`, and presentation. It does not classify on its own for current workers.

Jev never manufactures authorization. Automatic continuation supplies a fixed prompt that grants no new approval. Evidence for the work question is the whole chronological set of real user messages since the last context reset, because authorization can be several exchanges back. If that history is incomplete, the worker abstains from continuation.

Diagnostics go to logs, not the UI. Each decision is recorded with its exact evidence, thresholds, and applied outcome in `jev-decisions/decisions.*.jsonl` under each worker root and under the daemon data directory (four rotating 8 MiB segments). The UI shows plain outcomes only, for example "Classifier: The agent appears to be waiting for you."

Simplicity over bulletproofing. Where a person retries, a notice is enough. Durability is spent only where a program would otherwise double-execute (continuation, retries, quota resume).

## The contract today: turn-verdict-v5

Constants: `mj_core::assessment::PROTOCOL` = 25, `AUTOMATION_CONFIDENCE` = 0.90; `mj_core::activity::verdict::ACT_CONFIDENCE` = 0.85, `NO_INPUT_CONFIDENCE` = 0.15, `SERVER_RETRY_CONFIDENCE` = 0.90. Questions are the bundled `mj-core/src/activity/verdict_questions.json`, shared byte-for-byte with the proxy. Older question files (`_v1` to `_v4`) are frozen for released workers.

### Evidence

`mj_core::activity::verdict::TurnEvidence` is the request `state`:

- `harness`, `phase` (`running` or `replied`), `silent_for_s`
- `tools_in_flight` (title and running seconds, at most 16), `background_commands`, `queued_commands`
- `user_prompt_tail` (1 KiB), `assistant_text_tail` (2 KiB)
- `transcript_summary`: the conversation from the latest delivered user message, with all transcript tool entries removed before budgeting (the "0+1" policy chosen in the 2026-09-20 comparison), at most 48 KiB. Cleared when `authorization` is present.
- `authorization`: `ContextHistory` with whole real user messages since the last context reset (32 KiB) and whole recent assistant replies (16 KiB), plus explicit omission flags. Present for `replied` assessments. Dropped, with the summary, if the serialized request would exceed 60 KiB.
- `completion` (replied only): stop reason and a bounded provider diagnostic (message 4 KiB, code 128 B, reset text 256 B)

The evidence collector lives in `mj-transcript/src/turn_context.rs` (process-local `TurnContext`, fed from `DurableRelay::record_session_update`) and `mj_core::assessment::ContextHistory` (durable, persisted in the relay snapshot and seeded from the controller at a checked frontier after upgrades).

### Questions and answers

Four Choice questions, each returning `choice` and `confidence` (the fourth was added on 2026-09-29 with the `/v6/turn-verdict` route; v5 is frozen in `verdict_questions_v5.json`):

- `background`: `needed`, `unneeded`, `unclear`. Judged only over the `background` list in the evidence; used only for the quiet rule, never for the action policy.

- `failure`: `none`, `transient_provider`, `quota`, `other`, `unclear`. Judged from the current final reply and diagnostic only, never quoted or historical failures.
- `input`: `none`, `redundant_request`, `required`, `unclear`. Does the assistant currently need a response from this user? An unresolved decision, choice, missing fact, approval, review, or external action counts even without a question mark and while independent work continues. Chronological authorization distinguishes decisions governing requested work from optional offers after a delivered answer; already given permission is `redundant_request`.
- `work`: `finished`, `authorized_unfinished`, `waiting`, `unclear`. Assessed against all chronological user instructions; later status questions do not cancel earlier tasks; runtime facts distinguish real background work from historical tool output.

`mj_core::activity::verdict::TurnVerdict::parse` also accepts the older v3/v4 shape (`needs_user_input`, `work_state`, `retryable_server_error`) and maps v5 onto it, so both proxies' answers feed one decision path.

### Action policy

`mj_core::assessment::Verdict::action(authorization_complete)`, in order:

1. `input = required` at 0.85 or more: `AwaitInput`.
2. `failure` at 0.90 or more: `transient_provider` gives `RetryProvider`, `quota` gives `RecoverQuota`, `other` gives `AwaitInput`.
3. Any failure choice other than a confident `none`: `Uncertain`. Unknown failures must not become nudges.
4. `work = authorized_unfinished` at 0.90 and `input` in {`none`, `redundant_request`} at 0.90 and authorization history complete: `Continue`.
5. `work` at 0.85 and `input = none` at 0.85: `finished` gives `Finished`, `waiting` gives `Wait`.
6. Otherwise `Uncertain`.

The worker (`mj-worker/src/relay/verdict.rs::apply_turn_assessment`) stores the verdict and action in the durable `TurnAssessment`, sets a status (`Assessed`, `Scheduled` for an armed retry, `Deferred` for continuation or quota resume, `Superseded` when a question, paused goal, budget limit, or queued prompt suppresses automatic action) and a reason string, then maps the action to a process-local activity `Decision`: `AwaitInput` to `AwaitingInput`, `Finished` to `InferIdle`, `Wait` to `ExpectContinuation`, everything else `KeepCurrent`.

The activity decision is applied only if the turn generation is unchanged and nothing "blocks" it: a foreground turn or tool, a running goal, a queued prompt, or a closed session. It is process-local, lost on restart, and invalidated by any new foreground activity or a change of background-task identity. A valid answer, including an uncertain one, is cached until the relevant evidence changes (`background_commands` or `queued_commands`, a new prompt, a harness turn). Only request failures (transport, malformed reply) retry, after 60 s doubling to 300 s. (`docs/src/content/docs/sessions.md` still says uncertain answers retry; that sentence is stale.)

Note two things the policy does that no plan stated: `failure = other` at 0.90 or more yields `AwaitInput`, so a confidently classified local error looks like a question to the user; and `Finished`, `Wait`, and `Continue` all require `failure = none` at 0.90 or more, so the effective threshold for idle inference is higher than the stated 0.85 whenever the failure axis is unsure.

For a running turn, the worker asks after 60 s of silence (doubling to 5 min) with `phase = running`; the only decision it may take is `AwaitingInput`, which ends Mjolnir's tracked turn with stop reason `awaiting_input` while the harness keeps serving; a late harness reply is discarded.

### Where the result is visible

- `RelayOperationalState.assessment` (a `Summary`: turn id, revision, status, action, reason, retry time) and `turn_completion` (command id, ordinal, decision), published to the daemon.
- `ActivityFacts.inferred_idle_since_ms` and `expected_continuation`, folded into `ActivityState` by `mj_core::activity::classify`.
- The decision logs above, with `mj_jev=info` tracing in `worker.log`.

## Stakeholders: who consumes a Jev answer today

Each row is a consumer of the verdict or of the activity inference derived from it. File paths are current as of 2026-09-29.

| Stakeholder | Reads | Threshold | Action |
| --- | --- | --- | --- |
| Running-turn input detection (`mj-worker/src/acp/verdict_client.rs`, `acp/session.rs`) | `input = required`, phase running | 0.85 | Ends tracked turn with `awaiting_input`; transcript notice; `mj wait` returns `input_required` |
| Replied-phase activity inference (`mj-worker/src/relay/verdict.rs`, `mj-core/src/activity.rs`) | `Finished`, `Wait`, `AwaitInput` actions | 0.85 | `ActivityState::Idle` (inferred) or `Expecting`; never makes a worker replaceable |
| Automatic authorized continuation (`mj-controller/src/daemon/continuation.rs`, `mj-core/src/continuation.rs`) | `Continue` action, `continuation.eligible()` | 0.90 on work and input | Submits the fixed Continue prompt, at most three per user message; review waits for the chain to settle |
| Transient provider retry (`mj-worker/src/worker_runtime/unix/dispatch.rs`) | `RetryProvider` | 0.90 | Arms durable 1/2/4/8/16-minute backoff, submits `Continue` when due and quiet, preserving the original authorization |
| Quota recovery (`mj-controller/src/daemon/continuation.rs`, `mj-controller/src/quota.rs`) | `RecoverQuota` | 0.90 | Daemon computes the reset deadline from quota reports and schedules one resume 60 s after all exhausted windows reset; separate allowance |
| `mj wait` and `mj wait --turn` (`mj-controller/src/server/api/wait_policy.rs`) | `turn_completion.decision`, assessment status | as above | Pending during classification, retry backoff, or expected continuation; final on finished or input |
| Sub-agent `wait` tool (`mj-controller/src/server_runtime/api.rs::subagent_status`) | child session record, materialized summary, start status, close request; not the child's Jev assessment | | Answers the parent when all named children are done, from the child's own finished-turn message |
| TUI and web attention (`mj-tui/src/dashboard_sessions.rs`, `mj-chat`) | `ActivityState`, `last_turn_outcome` | | Waiting, Working ("expecting the agent to continue"), Idle |
| Turn review (`mj-controller/src/review_host/`, `daemon/continuation.rs::publish`) | continuation settlement | | Intended to run once after the continuation chain settles. For protocol-25 workers the daemon appears to start review at the physical idle edge before the worker's assessment has an action (unverified by a test; see plan) |
| Help search (`mj-core/src/help_search`, proxy `/v1/help-search`) | separate relevance questions | 0.70 | Not a turn stakeholder; listed for completeness |

Lifecycle decisions that do not consult Jev today: worker upgrade (`mj-controller/src/worker_upgrade.rs`), checkpoint admission (`mj-controller/src/controller/checkpoint/`), session move (`mj-controller/src/controller/move_session.rs`, `daemon/session_move.rs`), and sub-agent parking. They read `mj_core::activity::is_quiet` / `has_work_in_flight` / `safe_to_replace`, which are computed from process facts only. Every plan up to now stated the opposite constraint on purpose: "inferring idle must not erase owned work from that separate safety predicate."

## Quiet: the open design problem

"Quiet" means it is safe to stop the worker process without losing anything: safe to upgrade the worker binary, cut a checkpoint, or move the session to another target. Today there are two independent notions:

- `has_work_in_flight` (`mj-core/src/activity.rs`) answers "would killing the worker destroy something?" from process facts: an open turn, tools in flight, queued commands, background commands, user shells, agent terminals, a pending goal decision, unknown provider state. It ignores Jev on purpose.
- `driver_present` / `classify` answers "will something move this session on without Mjolnir?" and does use Jev's inferred idle and expected continuation.

Both fail, in opposite directions:

- False "not quiet": a `sleep infinity` or a watcher left running by the agent keeps `background_commands > 0` forever. The session is never upgraded, checkpointed, or moved, and the daemon cannot be upgraded routinely. This is the case Jev was mainly designed to fix, and it already fixes it for the activity display and for `mj wait`, but not for the lifecycle predicates.
- False "quiet": on 2026-09-29 session `6d26c6d3c2659ce212d82e60a7d89a37` (Claude) had started a `cargo nextest` run as a background task and ended its turn saying it was waiting. Jev answered `work = waiting` at 0.69 (uncertain, so no inference). At 14:54:17.85Z the task finished; Claude Code queued a `<task-notification>` and began a new turn internally. For about one second, `background_commands` was 0 and no agent output had reached the worker yet, so every process fact said idle. The worker-upgrade coordinator judged the session quiet, took the idle lease, and restarted the worker at 14:54:19Z; the notification turn died with `[Request interrupted by user]`. Jev would not have helped either: its evidence at that moment said zero background commands and it did not know a turn was about to start.

Jonathan's direction (2026-09-29): standardize quietness on the Jev classification with the existing heuristic as the fallback, rather than keeping two paths. Tweak Jev's questions if needed so it answers the quiet question well. The rule below is implemented as `mj_core::activity::quiet_at` (2026-09-29); `is_quiet`, `has_work_in_flight`, `safe_to_replace`, and `routine_checkpoint_wait` all delegate to it, so upgrade, checkpoint, move, park, and provider-retry submission share it without any call-site change.

### The single definition of quiet

One function, used by upgrade, checkpoint, move, park, and the activity display, with three layers:

1. Observed turn facts that Jev may never override. An open prompt or harness turn, a tool call in flight, a queued command, a pending goal decision, `acp_ready = false`, a held checkpoint barrier, and a "turn imminent" fact: a Claude background task settled within the last N seconds and no turn has opened since. That last fact is what the incident lacked, and the signal for it already arrives: when a task settles, the Claude adapter publishes `async_task_state_update` with `state: completed|failed|stopped` (claude-agent-acp 0.84.0, `dist/async-tasks.js`, `taskNotification` then `publishState`), and the worker already parses it in `mj-worker/src/acp/claude_tasks.rs::claude_async_task_control_update`, today only to clear the stop control. Claude Code always follows a settled background command with an autonomous task-notification turn (the adapter's `AUTONOMOUS_RESULT_ORIGINS` includes `task-notification`), so a settled task means a turn is coming. The worker should record the settle time; the imminent-turn window closes when a harness turn opens or after a bound (60 s is the starting point; the plan measures the real gap). Equivalent facts for other harnesses are listed in the plan.
2. Jev's judgment about leftover processes. When layer 1 is clear and the only remaining process facts are background commands or agent terminals, the replied-phase request carries the named commands (`TurnEvidence::background`: id, command line, age) and a fourth question, `background`: is the agent or the user still depending on any listed process? A confident `unneeded` (`Verdict::background` at 0.85 or more, stored per evidence generation like the idle inference, published as `ActivityFacts::background_needed = Some(false)`) makes those processes stop counting; `needed` or no answer leaves them counting. A user's own shell is never judged. The judgment lives only in the worker's memory; a restarted worker that sees leftover processes for an assessed turn it has not asked about itself asks Jev once more (#1202). On the hand-built fixtures a dev server left up for later scored `unneeded` 0.84 to 0.91 and a running test suite the reply promised to report on scored `needed` 1.00.
3. The existing process-fact rule when Jev is pending, uncertain, or unavailable: leftover background commands and terminals keep the session busy.

The implementation records a settle only from the Claude adapter's edge update with `completed` or `failed`; a task leaving the `background_tasks_changed` level does not count, because a stopped task leaves the level too and no turn follows a stop. The worker hides the settle once a turn starts after it (`activity_turn_started_at_ms` moves past it), and every consumer applies the 60 s window with its own clock, so a stale published settle cannot hold a session busy for more than a minute.

This makes the false "not quiet" fixable (a `sleep infinity` is judged unneeded and the session becomes quiet), keeps the false "quiet" impossible where facts exist (a turn or an imminent turn is never overridden), and reduces to today's behavior whenever Jev is silent.

### Why a narrower question

In the current worker decision logs, 43 of 54 assessed decisions ended `uncertain` (2026-09-27 to 2026-09-29). With that rate, a rule of "Jev decides, heuristic fills in" behaves like the heuristic 80 % of the time. The three-axis `work` question is about authorization and remaining work, which is a harder question than "is anyone waiting on this `sleep`?" The plan measures the confidence distribution of a dedicated `background_needed` question on real scenarios before relying on it.

## What the evaluations found

These findings shaped the current evidence and thresholds. Details, tables, and artifact paths are in the removed notes listed at the end.

- Tool history is noise. In the bifrost2 incident (session `240b3367…`, 2026-09-20), raw tool titles containing shell and quoted document text held post-reply `finished` confidence at 52-56 %. Removing tool history raised it to 86-88 %; short factual descriptions of tools reached 99 %. A prose warning in the question did nothing. This led to the shared transcript summary and then to the 0+1 evidence policy.
- 0+1 (latest delivered user message onward, no transcript tool entries, live runtime facts kept) was the strongest simple variant in a four-point strict pilot and an eighteen-label exploratory set (18/18 category matches, 17 above threshold, versus 16/18 for the others), used 24 KB instead of 212 KB of request bytes, and was faster. One counterexample (an implementation "not yet deployed") went from 89 % to 58 %, which argues for keeping the threshold and watching abstentions rather than lowering it.
- Question wording moves scores a lot. The v3 input question scored a real approval-plus-heap case at 0.67 until it was rephrased as a binary predicate that allows approval requests during independent work (0.93). The v4 retryability question needed calibration to score text-only capacity refusals above 0.90 while quoted examples, quota, and auth stayed below 0.20.
- Authorization needs history. With only the latest user exchange, the one mined continuation positive scored `unfinished` 0.37; with earlier authorization present, 0.94. The 0.90 continuation cutoff was chosen so the four synthetic positives pass with zero false positives on thirteen mined negatives; it is not a calibrated probability.
- Corpus reality. Historical API events do not retain Jev's runtime counters, and relay journals mostly start after checkpoint trimming. Exact runtime evidence existed only in the per-worker decision logs, which began in late September. Any real-scenario suite has to be built from those logs going forward, not reconstructed from the controller database.
- Repeatability: three repeats of one request vary by a few points of confidence; single observations are not evidence of improvement.
- Wording matters more than thresholds (2026-09-29, 74 recorded fixtures plus 3 hand-built, three repeats). Telling the `failure` question that an ordinary reply is `none` with high confidence moved that axis from 0.31 to 0.86 to 0.99 to 1.00 on the same evidence; telling `work` what waiting looks like (sub-agents, a started build, a promised relay) moved the incident's `waiting` from 0.69 to 0.99. Telling `work` that a question answered inside an open goal is finished over-corrected in one pass (15 wrong `finished` on should-continue positives) until the rule "a reply that itself says work remains is never finished" was added. What wording could not move: `redundant_request` confidence and a handback that ends with a tool call rather than text; those need evidence fields.

- Current decisions and optional scope (2026-09-30): Luna mined 17 additional real cases from 11 sessions, split by session before comparison. The selected input wording detected 9/9 required tuning requests versus 3/9 baseline, 6/12 held-out versus 3/12, and 16/42 existing-suite requests versus 9/42, with three repeats and no confident input alerts on 210 scored controls. It catches the 06:50 decision alongside a background benchmark, but misses two held-out requests, loses some finished/wait inferences, and retains the baseline's three harmful A03 goal-completion actions. The prompt was frozen before held-out evaluation; labels and thresholds stayed fixed. Details and digests: [input-detection experiment](jev-input-detection-20260930.md).

## What the live logs show (2026-09-26 to 2026-09-29)

The per-worker decision logs on Jonathan's host hold 502 decisions across 22 sessions: 168 worker turn-end assessments (56 with a v5 verdict), 131 running-turn checks on the older contract, and 203 daemon continuation decisions on the legacy contract. The catalog of 45 scenarios drawn from them is the seed of the test suite in the plan. The headline facts:

- Actions actually taken: `uncertain` 48 and `finished` 8 of the 56 v5 verdicts. `Continue`, `RetryProvider`, `RecoverQuota`, `AwaitInput`, and `Wait` never occurred. All 203 daemon continuation decisions were uncertain because `no_input_needed` never reached 0.90. Automatic continuation has never fired on this host.
- Real should-continue cases exist and are rare: a pass over 1,973 agent-then-user turn boundaries in 1,038 sessions since 2026-09-16 found 23 human positives (16 where the agent stopped with stated work remaining and the user typed "continue" or similar, 7 where it asked permission it already had) and 4 program-authored ones, about one percent of boundaries; four waited 1.9 to 6.2 hours for a person to notice. The blocker is the `input` axis: `work = authorized_unfinished` usually scores 0.92 to 0.99, `input = none` usually 0.37 to 0.82, even on replies that ask nothing. Most bare "continue" messages in the logs follow a provider failure or a restart, not a clean stop, and must not count as positives.
- Jev confident and wrong: a child that handed back with a tool call after its last text scored `authorized_unfinished` 0.99 (the evidence carries the last assistant text but not the final tool call); question-and-answer turns inside a session with an open goal scored `authorized_unfinished` 0.94; a Codex usage-limit message appended to normal text scored `quota` only 0.71; a plan awaiting approval and a "look at these samples" request scored `input = none`.
- Uncertain where a person would be sure: the incident's "waiting for the test run" (0.69 and 0.71), a parent waiting on two live sub-agents (0.76 unfinished instead of waiting), a finished reply with a caveat sentence (0.61 to 0.68 finished across six probes).
- Children never qualify for `Continue` because their authorization history contains only assistant messages (the parent's spawn prompt is not a user message), so `validate()` fails. Children are meant to be excluded, but by the daemon's rule, not by accident.
- The same shape as the incident survived once: session `a460eb82…` at 14:45:14Z on 2026-09-29 had 3 seconds between the task count dropping to 0 and the harness turn opening; no upgrade was due, so nothing happened.

## Known gaps

- Quiet is not unified (above).
- (Closed 2026-09-29.) `Continue` maps to `KeepCurrent` in the activity inference, so a confident `authorized_unfinished` with tasks listed used to leave the continuation blocked behind them for good. The `background` judgment now discharges unneeded tasks: `driver_present` goes false, and the worker's admission check (`relay/commands.rs`, which already tested `driver_present`) lets the continuation through. A worker test pins it.
- (Closed 2026-09-30.) The worker used to record `Continue` as `Deferred` whether or not `[continuation]` was enabled. The setting now travels to the worker at launch (`MJ_CONTINUATION_DISABLED`), and a `Continue` verdict is recorded as `Assessed` with reason `continuation_disabled` when nobody will act on it. Children are excluded by the daemon; they also cannot reach `Continue` because their history holds no user message.
- Provider-retry submission requires `is_quiet`, so a leftover background command blocks an armed retry.
- (Closed 2026-09-29.) The dead `RuntimeEvent::ContinuationExpected` and `TurnVerdict::should_retry_server_error` are removed.
- The proxy runbook now records v5 and v6; v6 is not deployed as of 2026-09-29 and must be before a worker that calls it ships.
- Turn-imminent detection: Mjolnir cannot see a harness turn that has started but produced no output. For Claude, the settle signal (`async_task_state_update`) is available and unused for this purpose. For Codex exec cards, Kimi tasks, and the other harnesses, whether an equivalent signal exists is unchecked.
- The uncertain rate is high; there is no measurement of why (evidence missing, question wording, or genuinely ambiguous cases).
- Real-scenario tests: existing tests pin mechanics with fixed fake probabilities; model behavior is checked only by small synthetic runs and two one-off replays. Nothing replays a live-log scenario end to end.
- `Expecting` (expected continuation) has no timeout, by design; a wrong `waiting` verdict can show "expecting the agent to continue" until the next turn.
- The sub-agent `wait` tool and parking answer from child completion; whether the parent's own Jev state should affect them is unspecified.

## History: the plans this document replaces

Read any of them with `git show <commit>:<path>`; the commit is the last one that touched the file before removal. They were removed in the commit that added this document.

- `.agents/plans/jev-turn-verdicts.md` (b54b2449, 2026-09-19). First integration: two-direction classifier (running silence to awaiting input, replied to expected continuation), `TurnEvidence`, `Expecting` activity state, TypeSafe direct client, 0.85 threshold, database revision 39.
- `.agents/plans/public-jev-proxy.md` (e5c72c8e, 2026-09-19). Cloudflare Worker proxy `/v1/turn-verdict`, shared question JSON, rate limits, direct-versus-hosted routing.
- `.agents/plans/jev-turn-evidence-comparison.md` (af11979f, 2026-09-20) with `.agents/docs/jev-turn-evidence-comparison-20260920.md` and `.agents/docs/jev-bifrost2-evidence-experiment-20260920.md`. The evidence experiments summarized above; adoption of 0+1.
- `.agents/plans/automatic-authorized-continuation.md` (70554040, 2026-09-20) with `.agents/docs/jev-continuation-evaluation-20260920.md`. Continuation contract, `/v1/continuation-verdict`, three nudges per user message, the `[continuation]` setting, review after settlement, Luna-mined scenarios.
- `.agents/plans/jev-background-activity-decisions.md` (b9bba65d, 2026-09-20). Inferred idle with background tasks listed, generation-scoped invalidation, retry cadence, default `mj_jev=info` logging, and the rule that inference never makes a worker replaceable.
- `.agents/plans/separate-input-from-work.md` (66849553, 2026-09-21) with `.agents/docs/jev-turn-verdict-v3-evaluation.md`. Independent input and work questions (v3), parent-only silence clock so child traffic does not reset it.
- `.agents/plans/jev-decision-transparency.md` (dc596b44, 2026-09-21). Removed the UI inspector; decisions live in rotating logs; the plain "Classifier: …" notice.
- `.agents/plans/quota-limit-recovery.md` (34adbbb0, 2026-09-21). Quota classification (`/v2/continuation-verdict`), deterministic reset scheduling, durable recovery, database revision 44.
- `.agents/plans/jev-server-retry.md` (87238fc7, 2026-09-23). v4 retryability question, generic transient-provider retry across harnesses replacing Codex-only capacity detection.
- `.agents/plans/jev-turn-wait.md` (e1253507, 2026-09-26). Both `mj wait` forms consume the command-bound completion decision.
- `.agents/plans/unify-jev-turn-assessment.md` (a3d4f61f, 2026-09-27). The v5 three-axis contract, one durable worker assessment for every completion origin, daemon consumption instead of a second classification, protocol 25, database revision 58.

`.agents/plans/shared-transcript-summary.md` is kept: its scope (compaction, reviews, SessionWiki) is wider than Jev. `.agents/docs/jev-proxy.md` is kept as the operations runbook. The raw synthetic runs `.agents/docs/jev-turn-verdict-v3-synthetic*.json` are kept as data.
