# Recover workers after terminal harness preparation failure

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

A session whose harness cannot start must remain observable and automatically receive a newer worker build that fixes startup. The daemon is the background controller; the worker owns the harness process and publishes its startup state. Session `0efccbcef11b71ab37df2f0e62e9cd9e` demonstrates the failure: worker 2.34.0 reports failed Claude preparation, rejects admission repair, and never reaches the automatic upgrade coordinator.

## Progress

- [x] (2026-10-09) Diagnose rejection before publication of the worker snapshot.
- [x] (2026-10-09) Inspect subsequent replacement admission: failed preparation also fails the ordinary idle predicate and idle reservation.
- [x] (2026-10-09) Add an isolated relay regression and reproduce the admission-repair rejection before implementation.
- [x] (2026-10-09) Implement observation and replacement based on the worker's terminal startup state.
- [x] (2026-10-09) Find and fix abandoned replacement intents waiting forever for readiness after terminal startup failure.
- [x] (2026-10-09 01:47Z) Pass the regression, core suite, controller suite with one isolated timing-test retry, Clippy, and formatting.
- [x] (2026-10-09) Complete controller documentation tests and prepare the reviewed change for commit on master.

## Surprises & Discoveries

The daemon already special-cases failed preparation when selecting upgrade candidates, but the actor and workspace lease still require an ordinary idle harness. Fixing attachment alone would therefore leave the worker unreplaced. Workers publish preparation separately from durable relay state, and old workers' ReserveIdle cannot use that publication. Their existing checkpoint command does establish a barrier while preparation has failed.

The live session also has a durable `worker_restart_intents` row in `awaiting_readiness`. Observing terminal failure must settle an abandoned intent before the next upgrade can acquire it; a currently owned swap must retain its intent. The regression failed before implementation with the exact InvalidState admission repair error seen in production.

The full controller run passed 1,962 tests with 10 ignored and failed only `mbx_clean_survives_its_release_deadline_and_is_not_confirmed`, which relies on a 100 ms subprocess budget and four-second completion deadline. Its isolated rerun passed in 2.34 seconds. The core suite passed 485 unit tests and two recorded-scenario integration tests. Clippy found a single-pattern match in the new fixture; changing it to `if let` resolved the warning. The fixture's durable worker root must end in the session id, as real worker placements do.

## Decision Log

- Decision: use terminal Failed as proof that this process cannot start its harness, and keep eligibility in `RelayOperationalState::safe_to_replace`.
  Rationale: the daemon and replacement lease must ask the same decision owner. Live shells, terminals, tools, background commands, native agents, and existing barriers still block replacement. Accepted prompts remain durable and survive replacement. Establish and verify a checkpoint barrier for failed preparation because old workers' ordinary idle reservation cannot include the terminal startup state.
  Date/Author: 2026-10-09, Codex.

- Decision: settle terminal failure in `mj-controller/src/worker_lifecycle.rs::observe_worker_restart_outcome` under observation ownership held through the snapshot read.
  Rationale: an active swap prevents this observation from acquiring ownership, so a stale failure cannot erase another task's intent. Pending preparation remains unresolved; an abandoned terminal failure can admit the next upgrade.
  Date/Author: 2026-10-09, Codex.

## Outcomes & Retrospective

The regression now passes and proves connected startup-failure visibility, retention of an actively owned restart intent, settlement after its owner leaves, replacement admission despite an old worker refusing ReserveIdle, preservation of queued work, and refusal while a shell is running or preparation continues. Required source validation passed, including controller documentation validation (zero documentation cases). The live session needs a daemon containing this change installed through the normal deployment workflow. All runtime tests used isolated configuration and data directories; live session data was read only.

## Context and Orientation

`mj-controller/src/session_manager/actor.rs` reconnects and repairs delegation admission before publishing a snapshot. `mj-controller/src/daemon/snapshot.rs` submits connected snapshots to background upgrade policy. `mj-core/src/relay/snapshot.rs` owns the shared replacement predicate. `mj-controller/src/controller/checkpoint/workspace_lease.rs` reserves a barrier and rechecks replacement safety before a swap. The barrier prevents new work from starting, and dropping its control connection releases it.

`mj-controller/src/worker_lifecycle.rs` settles durable replacement intents after an abandoned swap. Observation owns the worker before reading its snapshot, preventing a stale observation from settling a newer replacement.

