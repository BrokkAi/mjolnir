# Clean up the sub-agent tools to match the spawn/wait design

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md` (repository root: `.agents/PLANS.md`).

## Purpose / Big Picture

A parent agent (a Claude or Codex session managed by Mjolnir) delegates work through the `mj-agents` MCP tools: `list_profiles`, `spawn`, `list_agents`, `send_input`, `wait`, `interrupt`, and `close`. The intended contract is pure spawn/wait: `spawn` starts a child session and returns its id immediately; the parent keeps working; the parent collects a child's output only by calling `wait`, whose tool call blocks until the named children finish their turn or the timeout. Nothing is ever pushed into the parent agent's conversation. When a child finishes, the human user — not the model — gets a one-line notice in the parent's transcript view, and the child's output stays in the child transcript and in `wait`'s answer.

The implementation already behaves this way mechanically, but its words and some of its plumbing were copied from the turn-review system, where reports really are pushed into a supervisor session. As a result the model is told, falsely, that "results arrive in the parent conversation", and the completion path is named and shaped like a delivery-to-parent-agent mechanism (`deliver_subagent_completion`, `mark_subagent_turn_delivered`, a child-output parameter that is extracted and then discarded). After this cleanup the model-facing text tells the truth, the retry path is explicit, and the notice path reads as what it is: a user-visible notification. No harness-visible wire format changes except tool prose.

## Progress

- [x] (2026-09-15) Milestone 1: truthful model-facing contract text in `mj-worker/src/subagent_mcp.rs`, with tests. Instructions factored into `SERVER_INSTRUCTIONS`, the degraded-path fallback into `pending_reply`, `request_key` documented in the spawn schema.
- [x] (2026-09-15) Milestone 2: rename the completion-notice path and remove the dead child-output plumbing, with tests. `record_subagent_completion_notice` (no `output` parameter), `mark_subagent_turn_noticed`, `SubagentRecord::noticed_turn` with the stored name kept as `delivered_turn`, backward transcript scan deleted, serde round-trip test added.
- [x] (2026-09-15) Milestone 3: full validation (`cargo fmt`, `cargo test`, `cargo clippy`) and commits.

## Surprises & Discoveries

- Observation: the worker socket already blocks every tool call until the controller answers, so the "accepted, results arrive later" placeholder is only the lost-daemon degraded path, not the normal flow.
  Evidence: `mj-worker/src/worker_runtime/subagents.rs` `serve_one` — "Block until the daemon completes this request and return its result as the tool's answer"; the controller-side comment at the `complete_subagent_request` call in `mj-controller/src/server_runtime.rs` says "The result reaches the model as the tool call's own answer… It is not injected as a turn."
- Observation: `deliver_subagent_completion` accepts the child's final output and immediately drops it (`let _ = output;`), while its caller scans the child transcript backwards to produce that output — dead work on every completed child turn.
  Evidence: `mj-controller/src/server_runtime/api.rs` (`let _ = output;` inside `deliver_subagent_completion`) and the `.transcript.iter().rev().find_map(...)` block in `mj-controller/src/server_runtime.rs` immediately before the `subagent_completion_jobs` spawn.
- Observation: a TUI test fixture also constructs `SubagentRecord`, so the field rename's reference list spans a crate the initial grep did not cover; a full-repo grep is the reliable check for such renames.
  Evidence: `mj-tui/src/lib.rs:3771` failed to compile with `struct SubagentRecord has no field named delivered_turn` during the first full-suite run; fixed to `noticed_turn: None`.

## Decision Log

- Decision: there is no synchronous or pushed delivery to the parent agent. Child output reaches the model only as the `wait` tool call's own answer; completion reaches the user only as a one-line notice. The user confirmed this is the intended design and that the earlier "steer an active turn or queue a continuation" idea from the original sub-agent plan is rejected and must not be given credence.
  Rationale: the parent is a user-driven harness session that Mjolnir deliberately never wakes; pushing turns into it would forge user turns and cannot work across harnesses.
  Date/Author: 2026-09-15, user.
- Decision: keep the blocking-socket mechanics exactly as they are. `spawn` answers when the child exists (it returns `child_session_id` after session creation and prompt submission, without waiting for the child's turn); `wait` answers when the children finish or the timeout expires.
  Rationale: this is the spawn/wait design; only text, names, and dead parameters change.
  Date/Author: 2026-09-15, ZCode.
- Decision: the degraded no-answer placeholder tells the model the request is still queued and how to collect it (repeat the call with the same `request_key`; for spawns without a key, check `list_agents` before spawning again), instead of promising a push that never comes.
  Rationale: repeating a spawn without a key would duplicate the child, so the note must route retries through idempotency.
  Date/Author: 2026-09-15, ZCode.
- Decision: rename the Rust field `SubagentRecord.delivered_turn` to `noticed_turn` while keeping the serialized name `delivered_turn` via `#[serde(rename = "delivered_turn")]`.
  Rationale: relation payloads are stored JSON in the `subagent_sessions` table; keeping the wire name makes the change compatible with existing databases (older reads and writes unaffected), so no migration revision bump is warranted. The Rust name stops implying delivery to the parent agent.
  Date/Author: 2026-09-15, ZCode.
