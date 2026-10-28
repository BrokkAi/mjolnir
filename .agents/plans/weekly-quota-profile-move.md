# Move a session to another profile when its weekly quota runs out

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.


## Purpose / Big Picture

Today, when a session's turn stops because the subscription quota is used up, the daemon schedules an automatic continuation one minute after the quota resets, provided the reset is at most 16 hours away. A five-hour window resets soon, so this works. A weekly window usually resets days later, so the session just records a notice and stops, even when the user has another enabled profile of the same harness that offers the same model and still has quota.

After this change, when the stop is caused by a weekly (or longer) window and the session cannot continue for more than five hours, the daemon moves the session in place (same target, same harness, same native conversation, only the profile changes) to the best other profile that offers the session's model, and continues the interrupted work there. When the stop is the five-hour window, or no suitable profile exists, behaviour stays as it is today. To see it, run a session on a profile whose weekly window is exhausted while another profile of the same harness has quota: the source conversation shows a notice that the session is moving, the session moves to the other profile, a notice reports the switch, and the agent continues without user input.


## Terms

A quota window is one allowance a provider reports for a profile, represented by `QuotaWindow` in `mj-client/src/quota.rs` (label, remaining percentage, duration, optional model scope, reset time). Claude and Codex label the five-hour window `5H` and the weekly window `Week`; Claude also reports model-scoped weekly windows labelled like `Week (Fable)` with `model_scope` set and a duration of 604,800 seconds. A window is exhausted when `crate::quota::exhausted` says so (no quota left and not yet reset).

A quota recovery is the durable record `mj_core::continuation::QuotaRecovery` stored in the worker's relay snapshot by `RelayCommand::SetQuotaRecovery`. A recovery with `retry_at_ms: None` is an abstention: it marks the stopped turn as handled and shows its notice but schedules nothing.

A Move is the daemon lifecycle operation in `mj-controller/src/daemon/session_move.rs` and `mj-controller/src/controller/move_session.rs` that checkpoints a session and restores it under another profile and/or target. It is durable: its `MoveOperation` (in `mj-core/src/state/session_move.rs`) is saved in the `session_moves` table and resumes after a daemon restart. An in-place Move keeps the target and workspace and only replaces the harness launch configuration. Native continuity (`Config::native_continuity` in `mj-core/src/config.rs`) means the destination continues the same native conversation; it requires the same harness and model service.

The load-balancing order is the ranking new-session profile selection uses: `rank_candidates` in `mj-controller/src/server/api/subagents.rs` (quota pace score descending, exhausted below positive, unknown last, then profile id). A usage-priced profile (`CandidateQuota::UsagePriced`, an API-key profile such as Claude on localhost with a key) is billed per request.


## Design

The daemon continuation service (`mj-controller/src/daemon/continuation.rs` and `continuation/quota.rs`) stays the single owner of "what to do about this quota stop". Its existing order is classify, then `quota::prepare` (computes a `QuotaRecovery`), then `quota::schedule` (submits `SetQuotaRecovery`), then the 100 ms timer calls `quota::resume` when the deadline comes. This plan adds one branch to `prepare` and `schedule`; the timer and resume path are unchanged except for one clearing rule.

Decision in `prepare`. After it computes the windows and the reset as today, it decides whether the stop is a long-window stop. Only windows that apply to the session's model count (`crate::quota::window_applies`). The stop is a long-window stop when an applicable exhausted window is a weekly-or-longer window and the time until the session could continue (the `recovery_reset` result) is unknown or more than five hours away. As a second case, when no applicable window is exhausted but the provider's own explicit reset (from the turn diagnostic or the message text) is more than five hours away, it is also a long-window stop, because a five-hour window always resets within five hours. A stop whose exhausted windows are all five-hour windows, or whose reset is at most five hours away, keeps today's behaviour exactly. Weekly-or-longer classification lives in one place in `mj-client/src/quota.rs` as a public method on `QuotaWindow` that reuses the existing private `is_weekly_quota_window` label rule (week, weekly, 7d, month, monthly), also recognises the scoped form (label with a ` (scope)` suffix when `model_scope` is set) and a `duration_seconds` of at least seven days.

Destination choice. For a long-window stop, `prepare` asks for destination candidates: enabled profiles from the same candidate source new-session selection uses (`SubagentBackend::session_profile_candidates` on the `ApiBackend` that `RuntimeState::report_backend` holds), which already excludes refused logins and attaches discovered models, the last quota report and profile-health marks. A candidate qualifies when it is not the session's current profile, has the session's harness, has native continuity with the source profile, offers the session's model by exact id, has no unhealthy mark for that model, and its quota for that model is not exhausted (pace score not `Some(0)`). The session's model is the accepted model from the relay snapshot (`mj_core::acp::AcceptedSessionConfig::from_configuration(&operational.config, &operational.config_options).model`); with no known model there is no Move. Qualifying candidates are ordered by `rank_candidates` with no anchor and then stably partitioned so usage-priced candidates come last. The first candidate is the destination.

