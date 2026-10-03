# Record Codex turn failures as failures, and start sub-agents on their model

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

GitHub issue 1217: a freshly spawned sub-agent (a child session that a parent session starts through the `mj-agents` MCP tools and that reports back with a `handback` tool call) sometimes handed back "Not completed. The reminder arrived before I had inspected the lane" a few seconds after it was spawned, without doing any work.

The cause is a chain of three facts, all confirmed from the child's relay journal (the worker's append-only event log, `relay-journal/` under the worker root on the target) and the Codex logs in the child's harness home. First, the child's first model request failed: OpenAI answered HTTP 400 `model 'gpt-6-luna' is not enabled in rustponsesapi`. Second, the Codex ACP bridge (`@brokkai/codex-acp`, the adapter process that speaks the Agent Client Protocol to Mjolnir and drives the Codex app-server) reported that failed turn as an ordinary end of turn, `stopReason: end_turn`, with the error JSON as agent text, because Mjolnir did not ask for typed failures. Third, Mjolnir therefore treated the turn as finished without a report and sent the handback reminder, whose old text told the child to hand back "now ... then stop", which it did.

After this work: a Codex turn that fails is recorded as a failed turn (stop reason `error`, or `QuotaLimit` for an exhausted plan), with the provider's sentence as a one-line warning in the transcript, so no reminder is sent and a parent's `wait` reports the child as failed with the reason. Codex warnings and retry notices that the bridge now sends as typed records appear as one transcript line each. A child spawned with a model starts its Codex thread (and its Claude session) on that model, instead of starting on the profile default and switching before the first turn, which is the condition under which the 400 was seen.

The earlier part of the fix is already committed (commit e97c0266): the reminder now tells an unfinished child to keep working, and the transcript shows Mjolnir's own prompts (the reminder, quota resumes) as user turns.

## Progress

- [x] (2026-10-03 18:30Z) Diagnose issue 1217 from the journals and Codex logs on precision-3260.
- [x] (2026-10-03 19:20Z) Milestone 1: reminder text and transcript visibility; committed e97c0266.
- [x] (2026-10-03 20:40Z) Milestone 2: typed Codex failures become failed turns; typed records render in the transcript; credential sync reads an `access` record outside a turn. Committed 03e1b5ac.
- [x] (2026-10-03 21:20Z) Milestone 3: a sub-agent's harness (and a turn reviewer's) starts on its chosen model through `LaunchSpec::startup_model`.
- [x] (2026-10-03 21:50Z) Full validation: clippy clean; cargo nextest 5901 passed; live check in an isolated instance (see Artifacts). Milestone 3 committed.

## Surprises & Discoveries

- Observation: the failed first request went over a Responses websocket that Codex had opened and prewarmed at thread start with the profile default model, and the request then asked for a different model.
  Evidence: in the child's Codex log database (`logs_2.sqlite` in the harness home), the first `/responses` request carries `model=gpt-6-astra auth_connection_reused="true"` and a `last_model_response_id`, while the turn ran `gpt-6-luna`. The next turn opened a new connection and succeeded. Both failing children show this; one succeeding child with a complete log shows the same mismatch, so the mismatch is the setup for the failure, not a guarantee of it. The failure itself is on OpenAI's side and was seen in 2 of about 50 `gpt-6-luna` children on that host.
- Observation: when a client opts in to typed failures, the bridge no longer sends Codex warnings, config warnings or retry notices as agent text; it sends them as `session_info_update` records under `_meta.jetbrains.air.sessionFailure`, which Mjolnir ignored.
  Evidence: `createWarningEvent` and `createConfigWarningEvent` in `codex-acp/src/CodexEventHandler.ts`.
- Observation: the typed record drops Codex's precise error kind (`codexErrorInfo`). Mjolnir's quota handling needs `usageLimitExceeded` and credential sync needs `unauthorized`.
  Evidence: `TurnDiagnostic::is_usage_limit` and `AUTH_FAILURE_ERROR_KINDS` in `mj-core`.

## Decision Log

- Decision: opt in to the bridge's typed failures rather than detect Codex's `threadStatus: systemError` notification or change the bridge's legacy path.
  Rationale: the typed record is the interface the bridge provides for exactly this; `systemError` is a side signal about the thread, and changing the bridge's legacy behaviour would need a bridge release. The costs of opting in (the lost error kind, warnings that no longer arrive as text) are handled in Milestone 2.
  Date/Author: 2026-10-03, Claude.