- Decision: do not touch the review MCP server (`mj-worker/src/review/mcp.rs`). Its "reports arrive as later messages in this session" text is true there, because the review supervisor is a Mjolnir-driven session that Hel explicitly resumes with lane reports.
  Rationale: the bug is copying that sentence into the sub-agent server, not the sentence itself.
  Date/Author: 2026-09-15, ZCode.

## Outcomes & Retrospective

Complete. The `mj-agents` server now tells the model the spawn/wait contract: `SERVER_INSTRUCTIONS` directs result collection through `wait`, the degraded-path `pending_reply` routes retries through `request_key` (and `list_agents` before a keyless re-spawn), and neither string promises a push. The completion path reads as what it is: `record_subagent_completion_notice` posts the unchanged one-line user notice without the dead `output` parameter, `mark_subagent_turn_noticed` persists the ordinal, and `SubagentRecord::noticed_turn` keeps the historical `delivered_turn` wire name so stored relation payloads deserialize unchanged (proved by the serde round-trip test).

Validation: focused suites (worker 9, core 3, controller 1) then the full default-member suite with only the two documented environment-broken tests skipped; `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` all clean. Behavior is unchanged on the wire apart from the tool prose; the socket mechanics tests passed unmodified.

## Context and Orientation

Mjolnir ("the daemon") manages agent sessions. Each session has a target-side `mj-worker` process ("the worker") with a private root; the controller side of the daemon talks to it over a durable relay. A parent session is a Claude or Codex session; a child (sub-agent) is an ordinary Mjolnir session that borrows the parent's target and filesystem. The MCP server `mj-agents` is the worker binary re-invoked as `hel worker subagent-mcp --socket <path>`; it is injected into parent sessions only (see `mj-worker/src/acp.rs`, `extra_mcp`). A notice is a `RelayCommand::RecordNotice` from `mj-core/src/relay/snapshot.rs`: it is recorded as a `RelayObservation::Notice` in the relay journal and rendered in the TUI/web transcript for the human; it is never forwarded to the harness (the `mj-worker/src/worker_runtime/unix.rs` command-to-ACP mapping maps `RecordNotice` to `None`).

The pieces this plan touches:

