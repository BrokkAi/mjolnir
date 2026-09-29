# Unify "quiet" on Jev and test Jev against real session scenarios

This ExecPlan is a living document maintained according to `.agents/PLANS.md`. The design it implements is `.agents/docs/jev.md`; read that first. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current.

## Purpose / Big Picture

Mjolnir asks TypeSafe's Jev classifier what a session is doing after each turn, then acts: it shows the session as idle or waiting, continues authorized work, retries provider failures, resumes after quota resets, and answers `mj wait`. A separate, older rule decides whether a session is "quiet", meaning safe to stop its worker process for an upgrade, a checkpoint, or a move. The two rules disagree in both directions. A `sleep infinity` left by the agent keeps a session "not quiet" forever, so it is never upgraded. A Claude background task that finishes starts a new turn one second later, and in that second the session looks quiet, so on 2026-09-29 an upgrade killed the turn and the user had to type "continue" 18 minutes later.

After this plan, there is one definition of quiet. Observed facts about turns always win; Jev judges only leftover processes; the old rule is the fallback when Jev is silent. Every Jev consumer is tested against a suite of real scenarios taken from Jonathan's session logs, replayed two ways: deterministically (recorded evidence and recorded Jev numbers must produce the expected Mjolnir action) and against the live model (the current questions must produce the expected answers with measured confidence). A person can see it working by running the Rust test suite, by running the scenario replay script and reading its report, and by leaving a `sleep infinity` in a Claude session and watching the worker get upgraded anyway, while a session whose background test run just finished is left alone.

## Progress

- [x] (2026-09-29 16:30Z) Consolidated twelve Jev ExecPlans and four evaluation notes into `.agents/docs/jev.md`; removed the originals; pointed `.agents/docs/jev-proxy.md` at the new document.
- [x] (2026-09-29 16:30Z) Mined the local logs into a 45-scenario catalog (scratch copy at the session scratchpad, `jev-live-scenarios.md`; to be turned into fixtures in Milestone 1) and inventoried intent-versus-test gaps (scratch `jev-test-gap-inventory.md`; findings folded into this plan).
- [x] (2026-09-29 19:40Z) Milestone 1, extraction and deterministic replay: `scripts/jev-scenarios-extract.py` (worker-decision and transcript modes), 78 fixtures in `mj-core/tests/jev-scenarios/` (35 with recorded evidence and verdicts from the catalog, 43 transcript-built should-continue positives, program nudges, and controls), `mj-core/tests/jev_scenarios.rs` passing with S01 and S18 marked `known_failure: ["quiet"]`, `scripts/jev-scenarios-eval.py` with its offline test.
- [x] (2026-09-29 21:30Z) Milestone 1, live replay: 74 fixtures, 222 requests, 0 errors, report at `/mnt/optane/mj-jev-scenarios/results-20260929T1626/report.md`. Findings are in Surprises & Discoveries and drive Milestone 3.
- [ ] Milestone 2: unified quiet with the imminent-turn fact, the `background_needed` question, and all lifecycle consumers switched.
- [ ] Milestone 3: contract fixes found by the corpus (final tool calls in evidence, appended provider messages, question-answer turns inside open goals, approval gates) and threshold review from measured distributions.
- [ ] Milestone 4: close the deterministic gaps in the inventory (action-policy boundaries, protocol-25 continuation consumption, review gating, `Deferred` with continuation off, retry submission behind leftover tasks, dead code, proxy v5 `authorization` test).
- [ ] Milestone 5: documentation, proxy route if the questions change, release notes.

## Surprises & Discoveries

- Observation: automatic continuation has never fired on this host. 56 v5 assessments produced only `uncertain` (48) and `finished` (8); all 203 legacy daemon decisions were uncertain because `no_input_needed` never reached 0.90.
  Evidence: `jq` over `~/.local/share/hel/workers/*/jev-decisions/*.jsonl` and `~/.local/share/mjolnir/jev-decisions/*.jsonl` on 2026-09-29.
- Observation: the Claude adapter already publishes `async_task_state_update` with `state: completed` when a background task settles, and Claude Code always follows a settled background command with an autonomous task-notification turn. The worker parses that update today only to clear the stop control.
  Evidence: `~/.cache/mjolnir/harnesses/claude/claude-agent-acp-0.84.0/node_modules/@agentclientprotocol/claude-agent-acp/dist/async-tasks.js:495-520`, `acp-agent.js:258` (`AUTONOMOUS_RESULT_ORIGINS`), `mj-worker/src/acp/claude_tasks.rs::claude_async_task_control_update`.
