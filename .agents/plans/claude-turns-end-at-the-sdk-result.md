# End a Claude prompt turn at the SDK result, not at the adapter's prompt reply

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` at the repository root and must be maintained in accordance with it.

## Purpose / Big Picture

Today a Claude session in mj can show "running" for hours after the model has finished answering the user. Everything that mj does at the end of a turn then never happens: the automatic-continuation check, quota recovery, automatic review, the idle state in the TUI and web UI, and checkpoints that wait for an idle session. On 2026-09-23 a session in `/home/jonathan/Projects/bifrost2` hit the five-hour usage limit at 3:32 AM inside a prompt that had been "running" since 11:33 PM the previous evening; mj never noticed, and the user had to type "continue" at 8:18 AM.

The cause is on the boundary between mj and the Claude adapter. The adapter (`@agentclientprotocol/claude-agent-acp`, the Node program mj launches to talk to Claude Code) answers the `session/prompt` request only when it decides the turn is over, and it deliberately keeps that request open while background subagents the turn started are still alive, and after a steer it waits for a Claude Code "idle" signal that Claude Code 2.1.270 and later sends only once all background work has drained. mj currently treats the `session/prompt` reply as the only thing that ends a prompt turn. So when the adapter holds the reply, mj's turn never ends.

After this change, mj ends a Claude prompt turn when Claude Code reports the result of the model cycle that answered the prompt. That report is the SDK `result` message, which the adapter already forwards to mj on request. The adapter's later `session/prompt` reply is awaited in the background and ignored. Output that Claude produces afterwards (background subagent follow-ups, task notifications, quota errors) is recorded the way mj already records self-started Claude work: as a harness turn that opens on agent output and settles on the adapter's origin marker. The user-visible result: a Claude session becomes idle when the answer is complete, the continuation and quota checks run, and a session that hits its usage limit during a follow-up cycle is recovered at the reset time as designed.

## Progress

- [x] Milestone 1 (2026-09-23): the session asks for SDK `result` messages, and the worker parses them into `mj_core::acp::ClaudeTurnResult` and forwards them as `RuntimeEvent::ClaudeTurnResult` on the ordered runtime-event stream. The type and the event live in `mj-core`, not `mj-worker`, because `RuntimeEvent` is defined there (see Decision Log).
- [x] Milestone 2 and Milestone 3 (2026-09-23): replaced by Milestone A of `.agents/plans/claude-turn-boundary-follow-ups.md`, which does the same job in a different shape: the relay coordinator finds the result that answers the running prompt and asks the prompt loop to end it; the loop checks its own state, reports the completion, and keeps the adapter's reply alive in a detached task. Steering and plan hand-off need no injection bookkeeping (see Decision Log).
- [x] Milestone 4 (2026-09-23): tests, documentation, and the isolated live check, done as Milestone D of the part-two plan (test names and live results are listed there).
- [x] Commit each validated milestone on the current branch (Milestone 1 is committed together with part two's Milestone A).

## Surprises & Discoveries

- Observation: the adapter's prompt hold is unconditional and predates the 0.81.0 update. `deferredSettle` and `turnAwaitingSubagents` exist in the 0.73.0 copy under `~/.cache/mjolnir/harnesses/claude/claude-agent-acp-0.73.0/`. There is no client capability, environment variable, or option that disables it.
  Evidence: `grep -c deferredSettle` on both installed versions returns 23 for 0.73.0; `grep process.env` and `grep clientCapabilities` in 0.81.0 `dist/acp-agent.js` show no switch for the hold.
- Observation: in the incident, all five subagents had failed by 3:41 AM but the adapter still did not release the prompt. Two adapter rules explain it: a follow-up result releases a held turn only when `num_turns > 0` (a quota-rejected reply makes no model call), and a steered turn releases only at the SDK `idle`, which Claude Code 2.1.270+ sends only after all background work drains.
  Evidence: worker journal `~/.local/share/mjolnir/workers/51a8864993190bfa7f45b7711daf1d70/relay-journal/active.jsonl`, ordinals 48822–48883 (rateLimit `rejected`, seven `task-notification` settle markers, no `command_completed` for `prompt-d6779e…`).
- Observation: the adapter never forwards the echo of a steered message. The echo arrives as an SDK `user` message with `isReplay` set; the adapter removes its uuid from `steeredEchoes` and stops (`break`) before any `session/update` is sent.
  Evidence: `~/.cache/mjolnir/harnesses/claude/claude-agent-acp-0.81.0/node_modules/@agentclientprotocol/claude-agent-acp/dist/acp-agent.js`, the `"isReplay" in message` branch near line 4101.
- Observation: Claude Code reports on every result how many user messages are still queued (`queued_turn_count`). A steer's injected message is one of them, so the result of the cycle it interrupts carries a count above zero.
  Evidence: `sdk.d.ts` in `@anthropic-ai/claude-agent-sdk` 0.3.280 (field documentation on `SDKResultSuccess` and `SDKResultError`); the bundled Claude Code 2.1.280 binary sets it from its command queue at each result (`queued_turn_count=…dKr(this.messageQueue)`).
- Observation: a plan review answered with a cancelled outcome (mj's plan hand-off) makes the adapter throw "Tool use aborted" instead of returning a deny with `interrupt: true`, so the planning cycle is not guaranteed to end with an interruption report; it can end with an ordinary success result.
  Evidence: `parseClaudePermissionSelection` in `dist/permissions/effects.js` throws unless the outcome is `selected`; `mj-worker/src/acp/plan_tests.rs` answers the planning prompt with `end_turn`.
- Observation: the adapter fails a prompt at once for an error result (`failActive`), without the hold that `settleOrDefer` applies to successful results.
  Evidence: `failActive` and `failActiveWithSessionFailure` in `dist/acp-agent.js` near line 2343.
- Observation: the Cargo package names are `brokk-mj-worker`, `brokk-mj-core`, and `brokk-mj-transcript`; `cargo test -p mj-worker` matches no package. The commands in Concrete Steps work with those names.
  Evidence: `mj-worker/Cargo.toml` and siblings.

## Decision Log

- Decision: mj ends a Claude prompt turn at the SDK `result` message of the cycle that answered the prompt, and no longer at the adapter's `session/prompt` reply. The reply is still awaited (in a detached task) so the adapter's turn is not cancelled, and it is ignored when it arrives.
  Rationale: the adapter's reply is a policy decision by the adapter ("hold the prompt while subagents run") layered on top of the real turn boundary. The `result` message is the boundary itself: Claude Code emits exactly one per model cycle, it names its origin (`human` for the user's prompt), and it carries the stop reason inputs. mj already models out-of-prompt Claude work as harness turns, so it does not need the adapter's hold.
  Date/Author: 2026-09-23, Fable (design) at the user's direction ("fix the root cause that mj never sees claude turns end").
- Decision: this reverses the alternative rejected in `.agents/plans/turn-completion-across-restart.md` ("Alternatives considered": complete the prompt when the settle marker arrives with `prompt_in_flight`). That rejection was right for the origin `usage_update` marker, which the adapter omits when a cycle produced no assistant usage and which Codex does not emit at all. This plan uses the SDK `result` message, which is emitted for every cycle, and applies to Claude only; Codex keeps ending turns at its reply.
  Rationale: the earlier reasoning targeted a weaker signal. The definition "a turn is one `session/prompt` and everything until its reply" is replaced, for Claude, by "a turn is one `session/prompt` and everything until the `result` of the cycle that answered it".
  Date/Author: 2026-09-23, Fable.
- Decision: the harness-turn mechanism (open on agent output with no active prompt, settle on the origin marker) is left unchanged in this plan.
  Rationale: it works today for self-started cycles and the incident did not involve it. Moving harness-turn settlement onto `result` messages is a possible follow-up, recorded under Outcomes.
  Date/Author: 2026-09-23, Fable.
- Decision: the stop reason recorded for a result-driven completion is derived from the result with the same mapping the adapter uses, so downstream code (`classify_prompt_completion`, continuation eligibility, capacity and quota handling) sees the same values it sees today.
  Rationale: the continuation gate keys on the stop reason. Recording `end_turn` for a refusal or a max-turns error would let the continuation nudge fire on an error.
  Date/Author: 2026-09-23, Fable.
- Decision: `ClaudeTurnResult`, its parser, and the `RuntimeEvent::ClaudeTurnResult` variant live in `mj-core` (`mj-core/src/acp/claude_result.rs` and `mj-core/src/acp.rs`), not in `mj-worker/src/acp/claude_tasks.rs` and `mj-worker/src/acp.rs` as the Plan of Work said.
  Rationale: `RuntimeEvent` is defined in `mj-core/src/acp.rs` and only re-exported by the worker, and the relay also needs the type. A variant cannot be added to it from the worker crate.
  Date/Author: 2026-09-23, Opus (implementation).
- Decision: Milestones 2 and 3 are implemented as Milestone A of `.agents/plans/claude-turn-boundary-follow-ups.md` instead of as written here. The ACP loop does not read results on a side channel; the relay coordinator reads them in order with the session updates and asks the loop to end the prompt. There is no `pending_injection` state and no echo tracking.
  Rationale: the part-two plan explains the ordering reason. Separately, the echo that Milestone 3 relied on never reaches mj: the adapter drops the steered message's echo without forwarding it (see Surprises). A result's own fields say what Milestone 3 needed: `queued_turn_count` above zero means another user cycle follows, and an error-shaped `[ede_diagnostic] … result_type=user` report means a cycle was interrupted.
  Date/Author: 2026-09-23, Opus (implementation), following Fable's part-two revision.

## Outcomes & Retrospective

Completed 2026-09-23, together with `.agents/plans/claude-turn-boundary-follow-ups.md`, which also carried out both candidate follow-ups this plan named (harness turns settle on results; Stop interrupts a self-started turn). In the isolated live check a Claude prompt that started a background subagent went idle about thirteen seconds after it was sent, while the subagent was still running; the adapter answered `session/prompt` about nine seconds later, and the worker discarded that reply. The subagent's follow-up ran as "Agent continued on its own" and settled at its result. Details and remaining gaps are in the part-two plan's Outcomes.

What changed from this plan: the parser and the runtime event live in `mj-core`; the completion is decided in two steps (the relay coordinator finds the answering result on the ordered event stream, and the prompt loop, which alone knows about cancels, plan hand-offs, and newer prompts, reports it); steering needs no injection state because results carry `queued_turn_count` and an interruption diagnostic; and results whose text follows them, and failures, are left to the adapter's reply because the adapter never holds those replies. The echo this plan relied on for Milestone 3 turned out never to reach mj.

## Context and Orientation

mj is a Rust workspace. The daemon (`mj-controller`) owns the database and the UI; one worker process per session (`mj-worker`) runs the agent and owns the durable "relay journal", a log of everything that happened in the session. Shared types live in `mj-core`; the transcript projection that turns journal events into what the UI shows lives in `mj-transcript`.

The worker talks to Claude through the Agent Client Protocol (ACP): JSON-RPC over the adapter's stdin and stdout. mj sends `session/prompt` with the user's text; the adapter streams `session/update` notifications (text chunks, tool calls, `usage_update`) and finally replies to `session/prompt` with a `stopReason`. The adapter also forwards raw Claude Code SDK messages when asked. mj asks for them at session creation in `mj-worker/src/acp/launch.rs` (function that builds the `_meta` for `session/new`; it currently requests only `{"type":"system","subtype":"background_tasks_changed"}` under `claudeCode.emitRawSDKMessages`). Those messages arrive as the `_claude/sdkMessage` notification, typed as `ClaudeSdkMessageNotification` in `mj-worker/src/acp/claude_tasks.rs` and handled in `mj-worker/src/acp/drive.rs` (the `on_receive_notification` block near line 257), which today turns them into `RuntimeEvent::ClaudeBackgroundTasksChanged`.

The ACP loop that owns one prompt is in `mj-worker/src/acp/session.rs`. Near line 616 the `CommandRequest::Prompt` arm builds the request, at lines 672–676 it creates `prompt: ActivePrompt`, a pinned future for the `session/prompt` reply (`connection.send_request(...).block_task()`), sets `prompt_running = true`, and enters a `tokio::select!` loop (line 698). The arm `response = &mut prompt, if prompt_running` (lines 711–842) handles the reply: it settles a pending steer, maps the reply to a stop reason string, emits `RuntimeEvent::PromptFinished { request_id, stop_reason, usage, diagnostic }`, and `break`s out of the loop. Two other arms already finish a prompt before the reply: the input verdict (`AWAITING_INPUT_STOP_REASON`, lines 843–857) and the stall watchdog (`TURN_STALLED_STOP_REASON`, lines 858–935). Both `break`, which drops the `prompt` future.

Dropping that future matters. The ACP crate (`agent-client-protocol` 2.0.0, `src/jsonrpc.rs`) arms a cancellation guard on every sent request: dropping the handle before the reply sends `$/cancel_request`, and the adapter routes that to its `cancel()`, which interrupts the live cycle and settles a held turn as cancelled. `block_task(self)` consumes the handle, so once the future exists the only way to keep the request alive without waiting in the loop is to keep polling the future somewhere else, for example in a spawned task that awaits it and discards the reply.

`RuntimeEvent::PromptFinished` reaches the relay coordinator in `mj-worker/src/worker_runtime/unix/dispatch.rs` (lines 469–484), which removes the request from `in_flight` and calls `record_command_completed` in `mj-worker/src/relay/commands.rs` (lines 1201–1274). That function requires the command to be in flight (`require_in_flight`, lines 1388–1396): a second completion for the same command makes the coordinator fail and the worker exit. It appends `RelayObservation::CommandCompleted` to the journal and then `promote_next_queued_command` (line 1272), which dispatches the next queued prompt immediately. The ACP loop must therefore have left the prompt arm by then, or the queued prompt is rejected with "a prompt is already running" (session.rs lines 1081–1090).

`mj-core/src/relay/snapshot/apply.rs` applies `CommandCompleted` for a prompt (lines 326–417): it clears `active_prompt`, clears any harness turn, moves execution from Running to Idle, and records `continuation.completed_command_id`, which the daemon's continuation and quota-recovery checks require (`mj-controller/src/daemon/continuation.rs`, functions `eligible` and `quota_eligible`).

Steering. When the user types while a Claude prompt is running, mj does not cancel; it "steers" by calling the adapter's `_session/steering` extension (`start_steer` in `mj-worker/src/acp/drive.rs`, around line 1321), which injects the new text into the running turn. The adapter replies `{"outcome":"injected"}`. Claude Code then aborts the cycle it was in (if one was in progress), reports that aborted cycle with an error-shaped `result`, echoes the injected text back as a user message, and runs a new cycle whose `result` is the real answer. If the turn was already held (its cycle had finished), there is no abort result; the echo and the new cycle simply follow. The adapter recognises the abort result by its diagnostic text: `is_error` true and a `result` string starting with `[ede_diagnostic]` containing `result_type=user` (function `isEmptyUserInterruptionDiagnostic` at the top of `dist/acp-agent.js`). In mj the steer is confirmed by `RuntimeEvent::SteerApplied` → `RelayCommandOutcome::Steered` (dispatch.rs lines 559–568); `active_prompt` keeps the original command id and the original `session/prompt` reply still ends the turn today.

Plan approval. When Claude asks to leave plan mode and the user chooses "continue in bypass", mj resolves the permission and sends the implementation instruction as a second `session/prompt` under the same `request_id` (session.rs lines 1166–1191, fed by the `PlanImplementation` channel registered at lines 677–682). Claude Code interrupts the planning cycle first and reports it with an error-shaped result whose diagnostic contains `result_type=user` and `stop_reason=tool_use`. That interruption result must not end the turn either.

The SDK `result` message. Its shape is declared in the bundled SDK, `~/.cache/mjolnir/harnesses/claude/claude-agent-acp-0.81.0/node_modules/@anthropic-ai/claude-agent-sdk/sdk.d.ts` (`SDKResultSuccess` near line 5508 and `SDKResultError` near line 5447). The fields this plan uses:

    type: "result"
    subtype: "success" | "error_during_execution" | "error_max_turns" | "error_max_budget_usd" | "error_max_structured_output_retries"
    is_error: boolean
    stop_reason: string | null          (success only; "refusal", "max_tokens", "end_turn", ...)
    num_turns: number
    result: string                      (success only; the final text or a diagnostic)
    errors: string[]                    (error subtypes)
    origin?: { kind: "human" | "task-notification" | "peer" | "coordinator" | "observer" | "observer-activity" | "channel", ... }
    usage: { input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, ... }
    total_cost_usd: number
    user_message_uuid?: string
    uuid: string

A result "answers the user's prompt" when `origin` is absent (older Claude Code) or `origin.kind == "human"`. Every other kind is an autonomous cycle (a background task woke the model); those never touch the prompt turn. The adapter itself keeps the same set, `AUTONOMOUS_RESULT_ORIGINS`, near line 233 of `dist/acp-agent.js`.

The adapter's stop-reason mapping (dist/acp-agent.js, the `case "result"` handler around lines 3670–3830) is: `stop_reason == "refusal"` → `refusal`; `subtype == "success"` → `max_tokens` when the last assistant stop reason was `max_tokens`, otherwise `end_turn`; `error_during_execution` → `end_turn` unless `is_error`, in which case the adapter fails the turn with a provider error; `error_max_turns`, `error_max_budget_usd`, `error_max_structured_output_retries` → `max_turn_requests`. mj's own strings for these are whatever `mj-worker/src/acp/session.rs` writes today when it formats the ACP reply's `stop_reason` (it uses `format!("{:?}", stop_reason)` on the ACP enum, so check the exact spelling in the existing tests before choosing constants).

## Plan of Work

Milestone 1 makes the result visible inside the worker. In `mj-worker/src/acp/launch.rs`, extend the `emitRawSDKMessages` filter to also request `{"type":"result"}`; keep the background-task entry. In `mj-worker/src/acp/claude_tasks.rs` (or a new sibling module `claude_results.rs` under `mj-worker/src/acp/`), add a parser `claude_turn_result(&serde_json::Value) -> Result<Option<ClaudeTurnResult>, String>` that returns `None` for non-result messages and a typed struct otherwise:

    pub(crate) struct ClaudeTurnResult {
        pub origin_kind: Option<String>,   // None when absent (older CLI); Some("human") etc.
        pub subtype: String,
        pub is_error: bool,
        pub stop_reason: Option<String>,   // SDK stop_reason on success results
        pub num_turns: u64,
        pub diagnostic: Option<String>,    // `result` text on success, first "[ede_diagnostic]" entry of `errors` otherwise
        pub usage: Option<mj_core::usage::TokenUsage>,
        pub cost_usd: Option<f64>,
    }
    impl ClaudeTurnResult {
        pub fn answers_user_prompt(&self) -> bool;     // origin None or "human"
        pub fn is_interruption_report(&self) -> bool;  // diagnostic starts with "[ede_diagnostic]" and contains " result_type=user" (regex on word boundaries as the adapter does)
        pub fn relay_stop_reason(&self) -> String;     // the adapter's mapping, in mj's spelling
    }

In `mj-worker/src/acp/drive.rs`, the `_claude/sdkMessage` handler currently returns early for anything that is not a background-task level. Change it to dispatch on message type: background-task levels behave as today; a result message becomes a new runtime event `RuntimeEvent::ClaudeTurnResult(ClaudeTurnResult)` (add the variant in `mj-worker/src/acp.rs` next to `ClaudeBackgroundTasksChanged`). Malformed results are reported as `RuntimeEvent::Warning` and otherwise ignored, mirroring the existing malformed-level handling. Milestone 1 ends with a unit test on the parser (success, refusal, error subtypes, autonomous origins, absent origin) and an update to `claude_session_metadata_subscribes_to_background_task_levels_for_all_policies` in `mj-worker/src/acp/tests.rs` (line 285) asserting the filter now also lists `{"type":"result"}`.

Milestone 2 ends the prompt on the result. The ACP loop in `session.rs` needs to observe result events for its own session while the prompt arm runs. Find how the loop already receives runtime signals (the `spec` handle and the channels the verdict client and the stall watchdog use) and add a receiver for `ClaudeTurnResult` events on the same path; if the drive handler and the session loop communicate only through the coordinator, add a `watch`/`mpsc` channel in the `SessionSpec` (or the struct that carries `acp_activity` and `step_clock`) that the drive handler feeds and the loop polls as a new `select!` arm, active only while `prompt_running`, for `HarnessKind::Claude`. Do not route the decision through the relay coordinator: the loop must exit its prompt arm before `promote_next_queued_command` runs, and only the loop owns the prompt future.

The new arm applies this rule. Ignore the result if it does not answer the user prompt, if a cancel is in flight (`cancel_deadline.is_some()`; the reply will end the turn promptly because the adapter's cancel settles held turns), or if an injection is pending (Milestone 3). Otherwise the result ends the turn: settle any `pending_steer` exactly as the reply arm does (the 2-second wait and the `CommandInterrupted` fallback), build `RuntimeEvent::PromptFinished { request_id, stop_reason: result.relay_stop_reason(), usage: result.usage, diagnostic }`, emit it, set `prompt_running = false`, hand the `prompt` future to a detached task, and `break`. The detached task is the important part: create it with the same task facility the loop already uses for background work (look for `spawn_local`/`JoinSet`/supervised task helpers in `mj-worker/src/acp/`; the ACP connection is `!Send`, so a `LocalSet`-bound spawn is expected). The task awaits the future, logs the reply at debug level with its stop reason and whether it differed from the recorded one, and drops it. It must never emit `PromptFinished`, `CommandInterrupted`, or any event carrying that `request_id`; the loop is already past that prompt. If the worker shuts down while the task is pending, the task is dropped with the runtime, which then sends `$/cancel_request` on a connection that is closing anyway; that is acceptable.

Because `PromptFinished` is now emitted with the same shape as before, `dispatch.rs`, `commands.rs`, `apply.rs`, and the projection need no change for the ordinary case. Verify one thing in `mj-transcript/src/projection`: the result arrives before the adapter's final `agent_message_chunk` flush? It does not; the adapter forwards text as it streams, and the SDK `result` is emitted after the assistant message, so the transcript has the full answer when the turn closes. Keep an eye on the existing behaviour for output that arrives after a completed prompt: `opens_harness_turn` in `mj-worker/src/relay.rs` (line 932) opens a harness turn on agent output with no active prompt, which is exactly what a background subagent follow-up should become.

Also cover the case where no result comes. The ACP reply remains a valid completion path: if the adapter replies before any answering result (local slash commands with no model cycle, adapter-side errors such as "a prompt is already running", cancellation), the existing reply arm still ends the turn. Both arms guard on `prompt_running`, and whichever fires first sets it false, so a second `PromptFinished` for the same id cannot be emitted. Add an assertion-style comment and a test for this ordering.

Milestone 3 handles injections. Add to the loop a small state `pending_injection: Option<Injection>` set when a steer is confirmed injected (`settle_steer` returns the applied outcome) and when the plan-implementation prompt is sent. While it is set, an answering result that `is_interruption_report()` is ignored and clears nothing; an answering result that is not an interruption report ends the turn and clears the state. Because the adapter forwards the SDK's echo of the injected text as a `user_message_chunk` session update (mj drops those from the relay in `session_update_is_relay_visible`, `mj-core/src/acp.rs:85-122`, but `drive.rs` sees them first), also clear `pending_injection` when such an echo arrives after the injection; after that any answering result ends the turn, including an error result from the steered cycle (for example a usage-limit rejection). Record in the Decision Log which of the two clearing signals fired in the fake-bridge tests, so the next contributor knows both are exercised.

For the plan-implementation path (session.rs lines 1166–1191), the second `session/prompt` replaces the `prompt` future. Treat the interruption result of the planning cycle as an injection interruption (ignored) and let the implementation cycle's result end the turn. If the implementation prompt is sent after that interruption result already arrived, nothing is pending and the first answering result of the implementation cycle ends the turn; both orders must be tested.

Milestone 4 is validation and documentation. Extend the fake ACP bridge in `mj-worker/src/acp/tests.rs` (see `steering_bridge` near line 3122 and `silent_after_prompt_bridge_with_late_reply` near line 2677) with a bridge that streams an answer, emits `_claude/sdkMessage` `{type:"result", origin:{kind:"human"}, subtype:"success", ...}`, and then never replies to `session/prompt` until a later `session/prompt` arrives (the adapter's hand-off). Tests to add, named for behaviour: a Claude prompt ends when its result arrives even though the adapter holds the reply; the held reply is awaited and discarded without cancelling the adapter's turn (assert no `$/cancel_request` reaches the bridge); a prompt queued after the result dispatches at once and the adapter's late reply to the earlier prompt does not complete the new one; a `task-notification` result after the prompt ended opens and settles a harness turn instead of touching the finished prompt; an autonomous result during the prompt does not end it; the steer abort result is ignored and the steered cycle's result ends the turn, in both orders (abort present, abort absent); the planning cycle's interruption result is ignored and the implementation result ends the turn; a cancel in flight lets the reply, not the result, end the turn; an adapter reply arriving before any result still ends the turn; Codex and other harnesses are unaffected (the filter and the arm are Claude-only). Extend `mj-worker/src/worker_runtime/relay_tests.rs` (`a_self_started_turn_holds_a_barrier_but_not_a_prompt`, line 2773, is the model) with a coordinator-level test that a `PromptFinished` produced this way records `CommandCompleted`, sets `continuation.completed_command_id`, moves execution to Idle, and that a subsequent settle marker with `prompt_in_flight: false` behaves as a harness turn. Run the daemon-side continuation tests unchanged; they should now be reachable for this shape.

Update `.agents/docs/claude-autonomous-turns.md`: replace the statement that only the prompt reply ends a turn, describe the `result` subscription, the answering-result rule, the injection exceptions, and the detached reply; fix the stale file paths it cites (`src/hel_worker.rs` is now `mj-worker/src/relay.rs`, `src/hel_worker_runtime/unix.rs` is `mj-worker/src/worker_runtime/unix.rs`). Add a short note to `.agents/plans/turn-completion-across-restart.md` under its Decision Log pointing to this plan as the revision of its turn definition for Claude.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Do not change the build layout or `target/` handling; Rust builds here use mbx caching as configured. After each milestone:

    cargo test -p mj-worker
    cargo test -p mj-core -p mj-transcript
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Before the final commit, run the full `cargo test` once. Commit each milestone on the current branch (`master`) with a plain-language message; do not push.

For the live check, never use the default instance. Start an isolated daemon:

    MJ_CONFIG_DIR=$(mktemp -d)/config MJ_DATA_DIR=$(mktemp -d)/data mj --instance claude-turn-end daemon-run

and create a Claude session in a scratch directory with `mj --instance claude-turn-end`. Send a prompt that asks Claude to start one background subagent (for example "use the Agent tool in the background to count the files here, then tell me you started it and stop") and observe in the TUI that the session goes idle right after Claude's reply while the subagent still runs, that the subagent's follow-up appears as an "Agent continued on its own" harness turn, and that the session is idle again after it settles. Stop the isolated daemon afterwards and delete the temporary directories.

## Validation and Acceptance

Acceptance is behavioural. In the isolated live check above, `mj --instance claude-turn-end sessions` (or the TUI) shows the session idle within a few seconds of Claude's final sentence, not after the background subagent finishes. The worker log for that session shows a `PromptFinished` at that moment and, later, a harness turn opening and settling for the follow-up. In the worker relay journal, `command_completed` for the prompt is recorded before the adapter's `session/prompt` reply arrives (the reply is not journaled at all once the prompt is complete).

In tests, the new fake-bridge tests listed in Milestone 4 fail before Milestone 2 (the prompt stays running until the bridge replies) and pass after. All existing tests keep passing; in particular `classifier_marks_silent_parent_awaiting_input_despite_continuous_native_child_traffic`, the steering tests, and the harness-turn tests.

## Idempotence and Recovery

All steps are additive edits and tests; re-running them is safe. If a milestone's tests fail, the previous milestone's commit is a clean state to return to. The result subscription is requested per session at `session/new`, so an adapter that ignores the extra filter entry simply sends no results, and the reply-driven completion path continues to work exactly as before; this keeps older adapter versions functional. The isolated live instance uses its own config and data directories and must be stopped and removed after the check.

## Artifacts and Notes

Incident evidence for the record (all times UTC; Chicago is UTC−5):

    worker journal, session 51a8864993190bfa7f45b7711daf1d70
    43257 04:33:58Z command_queued prompt-d6779e…  ("we're back. continue, …")
    43510 04:35:53Z settle marker origin=human       (no session/prompt reply follows)
    44060 04:42:50Z command_completed steer → prompt-2fdfaea9… injected
    44259 04:44:00Z settle marker origin=human       (still no reply)
    48822 08:32:37Z usage_update _claude/rateLimit status=rejected resetsAt=1790156400
    48850 08:32:40Z agent text "You've hit your session limit · resets 4:40am" (origin task-notification)
    …six more identical task-notification cycles through 09:02:31Z…
    48887 13:18:15Z command_completed steer → prompt-20fb4b08… (user typed "continue")

`mj-controller/src/daemon/continuation.rs` `eligible` and `quota_eligible` both require `continuation.completed_command_id`, which only `CommandCompleted` for a prompt sets (`mj-core/src/relay/snapshot/apply.rs:346`).

## Interfaces and Dependencies

No new crates. Use `serde_json` for parsing, the existing `RuntimeEvent` enum in `mj-worker/src/acp.rs`, the existing `mj_core::usage::TokenUsage`, and the ACP crate already in use. At the end of Milestone 2 the following must exist:

In `mj-worker/src/acp/claude_tasks.rs` (or `claude_results.rs`):

    pub(crate) struct ClaudeTurnResult { /* fields listed in Plan of Work */ }
    pub(crate) fn claude_turn_result(message: &serde_json::Value) -> Result<Option<ClaudeTurnResult>, String>;

In `mj-worker/src/acp.rs`:

    pub enum RuntimeEvent { /* existing variants */, ClaudeTurnResult(ClaudeTurnResult), }

In `mj-worker/src/acp/session.rs`, inside the prompt loop, a `select!` arm that consumes `ClaudeTurnResult` events for the running prompt and, when the result answers the prompt, emits `RuntimeEvent::PromptFinished` with the derived stop reason, detaches the reply future into a task that discards the reply, and leaves the prompt arm.

The `emitRawSDKMessages` value sent at `session/new` for Claude must equal:

    [{"type":"system","subtype":"background_tasks_changed"},{"type":"result"}]
