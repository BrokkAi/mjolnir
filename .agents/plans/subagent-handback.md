# Sub-agents hand back an explicit report

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current as work proceeds. Maintain it in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

A Mjolnir sub-agent (a child session started through the `mj-agents` MCP tools) used to report back through whatever its last message happened to be. A child that ended a long turn with "Done." gave its parent nothing to work with. Claude Code's own subagents solved the same problem with an explicit `SubagentHandback` tool, a reminder when a subagent ends without calling it, and a framed report. This change brings that design to Mjolnir.

After this change a Claude or Codex child sees one MCP tool, `handback`, and its first prompt says to finish by calling it. If a child ends its turn without a report, Mjolnir sends it one visible reminder prompt. The parent's `wait` answers with the handed-back report and says where the report came from (`report_source: handback | last_message`); a child that owes a report reads as `running` until the reminder turn ends. `mj wait` on a child session behaves the same way.

The same change removes the caller-chosen `request_key` from `spawn` and from the HTTP sub-agent route, and stops hiding Claude's `TaskStop` and `TaskOutput`, which also stop and read a parent's own background shell commands.

## Progress

- [x] (2026-09-24) Compared Claude Code's subagent tools (extracted from 1,071 local transcripts) with `mj-agents`; agreed the scope with the user.
- [x] (2026-09-24) Core contract in `mj-core/src/subagent.rs`: `SubagentToolAction::Handback`, `SubagentRecord::handback_tool`, `SubagentMcpRole`, `SubagentReport`, and the shared `report_state` rule with tests.
- [x] (2026-09-24) Store: migration 49 adds `subagent_handbacks` (breaking, because older readers refuse the new record field) and helpers in `mj-controller/src/database/sessions.rs`.
- [x] (2026-09-24) Registration decides the tool; launch passes it; the worker serves the child role; Claude staging names the role.
- [x] (2026-09-24) Daemon: the `Handback` action, the report rule in sub-agent `wait` and `list_agents`, the reminder in the child turn-end hook, and the session wait.
- [x] (2026-09-24) `request_key` removed from the model and HTTP surfaces; `TaskStop`/`TaskOutput` unhidden.
- [x] (2026-09-24) Automated tests, clippy.
- [x] (2026-09-24) Live runs on `morannon-podman`: Claude Opus with Codex luna children (#1144), Claude Opus with DeepSeek flash children (#480), Codex gpt-6-sol with DeepSeek flash children (#1137). Handbacks, reminders and the parents' `wait` answers were exercised; two defects found and fixed (early `completed`, and stopped children unreadable in the TUI).
- [x] (2026-09-24) tmux check of the TUI's Sub-agents view; stopped children now open as read-only stored transcripts.

## Surprises & Discoveries

- Observation: a child's first prompt is built before registration, but only registration knows whether the child gets the tool (a Claude child needs a harness home of its own).
  Evidence: `build_subagent_prompt` runs before `start_subagent` in both spawn paths. Registration therefore appends the handback sentence and both paths send `relation.initial_prompt`.
- Observation: `save_state` rewrites every sub-agent record from the saving controller's in-memory copy, so a report stored in `record_json` could be overwritten by an older copy.
  Evidence: `mj-controller/src/database/state_io.rs`, the `subagent_sessions` upsert loop. Reports live in their own table for that reason.
- Observation: the store's copy of a child's turns can lag the live snapshot the turn-end hook sees. Deciding the reminder from the store could see the previous turn, skip the reminder, and leave a `wait` pending until its timeout.
  Evidence: the run loop enqueues materialized projections asynchronously (`conversation_projections.enqueue`). The hook decides from the live snapshot's turn and in-flight commands and reads only the recorded report from the store.

- Observation: upstream took migration 48 (returned steering) while this work was in progress.
  Evidence: merge of `origin/master` on 2026-09-24 conflicted in `migrate_schema`; the handback table became migration 49.

- Observation (live run, 2026-09-24): a parent's `wait` right after `spawn` answered `completed` with no output for a child whose first turn had started 45 ms earlier. The same happened to a child whose turn failed on a Codex usage limit: its reason never reached the parent.
  Evidence: child `9931f98d` events show `turn_started` for its first prompt at 959384 ms and the parent's wait answering at 959429 ms; the turn ran for another three minutes. Child `ea722d76` ended with "You've hit your usage limit" and the wait reported `state: completed, output: null`. The sub-agent wait read the store, which lags the live session, and treated any idle child as finished.
- Observation (live run): three of four children skipped `handback` in their task turn and handed back only after the reminder.
  Evidence: `subagent_handbacks.handback_command_id` starts with `handback-reminder-` for three children. The instruction trailed long parent prompts; it now leads them.

## Decision Log

- Decision: every Claude or Codex child gets `handback`, whichever spawn path created it, and `mj wait` learns the same rule.
  Rationale: the user asked why the MCP and HTTP paths should differ; the only reason had been avoiding a change to the session wait, which turned out to be small (the pending report is treated like an armed retry in `resolve_wait`).
  Date/Author: 2026-09-24 / user and Claude.
- Decision: one reminder per task turn, sent as an ordinary prompt with command id prefix `handback-reminder-`; after it, the last message stands.
  Rationale: matches Claude Code's `[handback-send-enforce]`; a prompt to a child is how Mjolnir already drives children, and the prefix lets every reader recognise the reminder turn without extra state.
  Date/Author: 2026-09-24 / user and Claude.
- Decision: no child-to-parent messages during a turn.
  Rationale: in the transcripts children sent 355 handbacks and at most 5 mid-run messages. A child that needs a decision hands back a question; the parent answers with `send_input`.
  Date/Author: 2026-09-24 / user.
- Decision: record the acceptance ordinal of every parent prompt to a child (the first prompt and each `send_input`) in migration 50, and treat an idle child as unfinished until a finished turn reaches it. Report a failed or interrupted turn as `failed` or `interrupted` with its reason instead of `completed`.
  Rationale: the store lags the live session, and the parent needs to know a child failed and why, so it can re-prompt or pick another profile.
  Date/Author: 2026-09-24 / Claude, from the live run.
- Decision: resuming a stopped child stays out of scope.
  Rationale: no resume path handles a child that borrows its parent's container, and child startup always creates a fresh native session.
  Date/Author: 2026-09-24 / user.

## Outcomes & Retrospective

Live runs showed the design working end to end: every child that finished a task turn had its report recorded, and the parents' `wait` answers carried `report_source: handback`. Most children (three of four in the first wave) handed back only after the reminder, which is why the instruction now leads the first prompt.

The runs also found two defects outside the handback itself, both fixed in this work: the sub-agent `wait` read an idle child as finished before the store had seen its new turn (and hid failed turns as `completed`), and the TUI could not show a stopped child's conversation at all. A third observation, a child whose worker relay never answered its first hello so its first prompt was never delivered, reproduced once and is not explained yet; its parent's `wait` correctly kept it `running`, and the parent replaced it.

Two parents hit provider usage limits (`codex2`, `codex3`); the TUI's quota pane had shown them correctly as 0% remaining.

## Context and Orientation

- `mj-worker/src/subagent_mcp.rs` serves the `mj-agents` MCP server. `--role parent` serves `list_profiles`, `spawn`, `list_agents`, `send_input`, `wait`, `interrupt`, `close`; `--role child` serves only `handback`.
- `mj-worker/src/worker_runtime/subagents.rs` queues tool requests; the daemon polls every worker with `RelayRequest::SubagentRequests` (`mj-controller/src/server_runtime/run.rs`) and runs them through `ApiBackend::execute_subagent_tool` (`mj-controller/src/server_runtime/api.rs`). The requesting session is the parent for delegation and the child for `handback`.
- `mj_core::subagent::report_state` is the one rule: `Delivered` when a handback names the last finished turn, `Pending` when a normally finished task turn has none (a reminder is due or on its way), otherwise `Fallback` to the turn's last message.
- `mj-controller/src/server/api/wait.rs` and `wait_policy.rs` implement the session wait; `WaitObservation::apply_subagent_report` folds the rule in.

## Plan of Work

Implemented as described in the Progress list. The remaining step is the live validation below.

## Validation and Acceptance

Automated: `cargo test` and `cargo clippy --all-targets -- -D warnings` on the dev profile. New tests cover every row of `report_state`, the child role's tools and refusals, the handback action (one report per turn, refusals), the reminder (sent once, not sent when work is queued), both waits, migration 49, and the staged Claude role.

Live, on an isolated instance with `morannon-podman`: a Claude Opus parent with Codex luna children, a Claude Opus parent with DeepSeek flash children, and a Codex sol 6 parent with DeepSeek flash children each solve a real ticket; the parents' `wait` answers carry `report_source: "handback"`; the TUI's Sub-agents view shows the children and their work.

## Idempotence and Recovery

Migration 49 creates its table only when absent and raises the compatibility floor in the same transaction. A reminder is recorded after it is sent; a failed send is recorded so no wait keeps waiting for it. A replayed spawn is still deduplicated by the per-call request id.

## Interfaces and Dependencies

- `hel worker subagent-mcp --socket <path> --harness <kind> --role parent|child`.
- `SubagentToolAction::Handback { message }`; answer `{"delivered": bool, "message": ...}`.
- Sub-agent `wait` agent objects gain `report_source`; the HTTP `WaitResponse` gains `report_source` for child sessions.
- `POST /api/v1/sessions/{id}/subagents` no longer accepts or returns `request_key`.