- Observation: a child session's authorization history contains only assistant messages, so `ContinuationEvidence::validate` fails and `Continue` is impossible for children by accident rather than by rule.
  Evidence: decision `assessment-b9babd0d…-510`, `authorization.messages` roles `assistant,assistant,assistant`; `mj-core/src/continuation.rs:155` requires `user > 0`.
- Observation: `Verdict::action` returns `Continue`, which `apply_turn_assessment` maps to `Decision::KeepCurrent`; so the documented "continuation re-checks when Jev judges the background work idle" cannot happen under protocol 25.
  Evidence: `mj-worker/src/relay/verdict.rs:221-229`; `mj-worker/src/relay/continuation_tests.rs:391` reaches that path only by hand-writing the action.
- Observation: rebuilding authorization histories under production budgets makes 14 of the 43 transcript fixtures `authorization_complete: false`, so `Continue` is impossible for them by rule before Jev is even asked. `ContextHistory::user` marks the history incomplete for the rest of the context once user text since the last reset passes 32 KiB (one pasted document does it) and never clears the flag, and a final reply over 16 KiB is evicted as `final_reply_omitted`. Sessions 51a88649, 0b4b14fb, and bf8745bb, which hold most of the long-goal positives, are affected.
  Evidence: fixture files N03, N07, N09, P01, P02, P03, P11 (final reply omitted), P12, R03 (final reply omitted), R06, S26, S32; `mj-core/src/assessment.rs::ContextHistory::user`.
- Observation: the checked-in fixtures total about 1.8 MB, most of it whole authorization histories, which is what production sends.
  Evidence: `du` of `mj-core/tests/jev-scenarios/`.
- Observation (first live replay, 2026-09-29, `jev-latest`, current v5 questions, three repeats): no request produced an action the fixture lists as wrong. Actions matched the expectation in 86 of 222 requests. `Continue` fired on 6 of the 16 plain positives (P04, P07, P08 in two of three repeats, P09, P14, P16), all at `input = none` 0.94 to 0.97 and `work` 0.89 to 0.99, and never on a control. The three blockers, in order of how often they decided the outcome:
  1. The `failure` axis answers `none` with low confidence on ordinary replies (S01 0.61 to 0.71, S08 0.57 to 0.71, S10 0.38, S31 0.31 with `quota` 0.37, S41 0.54 to 0.64, P02 0.52 to 0.57, P03 0.82 to 0.86, P05 0.78 to 0.82, P12 0.71 to 0.74). Because `Finished`, `Wait`, and `Continue` all require `failure = none` at 0.90 or more, this alone blocked P02, P03, P05, and P12 and most of the `Wait` cases.
  2. `redundant_request` is never chosen. Every "Want me to proceed?" (R01 to R05) came back `input = required` at 0.35 to 0.79 or `none` at 0.77, so a permission the user already gave reads as a new question.
  3. `work` under-scores `waiting`: parents with live sub-agents (S20, S21) score `authorized_unfinished` 0.72 to 0.78; the incident pair (S01, S18) scores `waiting` 0.50 to 0.72; a Codex agent waiting for suites (A02) scores `authorized_unfinished` 0.42 to 0.47. Question-and-answer turns inside an open goal (S19, S22, S34, S36, S44) score `authorized_unfinished` instead of `finished`, and the handback-without-closing-text child (S14) is still 0.98 `authorized_unfinished`.
  On the positive side, the failure strings are now clean: S23 (quota appended to prose) 0.99, S26 (capacity) 1.00, S27 (session limit) 0.99, and the stale-quota and transport-warning false alarms stayed under 0.90 (0.78 to 0.82). Controls N08 and N09 reach `await_input`; N01 to N07 abstain with `required` at 0.37 to 0.68, so genuine gates are also under-scored.
  Evidence: `/mnt/optane/mj-jev-scenarios/results-20260929T1626/report.md`.

## Decision Log

