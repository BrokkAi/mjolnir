# Preserve cancellation during reachable-container Suspend recovery

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current. The user approved the reachable-target scope for #1677 and requires a simpler implementation. The earlier checked-in plan `.agents/plans/recover-suspend-without-original-worker.md` describes the shared Suspend machinery; the facts needed here are repeated below.

## Purpose / Big Picture


When a user cancels Suspend during the early retained-container check, the next daemon must honor that cancellation if the target remains reachable. The daemon is the controller process; a worker owns the agent and its durable journal. Today that early check can fail before the controller writes the worker's existing durable rollback instruction. The accepted Suspend stays Closing, and another daemon can finish it against the user's cancellation.

Reuse the existing RESTART instruction and ordinary rollback. Keep accepted Close authoritative: an already sealed journal only finishes verified teardown. Do not add a database migration, protocol, lifecycle state, general retry mechanism, or guarantee for an unreachable target or a handoff gate that has already closed. A handoff gate prevents new controller work while the daemon is being replaced.

## Progress


- [x] (2026-11-24 13:11Z) Confirmed #1677 is unassigned, claimed it, and added agent-in-progress. The user approved the small reachable-target fix.
- [x] (2026-11-24 13:13Z) Fast-forwarded the current master branch to origin/master at 9941e402c before edits. Assigned disjoint controller implementation and test/validation owners. No build is running.
- [x] (2026-11-24 13:14Z) Approved a lifecycle.rs-only recovery boundary and confirmed daemon restoration does not erase RESTART or continue a restored Running/Parked Suspend. No daemon change is needed for the approved scope.
- [x] (2026-11-24 13:17Z) Production is frozen in lifecycle.rs only; one shared helper replaces the duplicated cancellation write. The isolated container regression covers accepted and absent operations, shutdown-only interruption, existing restart reason, and retained-checkout rollback after reload.
- [x] (2026-11-24 13:21Z) All three exact controller filters passed one test each. Strict controller all-targets Clippy, targeted rustfmt and diff checks passed. The fixture's first run needed its fake-command directory created; only that failing test was rerun. No compiler/test remains active.
- [x] (2026-11-24 13:22Z) Committed 215114758 and requested review. Partial reviewers independently identified that ProcessExecutor must not make cleanup immune to daemon handoff.
- [x] (2026-11-24 13:28Z) Added the operation-scoped gate-linked cancellation executor and deterministic stalls during both the replacement probe and marker write. The updated exact regression passed one test; strict controller all-targets Clippy, formatting and diff checks passed. All three final review findings describe the same now-corrected handoff issue.
- [ ] Review integrated diff, commit on current branch, request review, and push to origin/master. Close #1677, remove active-work label, and dispatch platform CI if warranted by the final change.

## Surprises & Discoveries


The early target recovery runs before loading the accepted operation and before the cancellation-aware worker stop path. `ensure_suspend_target_running` returns SuspendPending on observation failure, which retains Closing but does not itself write RESTART. Writing that instruction requires both a reachable container and an actual accepted operation ID. Loading an absent operation can synthesize a new ID, so cancellation cleanup must not invent acceptance for legacy or raw checkouts.

The first regression run failed because the isolated fake Podman bin directory did not exist when its dispatcher was installed. Creating that fixture directory fixed the setup; the production change was unchanged. The focused retry passed one test, and the other two selected controls passed one each.

Review found that ignoring the original user-cancel flag with ProcessExecutor also ignores handoff interruption. Existing CancellableProcessExecutor accepts an independent CancellationToken, and the upgrade gate publishes drain before waiting for admission. An operation-scoped AbortOnDropHandle watcher can join those existing mechanisms. The daemon's user-cancel and shutdown causes are mutually exclusive, so polling shutdown_interrupted after user cancellation cannot supply the needed shutdown signal.

