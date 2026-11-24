# Finish reachable-container recovery before handling Suspend cancellation

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current. The user approved the reachable-target scope for #1677 and requires simpler code. The checked-in `.agents/plans/recover-suspend-without-original-worker.md` describes the established Suspend machinery; the facts needed here are repeated below.

## Purpose / Big Picture


When a user cancels Suspend during an early retained-container check, make the reachable target available for the existing cancellation path to roll back in the same operation. The daemon is the controller process; a worker owns the agent and its durable journal. The early check currently returns on cancellation before the normal controller path can record rollback. A replacement daemon can then complete the original Suspend contrary to that cancellation.

The fix is one extra attempt through the existing target-recovery function after a failed user-cancelled probe. Once it succeeds, return to the existing caller. That caller already owns cancellation, writes the existing RESTART instruction, and settles rollback under the current lifecycle owner. No early record interpretation, separate settlement path, service, executor mode, schema, protocol, lifecycle state or retry loop is needed.

## Progress


- [x] (2026-11-24 13:11Z) Claimed #1677, added agent-in-progress, and confirmed the user-approved reachable-target scope.
- [x] (2026-11-24 13:13Z) Fast-forwarded current master to 9941e402c and assigned disjoint production and test/validation ownership.
- [x] (2026-11-24 13:21Z) Initial three exact controller filters and strict all-targets Clippy passed; committed 215114758 for review.
- [x] (2026-11-24 13:33Z) Explored handoff-aware per-call cleanup and return-Pending boundaries in 8c109c42c/ccdebf7fb. Regression/lint passed, but reviews exposed competing settlement ownership.
- [x] (2026-11-24 13:40Z) Accepted MJ Advisor's smaller structural cut, superseding the per-call watcher, early record write, return-Pending and proposed daemon replay designs before daemon edits began. Production owner acknowledged the revision.
- [x] (2026-11-24 13:41Z) Tracked the pipeline-wide shutdown-only executor limitation separately in #1678. No implementation of that follow-up is part of this task.
- [x] (2026-11-24 13:45Z) Reduced lifecycle.rs change and same-call rollback fixture are complete. All three exact filters passed one each; strict controller all-targets Clippy and both Rust formatting/diff checks passed. No build/test remains active.
- [x] (2026-11-24 13:47Z) Final source commit fe9cc00b5 reviewed; explicitly overrode the three overlapping shutdown/handoff findings as the #1678 follow-up recommended by MJ Advisor. Unrelated upstream merged cleanly; integration review found zero own changed files.
- [x] (2026-11-24 13:48Z) Pushed 65e6ecbbd to origin/master. #1677 closed and active-work label removed. Dispatched platform CI run 45407265706 on that exact code commit; it is pending. No build installed.

## Surprises & Discoveries


The early target check was the one step that did not accommodate user cancellation. The pipeline after it already reads the actual accepted operation, preserves a matching restart reason, writes RESTART, and restores the session. Performing those decisions inside target recovery created a second settlement discipline. Returning Pending after the marker left an already-Closing session without an owner in the running daemon. Extending a local watcher also missed later cancelled-path I/O.

MJ Advisor checked the current code and identified the smaller cut: only finish the target check, then let the existing caller settle cancellation. Its ordinary cancelled paths already use ProcessExecutor at multiple sites, including worker rollback. That pipeline-wide shutdown limitation needs one consistent executor-owned solution, tracked in #1678, rather than a partial guarantee at this helper.

Upstream `mj-daemon/src/daemon/close.rs::suspend_admitted` restores a fresh prior Running/Parked state on some errors, while startup/retry already Closing stays Closing. This makes a return-Pending shortcut insufficient. The final design completes existing settlement in the same call instead; no daemon change is needed.

The initial regression needed its fake Podman bin directory created before installing the dispatcher. A later test-only tokio::spawn was replaced with the repository runtime helper after Clippy. These fixture fixes did not require production behavior changes.

## Decision Log


Decision: after a failed early recovery plus user cancellation in Closing, make one attempt through the same target ownership/state checks with ProcessExecutor, call end_resumable_work once, and return the resulting recovery outcome to the existing caller.
Rationale: cancellation means rollback, which requires a reachable target. The caller already owns recording and settling cancellation. Keeping one decision owner deletes the early ACCEPTED/REQUEST/RESTART handling, per-call watcher, and second settlement discipline.
Date/Author: 2026-11-24 13:40Z, parent agent accepting MJ Advisor's ruling.

