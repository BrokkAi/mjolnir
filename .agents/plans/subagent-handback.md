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
- [ ] Live runs on `morannon-podman` with three parent/child model pairs, and a tmux check of the TUI's Sub-agents view.

## Surprises & Discoveries

- Observation: a child's first prompt is built before registration, but only registration knows whether the child gets the tool (a Claude child needs a harness home of its own).
  Evidence: `build_subagent_prompt` runs before `start_subagent` in both spawn paths. Registration therefore appends the handback sentence and both paths send `relation.initial_prompt`.
- Observation: `save_state` rewrites every sub-agent record from the saving controller's in-memory copy, so a report stored in `record_json` could be overwritten by an older copy.
  Evidence: `mj-controller/src/database/state_io.rs`, the `subagent_sessions` upsert loop. Reports live in their own table for that reason.
- Observation: the store's copy of a child's turns can lag the live snapshot the turn-end hook sees. Deciding the reminder from the store could see the previous turn, skip the reminder, and leave a `wait` pending until its timeout.
  Evidence: the run loop enqueues materialized projections asynchronously (`conversation_projections.enqueue`). The hook decides from the live snapshot's turn and in-flight commands and reads only the recorded report from the store.

- Observation: upstream took migration 48 (returned steering) while this work was in progress.
  Evidence: merge of `origin/master` on 2026-09-24 conflicted in `migrate_schema`; the handback table became migration 49.

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
- Decision: resuming a stopped child stays out of scope.
  Rationale: no resume path handles a child that borrows its parent's container, and child startup always creates a fresh native session.
  Date/Author: 2026-09-24 / user.

## Outcomes & Retrospective

To be completed after the live runs.

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
