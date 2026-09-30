# Make asynchronous test fixtures reliable under parallel execution

This ExecPlan follows `.agents/PLANS.md` and must be kept current as implementation proceeds.

## Purpose / Big Picture


The user requested fixing six tests that failed in broad validation and passed individually. The suite should check checkpoint ordering, restart records, classifier results, and viewer startup without treating arbitrary scheduling delays as application failures. All production timeouts and lifecycle behavior remain unchanged. Success is an ordinary parallel `cargo test` run passing these tests, plus a deliberate viewer stall proving that the intended timeout still fires.

## Progress


- [x] (2026-09-30) Read all six failures and their fixtures; distinguish expected viewer disconnect from worker watchdog expiration.
- [x] (2026-09-30) Inspect shared relay waits, classifier initialization, worker startup, and existing test isolation.
- [x] (2026-09-30) Run the unchanged parallel worker suite: 694 passed, 10 ignored. Reproduce all four originally failing relay tests under deliberate CPU contention.
- [x] (2026-09-30) Make viewer stalling explicit and synchronize the checkpoint test with its persisted connection and configured barrier.
- [x] (2026-09-30) Isolate relay fixtures from the ambient classifier and use bounded, descriptive waits for external I/O and startup records on launch-watchdog failure.
- [x] (2026-09-30) Pass ordinary parallel `cargo test`, including all six original failures, integration targets, and doctests; pass formatting and clippy with warnings denied.
- [x] (2026-09-30) Run the original five worker failures together five times on one CPU, and the viewer failure/timeout regression five times on one CPU; all passed.
- [x] (2026-09-30) Review the final diff and prepare the required commit on the existing `hel3` branch.

## Surprises & Discoveries


The viewer's fake server keeps returning `Starting` while the client is supposed to time out. A queued reply may then reach a closed connection, and the fixture panics on `Broken pipe`. Three restart tests use a five-second bound around full worker startup, which includes hashing the debug executable and resolving a real login environment. The checkpoint fixture and other relay tests resolve the default network classifier even though classifier decisions are irrelevant to their assertions. The failed verdict test uses an explicitly gated local HTTP server but allows only two seconds for receiving its request.

The unchanged worker suite passed a normal run in 394.64 seconds. Constraining its existing binary to one CPU while running 120 relay-fixture threads reproduced all four original relay failures and eleven additional failures. Most were short watchdogs; other stress failures included the real five-second login-environment deadline and a paused-clock fixture whose wall-clock-based retry deadline shifted under contention. The constrained run is diagnostic evidence, not the acceptance configuration. The normal parallel suite remains the acceptance check.

## Decision Log


- Decision: Correct fixture ownership and synchronization before adjusting watchdogs.
  Rationale: A timeout's expected disconnection is a test success, and a checkpoint event must be observed before ordering assertions. Larger sleeps cannot establish those facts.
  Date/Author: 2026-09-30 / Codex.
- Decision: Preserve production deadlines and exercise the same relay coordinator with an explicit absent classifier in protocol tests.
  Rationale: These tests need local relay behavior, while separate verdict fixtures already supply controlled local endpoints. Process-global environment changes would race with other tests.
  Date/Author: 2026-09-30 / Codex.

## Outcomes & Retrospective


Implementation and validation are complete. The normal parallel workspace test run exited zero, including all six original failures, integrations, and doctests. Clippy with warnings denied and formatting passed. All original regressions passed five additional runs on one CPU. The viewer owns an explicitly unanswered request until the client closes it, so its response cannot race the expected disconnect. Protocol tests exercise the existing coordinator with no default classifier, and the checkpoint test asserts ordering only after observing durable progress. Shared test watchdogs have descriptive failure messages; worker startup failures also report their recorded startup step. No production deadline, instance data, or host configuration changed.

## Context and Orientation


`mj-cli/src/daemon.rs` contains `viewer_fixture` and the viewer timeout regression. `mj-worker/src/worker_runtime/relay_tests.rs` contains checkpoint and restart regressions and an existing shared predicate wait. A predicate wait repeatedly checks the relay owner's reported state until it satisfies a condition. `mj-worker/src/worker_runtime/unix/dispatch.rs` runs the relay coordinator, the task that writes harness events and admits commands. Its inner function already accepts an optional classifier client. `mj-worker/src/worker_runtime/unix/dispatch/verdict_tests.rs` owns gated local HTTP fixtures. `mj-worker/src/test_support.rs` contains helpers shared by worker tests. All fixture relay/profile homes use temporary directories; no host instance is involved.