- Decision: recover the two error kinds Mjolnir depends on from the typed record's category and actions: `limit` with no actions is `usageLimitExceeded`, `access` is `unauthorized`. Every other failure keeps its category as the diagnostic code.
  Rationale: in the bridge's published policy table (`SESSION_FAILURE_POLICY`), only `usageLimitExceeded` maps to `limit` with no actions (rate limits carry `retry`, context-window limits carry `new_session`), and only authentication failures map to `access`. This is an exact inverse of the bridge's table, not a guess from text.
  Date/Author: 2026-10-03, Claude.
- Decision: do not add an automatic retry for this failure.
  Rationale: it is an HTTP 400 `invalid_request_error`. Codex does not retry it, and Mjolnir's turn assessor (Jev, which owns retryability: `diagnostic.rs` says "retryability is Jev's decision") is instructed that invalid input is not transient. A second retry rule would be a second owner of the same decision. Recording the turn as failed already lets the existing assessor arm its server retry when it judges a failure transient. The root-cause change is Milestone 3.
  Date/Author: 2026-10-03, Claude.
- Decision: carry the spawn model to the worker as `WorkerLaunchConfig::initial_model`, used only while the session has no accepted model. One function, `LaunchSpec::startup_model`, answers "which model does the harness start on" for both Codex (the `CODEX_CONFIG` pin) and Claude (the `session/new` options).
  Rationale: the accepted model (what the harness has confirmed) stays the authority as soon as it exists, so a later user change is never overwritten by the launch value. Seeding the relay's accepted configuration with a value the harness had not accepted would blur that fact.
  Date/Author: 2026-10-03, Claude.

## Outcomes & Retrospective

Milestones 1 to 3 are implemented. A failed Codex turn is now a failed turn with the provider's reason, so a sub-agent whose first request fails is reported as failed instead of being told to hand back; a sub-agent's Codex thread and Claude session open on the spawn model, and a turn reviewer's on its configured model. Ordinary sessions created with a model (`mj new --model`, the web form) still open on the profile default and switch before the first turn, because the session record does not keep the creation model; carrying it would need a stored field and a migration, and is left for a separate change if the 400 is seen there. Whether starting on the spawn model prevents the 400 is likely but not proven: the failure is intermittent on OpenAI's side, and the evidence is the two failing children's logs.

## Context and Orientation