## Plan of Work

Skip reopening delegation on a worker reporting terminal preparation failure, allowing its authoritative snapshot to reach the daemon. Centralize failed-worker replacement eligibility in the operational state and remove the daemon-only override. In the upgrade lease use BeginCheckpoint for terminal preparation failure, then retain existing barrier verification and ownership. Extend the existing stdio relay fixture in `mj-controller/src/session_manager/tests.rs` to publish failed preparation and reject admission repair, then exercise reconnect and replacement admission with queued durable work in an isolated store.

## Concrete Steps

From the repository root, use the existing mbx-backed Cargo on PATH, without changing build directories. Run `cargo test -p brokk-mj-controller reconnect_retains_failed_preparation_for_worker_replacement` outside the sandbox before and after the fix. Then run full suites for touched crates (`brokk-mj-core` and `brokk-mj-controller`) and `cargo clippy --all-targets -- -D warnings`, also outside the sandbox. New test subprocesses set `MJ_INSTANCE=failed-preparation-replacement` and isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`.

The full combined test invocation stopped on the controller timing failure before running core tests or controller documentation tests. Finish with `cargo test -p brokk-mj-core`, `cargo test -p brokk-mj-controller mbx_clean_survives_its_release_deadline_and_is_not_confirmed`, and `cargo test -p brokk-mj-controller --doc`, setting `MJ_INSTANCE=worker-recovery-validation` and fresh temporary `MJ_CONFIG_DIR` and `MJ_DATA_DIR` for each invocation. Run `cargo fmt --all -- --check` and review `git diff --check`. Stage only the six changed Rust files and this plan, then commit directly to master without pushing.

## Milestones

The first milestone makes terminal startup failure observable. The existing stdio relay integration fixture publishes the worker's failed preparation state and rejects delegation admission; the new regression initially failed at that rejection and now retains a connected snapshot. The same regression verifies that a live replacement owner retains its durable intent and that an abandoned terminal failure settles it on the next sync.

The second milestone admits replacement safely. The shared operational-state predicate considers failed preparation while refusing independently running work. Old failed workers use their existing checkpoint dispatcher to establish a barrier, and the lease rechecks safety under that barrier. The regression confirms queued prompts remain and shell activity or pending preparation still prevent replacement.

The final milestone validates the complete behavior and prepares the commit. Existing lost-admission-acknowledgement reconnect regressions passed, the full affected suites ran in isolated stores, and workspace Clippy, formatting, and controller documentation checks passed. Commit the reviewed change to the current master branch.

## Validation and Acceptance

The regression must fail on the old reconnect path, then retain a connected snapshot with the original startup error, settle an abandoned replacement intent, and acquire and verify replacement admission. An actively owned intent must remain. It must retain accepted prompts across the barrier, and refuse replacement while preparation is still running or a user shell is live. Existing reconnect lost-acknowledgement regressions must still pass, so healthy workers continue to repair admission strictly.

## Idempotence and Recovery

All regression resources belong to temporary isolated stores; control connections and managers are shut down before removing their files. Production replacements retain the existing worker ownership and checkpoint barrier; failures drop the lease without stopping a worker. Stage only changed files and commit to the current branch, without pushing.

## Artifacts and Notes

The live daemon's error is `repair sub-agent admission on relay reconnect: ... InvalidState: harness preparation failed at harness-profile: missing staged Claude delegation server`. Its log reports `transport_dead=false`, so dead-process recovery cannot resolve this condition.

## Interfaces and Dependencies

Keep the existing wire protocol and persisted formats. A small operational-state query may expose whether preparation failed, but the shared `safe_to_replace` method remains the decision owner. Use existing `IsolatedTest`, `DurableRelay`, session manager, and `IdleWorkspaceLease` machinery for the integration test.

Initial plan: cover the attachment defect and the downstream replacement admission defect together so the result actually permits automatic recovery.

Updated after inspecting the live store: include settlement of an abandoned failed boot, which otherwise keeps automatic upgrades deferred even after reconnect and admission are fixed.

Updated after validation: record the passing regression and suites, the isolated retry of an existing mbx timing test, and the source-only Clippy fixture correction. Controller documentation validation also completed successfully; the reviewed change is ready for commit.
