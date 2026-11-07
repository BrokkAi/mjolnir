# Overlap managed checkout and worker preparation

This ExecPlan follows `.agents/PLANS.md` and must be updated as work proceeds.

## Purpose / Big Picture

New local sessions currently spend tens of seconds serially creating an editable Git checkout and preparing their worker and harness. Run those independent preparations concurrently and log launch phase boundaries at INFO so operators can account for time before subprocesses, controller loading, and readiness. A harness is the agent program; the worker is its session-owned supervisor. No live installation or store migration is needed.

## Progress

- [x] (2026-11-07) Measured recent top-level launches at 76.1 and 46.7 seconds to session open; verified Git objects are hardlinked but editable files are not.
- [x] (2026-11-07) Traced dependencies: bare target provisioning does not require a populated checkout; runtime preparation currently also inspects workspace build files and must separate that inspection.
- [x] (2026-11-07) Split checkout planning from materialization and joined materialization with worker preparation under the existing lifecycle owner.
- [x] (2026-11-07) Added INFO phase timing across admission, lifecycle startup, controller work, and worker readiness.
- [x] (2026-11-07) Validated overlap, failure ordering, late workspace/relative bridge discovery, touched-crate suites, formatting, and Clippy. The new controller fixture needed Tokio; 17 existing ACP deadline failures passed serial reruns.
- [x] (2026-11-07) Prepared the validated implementation for the required commit, review, and push on the current branch.

## Surprises & Discoveries

The existing two-lane helper already joins both lanes on errors and propagates the lifecycle permit to its child thread. Local raw checkout runs before that helper; remote bundle checkout already runs inside it. The prepare-harness subprocess calls the same launch resolver as the actual worker, including workspace build-cache discovery. That discovery must remain after checkout. Custom bridge and provider paths can also live inside the unfinished checkout, so runtime-only preparation must defer final executable resolution too.

## Decision Log

- Decision: Return a prepared managed-checkout description from planning and execute it inside the existing repository setup lane. Retain the existing immediate helper for tests by wrapping the same plan and execute functions. Rationale: one persisted checkout owner, no competing mutation of controller state, and existing cleanup owns partial materialization. Date: 2026-11-07.
- Decision: Shared runtime preparation skips workspace-sensitive build-tool discovery; the real worker still performs discovery before launching the harness. Rationale: avoid reading an absent or half-written checkout while warming shared runtime packages. Date: 2026-11-07.
- Decision: Use monotonic phase timers with session IDs and INFO start/end events, also covering failed scopes. Rationale: subprocess logs alone leave unexplained launch gaps and hide in-flight controller work. Date: 2026-11-07.

## Outcomes & Retrospective

Managed checkout now materializes in the existing repository setup lane while profile staging and shared runtime preparation run in the worker payload lane. Both lanes retain the lifecycle owner and join before worker startup or rollback. Workspace build-tool discovery and relative bridge/provider resolution run only during the subsequent full worker launch. INFO phase boundaries cover admission/registration, lifecycle scheduling, controller load, publication, credentials, preflight, checkout planning/materialization, profile staging, workspace initialization, and readiness. No live launch speedup has been measured or claimed; these changes require a later installation to affect the live daemon.

## Context and Orientation

`mj-controller/src/daemon/create.rs` admits and registers sessions, then starts their supervised lifecycle. `mj-controller/src/controller/provisioning.rs` owns target creation, joins repository setup and worker payload installation through `execute_concurrent_lanes`, and starts the worker only after setup succeeds. `mj-controller/src/controller/worktree.rs` currently plans, persists, and immediately creates the managed clone. Split the last operation from the persisted plan. Bare targets name existing machines and require no container creation. `mj-worker/src/worker_runtime/harness_launch.rs` selects and verifies runtimes; `unix.rs::prepare_managed_harness` is the pre-install command that must avoid workspace inspection.

## Plan of Work

