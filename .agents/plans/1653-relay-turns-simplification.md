# Epic #1653: Simplify worker relay turns, continuations, tool ownership, and snapshot state

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan follows `.agents/PLANS.md` from the repository root and must be maintained in accordance with it.

## Purpose / Big Picture

After this work lands, Mjolnir's worker relay and transcript projection have one clear owner for every turn-lifecycle decision and less duplicated state than before. Specifically, reopening a Codex session with an active native goal pauses the goal cleanly at session open instead of racing queued user prompts or restart continuations (#1592). Targeted `mj wait --turn` waits see already-committed queued continuations across daemon restarts instead of returning false errors, and `mj interrupt-turn`, TUI `Esc`, and the web viewer all cancel active prompts, queued restart continuations, and tracked Claude/Antigravity autonomous turns through one worker-published exact target (#1599, finishing the targeting tail of #971). The worker stores each queued or active prompt payload once instead of three times inside its private relay snapshot, refuses snapshot overflow with a non-retryable HTTP 409 precondition without dropping the worker connection, and validates startup prompts and inline-image prompts before persisting them (#1576). Live foreground tool tracking and durable transcript projection share turn-scoped ownership so late updates can never strand a session in `ActivityState::Tool` for hours (#1623) and interrupted foreground tool calls no longer stay `in_progress` forever in transcripts going forward (#1311). Finally, interrupted ACP commands report that the worker restarted rather than the relay (#1303).

Seven stale, already-mitigated, or wontfixed child issues under #1653 (#1029, #1032, #1426, #1436, #971, #1593, #1610) were closed before implementation began, alongside #1643 which was already closed in `a5f84dc13`.

## Progress

