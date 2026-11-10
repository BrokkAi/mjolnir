# Overlap managed checkout and worker staging

This ExecPlan follows `.agents/PLANS.md`. Keep its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections current.

## Purpose / Big Picture

New local sessions spend seconds serially materializing an editable Git checkout and staging their worker and profile. Run those independent operations concurrently. Log launch phase boundaries at INFO so operators can account for controller loading, publication, credentials, and readiness as well as existing subprocess timings. A harness is the agent program; the worker is its session-owned supervisor.

## Progress

- [x] (2026-11-07) Measured recent top-level launches at 76.1 and 46.7 seconds to session open. Verified Git objects share hardlinks while editable files do not.
- [x] (2026-11-07) Split persisted checkout planning from materialization and moved materialization into the concurrent repository lane.
- [x] (2026-11-07) Added INFO phase timers covering admission, controller work, both setup lanes, full harness preflight, and readiness.
- [x] (2026-11-07) Initial tests and Clippy completed; the controller suite passed, and 17 worker ACP deadline failures passed targeted serial reruns. All remaining binary/integration/doc tests passed.
- [x] (2026-11-07) Review identified checkout-dependent runtime selection and weakened upgrade preflight. Removed worker changes; stage files concurrently, then run the original full harness preflight after the join. Also made admission failure timing use the drop-based timer.
- [x] (2026-11-07) Validated the review corrections and prepared them for the required clean review and push to origin/master.

## Surprises & Discoveries

The existing two-lane helper joins both lanes on errors and propagates the lifecycle permit to its child thread. Remote bundle checkout already runs in that helper; local raw checkout previously happened before it.

The prepare-harness command does more than install shared packages. It discovers workspace build tools, resolves relative executable paths, and selects ambient runtime programs from PATH. Even runtime selection can depend on the unfinished checkout. The same command also preflights upgrades before the old worker is stopped. Keeping the worker command unchanged and moving the whole preflight after the join preserves both contracts.

Separate Cargo package selections produced different test feature graphs and repeated expensive compilations through mbx. Keep both touched package selections together during follow-up tests; choose tests with libtest filters. Preserve normal mbx configuration and target storage.

## Decision Log

- Decision: Return a prepared managed-checkout description from planning, persist that intent, and execute it in the existing repository setup lane. Rationale: one lifecycle owner decides the checkout once; immutable plans can execute concurrently without changing controller state. Date: 2026-11-07.
- Decision: Return the pending full harness preflight from worker file staging. Run it only after repository and payload lanes join. Existing worker preparation callers still run full preflight immediately through their existing wrapper. Rationale: parallelize the operations that are actually independent, while preserving workspace-sensitive selection and upgrade validation. This supersedes the initial attempt to introduce runtime-only preparation in the worker. Date: 2026-11-07.
- Decision: Use monotonic INFO start/end timers, including failed scopes. Admission starts without a session ID and attaches the registered ID before the success scope ends. Rationale: errors and cancellation must not leave controller phases unaccounted for. Date: 2026-11-07.

## Outcomes & Retrospective

The implementation now overlaps clone materialization with profile/binary staging. Both lanes retain the existing lifecycle owner and join before full harness preflight, worker start, or rollback. No new worker command, protocol, schema, concurrency cap, or installation behavior is needed. Review corrections passed 124 affected regressions (3 ignored), workspace Clippy, and the complete final controller suite: 2,420 passed, 19 ignored, zero failures. No live launch speedup has been measured or claimed.

## Context and Orientation

`mj-controller/src/daemon/create.rs` admits and registers sessions and starts their supervised lifecycle. `mj-controller/src/launch_timing.rs` implements the drop-based phase timer. `mj-controller/src/controller/provisioning.rs` joins repository setup and worker payload staging with `execute_concurrent_lanes`, then runs full harness preflight and starts the worker. `PreparedWorkerPayload` carries the already-generated launch configuration so preflight does not recompute it.

`mj-controller/src/controller/worktree.rs` plans and persists a `PreparedManagedCheckout` before its existing Git materialization runs. Bare targets name an existing machine; their empty target provisioning plan does not require a populated checkout. `mj-controller/src/controller/worker_binary/launch.rs` separates file staging from completing its optional pending harness preflight. Its existing `prepare_worker_files_for` wrapper retains full preparation for other callers, including upgrades. `worker_binary/upgrade.rs` exposes the existing preflight helper within the controller. The final change does not modify mj-worker.

## Plan of Work