Add a small common launch phase timer in mj-controller and instrument admission/registration, lifecycle scheduling and controller load, publication, credentials, target and checkout planning, the concurrent setup lanes, payload staging, and readiness. Keep all timers out of stored or wire formats. Split raw checkout planning into a prepared description with an execute method. Return that description alongside the target's repository plan, then execute it in the same existing repository lane that already runs concurrently with payload installation. Both normal Create and resume execute the same prepared description, preserving failure disposition and cleanup. Separate runtime-only harness preparation from workspace-sensitive build tool setup; the actual worker still performs both.

## Concrete Steps

From the repository root, edit the files above and colocated tests. Run `cargo fmt --all -- --check`. Run the concurrency regression with `cargo test -p brokk-mj-controller managed_checkout_overlaps_payload_and_finishes_before_failed_launch_cleanup` and the workspace discovery regression with `cargo test -p brokk-mj-worker runtime_preparation_waits_until_launch_to_discover_workspace_build_tools`, outside the sandbox. Use the normal mbx Cargo configuration; never change target storage. After final changes run full touched-crate suites and `cargo clippy --all-targets -- -D warnings`, outside the sandbox. Record actual outcomes here.

## Validation and Acceptance

Regression coverage must prove repository checkout and worker preparation are simultaneously in flight, workspace-dependent work waits for a completed checkout, and cleanup cannot run while the sibling lane still writes. Use existing isolated-store test facilities and real Git for checkout; hold boundaries with channels rather than inferred elapsed-time wins. No test may touch the default daemon or live store. Existing managed-checkout and failure regressions must still pass. INFO logs must identify phase and session, start before work, and report monotonic elapsed time when a phase ends.

## Idempotence and Recovery

The lifecycle permit remains the single owner of the session. Persist the intended checkout before writing files so crash recovery can find it. Join both setup lanes before rollback; do not start the worker when either lane fails. Retain existing clone staging and cleanup. This changes neither schema nor protocol and does not install a build into the live instance.

## Artifacts and Notes

Original measurements: clone object seeding 82ms, editable checkout 11.9s, managed harness preparation 16s, and bridge initialization 13.7s in the slower launch. This session also had 16.2s before the first timed command.

## Interfaces and Dependencies

Use existing `CommandExecutor`, `ManagedWorktree`, `ProvisionStageGuard`, and `execute_concurrent_lanes`. Prepared checkout data owns the managed checkout and optional upstream; executing it calls existing `create_managed_worktree`. No new crate, concurrency cap, migration, protocol field, or runtime fallback is needed.

Revision 2026-11-07: initial plan based on measured launch records and source dependency inspection.

Revision 2026-11-07: implemented deferred checkout, shared runtime warming, INFO phases, and regressions for overlap/rollback plus late workspace and relative bridge discovery. Validation is compiling through the existing mbx storage and waits for a shared compilation already in progress.

## Validation Results

The 57 existing checkout tests passed. The final `TZ=UTC cargo test -p brokk-mj-controller -p brokk-mj-worker` run passed all 2,420 controller tests (19 ignored), including the new overlap/cleanup case. Worker library results were 735 passed, 17 elapsed-deadline failures, and 11 ignored. Rerunning only those 17 ACP cases with the same package selection and `--lib -- <failed test names> --test-threads=1` passed all 17 in 2.90 seconds. No product changes were made for those load-sensitive deadlines. The first overlap-test fixture used `futures::executor::block_on`; switching it to `#[tokio::test]` provided the reactor needed by the complete launch path, after which the test passed.

Cargo stops on a failing library suite, so `TZ=UTC cargo test -p brokk-mj-controller -p brokk-mj-worker --bins --test '*'` completed the remaining 5 binary and 20 integration tests, all passing. Documentation tests also passed (zero cases). Workspace `cargo clippy --all-targets -- -D warnings` passed, followed by a clean controller-only Clippy run after the test fixture correction. Formatting and `git diff --check` passed. Compilation was delayed by shared mbx work; build configuration and target storage were preserved.

Revision 2026-11-07: recorded completed implementation and validation, including the fixture correction and isolated rerun results. The implementation is ready for the repository's required commit/review/push workflow.