Upstream since the original diagnosis changed `run_accepted_suspension` in `mj-daemon/src/daemon/close.rs` to restore the pre-acceptance state on some errors with unchanged target/checkpoint. Bounded investigation confirmed this does not erase RESTART. Startup/retry already Closing retains Closing, while a fresh operation restored to Running/Parked does not automatically continue Suspend. Continue ordinary cancellation settlement after writing the marker; no daemon change is needed for this scope.

## Decision Log


Decision: fix reachable-target cancellation using existing target-side RESTART and rollback, without daemon-owned cancellation storage.
Rationale: this is the small scope the user approved. Guaranteeing cancellation through unreachable storage or a closed handoff gate would require a broader durable-intent design and is outside #1677.
Date/Author: 2026-11-24, parent agent and user.

Decision: cancellation arbitration belongs at the early recovery boundary, shared by its callers; reuse the existing idempotent cancellation write rather than add per-probe exceptions.
Rationale: container existence/state/start checks and admission reacquisition can fail at the same boundary. One owner should decide whether the operation is user-cancelled or interrupted only by daemon shutdown before propagating its error.
Date/Author: 2026-11-24, parent agent.

Decision: preserve the initial cancellable target-recovery result and allow one cleanup attempt only for user cancellation during Closing. Run that attempt through the same ownership-checked recovery plan inside the existing released handoff scope, and call end_resumable_work exactly once. The initial ProcessExecutor choice is superseded by the gate-linked executor decision below.
Rationale: a cancelled container command may leave an otherwise reachable retained target. Reusing its normal state/ownership checks avoids blind mutation replay and provides the existing storage needed for RESTART. Shutdown-only does not take this path. No loop or background retry owner is introduced.
Date/Author: 2026-11-24 13:14Z, parent agent accepting the implementation owner's bounded design.

Decision: only after successful re-admission and a Running target may cancellation cleanup load an actual accepted attempt and share the existing idempotent restart write. A failed initial probe with no ACCEPTED retains its original error; a matching accepted operation with a marker continues ordinary settlement.
Rationale: this prevents cleanup from manufacturing acceptance for raw/legacy Closing records and allows immediate rollback when the normal path is available. If admission has already closed, retain the existing error rather than claiming durable cancellation.
Date/Author: 2026-11-24 13:14Z, parent agent.

Decision: bind a fresh cancellation executor to the upgrade gate's drain notification for the new recovery attempt and its accepted-operation wire I/O. Ignore the original user-cancel flag, but any handoff/explicit Stop drain cancels this optional cleanup. Own and abort the watcher within the operation; keep historical common-path callers unchanged.
Rationale: recovery runs outside handoff admission and must stop on drain; admitted marker I/O must not hold the gate while an uncancellable transport stalls. Reuse the existing token/executor and gate notification rather than add a trait, service, timeout or lifecycle state. A drain that later lapses still cancels this attempt; recovery can retry from existing durable intent. Other process exit paths that only cancel a daemon token are outside this controller-only scope.
Date/Author: 2026-11-24 13:25Z, parent agent accepting the bounded handoff correction.

## Outcomes & Retrospective


Implementation and local validation are complete. One controller file now handles cancelled early recovery and shares the existing cancellation writer with ordinary Suspend. The isolated regression proves a matching durable RESTART and rollback to Parked on reload, retaining target and checkout contents. It covers absent acceptance, shutdown-only interruption and a distinct pre-existing restart reason. It also proves handoff interrupts stalled target recovery and stalled marker I/O, leaving the matching ACCEPTED recoverable. All three exact controller tests pass; the affected regression and strict controller Clippy passed again after the handoff correction. Review and delivery remain. Unreachable/missing/foreign targets and unavailable handoff admission retain their visible failure and data; this plan makes no stronger cancellation durability claim for them. No schema, protocol, daemon, Go, dependency or worker changes were needed.

## Context and Orientation


`mj-controller/src/controller/lifecycle.rs` contains `ensure_suspend_target_running` and its two admitted callers, `suspend_session_managed_controlled` and `recover_interrupted_close_managed`. Session admission serializes lifecycle changes. Target recovery releases the daemon handoff hold with `begin_resumable_work` and reacquires it through `end_resumable_work`; the session's worker permit still protects placement. Container recovery must remain outside that handoff hold.

