# Report Sub-Agent Finishes Through Wait

This ExecPlan is a living document maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

Parents should be able to call `wait` without tracking child IDs or choosing an any/all policy. The daemon will watch every non-stopped child, return immediately when an unreported finish exists, and return that child’s output once per distinct finish. If a child finishes while the parent is doing other work, the daemon will queue a short prompt that tells the parent to call `wait`. A durable finish marker ensures a retry of the same tool request returns the same answer without returning the report again on a later wait.

The behavior is observable in the `mj-core`, `mj-worker`, and `mj-controller` tests: legacy persisted wait requests deserialize, wait returns a report once, later child turns can report again, and the parent prompt is queued and withdrawn through the session relay.

## Progress

- [x] (2026-10-06) Read the investigation map and trace the tool, durable-result, child-completion, and relay prompt paths.
- [x] (2026-10-06) Remove wait child selectors; add durable finish identity and dynamic child resolution; update worker and user guidance.
- [x] (2026-10-06) Add prompt reconciliation tests and finish wait test coverage.
- [x] (2026-10-06) Compile, run focused tests, run full touched-crate tests, and run Clippy as requested.
- [x] (2026-10-06) Review the final diff and record outcomes and known limitations.
- [x] (2026-10-06) Fix worker fallback acknowledgment, same-turn reminder reconciliation, terminal identity stability, and retry/startup reconciliation.

## Surprises & Discoveries

- Observation: `subagent_sessions.record_json` stores the full `SubagentRecord`, so adding an optional serde-default finish marker requires no schema revision.
  Evidence: `mj-controller/src/database/delegation.rs` updates the stored JSON in the same transaction that records the durable tool result.
- Observation: the relay’s materialized queue retains each prompt’s content and command ID, allowing coalescing and withdrawal without adding database state.
  Evidence: `MaterializedQueuedPrompt` in `mj-core/src/state.rs` and `RelayCommand::RemoveQueuedPrompt` in `mj-controller/src/server_runtime/api.rs`.
- Observation: a worker-local wait timeout and daemon completion need one serialized decision; otherwise the daemon can mark a report collected after the model already received the fallback.
  Evidence: live waiter registration and cached completion status share the queue lock in `mj-worker/src/worker_runtime/subagents.rs`.
- Observation: an active wait reminder is not a coalescing key. A later finish during that turn needs one queued reminder; only a reminder still in the relay queue coalesces.
  Evidence: `ensure_parent_wait_prompt` checks outstanding waits and queued prompt content, and the wait path withdraws queued reminders.

## Decision Log

- Decision: identify ordinary finishes with the completed turn’s start position, completion ordinal, and reported state; identify terminal finishes without a turn span by terminal state, detail, and the last completed turn ordinal (or `None`).
  Rationale: the turn span distinguishes repeated turns for a resumed child. The last completed ordinal distinguishes terminal failures after separate runs and remains stable when session metadata changes. Repeated terminal state/detail without a later completed turn denotes the same terminal event.
  Date/Author: 2026-10-06 / Codex.
- Decision: persist report markers in the same SQLite transaction as the durable wait result.
  Rationale: durable request replay must not lose a report or return it twice after the result was recorded.
  Date/Author: 2026-10-06 / Codex.
- Decision: persist finish identities beside the durable result and let the worker report whether a live tool waiter received it.
  Rationale: when the worker-local deadline wins, the daemon can clear only markers written by that result and reconcile a fresh parent prompt. A legacy unit response counts as delivered because old workers cannot report the distinction.
  Date/Author: 2026-10-06 / Codex.

## Outcomes & Retrospective

Wait now resolves every non-stopped child on each observation. It answers immediately for an unreported finish or when no child is unfinished; only newly reported finishes carry output. A serde-default `reported_finish` identity distinguishes completed turn spans and terminal failures without a schema migration. The durable result and included finish markers commit in one SQLite transaction, so daemon replay returns the saved answer.

The daemon reconciles a fixed-text parent reminder after child completion, startup/provisioning failure, delivered wait results, and once at startup for every parent with child records. It coalesces only reminders still in the relay queue, suppresses reminders while a WaitAgents request is outstanding, skips closing/closed children, and withdraws still-queued reminders when a wait starts. Reconcile errors enter a periodic retry loop; child parking proceeds even when reminder submission fails.

The worker records its live-waiter completion decision with the cached result. If the local fallback returned first, delivery resets only the unchanged finish markers stored with that durable answer and reconciles a parent prompt. Terminal identities use the last completed turn ordinal instead of mutable session metadata. A prompt already active can be followed by one queued reminder when another child finishes later in the same turn.

Validation: full `brokk-mj-core` passed (635 unit tests, 3 scenarios); full `brokk-mj-controller` passed (2,223 tests); serial worker library and main targets passed (726 + 8 tests, 10 ignored), and all four worker integration targets passed (15 tests). A parallel worker run had two temporary-root ownership collisions (724 passed, 2 failed, 10 ignored); both failing tests passed individually. The delegation e2e passed in `subagent-wait-20261006`; touched-crate all-target Clippy and formatting checks passed. Detailed logs and failure names are in the sub-agent report directory.