- Decision: One `quiet` function with three layers (facts, Jev judgment of leftover processes, heuristic fallback), used by every lifecycle consumer.
  Rationale: Jonathan's direction on 2026-09-29: standardize quietness on the Jev classification with the heuristic as fallback instead of two paths; false "not quiet" from `sleep infinity` is the case Jev exists to fix. Observed turn facts stay outside Jev's reach because Jev cannot see them better than the worker can, and the incident shows Jev's evidence lags the facts.
  Date/Author: 2026-09-29, Jonathan and Fable.
- Decision: Add a dedicated `background_needed` question rather than reuse the `work` axis for the quiet judgment.
  Rationale: `work` asks about authorization and remaining work and ends uncertain in 48 of 56 live cases. "Is anyone still depending on this named process?" is narrower. The plan measures its confidence distribution before relying on it; if it is also mostly uncertain, the fallback keeps today's behavior.
  Date/Author: 2026-09-29, Jonathan confirmed "separate".
- Decision: Scenario fixtures are committed with the verbatim live evidence, including whole user and assistant messages, in the public repository. There is no paraphrased tier, no redaction, and no separate local corpus; the deterministic test and the live replay script read the same files.
  Rationale: Jonathan's choice on 2026-09-29 after a scan of all 1,239 records found no credentials or email addresses; he confirmed that the private repository name, the colleague's name, and the local photo path the scan flagged may be public. Whole-message evidence matters because the replied-phase request is exactly this history; paraphrasing would make the live replay test something production never sends. Still scan any newly extracted record for credentials before committing it.
  Date/Author: 2026-09-29, Jonathan and Fable.
- Decision: "Continuation never fires" is a real defect on the `input` axis, to be fixed in Milestone 3 against mined positives and controls, not by lowering the bar blind.
  Rationale: a second mining pass over 1,973 agent-then-user turn boundaries in 1,038 sessions (2026-09-16 onward) found 23 human should-continue positives (16 plain, 7 redundant permission requests) and 4 program-authored ones, plus 10 near-miss controls where a short nudge was really an answer. Positives are about one percent of boundaries, and four of them waited 1.9 to 6.2 hours for a person to notice. The one positive with a Jev record scored `work = authorized_unfinished` 0.99 and `input = none` 0.73. Across all worker verdicts `work` is usually 0.92 to 0.99 while `input = none` is usually 0.37 to 0.82; the legacy daemon's `no_input_needed` never exceeded 0.85. So the `input` question under-scores "no input needed" even when the reply asks nothing. The fix is evidence or wording for the input question, validated on the 27 positives and 10 controls; the 0.90 bar moves only if the corpus then shows zero wrong-at-threshold outcomes.
  Date/Author: 2026-09-29, Jonathan (asked for the mining) and Fable.
- Decision: Keep `.agents/plans/shared-transcript-summary.md`, `.agents/docs/jev-proxy.md`, and the two synthetic JSON runs; remove the other Jev plans and notes.
  Rationale: the transcript summary serves compaction, reviews, and SessionWiki, not just Jev; the proxy note is an operations runbook; the JSON files are raw data the runbook cites.
  Date/Author: 2026-09-29, Fable.

## Outcomes & Retrospective

Not started beyond consolidation. To be written at each milestone.

## Context and Orientation

Mjolnir is a Rust workspace. The daemon (`mj-controller`) is the control plane; each session has a worker process (`mj-worker`) that runs the agent harness (Claude Code, Codex, Kimi, and others) over the Agent Client Protocol (ACP) and keeps a durable relay journal of everything that happened. `mj-core` holds shared types and pure policy; `mj-transcript` holds conversation projections.

Jev is TypeSafe's classifier. The worker sends it bounded evidence about a turn (`mj_core::activity::verdict::TurnEvidence`) with three fixed questions (`mj-core/src/activity/verdict_questions.json`) and gets back three choices with confidences (`mj_core::assessment::Verdict`). `Verdict::action` in `mj-core/src/assessment.rs` turns them into one of `RetryProvider`, `RecoverQuota`, `Continue`, `AwaitInput`, `Finished`, `Wait`, `Uncertain`. The worker stores the result in a durable `TurnAssessment` (`mj-worker/src/relay/verdict.rs::apply_turn_assessment`) and derives a process-local activity inference (`inferred_idle_since_ms`, `expected_continuation`) that `mj_core::activity::classify` folds into `ActivityState`. Requests go through `mj-worker/src/acp/verdict_client.rs` to the public proxy in `services/jev-proxy/` or directly to TypeSafe when a key exists. Every decision is logged to `jev-decisions/decisions.*.jsonl` under the worker root (`mj-core/src/jev.rs`).