Move preparation happens in `prepare`, before anything is written, through a new daemon method that prepares an in-place profile-only Move and refuses unless the preparation is in place, keeps native continuity (`cross_harness == false`), is not active (needs no interruption acknowledgement), stops no sub-agents, and has no conversion. If the first candidate's preparation is refused, the next candidate is tried. The result of `prepare` becomes an enum: either today's wait (a `QuotaRecovery`), or a Move plan carrying the prepared Move request, an abstention `QuotaRecovery` whose notice says the session is moving and why, and the fallback `QuotaRecovery` that today's code would have produced (used if the Move cannot start). When a long-window stop finds no destination, the wait recovery's notice gains one sentence saying no other enabled profile of that harness with quota left offers the model.

Scheduling a Move plan. `schedule` keeps its existing guards (allowed, schedulable, same profile, unchanged evidence), then submits `SetQuotaRecovery` with the abstention, which marks the stopped turn handled so the service never prepares it again (this ends any retry loop if the Move later fails). It then starts the Move through a new daemon entry point that admits the Move lifecycle without waiting for it, and supervises completion in a spawned task that removes the finished lifecycle and logs a failure. If the Move cannot be admitted, `schedule` submits `SetQuotaRecovery` again with the fallback recovery, whose notice is extended with the reason the Move could not start. A daemon crash between the abstention and the Move admission leaves the session stopped with the "moving" notice and no Move; the window is the few milliseconds between two local calls, and the outcome is visible.

Timer clearing rule. The timer currently clears any pending recovery when the session is not `allowed` (a Move owns it, a lifecycle operation owns it, continuation is disabled) or its profile changed. A Move owns the session right after the abstention, and clearing the abstention then would let a failed Move restart the cycle. The timer now clears an abstention (no `retry_at_ms`) only when the session's profile no longer matches; records with a deadline keep today's rule.

Continuing on the destination. `MoveOperation` gains two optional fields (serde default, skipped when absent, so older daemons skip a newer in-flight row with a warning as they already do for `in_place` and `source_checkpoint_only`): `quota_stop_reason`, a clause saying why the session moved, and `quota_continuation`, the action the destination runs (`QuotaMoveContinuation::ResumeGoal` or `Prompt`). The continuation service chooses the action from the source snapshot with `goal_resumes_after_quota` (the predicate the timed goal resume uses) and passes both to the daemon-internal start path; they are never part of the wire request `MoveSessionRequest`. When they are present, the Move's queue admission (`admit_move_queue` in `move_session.rs`) first submits the stored action on the destination relay, then the queued prompts as today: a `GoalControl { Resume }`, or `RelayCommand::Prompt` with `mj_core::continuation::prompt_blocks()`. The command id starts with `quota-retry-` so `is_generated_prompt` treats it as generated, and includes the operation id so a resumed Move does not submit it twice. The completion notice then says the session switched profiles because of the quota stop and is continuing the interrupted work, instead of "The interrupted prompt was not replayed." A new prompt clears any quota recovery the destination relay still holds (`mj-core/src/relay/snapshot/apply.rs`).

Scope limits. Sub-agent sessions are excluded because the continuation service already excludes them (`allowed` in `continuation.rs`). The feature is part of automatic continuation and follows its existing on/off setting; there is no new setting. Sub-agent spawn ranking is unchanged.


## Plan of Work

Part A, continuation side, in `mj-client/src/quota.rs`, `mj-controller/src/quota/recovery.rs`, `mj-controller/src/daemon/continuation.rs` and `continuation/quota.rs`: the window classification method, the long-window decision as a pure function next to `recovery_reset`, candidate filtering and ordering, the `prepare` result enum and resolver type, the `schedule` Move branch, the timer clearing rule, and a new `Environment` closure that starts a prepared quota Move, so coordinator tests can supply a fake. `prepare` calls the daemon's Move preparation directly; coordinator tests replace the whole resolver.

Part B, Move side, in `mj-core/src/state/session_move.rs`, `mj-controller/src/daemon/session_move.rs` and `mj-controller/src/controller/move_session.rs`: the new `MoveOperation` field, the daemon methods `prepare_quota_move(session_id, profile_id) -> Result<MoveSessionRequest>` (with the refusals listed above, queue disposition `Start`, no interruption acknowledgement) and `start_quota_move(request, reason, continuation) -> Result<()>` (admit without waiting; a supervised task waits, removes the lifecycle and logs failures), threading the reason into the created `MoveOperation`, and the continuation submission and notice in queue admission.


## Validation