- [x] (2026-11-23 15:27Z) Audited all 14 child issues of #1653 against `16d810a23`, closed stale/mitigated/wontfixed issues (#1029, #1032, #1426, #1436, #971, #1593, #1610), updated #1653's body checklist, and completed read-only codebase reconnaissance for all remaining issues.
- [x] (2026-11-23 15:50Z) Milestone 1 (#1592 + #1303): Deleted implicit Codex startup goal auto-resume, enabled AIR `resumePolicy=pause` on Codex stored load/resume with negotiated pause guard, updated ACP restart interruption wording to "worker restarted", and validated targeted worker/daemon tests + clippy.
- [x] (2026-11-23 16:02Z) Milestone 2 (#1599 Gap 2): Added committed `queued_command_ids` to `CommittedTurn` and `TurnState`, observed `materialized_queued_prompts` under the turn trigger lane (scoping the `UPDATE` predicate to `materialized_sessions`), unified live/durable turn observation in `WaitObservation::observe_turn`, and validated targeted store/runtime/server/daemon tests + clippy.
- [x] (2026-11-23 16:26Z) Milestone 3 (#1576): Deleted obsolete 16 MiB `RELAY_SNAPSHOT_BYTE_BUDGET` cap and its four checks, collapsed 3x→1x live prompt payloads in `RelaySnapshot` (`dispatches` as sole owner, `RELAY_STATE_VERSION` 29→30 with v29 migration and persist-on-upgrade), mapped pre-commit wire-budget overflow at command admission to non-retryable `InvalidState` → HTTP 409 `Refusal::precondition` on both viewer `/api/actions` and public prompt API, fixed startup-prompt budget and pre-write inline-image validation ordering, and validated all five affected crates + clippy.
- [x] (2026-11-23 17:00Z) Milestone 4 (#1599 Gap 1 + #971 targeting tail): Added protocol-86 / state-31 `RelayCommand::CancelTurnTarget` (`ActivePrompt`, `QueuedRestart`, `Autonomous`, `GoalResume`, `CapacityRetry`, `QuotaRecovery`), unified worker `TurnControls` publication and cancel admission behind `resolve_turn_interrupt_target`, wired relay-local completion for queued restarts and unsubmitted retries, derived daemon/TUI/web interrupt capability from the worker's published offer (keeping protocol <= 85 legacy fallback and Codex goal withholding), and validated all six Rust crates, UTC chat goldens, Node viewer tests, and clippy.
- [ ] Milestone 5 (#1623 + #1311 + local activity cleanups): Scope live `ToolsInFlight` entries by owner and forbid partial `ToolCallUpdate` from creating entries, apply tool deltas inside journal commit, settle open foreground tool rows to `Failed` at going-forward transcript boundaries, delete dead `ToolsInFlight::open`, and remove unused counts from `turn_stall_facts`.

## Surprises & Discoveries

- Observation: Pinned `@brokkai/codex-acp` v1.15.0 (`src/CodexAcpClient.ts:693-720, 764-791`) only pauses an active stored goal on `loadSession` and `resumeSession` when `_meta.jetbrains.air.goal.resumePolicy` is `"pause"`. Under `"preserve"` or omitted metadata, the native thread can launch a background continuation before Mjolnir sends any prompt or RPC.
  Evidence: Verified byte-for-byte against the installed bundle at `/home/jonathan/.cache/mjolnir/harnesses/codex/brokkai-codex-acp-1.15.0_codex-0.170.0_pkg2/node_modules/@brokkai/codex-acp/dist/index.js` and `goal-recovery.test.ts:42-85`. Non-active states (`paused`, `blocked`, `usage-limited`, `budget-limited`, `complete`) are untouched by `"pause"`, so quota Wait and #1498 quota Move remain intact.

- Observation: The session reported in #1623 (`db80f207`) was an Antigravity (`gemini2`) session, unrelated to commit `a7ab7b11f` whose `#1623` reference was an upstream Claude ACP bridge issue. `ToolsInFlight::observe_at` (`mj-core/src/activity/tools.rs:128-167`) uses `.or_insert(...)` for `SessionUpdate::ToolCallUpdate` with `Pending | InProgress`, allowing any late nonterminal update after `finish_turn_activity()` clears the map to resurrect a tool entry with no turn owner forever.
  Evidence: Indexed metadata for `db80f207` and direct reading of `mj-core/src/activity/tools.rs:128-167` and `mj-worker/src/relay/tests.rs:5771-5803`.

- Observation: During a running prompt, non-detached hosted terminals are deliberately omitted from `background_commands` (`mj-worker/src/relay/background.rs:240-255`), so their synthetic fallback entry in `ToolsInFlight` can be the stall watchdog's only evidence of a healthy long command.
  Evidence: `mj-worker/src/relay/background.rs:240-255, 1034-1108` and `mj-core/src/activity.rs:944-957, 985-1007`. Hosted terminal entries therefore stay in `ToolsInFlight` under an explicit `HostedTerminal` owner rather than being removed blindly.

- Observation: ACP schema v1.21.2 has only four `ToolCallStatus` variants (`Pending`, `InProgress`, `Completed`, `Failed`), durable retention compaction (`mj-transcript/src/transcript.rs:115-176`) strips raw `_meta`, and adding a new typed field to relay observations changes v2 event digests (`mj-core/src/relay/snapshot/digest.rs:191-229`) on older readers. Meanwhile, SQLite `upsert_transcript_item` (`mj-store/src/database/state_io.rs:511-529, 583-617`) automatically sets `settled = 1` whenever a tool row is upserted with a terminal status, and `load_materialized_projection` already reloads unsettled tool rows across restarts.
  Evidence: Direct inspection of `mj-store/src/database/state_io.rs`, `mj-store/src/database/materialized.rs:1500-1590`, and `mj-transcript/src/projection.rs`.

- Observation: Pinned Codex ACP v1.15.0 `CodexAcpServer.cancel` accepts only a session ID and interrupts whatever turn the bridge currently holds (`src/CodexAcpServer.ts:5746-5777`), so worker-side autonomous ordinal matching cannot guarantee exact turn targeting for Codex native goals.
  Evidence: Direct inspection of `CodexAcpServer.ts:5746-5777` and `mj-daemon/src/daemon/verdicts.rs:115-145`.

- Observation: In `mj-store/src/database/committed.rs:46-109`, `observe_connection` gates its `UPDATE` column filter on `kind == "turn"` using column names specific to `materialized_sessions`. Adding `materialized_queued_prompts` to the `"turn"` lane requires scoping that `UPDATE` filter to `table == "materialized_sessions"`.
  Evidence: Direct inspection of `mj-store/src/database/committed.rs:46-109`.

## Decision Log

- Decision: Close #1029 (stale), #1032 (completed; speculative second file-write latch and per-profile stall schema wontfixed), #1426 and #1436 (mitigated by GLiDE handback check, liveness hold, and prompt/bounce rules), #971 (composer/Esc shipped; remaining `TurnControls` targeting folded into #1599), and wontfix #1593 (no accepted payload or trigger provenance for autonomous turns) and #1610 (no observed Grok ACP autonomous parent wake).
  Rationale: Focus effort on real structural defects and avoid speculative machinery or model-compliance heuristics.
  Date/Author: 2026-11-23 / User + Coordinator

- Decision: Fix #1592 by removing Mjolnir's implicit startup goal auto-resume and sending AIR `resumePolicy=pause` on Codex stored session reopen (`loadSession` and `resumeSession`), while keeping explicit recovery forms, `goal::resolve`, saved-answer replay (`mjGoalDecision`), explicit slash `GoalControl`, quota Wait (`quota-goal-resume-*`), and quota Move (`ResumeGoal`).
  Rationale: The previous ~828-line relay-admitted auto-resume attempt was reverted in `70b45ac58` for excessive complexity. Pausing active goals on reopen and letting explicit controls resume them eliminates the race by construction and removes code.
  Date/Author: 2026-11-23 / User + Coordinator

- Decision: For #1576, delete the obsolete 16 MiB `RELAY_SNAPSHOT_BYTE_BUDGET` cap (a speculative local-file cap from protocol 9 before terminal command compaction and `RELAY_RETRY_ID_RETENTION = 512`), collapse 3x-to-1x live prompt payloads at `RELAY_STATE_VERSION = 30`, map staged admission overflow of the real 2 MiB `RELAY_STATE_BYTE_BUDGET` wire limit to non-retryable `InvalidState` -> HTTP 409 `Refusal::precondition`, and fix the two small prompt pre-validation gaps (startup prompt budget check and pre-write inline-image validation). Skip Linux `E2BIG` shell-arg handling and unverified Kimi `wire.jsonl` 1 MiB line handling without opening new tickets.
  Rationale: `relay-state.json` never crosses the wire; completed commands are already compacted to 512 digest-only receipts, individual commands are capped at 1 MiB (`RELAY_COMMAND_BYTE_BUDGET`), and the wire operational state is already capped at 2 MiB (`RELAY_STATE_BYTE_BUDGET`). Removing the 16 MiB cap eliminates per-event whole-snapshot serialization, removes the internal-event overflow footgun, and follows repository policy on removing caps whose reason no longer holds.
  Date/Author: 2026-11-23 / User + Coordinator

- Decision: For #1311, settle open foreground tool rows to `Failed` at committed turn/restart/clear/close boundaries going forward only, with no historical store backfill and no new relay observation fields or SQL schema changes.
  Rationale: Going-forward projection settlement solves the live and transcript defect cleanly without changing event digests or adding one-off migration machinery for historical transcripts.
  Date/Author: 2026-11-23 / User + Coordinator

- Decision: Keep #1643's 3-line `CheckingQuota` fix in `mj-core/src/relay/snapshot.rs` untouched rather than refactoring daemon `ManagedSessionView` and `wait.rs`/`verdicts.rs` composition; take only the local low-risk cleanups (delete dead `ToolsInFlight::open`, drop unused counts in `turn_stall_facts`, snapshot tools once in `operational_state()`).
  Rationale: `CheckingQuota` is display-only and #1643 already works cleanly; threading daemon quota state through views adds review surface out of proportion to benefit.
  Date/Author: 2026-11-23 / User + Coordinator

- Decision: Implement Rust changes across five independently validated and pushed milestones; leave Go port parity (`go/`) to the dedicated Go sync pass rather than mixing Go rewrites into these Rust checkpoints.
  Rationale: Repository history (`23661f95c` and recent `go port: sync ...` commits) maintains `go/` via dedicated batch syncs. Each Rust milestone bumps `RELAY_STATE_VERSION` cleanly when needed (30 in Milestone 3, 31 in Milestone 4 if Milestone 3 is pushed before Milestone 4).
  Date/Author: 2026-11-23 / Coordinator

## Outcomes & Retrospective

To be updated at each milestone checkpoint and upon completion of #1653.

## Context and Orientation

Mjolnir splits responsibilities between a control-plane daemon (`mj-daemon`, `mj-controller`, `mj-server`, `mj-store`, `mj-transcript`, `mj-session-runtime`) and one data-plane worker process per live session (`mj-worker`, `mj-worker-bin`), sharing wire and state types in `mj-core`.

The worker's `DurableRelay` (`mj-worker/src/relay.rs`, with submodules `commands.rs`, `journal.rs`, `background.rs`, `checkpoint_hold.rs`, `requests.rs`, `replay.rs`, `verdict.rs`) owns the durable append-only journal (`relay-journal/`), the private folded snapshot (`relay-state.json`, currently `RELAY_STATE_VERSION = 29`), and the wire protocol (`RELAY_PROTOCOL_VERSION = 85`, defined in `mj-core/src/relay.rs`). Events are folded identically by worker and daemon via the pure reducer `apply_relay_event` in `mj-core/src/relay/snapshot/apply.rs`.

The worker drives an Agent Client Protocol (ACP) bridge subprocess in `mj-worker/src/acp/{session,drive,launch,goal}.rs` via the runtime dispatch loop in `mj-worker/src/worker_runtime/unix/dispatch.rs`. Process-local open foreground tools are tracked outside the journal in `ToolsInFlight` (`mj-core/src/activity/tools.rs`), which feeds both `mj_core::activity::classify` (`mj-core/src/activity.rs`) and the opt-in turn stall watchdog (`mj-worker/src/acp/drive.rs`).

On the daemon side, relay events are projected into materialized transcript rows and turn state by `mj-transcript/src/projection.rs` (with `projection/observation.rs`, `projection/session_update.rs`, `projection/terminals.rs`) and persisted atomically to SQLite by `mj-store/src/database/{materialized,state_io,committed}.rs`. Compact committed summaries (`CommittedTurn` in `committed.rs`) notify waiting HTTP callers (`mj-server/src/server/api/wait.rs` using `mj-session-runtime/src/wait_policy.rs`) without reading full transcripts. User-facing turn controls (`TurnControls` in `mj-core/src/relay/snapshot/turn_control.rs`) are published on `RelayOperationalState` and turned into API capabilities and verdicts in `mj-daemon/src/daemon/verdicts.rs`, consumed by `mj-chat/src/chat/turn_control.rs` and `mj-server/src/web/viewer.js`.

All Cargo invocations in this workspace run via `/home/jonathan/.local/share/mbx/bin/cargo` (the `mbx` shim). Every `cargo test` run must execute outside the restricted sandbox because tests use loopback TCP and Unix sockets.

## Plan of Work

The work proceeds in five small, independently reviewable and pushable milestones. Each milestone validates only the affected crates and tests, runs `cargo clippy -p <crates> --all-targets -- -D warnings`, commits on `master`, passes `request_review`, and pushes to `origin/master`.

### Milestone 1: Remove implicit Codex startup goal auto-resume (#1592) and fix worker-restarted wording (#1303)

At worker startup in `mj-worker/src/acp/session.rs`, after `SessionConfigured` is published, `goal::recover` currently checks for an active idle native goal and directly issues a `_session/goal` Resume RPC over ACP without consulting the relay command queue. Simultaneously, `mj-worker/src/acp/launch.rs` only sends AIR `resumePolicy = "pause"` when an explicit recovery form is requested (`GoalRecoveryContext::asking()`), leaving ordinary stored Codex reopens under `"preserve"` where the bridge can start autonomous goal execution before Mjolnir dispatches queued prompts.

In `mj-worker/src/acp/launch.rs`, update the stored-session request metadata builder (`session_request_meta` or a dedicated stored-reopen path used by `loadSession` and `resumeSession`, while keeping fresh `session/new` unaffected) so Codex stored-session reopens always supply `_meta.jetbrains.air.goal.resumePolicy = "pause"`, independent of whether a recovery form was requested or what is in the queue. Preserve all existing AIR, Codex, and MCP metadata fields. In `mj-worker/src/acp/session.rs`, ensure the negotiated pause-capability check covers affected stored reopens without breaking adapters that advertise no goal capability.

In `mj-worker/src/acp/goal.rs`, delete the implicit startup fallback branch (lines 425-443) that calls `control(..., Resume)` for an active idle goal without a user decision or explicit form, along with any now-unused failure-explanation plumbing created only for that fallback. Keep explicit recovery forms (`goal_resume_request` and pending model choice), interactive `goal::resolve`, crash replay of an already-saved explicit user decision (`mjGoalDecision`), explicit slash `GoalControl`, quota Wait (`quota-goal-resume-*`), and quota Move (`ResumeGoal`).

In `mj-worker/src/relay/journal.rs` around line 1382, change `"relay restarted while the ACP command was in flight; it was not replayed"` to `"worker restarted while the ACP command was in flight; it was not replayed"`, matching the adjacent user-shell interruption wording on line 1368.

Update unit tests in `mj-worker/src/acp/goal.rs` (`goal_recovery_resumes_only_when_needed_and_explicit_open_asks` and the rejection/timeout/FIFO cases moved to explicit resume entry points) and add a reopen test verifying that both `session/load` and `session/resume` send AIR `pause` and emit zero startup Resume RPCs when reopening an active goal so queued prompts run first. Verify existing quota Wait and #1498 quota Move regressions remain green. When merged and pushed, close #1592 and #1303.

### Milestone 2: Share committed queue IDs in durable targeted `mj wait --turn` (#1599 Gap 2)

When a daemon restarts after committing `CommandInterrupted { continuation_id: Some(...) }` alongside the queued continuation in `materialized_queued_prompts`, a targeted `mj wait --turn` issued before the session actor attaches a worker snapshot falls back to durable `TurnState`, which currently holds only `(execution, active_turn, last_turn_outcome)`. Because `queued_command_ids` is empty, `wait_policy::turn_end` cannot see that `MaterializedTurnOutcome::is_resubmitted_by` matches the queued continuation and immediately returns a false `error` result.

In `mj-store/src/database/materialized.rs`, add `read_materialized_queued_command_ids(conn: &Connection, session_id: &str) -> Result<Vec<String>>` executing `SELECT command_id FROM materialized_queued_prompts WHERE session_id = ?1 ORDER BY ordinal`, and add a transactional loader alongside `load_materialized_turn_outcome` that reads `(execution, active_turn, last_turn_outcome, queued_command_ids)` inside one deferred read transaction (keeping the existing three-field helper intact for unrelated callers).

In `mj-store/src/database/committed.rs`, add `pub queued_command_ids: Vec<String>` to `CommittedTurn` and populate it inside `CommittedTurn::read`. In `observe_connection`, register `("materialized_queued_prompts", "turn", "session_id")` on the existing `"turn"` notification lane, and restrict the `UPDATE` column-filter predicate to `table == "materialized_sessions"` so SQLite trigger creation succeeds on `materialized_queued_prompts` (which does not carry `materialized_sessions` columns). Because `CommittedTurn` equality drives `wait_revisions`, queue-only inserts and deletes will wake waiting callers automatically without a second wake mechanism.

In `mj-server/src/server/api/types.rs`, add `pub queued_command_ids: Vec<String>` to internal `TurnState` (no HTTP wire change). Populate it in both cached and cold-read branches of `mj-daemon/src/server_runtime/api.rs`. In `mj-session-runtime/src/wait_policy.rs`, factor one helper method on `WaitObservation` that populates `execution`, `active_turn`, `last_turn_outcome`, and `queued_command_ids`, and call it from both `observe_worker_turn` and the durable branch of `mj-server/src/server/api/wait.rs`.

Extend `compact_turn_publications_are_committed_and_session_specific` in `mj-store/src/database/committed.rs` to test isolated queue-only insert/delete revision bumps and cold reopen, and extend `a_wait_on_a_turn_follows_the_workers_retry_of_it` in `mj-server/src/server/api/tests.rs` to cover durable-only reads after an interruption-plus-continuation commit (pending until continuation completes or cancels, while a plain interruption without continuation still returns error).

### Milestone 3: Delete 16 MiB snapshot cap, deduplicate snapshot payloads at state rev 30, refuse wire overflow cleanly, and fix prompt pre-validation (#1576)

Today, `RelaySnapshot` stores every live queued or active prompt payload three times (`HandledRelayCommand.command`, `RelayDispatchRecord.command`, and `StoredQueuedRelayCommand.payload` / `StoredActiveRelayPrompt.prompt`), and enforces a speculative 16 MiB private-file ceiling (`RELAY_SNAPSHOT_BYTE_BUDGET` from protocol 9, before terminal compaction existed) by serializing the entire private snapshot on every state-moving event (`mj-worker/src/relay/journal.rs:753`). Worse, any staged budget overflow (`RELAY_SNAPSHOT_BYTE_BUDGET` or the real 2 MiB wire limit `RELAY_STATE_BYTE_BUDGET`) bubbles out of `DurableRelay::submit_command_inner` as an unclassified error, which `requests.rs` maps to `RelayErrorCode::Internal { retryable: true }`, causing `mj-session-runtime`'s actor to drop the worker connection, reconnect, retry once, and surface a 500/internal error.

First, delete `RELAY_SNAPSHOT_BYTE_BUDGET` from `mj-core/src/relay.rs` and remove its four private-file checks: the per-event full-snapshot serialization check in `mj-worker/src/relay/journal.rs:753`, the `persist_relay_snapshot` check in `journal.rs:87`, the `4 * RELAY_SNAPSHOT_BYTE_BUDGET` legacy read cap in `mj-worker/src/relay.rs:755-762`, and the `pending_prompt_context` check in `mj-worker/src/relay/requests.rs:443-447`. Keep all real wire/frame budgets (`MAX_FRAME_BYTES`, `RELAY_REPLAY_BYTE_BUDGET`, `RELAY_COMMAND_BYTE_BUDGET`, `RELAY_EVENT_BYTE_BUDGET`, and `RELAY_STATE_BYTE_BUDGET = 2 MiB` for the payload-free operational state sent on `Attach`).

Second, make `RelaySnapshot.dispatches[command_id].command` the sole live payload owner and advance `RELAY_STATE_VERSION` from 29 to 30 in `mj-core/src/relay.rs` (breaking for older writers/readers; `RELAY_PROTOCOL_VERSION` stays unchanged). In `mj-core/src/relay/snapshot.rs` and `mj-core/src/relay/snapshot/apply.rs`:
- Replace `StoredQueuedRelayCommand` and `StoredActiveRelayPrompt` payload-bearing fields with the existing payload-free metadata shapes (`QueuedRelayPrompt` and `ActiveRelayPrompt`), letting Serde ignore legacy v29 flattened `prompt`/`key`/`value` keys on nested entries while preserving strict top-level snapshot fields.
- Remove `command: Option<RelayCommand>` from `HandledRelayCommand` while keeping acceptance/terminal ordinals, outcome, failure, and compact terminal identity/digest fields. Add a private deserialize-only helper on `HandledRelayCommand` that accepts legacy v29 `command` and derives missing terminal digest/identity fields for terminal receipts before discarding `command`.
- Resolve live idempotency (`mj-worker/src/relay/commands.rs`), steering blocks (`apply.rs`, `commands.rs`), `RetryAssessmentStarted` supersession (`apply.rs`, iterating map entries with command IDs rather than values alone), seed restore (`mj-worker/src/relay.rs`), and legacy unqueued-config adoption (`mj-worker/src/relay.rs`) against `dispatches[command_id].command` using `RelayCommand::prompt_blocks` (keeping `prompt_blocks_for_command` only where dispatch prepends the restart note).
- In `DurableRelay::open` (`mj-worker/src/relay.rs`), persist the migrated v30 snapshot when an older revision was decoded, even if no new events arrived.

Third, turn staged wire-budget overflow at command admission (`RELAY_STATE_BYTE_BUDGET` on `next_snapshot.operational_state()` or `RELAY_EVENT_BYTE_BUDGET`) into a clean non-retryable refusal. In `mj-worker/src/relay/journal.rs`, `commands.rs`, and `requests.rs`, detect staged pre-commit `RelayBudgetExceeded` during `Submit`/`SubmitFront` admission and return outer `Ok(Err(RelayProtocolError { code: RelayErrorCode::InvalidState, retryable: false, message }))` with an actionable message explaining that the session queue is full and to wait for queued prompts to run or remove queued input. Do not remap unrelated post-commit or transport failures. Because `actor.rs` and `deliver_submit` treat non-retryable `InvalidState` as a final rejection, the worker connection and any held checkpoint barrier stay intact. At both daemon user-prompt submission boundaries—`mj-daemon/src/server_runtime/actions.rs` (viewer `/api/actions`) and `mj-daemon/src/server_runtime/api.rs` (public `prompt_with_id`)—convert definite `SubmitRefusalCode(InvalidState)` (`mj_client::session::Refused`) into `mj_core::refusal::Refusal::precondition` carrying the exact worker message so both endpoints return HTTP 409 with the worker text.

Fourth, fix the two approved pre-validation gaps from #1576:
- In `mj-daemon/src/daemon/actions.rs` (`QueueStartupPrompt`), validate the serialized relay command budget against the eventual `RelayCommand::SubmitPrompt` encoding before persisting `StartupStep::Prompt`.
- In `mj-server/src/server/validation.rs` (`decode_prompt_images_off_task` and prompt validation), compute normalized image attachment refs and validate the normalized prompt's serialized size before writing image blobs to the attachment store.

Update the snapshot tests (`mj-worker/src/relay/journal/tests.rs`, `snapshot_tests.rs`, `continuation_tests.rs`, `tests.rs`): remove obsolete 16 MiB private-snapshot fill tests while retaining `restart_continuation_reports_operational_queue_exhaustion` (`RELAY_STATE_BYTE_BUDGET`), add a literal v29-to-v30 migration test covering active prompt, queued prompt, queued config, uncompacted terminal receipt, compacted receipt, unpersisted tail, and idempotent replay (plus verifying that a queue of >16 MiB of large prompts now succeeds cleanly without hitting any private snapshot ceiling!), add an admission-overflow test against `RELAY_STATE_BYTE_BUDGET` proving the worker connection is reused and state stays intact, add HTTP 409 refusal tests for both viewer `/api/actions` and public prompt API, and add targeted regressions for startup-prompt budget rejection and non-orphaned images on prompt-size refusal. When merged and pushed, close #1576.

### Milestone 4: Worker-owned exact cancel targets at protocol 86 (#1599 Gap 1 + #971 targeting tail)

Today `TurnControls` (`mj-worker/src/relay/commands.rs:88-124`) returns early when there is no active prompt, omitting both queued restart continuations and tracked autonomous turns. As a result, `mj-daemon/src/daemon/verdicts.rs` recomputes a separate `running` predicate that misses queued restart continuations (causing `mj interrupt-turn` to fail with HTTP 409) and forces TUI/web fallbacks to emit untargeted raw `CancelTurn` commands.

Advance `RELAY_PROTOCOL_VERSION` from 85 to 86 (`RELAY_CANCEL_TURN_TARGET_PROTOCOL`) in `mj-core/src/relay.rs`, and advance `RELAY_STATE_VERSION` from 30 to 31 (since Milestone 3 is pushed as state 30; the new durable command variant is breaking for older replay). In `mj-core/src/relay/snapshot/turn_control.rs` and `snapshot.rs`, define:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum TurnInterruptTarget {
        ActivePrompt { command_id: String },
        QueuedRestart { command_id: String },
        Autonomous { start_ordinal: u64 },
    }

Add `RelayCommand::CancelTurnTarget { target: TurnInterruptTarget }` gated at protocol 86 in `RelayCommand::minimum_protocol`, preserving protocol-17 `CancelTurnFor` untouched for compatibility. In `mj-core/src/relay/snapshot/apply.rs`, wire `CancelTurnTarget` through the same `cancelling_prompt_id` and `cancelling_harness_turn` state used by existing cancel paths.

In `mj-worker/src/relay/commands.rs`, extract one worker-owned resolver `resolve_turn_interrupt_target(&self)` that selects the exact cancellable target under relay ownership: active prompt first, then queued restart continuation, then tracked autonomous harness turn for policies whose adapter supports safe cancel (Claude and Antigravity; explicitly omit Codex native goals because pinned `CodexAcpServer.cancel` cannot target a specific native turn). Use this single resolver in both `turn_controls()` publication (now published even with no active prompt so an empty `interrupt` is authoritative at protocol >= 86) and serialized `validate_turn_control` / cancel admission. For `QueuedRestart { command_id }`, accept cancellation both while the restart continuation is still queued (settling relay-locally without ACP readiness) and if that exact `command_id` has just promoted to `active_prompt` between render and click, rejecting if that `command_id` is gone so no unrelated prompt can ever be hit. Update worker dispatch (`mj-worker/src/worker_runtime/unix/dispatch.rs`) and ACP cancel routing (`mj-worker/src/acp/{session,drive}.rs`) so autonomous `CancelTurnTarget` carries and revalidates its `start_ordinal` before sending ACP cancel.

In `mj-daemon/src/daemon/verdicts.rs`, for workers at protocol >= 86, derive `capabilities.interrupt_turn`, `turn_interrupt_command`, and `InterruptTargetKey` directly from worker-published `TurnControls` (layered with daemon lifecycle, form, and Move gates), keeping the legacy `running`/raw-`CancelTurn` fallback strictly for workers at protocol < 86. Allowlist `CancelTurnTarget` in `mj-server/src/server/actions.rs`, `mj-daemon/src/server_runtime/actions.rs`, and `mj-session-runtime/src/session_manager/move_ownership.rs`. Update `mj-chat/src/chat/{turn_control,status,active/dispatch,active/render}.rs` and `mj-server/src/web/viewer.js` (duplicate-menu check and error-target capture alongside `cancel_turn_for`) to consume published `CancelTurnTarget` actions cleanly.

Validate with worker tests (queued restart cancel while ACP unready + surviving successor prompt, queued-to-promoted exact cancel, Claude/Antigravity autonomous exact cancel, Codex goal withheld refusal, legacy protocol <= 85 fallback), daemon/HTTP tests (`mj interrupt-turn` succeeding on queued restart continuation and returning 409 when nothing cancellable exists, overlapping interrupt-all dedupe), and TUI/web tests. When merged and pushed, close #1599.

### Milestone 5: Foreground tool ownership, going-forward transcript settlement, and local activity cleanup (#1623 + #1311)

Two bugs share the same missing boundary between live/durable tool calls and turn ownership. First, `ToolsInFlight` (`mj-core/src/activity/tools.rs`) blindly inserts a new live entry on `SessionUpdate::ToolCallUpdate(Pending | InProgress)`, and `DurableRelay::record_session_update` updates it before journal append commits while turn endings blanket-clear the entire map (#1623). Second, durable transcript projection (`mj-transcript/src/projection/observation.rs`) closes prose and thought streams on turn/restart/clear boundaries but never settles open foreground tool rows (#1311).

In `mj-core/src/activity/tools.rs`:
- Introduce a private `ForegroundOwner` enum (`Prompt(String)`, `HarnessTurn(u64)`, `HostedTerminal(String)`, `Unowned`) stored on each internal `Entry` (not serialized on wire `InFlightToolCall`), plus a per-connection/per-generation `retired_ids: HashSet<String>` cleared on `clear()`.
- Enforce that `SessionUpdate::ToolCallUpdate` with `Pending | InProgress` ONLY refines an already-live entry and never inserts a new entry. Enforce that a full `SessionUpdate::ToolCall` for an ID already in `retired_ids` cannot reopen a retired tool call on the same connection/generation.
- Provide `observe_for_owner(&self, update, started_at_ms, owner)`, `adopt_unowned(&self, owner)` (so a genuine Antigravity card arriving immediately before a `PostToolBatch`/turn open gets adopted by that turn and retired with it), `retire_owner(&self, owner)` (which removes matching entries and records their IDs in `retired_ids`), and `retire_terminal(&self, terminal_id)`.
- Delete unused `ToolsInFlight::open` and migrate the two watchdog fixtures in `mj-worker/src/acp/tests.rs` to observe a normal `SessionUpdate::ToolCall`.

In `mj-worker/src/relay.rs`, `journal.rs`, `background.rs`, `commands.rs`, and `checkpoint_hold.rs`:
- Determine the `ForegroundOwner` for incoming ACP `ToolCall` observations from active prompt or active autonomous harness turn (`Unowned` only for markerless full `ToolCall`, preserving `an_unsettled_tool_call_is_foreground_work_even_without_a_turn_marker`, and adopted if an autonomous turn opens right after). Use `HostedTerminal(terminal_id)` for worker-synthesized hosted terminal fallback cards so the stall watchdog keeps seeing long prompt-owned terminals while hosted terminal close/retirement clears them cleanly.
- Apply tool deltas and exact owner retirements inside `journal.rs`'s append critical section right after journal commit and before `refresh_idle_clock` / baseline publication, deriving retired owners from the committed durable transition (old vs new active prompt / autonomous harness turn, plus `SessionRestarted`/`ContextCleared`/`Closing`/`Closed`/bridge retirement). Rolled-back appends leave `ToolsInFlight` untouched.
- In `DurableRelay::operational_state()`, snapshot `foreground_tools` once instead of twice.
- In `mj-worker/src/acp/drive.rs`, delete the two unused queue/background count reads from `turn_stall_facts`.

In `mj-transcript/src/projection.rs`, `projection/session_update.rs`, and `projection/observation.rs`:
- Extend `ProjectionIndex` to track open non-fallback foreground tool item IDs (excluding worker-synthesized hosted-terminal fallback cards owned by `projection/terminals.rs`), indexed by their turn/owner and rebuilt from unsettled non-fallback tool rows in the loaded projection window across daemon restarts (leveraging SQLite's existing unsettled-row reload in `mj-store/src/database/materialized.rs:1500-1590`, requiring zero schema or digest changes).
- At committed retirement boundaries (`CommandCompleted` when no native goal keeps running, `CommandInterrupted` / `CommandRejected` for a matching started turn, `HarnessTurnSettled` when no prompt continues, `SessionRestarted` clearing running projection, `ContextCleared`, and `Closed`), clone each still-open indexed foreground tool row belonging to the ending turn, update its raw ACP status from `Pending | InProgress` to `ToolCallStatus::Failed` (never `Completed`), bump `last_changed_at_ms` monotonically, preserve `created_at_ms` and position, and emit those upserts inside the same `MaterializedSessionMutation`. SQLite automatically updates `settled = 1`.
- In `projection/session_update.rs`, prevent a stale `Pending | InProgress` update or re-announced full card from reverting an already-terminal tool row back to nonterminal, while still allowing a genuine late terminal update (`Completed`/`Failed`) with output details.

Validate with targeted tests covering late nonterminal partial updates, late full re-announcements of retired IDs, overlapping prompt + autonomous turn retirement, Antigravity card-before-hook adoption, rolled-back journal append, hosted terminal watchdog coverage, and transcript projection + SQLite `settled = 1` across interrupt/complete/restart/clear boundaries while preserving detached background tasks and historical unowned rows. When merged and pushed, close #1623 and #1311, and close epic #1653.

## Concrete Steps

All commands run from the clone root `/home/jonathan/Projects/mjolnir/.mj/clones/848be2800278d081f323ff76689c4b5e` using the `mbx`-backed `cargo` binary, with `cargo test` executed outside the restricted sandbox.

Milestone 1 validation commands:

    cargo test -p mj-worker acp::goal
    cargo test -p mj-worker relay::continuation_tests
    cargo test -p mj-worker restart_continuation_preserves_ordinary_user_work_alongside_a_native_goal
    cargo test -p mj-daemon quota_move_preserves_idle_active_goal_at_both_admission_gates
    cargo clippy -p mj-worker --all-targets -- -D warnings

Milestone 2 validation commands:

    cargo test -p mj-store compact_turn_publications_are_committed_and_session_specific
    cargo test -p mj-session-runtime wait_policy
    cargo test -p mj-server a_wait_on_a_turn_follows_the_workers_retry_of_it
    cargo clippy -p mj-store -p mj-session-runtime -p mj-daemon -p mj-server --all-targets -- -D warnings

Milestone 3 validation commands:

    cargo test -p mj-core relay::snapshot
    cargo test -p mj-worker relay::journal::tests
    cargo test -p mj-worker relay::snapshot_tests
    cargo test -p mj-worker relay::continuation_tests
    cargo test -p mj-session-runtime a_final_rejection_tells_the_submitter_only_the_relays_reason
    cargo test -p mj-daemon correlated_prompt_reply_waits_for_relay_submission
    cargo test -p mj-server every_prompt_the_relay_takes_reaches_the_validator
    cargo clippy -p mj-core -p mj-worker -p mj-session-runtime -p mj-daemon -p mj-server --all-targets -- -D warnings

Milestone 4 validation commands:

    cargo test -p mj-core relay::snapshot
    cargo test -p mj-worker relay::tests
    cargo test -p mj-daemon daemon::verdicts
    cargo test -p mj-server interrupt
    cargo test -p mj-chat turn_control
    cargo clippy -p mj-core -p mj-worker -p mj-session-runtime -p mj-daemon -p mj-server -p mj-chat --all-targets -- -D warnings

Milestone 5 validation commands:

    cargo test -p mj-core activity
    cargo test -p mj-worker relay::tests
    cargo test -p mj-worker acp::tests
    cargo test -p mj-transcript projection
    cargo test -p mj-store materialized
    cargo clippy -p mj-core -p mj-worker -p mj-transcript -p mj-store --all-targets -- -D warnings

After each milestone passes its targeted tests and clippy, stage only changed files, commit directly on `master`, run `request_review`, address or override findings per repository policy, and push to `origin/master`.

## Validation and Acceptance

Each milestone must prove its user-visible behavior with new or extended targeted tests that fail before the change and pass after it, verify per-filter test counts are non-zero (preventing silent zero-match test filters), pass `cargo clippy --all-targets -- -D warnings` on every touched crate, and pass `request_review` before pushing.

- Milestone 1 acceptance: Reopening a Codex session with an active goal sends AIR `resumePolicy = "pause"` on both `session/load` and `session/resume`, emits zero implicit startup Resume RPCs, executes queued prompts ahead of any goal resume, preserves explicit recovery forms and quota Move/Wait behavior, and emits `"worker restarted while the ACP command was in flight; it was not replayed"` on worker restart.
- Milestone 2 acceptance: A durable-only `mj wait --turn` read after an interruption-plus-queued-continuation commit stays pending until the continuation finishes or cancels, wakes when only `materialized_queued_prompts` changes, and still returns an error for an interruption with no continuation.
- Milestone 3 acceptance: `RELAY_SNAPSHOT_BYTE_BUDGET` and its four private-file checks are deleted; loading a literal v29 `relay-state.json` upgrades cleanly to v30 with one stored copy of each live prompt body and preserved terminal digests/idempotency; queueing >16 MiB of large prompts succeeds without hitting any private snapshot ceiling; overflowing the real 2 MiB `RELAY_STATE_BYTE_BUDGET` wire limit at command admission returns non-retryable `InvalidState` without dropping the worker connection and surfaces HTTP 409 with the worker message on both viewer `/api/actions` and public prompt API; oversized startup prompts are rejected at queueing time and oversized inline-image prompts leave no newly written blobs.
- Milestone 4 acceptance: `mj interrupt-turn`, TUI `Esc`, and web Interrupt cancel a queued restart continuation (even while ACP is unready) and tracked Claude/Antigravity autonomous turns using worker-published `CancelTurnTarget`, refuse stale targets cleanly without hitting successor prompts, keep Codex native goal cancellation safely withheld, and preserve protocol <= 85 legacy-worker compatibility.
- Milestone 5 acceptance: Late `ToolCallUpdate(Pending | InProgress)` or full re-announcement of a retired tool ID cannot resurrect `ActivityState::Tool` after a turn ends; rolled-back appends leak no live tool; overlapping turns retire only their own tools; hosted terminals keep feeding the stall watchdog; and going-forward interrupted/ended foreground tool rows transition to `Failed` (`settled = 1` in SQLite) in the same projection mutation as the turn boundary without altering detached background tasks or reopening on late nonterminal updates.

## Idempotence and Recovery

Every milestone keeps existing on-disk state forward-compatible. Milestone 3 removes the obsolete 16 MiB private-file ceiling, reads legacy v29 snapshots without an arbitrary private read cap, and rewrites v30 atomically using the existing rename-based snapshot persistence; older workers refuse v30 cleanly instead of silently downgrading. Milestone 4 gates `CancelTurnTarget` behind `RELAY_PROTOCOL_VERSION = 86` so daemons talking to older workers keep the pre-86 path. Milestone 5 touches only going-forward projections and process-local tool state, leaving historical transcript rows intact. All tests run in isolated temp directories and never touch the host's default Mjolnir instance.

## Artifacts and Notes

Read-only reconnaissance reports backing this ExecPlan remain preserved under `.mj/agents/`:
- Codebase map: `.mj/agents/9c60b78e08c57fc9aa58d579d0070a70/codebase-map.md`
- Activity & transcript analysis (#1623, #1311, #1032, #1643): `.mj/agents/70ec124f54d3d5307aad00459096db20/activity-transcript-analysis.md` and `plan-recon-1623-1311.md`
- Continuations, goal recovery & cancel analysis (#1599, #1593, #1592, #971): `.mj/agents/844af6ea72fb8498126a83d27dc2e120/continuations-cancel-analysis.md` and `plan-recon-1592-1599.md`
- Snapshot state, Grok, Muse & signals analysis (#1610, #1576, #1029, #1303): `.mj/agents/7ac42b6b7ca8160064128329e17de025/grok-snapshot-muse-signals.md` and `plan-recon-1576.md`
- Handbacks & dead-code audit (#1426, #1436): `.mj/agents/fd7f83ece3b9b8c046f956f63dc1340b/handbacks-and-deadcode.md`

## Interfaces and Dependencies

Key cross-crate interface changes across the five milestones:

1. In `mj-store/src/database/materialized.rs` and `committed.rs` (Milestone 2):

       pub fn read_materialized_queued_command_ids(
           conn: &Connection,
           session_id: &str,
       ) -> Result<Vec<String>>;

   `CommittedTurn` and `mj_server::server::api::types::TurnState` both gain `pub queued_command_ids: Vec<String>`.

2. In `mj-core/src/relay.rs` and `mj-core/src/relay/snapshot.rs` (Milestone 3):
   - `RELAY_STATE_VERSION = 30` (breaking for older writers/readers; v29 snapshots migrate on load).
   - `RelaySnapshot::queued_prompts: Vec<QueuedRelayPrompt>` and `RelaySnapshot::active_prompt: Option<ActiveRelayPrompt>` drop duplicated payloads; `HandledRelayCommand` drops `command: Option<RelayCommand>` and decodes legacy v29 `command` only to derive missing terminal identity/digest.

3. In `mj-core/src/relay.rs` and `mj-core/src/relay/snapshot/turn_control.rs` (Milestone 4):
   - `RELAY_PROTOCOL_VERSION = 86` (`RELAY_CANCEL_TURN_TARGET_PROTOCOL`), `RELAY_STATE_VERSION = 31` (if pushed after Milestone 3).
   - `TurnInterruptTarget` (`ActivePrompt { command_id }`, `QueuedRestart { command_id }`, `Autonomous { start_ordinal }`) and `RelayCommand::CancelTurnTarget { target: TurnInterruptTarget }`.

4. In `mj-core/src/activity/tools.rs` (Milestone 5):
   - Delete `ToolsInFlight::open`.
   - Internal `ForegroundOwner` (`Prompt(String)`, `HarnessTurn(u64)`, `HostedTerminal(String)`, `Unowned`) and per-generation `retired_ids`, exposed through owner-scoped `observe_for_owner`, `adopt_unowned`, `retire_owner`, and `retire_terminal` without altering wire `InFlightToolCall`.