Decision: preserve initial successful-probe behavior and shutdown-only failure behavior. Do not invent acceptance or write any rollback record inside target recovery.
Rationale: the common path already interprets checkout policy, operation identity and cancellation. A successful first probe needs no new handling; raw/legacy records must retain their existing policy decisions.
Date/Author: 2026-11-24, parent agent.

Decision: keep the broader cancellation-versus-handoff executor problem in #1678 rather than extend this fix across all cancellation paths.
Rationale: plain ProcessExecutor in cancelled rollback is pipeline-wide and predates this change. Per-call token/watchers repeatedly moved the boundary without giving a complete guarantee. The user prioritized simpler code and approved a reachable-target fix. This plan makes no new shutdown-only guarantee for the surrounding existing pipeline.
Date/Author: 2026-11-24 13:41Z, parent agent after design review.

## Outcomes & Retrospective


The reduced implementation is reviewed and pushed in 65e6ecbbd, closing #1677. The actual managed recovery entry rolls back cancellation to Parked in the same call, retaining its target and checkout contents and clearing the accepted/restart markers. A separate accepted operation preserves a preexisting rollback reason. Direct helper tests prove no early record writing, normal successful-probe behavior, and retained failure on shutdown-only or unreachable recovery. Three exact filters and strict controller Clippy pass. The discarded watcher and deferred-settlement mechanisms are removed from the final diff; production remains lifecycle.rs only. [Platform CI](https://github.com/BrokkAi/mjolnir/actions/runs/45407265706) is pending; the installed build is unchanged. Unreachable, missing or foreign targets retain a visible failure and their data. The three overlapping handoff review findings were explicitly overridden: the uncancelled retry can delay shutdown or continue after admission is released if transport stalls. That accepted limitation and the surrounding pipeline's executor problem remain in #1678, rather than being hidden behind a partial local watcher.

## Context and Orientation


`mj-controller/src/controller/lifecycle.rs::ensure_suspend_target_running` is called by `suspend_session_managed_controlled` and `recover_interrupted_close_managed` before policy and worker-record reads. Session admission serializes lifecycle ownership. The helper verifies the worker permit and placement, releases the daemon handoff hold with begin_resumable_work, runs target recovery, and reacquires the hold with end_resumable_work. Keep exactly that ownership and one reacquisition.

`mj-targets/src/targets/recovery.rs::ensure_recovery_target_running` checks existence, ownership and running state of an owned container. It starts only an eligible stopped target. On user cancellation, the first command executor can stop the check before the retained target is accessible. ProcessExecutor ignores that already-fired user flag so the same normal recovery checks can finish once; do not hand-roll container commands or replay an arbitrary mutation.

After the helper, `recover_interrupted_close_managed` observes user cancellation and the common Suspend path's restart_reason closure uses `record_suspend_cancellation`. `OriginDeltaSuspend` in `mj-controller/src/controller/lifecycle/origin_delta.rs` supplies the actual ACCEPTED operation ID, REQUEST for an accepted original-worker stop, and RESTART for rollback. Existing rollback preserves an already sealed Close and waits for accepted worker stopping where required. Keep these interpretations in their existing owners.

Tests belong in `mj-controller/src/controller/lifecycle/tests.rs`. The isolated container fixture uses a Podman command stand-in and real persisted records, so it can deterministically cancel the existence probe without live-container operations. Production ownership is lifecycle.rs only; the test owner edits this existing test file. The parent owns this plan, integration, commit and review.

## Plan of Work


Reduce target recovery to retaining its first result, making one normal recovery pass with ProcessExecutor only if that result failed due to a current user cancellation in Closing, reacquiring admission once, and returning the result. Preserve useful error diagnostics on failure. Delete the local cancellation executor, token/watch task, observed-running/record-cancellation arbitration, early OriginDeltaSuspend load and restart write, and Pending return. Keep the shared ordinary cancellation-record helper if it still improves the existing closure; avoid an unused executor parameter.