`mj-targets/src/targets/recovery.rs::ensure_recovery_target_running` checks container existence and ownership, starts only an eligible stopped owned target, and verifies it is running. `mj-daemon/src/daemon/support.rs` implements cancellation and handoff admission for the executor, which runs target commands. A user-cancel flag differs from shutdown interruption; accepted user cancellation must not be replaced by shutdown-only behavior.

`mj-controller/src/controller/lifecycle/origin_delta.rs::OriginDeltaSuspend` loads ACCEPTED, REQUEST and RESTART from the worker root. ACCEPTED identifies the operation, REQUEST identifies its accepted worker stop, and RESTART requests rollback. An existing matching RESTART is idempotent. The common Suspend path already records cancellation with an independent `ProcessExecutor`, waits for the exact original worker when REQUEST exists, and resolves sealed versus unsealed relay state. Reuse those decisions.

The controller's existing tests in `mj-controller/src/controller/lifecycle/tests.rs` have isolated durable stores, target command fakes and persisted cancellation fixtures. Extend the owned-container boundary there instead of creating a test framework. Relevant controls are `interrupted_suspend_honors_dead_workers_refusal_receipt`, `interrupted_parked_suspend_with_raw_export_restores_parked_on_cancel`, and `podman_managed_workspace_origin_delta_and_cutoff_quiescence_routes_through_container_exec`.

## Plan of Work


Implement the agreed shape in lifecycle.rs. Retain the initial cancellable recovery result. On failed recovery plus user cancellation in Closing, run the same normal ownership-checked recovery plan once inside the existing released handoff scope; invoke end_resumable_work exactly once and keep both results until arbitration. Lazily create a CancellableProcessExecutor with an independent token and an operation-scoped AbortOnDropHandle watcher on the upgrade gate's drain notification. An already draining gate cancels that token immediately. Use this same executor for cleanup recovery and the subsequent accepted-operation wire reads/write. After successful re-admission and a Running target, load only an actual accepted operation, preserve its matching restart reason, and persist RESTART with the shared existing cancellation helper. If an accepted operation received a marker and handoff has not interrupted, continue ordinary settlement. If there was no acceptance after a failed initial probe, preserve that original failure instead of allowing cancellation cleanup to create an operation. Preserve errors and Closing until settlement; do not strip SuspendPending as a shortcut. Unreachable targets and closed gates retain their error rather than a successful durability claim. The scoped watcher is aborted on all exits; historical common-path callers retain their previous executor.

The production owner edits controller production files only. The test/validation owner edits the existing test file and owns every build after production is frozen. In parallel, extend an existing persisted cancellation fixture to cancel during owned-container existence/inspect before REQUEST, then replay with a fresh executor. Check the matching restart operation, retained checkout and rollback result. Cover shutdown-only interruption without RESTART, and user cancellation versus drain where the existing fixture can represent it. Preserve existing refusal and sealed-close controls. The parent maintains this plan and commits the integrated result.

## Concrete Steps


Work from `/home/jonathan/Projects/mjolnir/.mj/clones/5e24f24efdf9c69e0cf514ba11b4716b` on the current master branch. Do not create branches, rebase, or install a binary. Use mbx's dev profile and existing build storage. Run tests elevated outside the restricted sandbox because socket tests need it; keep isolated fixture configuration and data directories. No live default-instance invocation is allowed.

After production and fixtures are frozen, the sole validation owner runs the final selected controller test filters through:

    mbx test -p brokk-mj-controller --lib <affected-filter>
    mbx clippy -p brokk-mj-controller --all-targets -- -D warnings

The selected filters are cancelled_container_suspend_probe_records_rollback_for_recovery, interrupted_suspend_honors_dead_workers_refusal_receipt, and interrupted_parked_suspend_with_raw_export_restores_parked_on_cancel, with one test expected from each. Run only these relevant controls, and rerun failures alone after fixes. Do not build CLI/worker binaries or run a broad E2E suite unless a concrete unresolved boundary requires it. If an agreed necessary production change touches another crate, add only that crate's affected validation.