- `mj-worker/src/subagent_mcp.rs` — the stdio MCP server the harness calls. Hand-rolled JSON-RPC loop (`initialize`, `ping`, `tools/list`, `tools/call`). `tool_definitions()` builds the schemas; `call()` forwards one JSON line over the parent worker's Unix socket and blocks for the one-line reply; `send()` is the synchronous socket helper.
- `mj-worker/src/worker_runtime/subagents.rs` — the other end of that socket. `serve_one` enqueues the request durably (`subagents.json`) and blocks (ceiling `MAX_WAIT_SECONDS + 60` = 3660 s) until the controller completes it; `SocketReply { accepted, result }` carries the optional result. `result: None` means the controller never answered in time.
- `mj-core/src/subagent.rs` — shared wire types: `SubagentToolRequest`, `SubagentToolAction`, `SubagentToolResult`, `SubagentRecord` (durable relation payload, including `delivered_turn`), `MAX_WAIT_SECONDS = 3600`.
- `mj-controller/src/server_runtime.rs` — the daemon loop. On each relay snapshot it drains queued `subagent_requests`, runs `execute_subagent_tool`, and sends the result back with `complete_subagent_request`. Separately, when a child goes idle with a fresh completed turn (`relation.delivered_turn != Some(outcome.completed_ordinal)`), it extracts the child's last agent text and calls `deliver_subagent_completion`, then `mark_subagent_turn_delivered`.
- `mj-controller/src/server_runtime/api.rs` — `deliver_subagent_completion` (submits the one-line `RecordNotice` to the parent session) and `execute_subagent_tool_inner` (the action implementations; `Spawn` returns `child_session_id`/`task_name`/`profile_id` immediately; `WaitAgents` polls child summaries every 250 ms until all are complete or the clamped deadline, then returns each child's `state` and `output`).
- `mj-controller/src/database.rs` — `mark_subagent_turn_delivered(child_session_id, turn)` mutates the relation's `delivered_turn` through the shared database writer.

Terminology: "the model" is the parent harness's LLM; "the user" is the human looking at the TUI or web transcript. Everything in this plan hinges on that distinction: tool text addresses the model; notices address the user.

## Plan of Work

### Milestone 1 — truthful model-facing text

All edits in `mj-worker/src/subagent_mcp.rs`.

1. Replace the `initialize` instructions string (currently "Delegate work to Mjolnir sessions in this target. Calls are accepted immediately; results arrive in the parent conversation and are visible in the Sub-agents workspace.") with spawn/wait wording, e.g.: "Delegate work to Mjolnir child sessions in this target. spawn starts a child and returns its child_session_id immediately; the child runs independently while you continue other work. Collect a child's result only by calling wait, which blocks until the named children finish their current turn or the timeout. The user can see every child in the Sub-agents workspace."
2. In `call()`, replace the no-result fallback note (currently "Mjolnir accepted the request. The result will arrive in this conversation; the child is also available in the Sub-agents workspace.") with pending/retry wording, e.g.: "Mjolnir has not answered this request yet; it stays queued. Repeat this call with the same request_key to collect its result. If this was a spawn without a request_key, check list_agents before spawning again so the child is not duplicated." Keep the `accepted: true` and `request_id` fields.
3. In `tool_definitions()`, give `spawn`'s `request_key` property a description: "Optional idempotency key. Repeating a call with the same key returns the original result instead of duplicating the work; useful for retries and long waits." Leave every other schema untouched.
4. In the `#[cfg(test)]` module, extend the tests: assert the initialize instructions name `wait`, assert neither the instructions nor the fallback note contain "arrive in", and assert the fallback note mentions `request_key` and `list_agents`. Factor the fallback note into a small helper (e.g. `fn pending_reply(request_id: &str) -> Value`) so the note is testable without a socket.

### Milestone 2 — notice-path naming and dead plumbing

1. `mj-controller/src/server_runtime/api.rs`: rename `deliver_subagent_completion` to `record_subagent_completion_notice` and delete the `output: &str` parameter (and `let _ = output;`). Update its doc comment to: records a one-line user-visible notice that a child finished a turn; the child's output is not included — it is collected with `wait` and read in the child transcript; the notice must not forge or start a user turn. The notice text itself ("Subagent {task_name:?} ({short_id}) finished turn {turn} ({outcome}).") is unchanged.
2. `mj-controller/src/server_runtime.rs`: in the child-idle branch, delete the backward transcript scan that builds `output` (the `.transcript.iter().rev().find_map(...)` block producing the "completed without a final text response" fallback); call the renamed function without the output argument. Everything else in that branch (idle check, `delivered_turn` comparison, `mark_subagent_turn_delivered`, join-set handling) stays.
3. Rename `mark_subagent_turn_delivered` to `mark_subagent_turn_noticed` in `mj-controller/src/database.rs`, including the `submit_database_write` label string, and update its two callers (`mj-controller/src/server_runtime.rs` closure; grep for other references).
4. `mj-core/src/subagent.rs`: rename the `SubagentRecord` field `delivered_turn` to `noticed_turn`, keeping the stored JSON name with `#[serde(default, skip_serializing_if = "Option::is_none", rename = "delivered_turn")]`. Update the field's doc comment to say: the parent-session turn ordinal for which the user-visible completion notice has been recorded, so restarts do not repeat it. Update all readers/writers of the field (`mj-controller/src/server_runtime.rs` comparison, `mj-controller/src/database.rs`).
5. Update tests that reference the old names or the old signature: `a_finished_subagent_is_recorded_as_a_notice_not_a_prompt` in `mj-controller/src/server_runtime/api.rs` (drop the output argument; keep asserting the notice shape, the `subagent-` command-id prefix, and that the child output does not appear in the notice), and any test touching `delivered_turn` or `mark_subagent_turn_delivered` (find them with the grep in Concrete Steps).

### Milestone 3 — validation and commits

Run the full checks, fix fallout, and commit each milestone separately (Milestones 1 and 2 may land as two commits or one if validated together). Update this plan's Progress, Surprises & Discoveries, and Outcomes sections with results.

## Concrete Steps

Work from the repository root, `/workspace/hel`. Edit only the files named above. To find every reference before each rename:

    grep -rn "deliver_subagent_completion" mj-controller/src
    grep -rn "mark_subagent_turn_delivered" mj-controller/src
    grep -rn "delivered_turn" mj-core/src mj-controller/src

Run tests outside the restricted sandbox with elevated permissions (the suite uses loopback TCP and Unix sockets). Per-file focused runs first, e.g.:

    cargo test -p brokk-mj-worker subagent
    cargo test -p brokk-mj-controller --lib a_finished_subagent

Then the required checks:

    cargo fmt --all
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

Two controller tests fail in this development environment for pre-existing, unrelated reasons (root can read chmod-000 paths, and a git-cache eviction environment quirk): `controller::git_cache::tests::cache_gc_evicts_the_oldest_mirror_to_meet_its_soft_cap` and `targets::tests::the_ec2_disk_probe_fails_instead_of_undercounting_an_unreadable_path`. They were verified failing before any of this work; skip them with `--skip cache_gc_evicts_the_oldest_mirror --skip the_ec2_disk_probe_fails` and note the skip in the commit message of whichever run they affect. Do not try to fix them here.

Commit with explicit paths (never `git add -A`), on the current branch, without pushing.

## Validation and Acceptance

Behavioral acceptance, all provable with unit tests:

- The `initialize` instructions and every tool description tell the model to collect results with `wait` and never claim that results arrive in the parent conversation. A new worker test asserts the strings: instructions contain "wait"; instructions and fallback note do not contain "arrive in"; fallback note contains "request_key" and "list_agents".
- The degraded path (controller never answers within the ceiling) still returns a well-formed tool result with `accepted: true` and the retry note, and the normal path is unchanged: existing tests `queue_survives_reopen_and_returns_cached_completion_idempotently`, `a_waiting_socket_call_returns_the_daemon_result_when_it_lands`, and `a_waiting_socket_call_gives_up_at_its_deadline` in `mj-worker/src/worker_runtime/subagents.rs` must still pass unmodified — they prove the socket mechanics are untouched.
- A finished child still produces exactly one user-visible notice and no prompt: `a_finished_subagent_is_recorded_as_a_notice_not_a_prompt` (updated signature) must pass, asserting `RelayCommand::RecordNotice` and that the child's output text does not appear in the notice.
- The rename is storage-compatible: existing relation payloads (JSON with `delivered_turn`) still deserialize after the field rename; add or extend a `SubagentRecord` serde test in `mj-core/src/subagent.rs` that round-trips a payload containing `"delivered_turn": 3` into `noticed_turn: Some(3)` and serializes back to the same JSON.
- `cargo test` (default members) passes except the two documented environment-broken skips; `cargo clippy --all-targets -- -D warnings` passes; `cargo fmt --all -- --check` and `git diff --check` are clean.

Manual spot-check (optional, if a local bare target is configured): start a parent with sub-agents enabled, ask it to spawn a child and immediately summarize something else, then ask it to wait; observe the tool call for `spawn` returns the child id at once, `wait` returns the output, and the parent transcript shows only the one-line completion notice.

## Idempotence and Recovery

Every step is a pure rename, string, or deletion with no schema, protocol, or migration change; the stored JSON field name is preserved by the serde rename. Re-running any milestone after a partial edit is safe: the greps name every call site. If Milestone 2 is abandoned halfway, the tree still compiles only when all four files are updated together — the focused tests catch that immediately. No user data, live sessions, or configuration is touched.

## Artifacts and Notes

The false strings being replaced, for the record (from `mj-worker/src/subagent_mcp.rs` before this plan):

    "instructions":"Delegate work to Mjolnir sessions in this target. Calls are accepted immediately; results arrive in the parent conversation and are visible in the Sub-agents workspace."
    "note":"Mjolnir accepted the request. The result will arrive in this conversation; the child is also available in the Sub-agents workspace."

The dropped-output evidence (`mj-controller/src/server_runtime/api.rs`):

    pub async fn deliver_subagent_completion(
        ...,
        output: &str,
    ) -> Result<()> {
        let _ = output;

## Interfaces and Dependencies

No new crates, traits, or wire types. Changed signatures:

- `mj-controller/src/server_runtime/api.rs`:
  `pub async fn record_subagent_completion_notice(&self, parent_session_id: String, child_session_id: &str, task_name: &str, turn: u64, outcome: &str) -> Result<()>`
- `mj-controller/src/database.rs`:
  `pub fn mark_subagent_turn_noticed(child_session_id: &str, turn: u64) -> Result<()>`
- `mj-core/src/subagent.rs`:
  `pub struct SubagentRecord { ..., #[serde(default, skip_serializing_if = "Option::is_none", rename = "delivered_turn")] pub noticed_turn: Option<u64>, ... }`

The MCP tool names (`list_profiles`, `spawn`, `list_agents`, `send_input`, `wait`, `interrupt`, `close`), the request/result wire types, the socket protocol (`SocketReply`), and the review MCP server are all out of scope and must remain byte-identical on the wire apart from the prose strings named in Milestone 1.

Revision 2026-09-15: created from the user's direction after auditing the implemented flow — mechanics already follow spawn/wait; this plan removes the reviewer-copied language, the delivery-flavored naming, and the dead child-output plumbing, and records the user's rejection of the original plan's steer/queue continuation idea.

Revision 2026-09-15 (implementation complete): all three milestones delivered and validated; `cargo fmt --all` additionally reflowed one pre-existing over-width line in `mj-worker/src/review/mcp.rs`, included because the workspace check requires a clean format run.