Tests follow the "Which tests to add" rules in `AGENTS.md`; these are tricky-condition and integration tests. Part A extends the coordinator tests in `mj-controller/src/daemon/continuation/tests.rs`: a weekly stop with a destination submits the abstention and starts the Move once; a five-hour stop schedules as today; a Move that cannot start submits the fallback deadline; the abstention survives while a Move owns the session and the turn is not prepared again. The decision boundary (exactly five hours, unknown reset, both windows exhausted, scoped weekly window, explicit-only reset) and the candidate order (usage-priced last, exhausted and unhealthy excluded) get one table-style test each. Part B extends the in-place Move regression in `mj-controller/src/controller/move_session/tests.rs` so a Move carrying the reason submits the continuation before queued prompts, exactly once across a resumed queue admission, with the new notice, and adds the stored-JSON compatibility check for the new field next to the existing ones. Run `cargo test -p mj-controller`, `-p mj-core`, `-p mj-client` and workspace `cargo clippy --all-targets -- -D warnings`.


## Progress

- [x] Part A: continuation decision, candidate choice, schedule branch, timer rule, tests (2026-10-28).
- [x] Part B: MoveOperation fields, quota Move daemon entry points, continuation at queue admission, tests (2026-10-28).
- [x] Integrated review and full validation: workspace clippy clean; mj-controller, mj-core, mj-client and mj-tui suites pass (2026-10-28). The one failure, `in_place_subagent_prompt_roster_uses_latest_child_state`, came from 73ba8f905 and was fixed separately.
- [ ] Commit, push, local install.


## Surprises & Discoveries

The worker rejects a `QuotaRecovery` whose `reset_at_ms` and `retry_at_ms` are not both set or both empty, so the abstention clears both; the reset time stays in its notice.

A native-continuity checkpoint keeps `operational.continuation`, including a quota recovery, across an in-place Move. A new prompt or an ordinary goal resume on the destination clears it (`mj-core/src/relay/snapshot/apply.rs`), so a surviving abstention is harmless.

The destination's relay goal snapshot is not guaranteed to be synchronized when queue admission first reads it: readiness (`native_session_is_ready`) checks the native session and `acp_ready`, not goal state, and the worker's goal recovery only warns when the adapter has not synchronized goal state (`mj-worker/src/acp/goal.rs`). The source snapshot at the quota stop is reliable instead: for Codex, `safe_to_replace` requires synchronized goal state before any Move is prepared.

An ordinary `GoalControl { Resume }` is acknowledged by the relay before the adapter runs it. If the adapter later rejects it, the rejection is reported in the conversation as a rejected command, but the Move has already completed and its notice says it is continuing. This is the same acknowledgement model the existing timed goal resume has.


## Decision Log

- 2026-10-27: The threshold compares the time until the session could continue (the latest reset of the exhausted windows) with five hours, so a weekly window that resets within five hours keeps today's wait. Unknown reset with an exhausted weekly window counts as more than five hours, because waiting without a known end is what the user wants to avoid.
- 2026-10-27: An explicit provider reset more than five hours away counts as a long-window stop even without window data, because a five-hour window cannot reset later than five hours from now. This covers failed quota refreshes.
- 2026-10-27: The destination must keep native continuity and the Move must be in place; the user specified a same-target, same-harness Move where only the profile changes.
- 2026-10-27: The abstention is written before the Move starts so a failed Move cannot be retried in a loop; the stopped turn is marked handled by the one record the service already uses for that.
- 2026-10-27: The continuation after the Move belongs to the durable Move operation, not to the continuation service, so it survives a daemon restart during the Move.
- 2026-10-28: Whether the destination resumes a goal or receives the continuation prompt is decided by the continuation service from the source snapshot that the quota stop produced, using the same predicate as the timed goal resume (`goal_resumes_after_quota` in `continuation.rs`), and stored on the Move as `quota_continuation` with the reason clause in `quota_stop_reason`. A first version decided on the destination; a retried queue admission could then choose differently under the same command id, and the destination's goal state may not be synchronized yet.
- 2026-10-28: The goal resume after a Move is an ordinary `GoalControl { Resume }` with a `quota-retry-` id, not the identity-guarded `quota-goal-resume-` command, because that guard requires a due deadline on the destination relay and the Move holds the session lifecycle during queue admission, so no automatic actor races it.
- 2026-10-28: The windows that decide the reset are filtered to those that apply to the session's model, as profile health already does. Before, an exhausted window scoped to another model could set the wait.
- 2026-10-28: A failed or cancelled automatic Move is reported with a daemon notice on the session, in addition to the log.


## Outcomes & Retrospective

Implemented as designed, with the goal decision moved to the source (see the Decision Log). Known limits: a daemon crash between writing the abstention and admitting the Move leaves the "switching" notice without a Move; an adapter that rejects the goal resume after the relay acknowledged it reports the rejection in the conversation after the Move's notice; sub-agent sessions are not moved because automatic continuation excludes them. Not yet exercised against a live provider quota stop.