Keep raw checkout planning and durable target setup serialized. In the existing concurrent lanes, execute the prepared checkout and stage worker/profile files. After both succeed, consume the returned payload to run the original full prepare-harness command. On either lane failure, join before rollback; never start a worker. Preserve the existing clone staging/rename and cleanup mechanisms. Resume executes the same prepared checkout before it returns from repository setup.

Add phase timers around admission/registration, lifecycle scheduling and controller load, publication, credentials, binary/harness preflight, checkout planning and materialization, payload/profile staging, workspace initialization, full harness preparation, and readiness. Timer ends describe elapsed scope duration rather than asserting success.

## Concrete Steps

From the repository root, run `cargo fmt --all -- --check` and `git diff --check`. Outside the sandbox, run focused validation with `TZ=UTC cargo test -p brokk-mj-controller -p brokk-mj-worker --lib -- controller::provisioning::tests controller::worker_binary::tests hung_launch_commands_leave_the_daemon_serving_and_end_when_cancelled`. This keeps the package feature graph stable while testing the changed setup and upgrade boundaries. Run workspace `cargo clippy --all-targets -- -D warnings` and the final controller suite after the review corrections. Reuse the controller test executable printed by the focused Cargo invocation to run all controller cases without changing the package feature graph: this run used `TZ=UTC target/debug/deps/mj_controller-18f16e366468d021`. Its existing tests continue to own their isolated stores and named instances. Commit only changed files on the current branch, request review, then push to origin/master. Dispatch nightly.yml on master for platform coverage.

## Validation and Acceptance

The isolated `managed_checkout_overlaps_payload_and_finishes_before_failed_launch_cleanup` regression uses a real Git clone. Its held checkout waits until worker file staging begins, proving overlap rather than inferring it from a faster timer. Full harness preflight asserts that the clone has been published and checkout completed, then deliberately fails; rollback must remove the completed clone while leaving the source intact. Existing worker preparation and upgrade tests must preserve their full preflight contract. INFO logs must identify phase and elapsed duration; admission errors also end their timing scope. All daemon/store tests use existing isolated-store facilities or a named instance, never live/default data.

## Idempotence and Recovery

Retain the existing lifecycle permit as the single owner of session mutation. Persist intended checkout paths before writing files so recovery can find them. Join both lanes before rollback or full harness preparation. The existing clone staging directory is renamed only after checkout succeeds and is removed on failed materialization. No live daemon or store is changed to validate this work.

## Artifacts and Notes

The slower measured launch spent 82ms seeding hardlinked Git objects, 11.9s writing the checkout, 16s preparing its managed harness, and 13.7s initializing the bridge. This session also had an uninstrumented 16.2s before the first timed command. The new timers make such controller work visible. Harness preflight remains after checkout because its decisions read the workspace.

Initial validation: 57 checkout regressions passed. The first full run passed 2,420 controller tests (19 ignored). Worker library results were 735 passed, 17 elapsed-deadline failures, and 11 ignored; those 17 cases all passed serial reruns in 2.90s. Remaining binary tests (5), integration tests (20), and documentation tests (zero cases) passed. Workspace Clippy passed, followed by clean controller Clippy after correcting the new test fixture to use Tokio. These initial results precede the review corrections. After the corrections, 124 affected tests passed (3 ignored), including real checkout/staging overlap, preflight waiting for the published checkout, upgrade preflight, and daemon responsiveness/cancellation. Workspace Clippy and formatting passed. Running the controller executable just built by the focused Cargo command with no filters passed the complete suite again: 2,420 passed, 19 ignored, zero failures, in 144.67 seconds. The admission timer owns the registered session ID when attached, so its failure scope can outlive registration without borrowing a later local value.

## Interfaces and Dependencies

Use existing `CommandExecutor`, `ManagedWorktree`, `WorkerLaunchConfig`, `ProvisionStageGuard`, and `execute_concurrent_lanes`. Prepared checkout data owns its managed checkout and optional upstream; execution calls existing `create_managed_worktree`. A prepared payload owns placement and the optional launch configuration requiring harness preflight. `prepare_worker_files_for` remains the complete-preparation entry point; only new-session setup explicitly separates staging from full preflight. No new crate or worker API is required.

Revision 2026-11-07: replaced runtime-only worker preparation after review demonstrated workspace-dependent selection and upgrade preflight requirements; retained parallel file staging and moved the complete existing preflight after the setup join. Admission timing now ends on failures too.

Revision 2026-11-07: completed validation of the review corrections and prepared the final implementation for commit/review/push. Full worker preparation is restored to its original behavior; the final implementation changes only the controller and its internal plan.