The worker (`mj-worker`, one process per session on the session's target machine) starts the harness bridge and speaks ACP to it. `mj-worker/src/acp/session.rs` builds the `initialize` request (the client capabilities, including `_meta.jetbrains.air.capabilities`) and sends each `session/prompt`; the branch that handles the prompt's response turns it into `RuntimeEvent::PromptFinished { stop_reason, usage, diagnostic }`, which the relay records as `command_completed` with `RelayCommandOutcome::Prompt`. A stop reason other than `EndTurn`, `AwaitingInput`, `Cancelled` or `QuotaLimit` classifies as an error everywhere (`mj_core::state::classify_prompt_completion`), and every consumer (sub-agent reports, `mj wait`, continuation, assessment) already treats it as a failed turn. `mj_core::diagnostic::TurnDiagnostic { message, code, http_status, reset_at }` carries the provider's reason. A JSON-RPC error from the bridge is already recorded this way by `prompt_error_outcome` in `mj-worker/src/acp/drive.rs`; typed failures reuse that shape, so no journal format or protocol changes are needed.

The transcript (what the TUI and web viewer show) is a projection of relay events, computed in `mj-transcript/src/projection/`. `session_update.rs` there handles each `session/update` notification; `SessionInfoUpdate` currently only feeds the goal and the title.

The bridge's typed failure record, when the client lists `sessionFailure` among its AIR capabilities, is `{id, revision, category, severity, title, details?, actions}`. On a failed turn it arrives on the prompt response as `_meta.jetbrains.air.sessionFailure` with `stopReason: end_turn`, and the error text is not sent as agent text. Warnings and retries arrive as `session_info_update` notifications with the same `_meta` key; records with the same `id` are revisions of one notice.

The worker's launch configuration is `mj_core::worker_launch::WorkerLaunchConfig`, written as `launch.json` by the controller in `mj-controller/src/controller/worker_binary/launch.rs` (`session_launch_config`). The worker keeps the selectors the harness has accepted in `LaunchSpec::accepted_config` (`mj-worker/src/acp/launch.rs`). Codex can only take its model before the thread starts, through the `CODEX_CONFIG` environment variable that `pin_accepted_bridge_selectors` in `mj-worker/src/worker_runtime.rs` writes into the bridge's supervisor spec before every bridge start; Claude takes it in the `session/new` options built in `launch.rs`.

## Plan of Work

Milestone 2. In `mj-core/src/diagnostic.rs`, add `SessionFailure` (the parsed record) with `SessionFailure::from_meta(meta)` reading `jetbrains.air.sessionFailure`, and `TurnDiagnostic::from_session_failure(&SessionFailure)` applying the code mapping from the Decision Log. When the title is a JSON provider error body (`{"error":{"message":...},"status":400}`), the diagnostic takes the inner message and the status. In `mj-worker/src/acp/session.rs`, add `sessionFailure` to the Codex AIR capabilities, and in the prompt-response branch check for a typed failure of severity `error` before the "returned without updates" check; when present, record the stop reason and warning through a function in `drive.rs` that shares the usage-limit rule with `prompt_error_outcome`. In `mj-transcript/src/projection/session_update.rs`, render a `SessionInfoUpdate` that carries a record as one system line keyed by the record's id, so later revisions replace it.

Milestone 3. Add `initial_model: Option<String>` to `WorkerLaunchConfig` (serde default, skipped when absent). Set it in `session_launch_config` from the sub-agent record's model; the reviewer's launch spec in `mj-worker/src/worker_runtime/reviewer.rs` takes its configured model the same way. Add `initial_model` to `LaunchSpec`, set from the launch config in `mj-worker/src/worker_runtime/unix.rs`, and add `LaunchSpec::startup_model()`. Make `repin_bridge_selectors`/`pin_accepted_bridge_selectors` and the Claude options take the startup model.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir4`:

    cargo nextest run -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-worker
    cargo clippy --all-targets -- -D warnings
    cargo nextest run

## Validation and Acceptance

Unit tests: a prompt response whose `_meta` carries an error record finishes the turn with stop reason `error`, a diagnostic holding the provider's message and status, and one warning line; a `limit` record with no actions finishes with `QuotaLimit`; an `access` record yields a diagnostic that credential sync reads as an authentication failure; a `session_info_update` record shows as one transcript line that a later revision replaces. A sub-agent's launch config carries its model, and the Codex pin and Claude options use it until an accepted model exists. The full suite and clippy pass.

## Idempotence and Recovery

All changes are additive and covered by tests; no stored data changes. Reverting a milestone's commit restores the old behaviour.

## Artifacts and Notes

Live check in an isolated instance (`--instance typed-fail`, real codex-acp and Codex, a profile whose `CODEX_CONFIG` names a model that does not exist), `mj wait --json` after the first turn:

    "outcome": "error", "stop_reason": "error",
    "diagnostic": {"message": "The 'gpt-0-nonexistent' model is not supported when using Codex with a ChatGPT account.", "code": "service", "http_status": 400}

and the transcript showed two lines, a Codex warning that now arrives as a typed record and the failure:

    warning: Model metadata for `gpt-0-nonexistent` not found. Defaulting to fallback metadata; ...
    warning: prompt failed: The 'gpt-0-nonexistent' model is not supported when using Codex with a ChatGPT account.

An ordinary turn in the same instance ("Reply with the single word pong.") finished normally.

The first child's journal, ordinals 31 to 38: Codex `threadStatus: systemError`, then `command_completed ... stop_reason: "EndTurn"`, then `command_queued handback-reminder-36`.

## Interfaces and Dependencies

In `mj-core/src/diagnostic.rs`:

    pub struct SessionFailure { pub id: String, pub category: String, pub severity: String, pub title: String, pub details: Option<String>, pub actions: Vec<String> }
    impl SessionFailure { pub fn from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<Self>; pub fn is_error(&self) -> bool }
    impl TurnDiagnostic { pub fn from_session_failure(failure: &SessionFailure) -> Self }

In `mj-worker/src/acp/launch.rs`: `pub initial_model: Option<String>` on `LaunchSpec` and `pub fn startup_model(&self) -> Option<String>`.