## Plan of Work


First change the fake viewer so exhaustion of its scripted statuses means an unanswered status request. It waits for client EOF instead of writing after the intended timeout. Keep unexpected response-write failures visible and retain the elapsed-time assertion.

Next let protocol fixtures call the existing inner coordinator with no classifier through a local helper. Do not change the production wrapper or global environment. In the checkpoint regression, send a connected event and await its persisted ordinal before asserting that configuration is required. Await the configured checkpoint through the shared predicate wait and check the runtime command channel directly.

Finally use one thirty-second test watchdog for external I/O and meaningful error labels, following the existing verdict-client test convention. This is a hang detector, not a production performance requirement. Apply it to the failing HTTP fixture and related waits, the existing relay predicate helper, and worker startup failure fixtures. Include the worker startup record in a startup-watchdog failure. Keep paused-clock timing assertions separate from real socket and process waits.

## Concrete Steps


Run from `/home/jonathan/Projects/mjolnir3`. Every `cargo test` runs with elevated permissions and existing isolated fixtures. Preserve mbx target selection. Reproduce with `cargo test -p brokk-mj-worker --lib`, then run focused checkpoint, restart, verdict, and viewer regressions after edits. Finish with `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Logs go under `/mnt/optane/mj-test-stability-*.log`. Stage only files changed for this work and commit on `hel3`; no push or branch change is authorized.

## Validation and Acceptance


Checkpoint tests must prove a connected session cannot admit its barrier before configuration, and configuration admits it without a harness command. Restart tests must retain the exact marker count and recovery ordering assertions. Classifier tests must still prove completion is published while the HTTP reply is held, stale results cannot affect newer work, and uncertainty/failure retains activity. Viewer tests must report a scripted error and time out on an unanswered status request, with the fixture joining successfully after client closure. Ordinary parallel workspace tests and clippy must pass; record any additional failures and investigate them rather than relying on isolated reruns as acceptance.

## Idempotence and Recovery


The changes affect tests and test-access visibility only. Fixtures remain isolated and repeatable; they do not launch an installed daemon or alter user configuration. Watchdogs stay bounded. Stop owned child tasks before temporary files are removed. Do not kill other contributors' processes or alter host build configuration.

## Artifacts and Notes


Original broad run: five worker failures were `Elapsed(())` at one-, two-, or five-second fixture deadlines. The CLI fixture panicked on `Broken pipe` at its unconditional response-write unwrap, then propagated that panic through its joined task. The original logs are `/mnt/optane/mj-configuration-skill-tests.log` and `/mnt/optane/mj-configuration-skill-cli-tests.log`.

The unchanged suite's normal passing reproduction is `/mnt/optane/mj-test-stability-before.log`. The CPU-contention diagnostic is `/mnt/optane/mj-test-stability-before-constrained.log`: 90 passed, 15 failed, 1 ignored, using the identical pre-change test binary and `taskset` on one allowed CPU. It includes the original checkpoint and three restart-marker failures.

Final `cargo test` exited zero; `/mnt/optane/mj-test-stability-tests.log` records the complete run. In particular, worker library tests passed 694 with 10 existing ignores, and CLI unit tests passed 254. `cargo clippy --all-targets -- -D warnings` exited zero on the dev profile, recorded in `/mnt/optane/mj-test-stability-clippy.log`. Formatting and diff checks passed. `/mnt/optane/mj-test-stability-original-five-constrained.log` records 25 passing worker regressions (five together, repeated five times); `/mnt/optane/mj-test-stability-viewer-constrained.log` records five passing viewer regressions on the exact CLI binary from the final workspace run.

## Interfaces and Dependencies


Use the existing Tokio runtime, channels, `run_relay_coordinator_with_verdict`, `DurableRelay`, and worker startup record. Any shared watchdog helper belongs in `mj-worker/src/test_support.rs` and accepts a future plus a short description. No new crate, dependency, environment switch, database revision, or protocol change is needed.

Plan created 2026-09-30 after reading the failures and confirming a viewer fixture race and worker waits that conflate scheduling with tested behavior.

Plan updated 2026-09-30 after implementation and diagnostic reproduction. Record that ordinary pre-change tests passed but the original relay failures reappeared under CPU contention, and keep deliberate stress distinct from normal validation.

Plan updated 2026-09-30 after complete validation. The normal parallel suite and all required checks passed, as did repeated constrained runs of every originally failing fixture. The reviewed change is ready for the required current-branch commit; no push is authorized.