Adapt the existing regression to show that direct reachable target recovery completes despite the user flag, without writing records there. Then invoke the full recovery entry with cancellation at the early existence probe and prove that it rolls back to Parked in that same call, retains checkout/target data, and clears acceptance/restart markers. Keep a distinctive pre-existing restart reason and shutdown-only/unreachable controls as relevant. Do not use a manually scheduled second replay as the main proof. Remove the discarded per-call watcher stall cases. Initial successful probe plus late cancellation must keep historical behavior.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir/.mj/clones/5e24f24efdf9c69e0cf514ba11b4716b` on the current master branch. Do not create a branch, rebase, install a binary, or touch the live default instance. Use mbx dev profile and existing build storage. Run Rust tests elevated outside the restricted sandbox. Keep fixture-owned isolated configuration and store directories.

The sole validator runs the final affected filters with full names and --exact:

    mbx test -p brokk-mj-controller --lib controller::lifecycle::tests::cancelled_container_suspend_probe_records_rollback_for_recovery -- --exact
    mbx test -p brokk-mj-controller --lib controller::lifecycle::tests::interrupted_suspend_honors_dead_workers_refusal_receipt -- --exact
    mbx test -p brokk-mj-controller --lib controller::lifecycle::tests::interrupted_parked_suspend_with_raw_export_restores_parked_on_cancel -- --exact
    mbx clippy -p brokk-mj-controller --all-targets -- -D warnings

Expect one matched test per filter. Retain unaffected passing controls during later rounds, and rerun failures only. No broad suite, CLI/worker build or E2E campaign is planned. Format the two changed Rust files and run git diff --check. The parent commits only changed files, requests review, and pushes HEAD to origin/master after review disposition. Fetch and merge upstream if moved; never rebase.

## Validation and Acceptance


Cancellation during an early owned-container probe with a reachable retained target must complete normal cancellation rollback in the current invocation. Assert the durable Parked outcome and retained checkout contents, not merely a restart marker followed by manual recovery. Shutdown-only still returns the existing failure classification; unreachable/missing targets do not claim success. The helper itself must neither synthesize ACCEPTED nor leave a marker for a separate settlement owner. Existing operation identity, refusal reasons and sealed Close authority remain in the common path.

Acceptance requires the focused tests and strict controller Clippy to pass, reviewed committed code, and successful push. The final response must state the reachable-target scope and #1678 limitation. No new durable state, protocol, lifecycle state, service, executor mode, timeout or retry loop is allowed.

## Idempotence and Recovery


Use the existing target recovery function's ownership and state checks for the extra attempt. The common path reuses its operation ID and idempotent restart handling. Keep archive verification and process teardown order unchanged. A failed target check remains an error with data retained. Clean up isolated test processes; logs belong in the validator's report directory.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1677. Broader follow-up: https://github.com/BrokkAi/mjolnir/issues/1678. Starting revision: 9941e402c. Implementation owner: 5eade66472957cbdf498fc9dac1adfa9. Test and sole validation owner: cdaf8f7b9d8c67970c0fd3917e3bb15e. Reports are under the corresponding .mj/agents directories; the decisive outcomes must also be recorded here before delivery.

Final evidence under the validator's report directory is same-call-cancelled_container_suspend_probe_records_rollback_for_recovery.log (1 passed, 1.14 seconds), same-call-interrupted_suspend_honors_dead_workers_refusal_receipt.log (1 passed, 0.45 seconds), and same-call-interrupted_parked_suspend_with_raw_export_restores_parked_on_cancel.log (1 passed, 0.38 seconds). same-call-controller-clippy.log records strict all-targets success in the dev profile (8.40 seconds). No binaries, daemon/E2E instances or live operations were used.

## Interfaces and Dependencies


Keep the existing CommandExecutor/ProcessExecutor, target recovery function, OriginDeltaSuspend, SuspendPending failure classification, session admission/worker permit, and ACCEPTED/REQUEST/RESTART formats. No core trait, dependency, daemon, worker or origin-delta reader change is needed. The sole additional behavior is finishing the existing early target recovery step before the established cancellation path runs.

Revision note (2026-11-24 13:41Z): Rewrote the plan around MJ Advisor's smaller same-owner cut. This supersedes the initial early record writer, local handoff watcher, deferred Pending settlement and proposed daemon replay. All are removed from the final implementation rather than retained as parallel paths. The surrounding pipeline's shutdown-only executor work is separately tracked in #1678.

Revision note (2026-11-24 13:45Z): The reduced design passes the full same-call cancellation regression, two affected controls, strict controller Clippy and formatting. Recorded exact nonzero counts and the unchanged broader handoff limitation; review and delivery remain.

Revision note (2026-11-24 13:48Z): Recorded the final review override, clean unrelated upstream integration, successful push, issue closure and exact pending platform CI run. No installation was performed; #1678 remains explicitly open.