"Quiet" today is `mj_core::activity::is_quiet` (`mj-core/src/activity.rs:505`): no checkpoint barrier and not `has_work_in_flight`, which counts an open turn, tools in flight, queued commands, background commands, user shells, agent terminals, a pending goal decision, and unknown provider state, and ignores Jev on purpose. `safe_to_replace` adds harness-specific conditions. Consumers: worker upgrade (`mj-controller/src/worker_upgrade.rs::PolicyState::due`, then `IdleWorkspaceLease::acquire_for_upgrade` in `mj-controller/src/controller/checkpoint/workspace_lease.rs`), checkpoint admission (`mj-controller/src/controller/checkpoint/barrier.rs`, `latched.rs`), session move (`mj-controller/src/controller/move_session.rs`, `mj-controller/src/daemon/session_move.rs`), sub-agent parking (`mj-controller/src/daemon/subagent_park.rs`), the API file-write lease, and provider-retry submission (`mj-worker/src/relay/commands.rs:1371`).

The Claude worker learns about background tasks from two adapter signals in `mj-worker/src/acp/claude_tasks.rs`: the `background_tasks_changed` level (the full current task list, replace semantics, used to build `claude_background_tasks` in `mj-worker/src/relay/background.rs`) and the `async_task_*` edge updates (`async_task_spawned`, `async_task_progress`, `async_task_state_update`), used only to track whether a task can be stopped.

Terms: a "turn" is one prompt-to-reply cycle; a "harness turn" is one the harness started on its own (for example after a task notification); "background command" is a process the agent left running with nothing waiting on it; "phase" is `running` (turn open) or `replied` (turn ended); "protocol 25" is the current worker-daemon relay protocol in which the worker owns assessment.

## Plan of Work

### Milestone 1: scenario corpus and two replay modes

What exists at the end: a fixture directory `mj-core/tests/jev-scenarios/` with one JSON file per scenario, a Rust integration test that replays every fixture deterministically, a Python script that replays the local verbatim corpus against the live model and writes a report, and an extractor script that turns a session id plus decision id into a fixture pair (raw to the local corpus, redacted to the repository).

Fixture shape (`mj-core/tests/jev-scenarios/S01-nextest-notification-killed.json`):

    {
      "id": "S01",
      "title": "Background nextest finished; notification turn killed by worker upgrade",
      "category": "quiet-kill-safety",
      "harness": "claude",
      "source": {"session": "6d26c6d3c2659ce212d82e60a7d89a37", "decisions": ["assessment-…-284"], "captured_at": "2026-09-29T14:54:18Z"},
      "facts": {"phase": "replied", "stop_reason": "EndTurn", "silent_for_s": 313,
                "tools_in_flight": [], "background_commands": 0, "queued_commands": 0,
                "goal_active": false, "task_settled_s_ago": 0, "harness_turn_open": false},
      "text": {"user_prompt_tail": "asks whether CI is red and to fix it",
               "assistant_text_tail": "says it is waiting for the Linux run of the scheduled tests and will continue when it reports"},
      "recorded_verdict": {"failure": ["none", 0.60], "input": ["none", 0.99], "work": ["waiting", 0.71]},
      "expected": {"failure": "none", "input": "none", "work": "waiting", "quiet": false, "action": "wait"},
      "outcome": {"mjolnir": "replaced the worker at 14:54:22Z; notification turn interrupted",
                  "judgment": "wrong", "next": "user typed continue 18 minutes later"}
    }

`facts` maps onto `mj_core::activity::ActivityFacts` plus the new fields Milestone 2 adds (`task_settled_s_ago`, `harness_turn_open`); unknown fields are an error so the fixture and the type cannot drift. `expected.action` is the acceptable Mjolnir action given a correct verdict; `expected.quiet` is the correct answer for the new quiet function. In place of the `text` object shown above, the committed fixture carries the full recorded `TurnEvidence` under `evidence`, verbatim; the deterministic test reads `evidence` for its facts and the live replay posts it unchanged.