Remaining limit: marker durability proves the daemon recorded the answer, not that the parent model read it. A current worker distinguishes its local fallback from a live waiter; a legacy worker’s unit response is treated as delivered because it omits that fact. The delivery-status response uses relay protocol 31 and preserves the old unit response for compatibility.

Follow-up validation: full controller tests passed (2,229 passed, 10 ignored). The full worker run had one temporary-root ownership collision in `checkpoint_only_start_refuses_missing_or_corrupt_state_without_starting_a_harness` (726 passed, 1 failed, 10 ignored); that test passed by itself. Focused controller wait/prompt tests passed (119), as did the same-turn prompt, terminal rename, retry/parking, legacy response, worker timeout, and socket waiter-registration regressions. Clippy passed for all targets of core/controller/worker. Logs are in `.mj/agents/faabf735b53046c406fa0b5fa102bc08/followup-*.log`.

## Context and Orientation

`mj-core/src/subagent.rs` defines the serialized worker-to-daemon sub-agent action and the durable relation between a parent and child. `mj-worker/src/subagent_mcp.rs` publishes the MCP tool schema and translates tool calls. `mj-worker/src/worker_runtime/subagents.rs` builds a bounded fallback answer when the daemon result is unavailable. `mj-controller/src/server_runtime/api.rs` executes the tool and observes child completion. `mj-controller/src/server_runtime/api/child_wait.rs` waits for durable/runtime publications. `mj-controller/src/database/delegation.rs` stores an executed tool result so a retry can reuse it. `mj-controller/src/daemon/delegation.rs` notices finished child turns and delivers durable results to parent workers.

The daemon is the long-running controller process; a worker is the per-session process that runs the agent. A “finish identity” is a stable value that distinguishes one completed child turn from another. A “level-triggered reconciliation” means checking the current durable facts each time and making the queue match them, rather than relying on one completion event.

## Plan of Work

First, remove child selectors, return policy, and the caller-selected timeout from the public action, MCP schema, and documentation. Tolerate those retired fields while deserializing durable wait requests. Then resolve the child relations on each wait loop iteration and compare each finished child’s identity to its durable marker. Include full output only for new reports, return immediately for a new report or when nothing remains unfinished, and otherwise wait on `ChildWaitFeed` until a child changes or the harness's default deadline expires.

The worker request-result transaction must write both the answer and each included child’s marker. This also protects daemon replay. Then add a parent prompt reconciler which checks current child records, outstanding wait requests, queued prompts, and close state. Invoke it after ordinary child completion, startup/provisioning failure, and delivered wait results. On a new wait, remove any matching queued prompt through the relay. Test the user-visible result and the coalescing/withdrawal behavior.

## Concrete Steps

Work from the repository root. Inspect with `rg` and `git diff`; format Rust using `cargo fmt --all -- --check` after edits. Run focused tests using crate filters first. The project instructions require `cargo test -p mj-core`, `cargo test -p mj-worker`, and `cargo test -p mj-controller`, plus `cargo clippy --all-targets -- -D warnings`; execute test commands outside the restricted sandbox with the repository’s normal Cargo/mbx setup and do not redirect `target/`.

Expected successful test commands end with `test result: ok` for each crate. Clippy must exit zero without warnings. If the delegation end-to-end suite is available, run its documented invocation in `tests/e2e/delegation.py`; otherwise note the concrete environment limitation.

## Validation and Acceptance

Acceptance requires tests showing: old persisted wait fields are ignored; new wait requests serialize without `params` while empty and legacy `params` shapes deserialize; replay uses the default harness budget measured from `created_at_ms`; a new finish is returned immediately and only once; a later child turn is independently reportable; wait blocks until one child changes; empty/all-settled waits return immediately; replay reuses the saved result; parent reminders queue only without a pending wait, coalesce, withdraw on wait, skip closing children, and include startup failures. The daemon’s existing finished predicate must remain unchanged.

## Idempotence and Recovery

The optional marker defaults to `None`, so old `record_json` values remain readable. No SQL migration is needed. The old wait fields are stripped only for `wait_agents`; unknown fields on current requests remain rejected. The unit action serializes as `{"action":"wait_agents"}` without `params`, while deserialization accepts absent, empty-object, and legacy-object params. The same durable request ID can be retried safely because its stored answer is reused.

## Artifacts and Notes

The implementation is in the Rust source and existing agent/skill guidance named above. Record test output paths and any unresolved race in the sub-agent handback report.

## Interfaces and Dependencies

The public action is the unit variant `SubagentToolAction::WaitAgents`. `DEFAULT_WAIT_SECONDS` is capped by the parent harness (including the Codex ceiling), and worker/daemon budgets are measured from the durable request's `created_at_ms`. `SubagentRecord.reported_finish` stores `Option<SubagentFinishIdentity>`. The controller owns `ApiBackend::ensure_parent_wait_prompt(parent_id)`, while wait requests call the matching withdrawal helper. The persisted answer and marker update go through `record_delegation_result_with_reports` in one transaction.

Plan update (2026-10-06): replaced the old per-child/per-policy wait model with an all-child, report-identity model after tracing the durable result and relay queue owners; this preserves replay semantics without a migration or new queue table.