Validation completed with the full controller::lifecycle::tests:: prefix and -- --exact on each selected name; each reported 1 passed, 0 failed. Strict Clippy finished the dev profile in 36.07 seconds with no diagnostics. Standalone rustfmt checked both edited Rust files. No new binary or live instance was used.

The parent reviews the diff, commits the changed files with `Fixes #1677`, calls request_review, resolves findings with scope judgment, and pushes HEAD to origin/master. Fetch and merge upstream if it moved; no rebase. Record delivery and any platform CI run here.

## Validation and Acceptance


A deterministic regression must fail without the production change: cancel during early owned-container observation with an accepted origin-delta operation and reachable target, replace the cancelled executor/controller, and show that recovery honors the matching RESTART and retains the checkout rather than completing Suspend. The shutdown-only control must not create RESTART. Cancellation must preserve an existing matching restart reason and must not synthesize ACCEPTED for a legacy path. Existing sealed-close behavior remains authoritative.

Acceptance requires the focused tests and strict affected-crate Clippy to pass, a clean reviewed committed diff, and a successful push. No new durable record, protocol, lifecycle state, speculative timeout or retry loop is permitted. The final response must state the reachable-target limit.

## Idempotence and Recovery


Use the existing operation ID and restart instruction so repeated cancellation/recovery does not create a new operation. Keep archive verification and process teardown order unchanged. If target recovery or the marker write fails, report the failure and retain data; do not claim a cancellation guarantee that was not persisted. Keep test logs in the validation owner's report directory and clean up any isolated processes before finishing.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1677. Starting revision: 9941e402c. Implementation owner: 5eade66472957cbdf498fc9dac1adfa9. Test and sole validation owner: cdaf8f7b9d8c67970c0fd3917e3bb15e. They share the checkout and have disjoint file ownership; neither may build before the parent releases validation.

Evidence is under .mj/agents/cdaf8f7b9d8c67970c0fd3917e3bb15e: new-regression-retry.log, interrupted_suspend_honors_dead_workers_refusal_receipt.log, interrupted_parked_suspend_with_raw_export_restores_parked_on_cancel.log, and controller-clippy.log. The first fixture-setup failure is new-regression.log. These are temporary report files; the decisive outcomes are embedded above.

The handoff correction's exact regression passed in 0.82 seconds (handoff-regression.log), including stalled recovery and RESTART transport. Strict Clippy passed in 8.21 seconds (handoff-clippy-retry.log). Its first run flagged test-only tokio::spawn, replaced mechanically by mj_core::runtime::spawn; only the failed lint was rerun. No active compiler or test remains.

## Interfaces and Dependencies


Keep the existing `CommandExecutor`, `CancellableProcessExecutor`, `ProcessExecutor`, `OriginDeltaSuspend`, `LifecycleFailureReason::SuspendPending`, ACCEPTED/REQUEST/RESTART formats, and session admission/worker permit. The new cleanup executor uses the existing CancellationToken and AbortOnDropHandle; no trait or dependency change is needed. Extract the existing cancellation write to one local helper accepting its executor; do not introduce an alternate interpretation of operation identity or cancellation.

Revision note (2026-11-24 13:13Z): Initial plan records the user-approved reachable-target scope and the current upstream integration check before implementation.

Revision note (2026-11-24 13:21Z): Implemented the shared reachable-target cancellation boundary and idempotent restart helper, documented upstream daemon compatibility, and recorded the three passing controller tests and strict lint. The single fixture setup failure was repaired without changing production behavior; review and push remain.

Revision note (2026-11-24 13:28Z): Replaced ProcessExecutor only in the new cancellation cleanup with an independent, gate-linked cancellation executor. A scoped watcher makes both resumable recovery and admitted record I/O interruptible on handoff/explicit Stop. The existing regression now stalls each transport boundary deterministically and passes, as does strict controller lint. This resolves all three overlapping review findings without a new service, trait, state or timeout.
