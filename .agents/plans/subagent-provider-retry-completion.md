# Keep sub-agent completion behind worker retry resolution

This ExecPlan follows `.agents/PLANS.md` and must remain current as implementation and validation proceed.

## Purpose / Big Picture

A child that encounters temporary provider capacity must retain its automatic retry instead of sending its parent a terminal failure. Issue #1442 records child `843c54dfed30787619bbd74e53b4892a`, whose parent closed it after a premature failure notice, before its retry timer fired. After this fix, child listings, parent notices, reminders, and parking wait for the worker to finish classifying and retrying the turn. Permanent failures still reach the parent.

## Progress

- [x] Diagnosed the live failure and claimed #1442 as jbellis with `agent-in-progress`.
- [x] Identified the existing authoritative turn-ending rule in the session wait API.
- [x] Share the wait API rule and use coherent worker observations in delegation.
- [x] Add isolated regression coverage for classification, queued/running retry, stale publication, disconnection, and final failure.
- [x] Demonstrate the regression fails with the old completion behavior and passes with the fix; all 54 affected API tests pass.
- [x] Run the full controller suite and workspace Clippy; confirm the one unrelated shell-profile failure passes alone.
- [x] Complete doctests (zero cases), formatting and diff checks, and review the change for commit and push.

## Surprises & Discoveries

The worker did arm a retry. Both `DelegationObservation::from_view` and `subagent_status` instead equated transcript execution being idle with the task having ended. The session wait API already retains a turn while assessment is pending or a retry is armed, queued, or running. Child progress is read from the committed database publication, which can briefly precede the matching actor view, so those generations must agree before announcing completion.

## Decision Log

Use the wait API's existing rule as the single controller interpretation of turn resolution. Add a shared snapshot translation rather than separate capacity-specific conditions. Keep provider classification, timers, and retries owned by workers. This needs no schema or wire change. A disconnected live worker cannot establish completion; terminal lifecycle records remain authoritative. This decision was made on 2026-11-02.

## Outcomes & Retrospective

Implementation now shares `WaitObservation::turn_end` through a worker snapshot translation. Delegation observations carry an outcome only when the worker has resolved it, eliminating the independent idle flag that could contradict retry state. Child listings and notices compare the committed completion ordinal with the connected actor publication before reporting it. The isolated regression fails with the previous completion behavior at `assessment: completion must not remind or park a retrying child`. Restoring the fix passes the regression and all 54 tests in `server_runtime::api::tests`.

The full controller run completed with 2,226 passed, 18 ignored, and one unrelated failure in `controller::mbx::install::tests::profile_block_selects_and_updates_the_login_shell_file_idempotently`: line 1157 selected `~/.profile` instead of `~/.bash_profile`. That test passed when run alone (1 passed), without changing its code. This was an assertion failure, not a timeout. Workspace `cargo clippy --all-targets -- -D warnings` passed. Full-run and focused evidence are at `/tmp/mj-1442-controller-tests.log`, `/tmp/mj-1442-mbx-profile-test.log`, `/tmp/mj-1442-api-tests.log`, and `/tmp/mj-1442-clippy.log`. Doctests completed separately with zero cases because the unrelated failure stopped Cargo before that phase. Formatting and diff checks passed. Implementation and validation are complete, with the full-run limitation recorded above.

## Context and Orientation

The worker executes harness turns and persists its assessment and retry timer. The controller's session actor publishes `ManagedSessionSnapshot`, containing a transcript projection and the worker's operational state from one synchronization. `mj-controller/src/server/api/wait_policy.rs` owns `WaitObservation::turn_end`, which recognizes retries by the original completion ordinal in their command IDs. `mj-controller/src/server/api/wait.rs` translates actor snapshots for HTTP wait requests. `mj-controller/src/session_manager/delegation.rs` publishes observations used by the completion coordinator in `mj-controller/src/daemon/delegation.rs`. Child listings and durable parent notices both use `child_snapshots` and `subagent_status` in `mj-controller/src/server_runtime/api.rs`.

## Plan of Work

Extract the existing worker snapshot translation into a `WaitObservation` method and expose a narrow pending-turn query that delegates to `turn_end`. Use it when creating delegation observations and when reading child progress for listings and notifications. Only resolve a live child's committed outcome against an actor snapshot for that same completion; hold mismatched or disconnected observations pending. Completion processing receives an outcome only after worker resolution, rather than receiving an independent idle flag alongside an intermediate outcome. Add a hard-won regression marked #1442 using isolated store state and a controlled published worker view. Exercise pending assessment, an armed retry, queued and running retry, a second provider refusal, and final permanent failure; existing successful-handback regressions protect success. Verify no failure notice or parking occurs before resolution.

## Concrete Steps

Work from the repository root on the existing master branch. Run focused tests outside the sandbox with `cargo test -p brokk-mj-controller <regression-filter>`. Run full final validation once with `cargo test -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings`, using the normal Cargo/mbx setup. Unit regressions must use their existing isolated process/store helpers and a named `MJ_INSTANCE`; never run a development binary against the default instance. Review `git diff --check`, stage only changed files, commit on master, and push `HEAD:master`. If origin moved, fetch and merge `origin/master`, then push without rebasing.

## Validation and Acceptance

The new regression must fail on the old completion path, pass with the shared worker resolution rule, leave parent mailbox output empty through assessment/retry transitions, and report only the final resolved outcome. Existing wait, handback, login failure, and parked-child tests must continue passing. Full controller tests and workspace Clippy must pass before submission.

## Idempotence and Recovery

All fixtures use temporary isolated stores. No live worker is interrupted and no provider request is replayed. Cargo storage is unchanged. Repeating validation or issue updates is safe. The fix can be reverted with an ordinary Git revert.

## Artifacts and Notes

The live evidence is in #1442. The failed child's final activity retained `capacity_retry: true`; its provider refusal was at 16:50:32.832 UTC and its explicit parent close ended at 16:51:24.109 UTC.

## Interfaces and Dependencies

Keep the shared interpretation inside `WaitObservation`; its worker snapshot translation is reusable by session waits and delegation. Use `SessionHandle::view()` for nonblocking reads, and use the existing committed state for report ownership and startup records. Preserve all public API, relay, and database formats.

Initial plan written on 2026-11-02 after confirming the notification/retry race and the existing session-wait behavior.

Implementation update on 2026-11-02: delegation now omits unresolved outcomes entirely, so completion code cannot mistake an intermediate stop for a resolved ending. The first compilation found and corrected an Arc receiver mismatch in the regression fixture.

Validation update on 2026-11-02: corrected the fixture to assert the actual `list_agents` state contract, demonstrated the old behavior failing the regression, and passed all 54 affected API tests. The complete controller suite and workspace Clippy are the remaining checks.

Full validation update on 2026-11-02: workspace Clippy passed; the full controller suite had one unrelated shell-profile selection assertion, which passed on its focused rerun. No unrelated production or test code was changed. Upstream advanced by release metadata, CI policy, and a chaos-fixture change; none touches the Rust behavior changed here.

Completion update on 2026-11-02: doctests, formatting and diff review are complete. The reviewed patch is ready for the repository-required commit and push, including an upstream merge if the push is rejected.