Deterministic replay: `mj-core/tests/jev_scenarios.rs` reads every fixture with `std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/jev-scenarios"))`, builds `ActivityFacts` from `facts`, builds a `Verdict` from `recorded_verdict`, and asserts two things: `Verdict::action` on the recorded numbers never produces an action the fixture marks as wrong (for example `continue` when `expected.work` is `finished`), and the quiet function on the facts returns `expected.quiet` (until Milestone 2 lands, this half asserts today's `is_quiet` and is allowed to fail for fixtures tagged `known_failure: "quiet"`, so the test documents the incident before the fix and proves the fix after). Worker-side fact production (a settled task becomes `task_settled_s_ago`) is covered in `mj-worker/src/relay/tests.rs`, not here.

Live replay: `scripts/jev-scenarios-eval.py` reads the fixture directory, posts each `evidence` with the current bundled questions to TypeSafe directly (key from `TYPESAFE_API_KEY` or `~/.secrets/typesafe_api_key`, never printed), repeats each request three times, and writes `results.jsonl` plus a Markdown report with, per scenario, the expected axes, the three observed choices and confidences, the resulting `Verdict::action` (re-implemented in Python from `assessment.rs` and pinned by a Python test), and per category: agreement rate, share above threshold, and wrong-at-high-confidence count. It never runs in CI. Compare its numbers to `recorded_verdict` to see drift between the questions at capture time and now.

Extractor: `scripts/jev-scenarios-extract.py --session ID --decision ID` reads the worker decision log for the final record of that decision id and the controller database read-only (`sqlite3 -readonly`) for the transcript around the completion time, scans every string in the evidence for credential-shaped text (bearer tokens, `ghp_`, `sk-`, AWS key ids, `password=`) and refuses to write on a match, and writes the fixture with `expected` and `outcome` left as `null` for the author to fill in.

Seed content: the 45 catalog scenarios; the 23 human and 4 program-authored should-continue positives and the 10 near-miss controls from the second mining pass (report kept at `/mnt/optane/mj-jev-scenarios/should-continue-positives-20260929.md` until the fixtures exist; most predate Jev, so their fixtures carry the transcript evidence and `recorded_verdict: null`, and only the live replay scores them). Hand-built fixtures cover the categories the logs lack (transient provider refusal, `redundant_request`, a quota message that should trigger `RecoverQuota`), using the real strings the logs do contain: "Selected model is at capacity. Please try a different model.", "You've hit your session limit · resets 1:30am (UTC)", "You've hit your usage limit… try again at Oct 3rd, 2026 3:55 PM".

### Milestone 2: unified quiet

What exists at the end: `mj_core::activity::quiet(facts, judgment) -> Quiet` where `Quiet` is `Yes`, `No(reason)`, and every lifecycle consumer calls it; the incident fixture S01 and its survived twin S18 both expect `quiet: false` and pass; a `sleep infinity` fixture expects `quiet: true` once Jev says nobody needs it and passes.

Layer 1, facts Jev may not override. Extend `ActivityFacts` with `task_settled_at_ms: Option<i64>` (most recent settle of a background task with no harness turn opened since) and treat it, within `IMMINENT_TURN_WINDOW` (60 s to start; measure the real gap from the corpus, which shows 1 s and 3 s), as an open turn for quiet purposes. The worker sets it in `mj-worker/src/relay/background.rs` when `claude_async_task_control_update` reports `completed`, `failed`, or `stopped` for a Claude task, and when a Claude task leaves the `background_tasks_changed` level; clears it when a harness turn opens (`opens_harness_turn` in `mj-worker/src/relay.rs`) or a prompt starts. For Codex exec cards and Kimi tasks, record the same fact on their settle signals if they have one; document in the code which harnesses cannot provide it. The other layer-1 facts are the existing ones: prompt or harness turn open, tools in flight (status `pending` counts, per scenario S42), queued commands, pending goal decision, `goal_active` or `goal_running`, `acp_ready == Some(false)`, checkpoint barrier, `capacity_retry_armed`.

Layer 2, Jev on leftover processes. Add a fourth question `background_needed` to `verdict_questions.json` (choice: `needed`, `unneeded`, `unclear`) asked only when layer 1 is clear and `background_commands + active_agent_terminals + active_user_shells > 0`. Its evidence is the existing `TurnEvidence` plus a new `background` list: for each background command, `{id, command (128 B), started_s_ago}` (the same data the stop control shows). The instruction text says: judge, from the assistant's own words and the command text, whether the agent or the user is still depending on the result of any listed process; a `sleep`, a watcher loop, or a server left up for later manual use is unneeded; a build, test run, or sub-agent whose result the assistant said it would relay is needed. Store the answer in `TurnAssessment` beside the other three (`Verdict` gains `background: Option<Judgment<Background>>`; older answers deserialize as `None`). The worker publishes `background_needed: Option<bool>` in `ActivityFacts` when the judgment is at or above `ACT_CONFIDENCE`, clears it on the same invalidations as inferred idle, and never restores it after restart.

Layer 3, fallback. `quiet` returns `No("background work")` when processes remain and `background_needed` is `None` or `Some(true)`, which is exactly today's `has_work_in_flight`. When `Some(false)`, the processes do not count, and the session is quiet if nothing else blocks. `safe_to_replace` keeps its Codex and Kimi conditions on top.

Consumers. Replace direct uses of `is_quiet` and `has_work_in_flight` in worker upgrade (`worker_upgrade.rs`, `daemon/snapshot.rs`, `workspace_lease.rs`, `session_manager/actor.rs`), checkpoint (`barrier.rs`, `latched.rs`, `routine_checkpoint_wait`), move (`move_session.rs`, `daemon/session_move.rs`), park (`subagent_park.rs`), and retry submission (`relay/commands.rs:1371`) with `quiet`. Keep `driver_present` for continuation admission and the activity display, but make it read the same layer-1 facts so a settled task also counts as a driver. Log the reason string at `debug` in the upgrade coordinator when a session is skipped, so the daemon log finally records the quiet judgment (today it records only the replacement).

The imminent-turn window is the one new heuristic. It is bounded, fact-based, and only ever makes a session less quiet; Jev is never asked to override it.

### Milestone 3: contract fixes the corpus exposed

Each item is a fixture (or several) that fails on the live replay today and a change that makes it pass without regressing the rest.

- Final tool calls after the last assistant text (S14, S15, S16). `TurnContext` keeps the last assistant text; a child that ends with `handback` or a parent that ends with `spawn` after its text is invisible to Jev. Add to `TurnEvidence` a `final_tool_calls` list: names and outcomes (not bodies) of tool calls after the last assistant text, from the same summary projection that feeds `transcript_summary`. Keep 0+1's rule of no tool bodies.
- Provider messages appended to normal text (S23). The `failure` instruction already says to use the current reply and diagnostic. Add an example of a limit message following ordinary progress text, and record in the evidence, when the harness gives it, the provider diagnostic separately from the reply text so Jev does not have to find it inside prose.
- Question-and-answer turns inside an open goal (S22, S36, S37). The `work` instruction says later status questions do not cancel earlier tasks, which is right for continuation and wrong for "what is this turn's state". Split the intent: `work` stays about authorized work overall; the action policy must not infer `Finished` or `Continue` from a turn whose user message was a question (a new evidence flag `latest_user_is_question`, computed deterministically from the delivered prompt; or ask Jev a Noul `answered_question`). Measure both on the corpus and pick the one with fewer wrong-at-high-confidence results.
- Approval gates and review requests (S09, S35, S39). Add criteria text for `input = required`: a presented plan or option list awaiting the user's choice, a request that the user look at artifacts, and a goal marked blocked. Add Codex `plan_proposal` presence as an evidence fact.
- The `input` axis under-scores "no input needed" (P01 to P16, R01 to R07, controls N01 to N10). The positives share three shapes: the agent states what remains open and ends without asking; the agent announces its next step in the first person and ends; the agent asks permission for work already assigned ("Want me to proceed?", "Say the word"). Change the `input` instruction and criteria so a reply that asks nothing, or asks only for permission the user already gave, scores `none` or `redundant_request` with confidence; keep the controls (a real "which option?", "I will not start without your word", a goal marked blocked) scoring `required`. Consider adding a deterministic evidence flag `reply_asks_question` (the final reply ends with a question or an offer) so Jev is not left to detect it. Acceptance is on the live replay: at least 20 of the 27 positives reach `Continue` at 0.90 with zero controls doing so; report the exact numbers in the Decision Log.
- Ended-to-wait turns for harnesses without notification turns. 28 status pings and several judge notes show agents ending a turn to wait for a background command that never wakes them. Under the current worker, `Continue` maps to `KeepCurrent`, so the documented re-check when the command finishes does not happen (Milestone 4 fixes the mapping). Add fixtures A01 to A04 so the pair "settled background command plus authorized unfinished work" yields a continuation once the mapping is fixed.
- Threshold review. From the live replay report, plot confidence distributions per axis for correct and wrong answers. Do not lower any threshold unless the corpus shows zero wrong-at-threshold outcomes; record the numbers in the Decision Log either way.

### Milestone 4: deterministic gaps from the inventory

Add unit tests at `Verdict::action` for the `Finished` and `Wait` outputs, the 0.85 and 0.90 boundaries on each axis, the `failure = none` below 0.90 rule, `redundant_request` with `finished`, and the running-phase mapping of v5 choices to `needs_user_input`. Add tests through `apply_turn_assessment` for every reason string. Add a daemon test with a protocol-25 worker whose assessment carries `Continue`, asserting the daemon submits the guarded prompt, and a companion test that with `[continuation] enabled = false`, or for a child session, the worker does not mark the assessment `Deferred` (change the worker to consult the setting it receives at launch, and to treat children by rule). Verify or fix review gating for protocol 25 (`daemon/continuation.rs::publish`): review must start after the continuation chain settles. Make provider-retry submission use `quiet` from Milestone 2 so leftover tasks judged unneeded do not block it. Remove `RuntimeEvent::ContinuationExpected` and `TurnVerdict::should_retry_server_error`. Add a proxy test that sends a v5 request with an `authorization` object and one that sends the Milestone 2 `background` list. Fix `docs/src/content/docs/sessions.md` where it says uncertain answers retry after a minute. Replace the source-text check in `mj-worker/src/acp/tests.rs:6425` with a behavior test that a container launch carries the key.

### Milestone 5: publication

If Milestone 2 or 3 changes the question file, add `/v6/turn-verdict` to the proxy, keep v5 frozen as `verdict_questions_v5.json`, deploy before releasing workers, and record the version in `.agents/docs/jev-proxy.md` (also record the v5 deployment that the runbook is missing). Update `sessions.md` for the new quiet behavior: a session with only unneeded background processes can be upgraded, checkpointed, or moved, and the processes are stopped with the worker. Release notes.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir4` (a git worktree on branch `hel4`). Do not redirect Cargo targets or change mbx caching. Never point a test binary at the live default instance; runtime checks use `--instance jev-quiet` with fresh `MJ_CONFIG_DIR` and `MJ_DATA_DIR`.

Milestone 1:

    python3 scripts/jev-scenarios-extract.py --session 6d26c6d3c2659ce212d82e60a7d89a37 --decision assessment-6d26c6d3c2659ce212d82e60a7d89a37-284
    # edit mj-core/tests/jev-scenarios/S01-*.json: replace "[paraphrase]" markers
    cargo test -p brokk-mj-core --test jev_scenarios
    python3 scripts/jev-scenarios-eval.py --output /mnt/optane/mj-jev-scenarios/results-$(date -u +%Y%m%dT%H%M)
    python3 -m pytest scripts/jev-scenarios-eval.test.py   # or the unittest runner, offline only

Expected: the integration test reports one `known_failure` (S01 quiet) until Milestone 2; the eval report lists 45+ scenarios with agreement and threshold counts per category.

Milestone 2:

    cargo test -p brokk-mj-core activity::
    cargo test -p brokk-mj-worker relay::tests::settled_claude_task_
    cargo test -p brokk-mj-controller worker_upgrade
    cargo test -p brokk-mj-core --test jev_scenarios    # S01 and S18 now pass without known_failure

Manual check with an isolated instance: start a Claude session, have it run `sleep infinity &` via a background Bash task and finish its reply; confirm the worker is upgraded on the next daemon start (daemon log: `replaced the session worker`). Then have it start a 60-second background command and reply "waiting"; restart the daemon; confirm the log shows the session skipped with a reason and the notification turn completes.

All milestones: `cargo fmt --all -- --check`, `cargo test` outside the sandbox on the dev profile, `cargo clippy --all-targets -- -D warnings`, `git diff --check`; in `services/jev-proxy`, `npm test`, `npm run check`, `npm run deploy:dry-run`. Commit each validated milestone on the current branch; do not push unless asked.

## Validation and Acceptance

Milestone 1 is accepted when every catalog scenario has a fixture, the deterministic test passes with S01 the only known failure, and one live replay report exists in the local corpus with per-category numbers.

Milestone 2 is accepted when: the S01 and S18 fixtures expect and get `quiet: false`; a fixture with `background_commands: 1`, command `sleep infinity`, and a confident `unneeded` judgment gets `quiet: true`; a fixture with the same process and Jev unavailable gets `quiet: false`; any fixture with a tool in flight, an open turn, a queued command, or an active goal gets `quiet: false` regardless of any Jev answer; and the worker test proves `async_task_state_update: completed` sets `task_settled_at_ms` and a subsequent harness turn clears it. The manual check above behaves as described.

Milestone 3 is accepted when each named scenario passes on the live replay at or above threshold in three of three repeats and the report shows no new wrong-at-high-confidence answers in other categories. Milestone 4 is accepted when each listed test exists and passes. Milestone 5 when the proxy smoke check returns typed answers on the new route and the runbook records the version.

## Idempotence and Recovery

Fixtures and scripts are additive. The extractor overwrites a fixture only with `--force`. The live replay script writes to a new output directory per run and never modifies fixtures. Jev question changes are additive on the proxy (new route) and frozen for old workers. The quiet change is behind one function; reverting Milestone 2 restores `is_quiet` call sites. No database migration is planned; if `Verdict` gains a field that older readers cannot ignore, raise the relay protocol and snapshot revision and add the isolated upgrade regression, as previous Jev changes did.

## Artifacts and Notes

The catalog and the gap inventory that seeded this plan were written to the session scratchpad on 2026-09-29 (`jev-live-scenarios.md`, `jev-test-gap-inventory.md`). Their durable content is the fixture set (Milestone 1) and the gap list above; copy anything else needed into this plan rather than referring to the scratchpad.

Incident evidence for S01: worker decision log records `-279` and `-284`; native transcript `profile/projects/hel-7d56b7a696721264-6d26c6d3c2659ce212d82e60a7d89a37/429954dc-….jsonl` (queue-operation enqueue 14:54:17.850Z, interrupt 14:54:18.935Z); daemon log `mj-daemon-20260929T142934.860Z-827782.log:699`.

## Interfaces and Dependencies

In `mj-core/src/activity.rs`:

    pub struct ActivityFacts {
        // existing fields, plus:
        pub task_settled_at_ms: Option<i64>,
        pub background_needed: Option<bool>,
    }
    pub const IMMINENT_TURN_WINDOW_MS: i64 = 60_000;
    pub enum Quiet { Yes, No(&'static str) }
    pub fn quiet(facts: &ActivityFacts, now_ms: i64) -> Quiet;
    pub fn is_quiet(facts: &ActivityFacts) -> bool;   // becomes `quiet(..).is_yes()`, kept for callers

In `mj-core/src/assessment.rs`:

    pub enum Background { Needed, Unneeded, Unclear }
    pub struct Verdict {
        pub failure: Judgment<Failure>,
        pub input: Judgment<Input>,
        pub work: Judgment<Work>,
        #[serde(default)] pub background: Option<Judgment<Background>>,
    }

In `mj-core/src/activity/verdict.rs`:

    pub struct BackgroundEvidence { pub id: String, pub command: String, pub started_s_ago: u64 }
    pub struct TurnEvidence {
        // existing fields, plus:
        #[serde(default)] pub background: Vec<BackgroundEvidence>,
        #[serde(default)] pub final_tool_calls: Vec<ToolOutcome>,   // Milestone 3
    }

In `mj-worker/src/relay/background.rs`: `fn note_task_settled(&mut self, task_id: &str, now_ms: i64)` called from the `async_task_state_update` handler; cleared in `opens_harness_turn` and prompt start.

Scripts use only the Python standard library, as the earlier evaluation scripts did. No new Rust dependency.

Revision note (2026-09-29): created from the consolidated design after mining the live logs and inventorying test gaps. Milestone contents beyond 1 and 2 are proposals awaiting Jonathan's review.
