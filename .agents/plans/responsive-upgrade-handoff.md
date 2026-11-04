# Keep daemon upgrades responsive through failed child connections

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation and validation proceed.

## Purpose / Big Picture


An upgrade must report changing wait ages every second and must not spend minutes waiting for a failed child worker connection. The daemon is the controller process; the node runs its subprocesses, and workers retain agent turns and journals independently. A parent may ask to close a child while an upgrade starts. That accepted close must survive controller replacement without losing the child's final journal or executing a second, unrelated request.

## Progress


- [x] (2026-11-04) Identify the observed failure: one durable `close_agent` held delegation and lifecycle admission while a failed proxy consumed the 300-second hello deadline.
- [x] (2026-11-04) Identify the stream ownership defect: `mj-core/src/node.rs::run_child` joins the stdin feeder before waiting for child exit, so an open request body prevents reporting an already-exited child.
- [x] (2026-11-04) Make child exit terminate stdin feeding while output drains. All six real node protocol tests pass, including a failed child with open stdin and 300 KB of stderr.
- [x] (2026-11-04) Make child close worker waits interruptible and recoverable across daemon replacement. The isolated two-process regression passes in 1.19 seconds; existing child close and cancellation regressions also pass.
- [x] (2026-11-04) Refresh interactive handoff notices every second while retaining bounded stderr and diagnostic logging. The real management-transport regression passes for both replacing and queued clients.
- [x] (2026-11-04) Pass focused regressions, full touched-crate suites, formatting, diff checks and workspace Clippy with warnings denied.

## Surprises & Discoveries


The remote proxy had already reported a refused worker socket. The node held the stream open waiting for more stdin rather than reporting child exit. Reducing the relay timeout would mask that ownership defect. The existing child close persists `Closing` before syncing its journal, and startup already recovers interrupted closes. `CommandExecutor::begin_resumable_work` already coordinates operations that yield upgrade admission. Child close remains recoverable through teardown, so it need not reacquire admission for further worker waits. Startup recovery and the parent's durable replay previously used different in-memory keys; both must join one child-close owner under the lifecycle lock.

## Decision Log


- Decision: Fix exit ownership in the node rather than add client polling or reduce healthy startup deadlines. Rationale: the process owner knows when its child exits and must stop reading stdin then. Date: 2026-11-04.
- Decision: Scope lifecycle changes to the durable child close, using existing close recovery and lifecycle ownership. Rationale: this incident does not authorize cancelling accepted nonrecoverable operations or bypassing journal capture. Date: 2026-11-04.
- Decision: Coalesce child suspensions under the existing lifecycle owner regardless of the caller key, and release both the outer delegation hold and the startup-recovery clone for these durable closes. Rationale: releasing only one of these nested holds would still block replacement, while separate recovery and replay could refuse each other or race. Date: 2026-11-04.
- Decision: Keep work on the current master branch and use existing mbx Cargo shims. Rationale: repository instructions require both; never redirect target storage. Date: 2026-11-04.

## Outcomes & Retrospective


The node reports failed child exits immediately with stderr retained, durable child closes release upgrade admission and resume under one lifecycle owner, and the splash refreshes blocker ages every second for both replacing and queued clients. Review also preserved reset detection after a still-running child closes both output pipes: the child retains the last output-channel sender until exit, so the forwarding owner continues observing resets. The extended process-group regression passes. The preliminary full build was interrupted for that correction; the final full touched-crate suites and workspace Clippy passed, with output in `target/responsive-upgrade-tests.log` and `target/responsive-upgrade-clippy.log`. Formatting and diff checks passed. All 18 isolated daemon-startup regressions passed, as did the controller suite (2,264 passed, 19 existing skips), core and CLI suites and terminal regressions. No live daemon, worker, mount or session data has been changed. Creating a tracking issue was rejected by automatic review because its proposed body included incident identifiers; no issue was created.

## Context and Orientation


`mj-core/src/node.rs::run_child` forwards a subprocess's stdin, stdout, stderr and final status over HTTP/2. Its real protocol tests are colocated. `mj-controller/src/daemon/delegation.rs` runs durable child tool requests; `mj-controller/src/server_runtime/api.rs` persists prepared effects and results. `mj-controller/src/daemon/close.rs::close_subagent_request` joins a lifecycle operation that serializes all child changes. `mj-controller/src/controller/lifecycle/subagent.rs` persists the close intent, synchronizes the journal, seals and closes the relay, and only then removes private worker storage. `mj-controller/src/daemon/process.rs` starts recovery of persisted closes. `mj-cli/src/daemon.rs` polls handoff admission and emits startup notices, while `mj-cli/src/splash.rs` renders the latest notice.

## Plan of Work


First, change the node's stream supervisor to observe child exit alongside stdin feeding, stop feeding after exit, drain both output pipes, and send the final status without waiting for the caller to close stdin. Preserve stream reset cancellation and process group teardown. Add a real failed command test with an open stdin and more than 64 KB of stderr.

Next, yield the child close's upgrade admission only after close intent is stored, make long relay waits respond to lifecycle shutdown, and preserve startup recovery of interrupted state. The close stays recoverable through `Destroying` and teardown; shutdown drops its relay wait and retains the journal unless its contents have already been synchronized. Durable delegation requests remain pending because the daemon joins and stops delegation jobs before interrupting lifecycles. Coalesce startup recovery and replay under one child-close owner. A two-process isolated regression blocks a close on a silent proxy in the old process, then recovers its final journal and exact request in the replacement process while startup recovery and replay overlap.

Finally, update handoff notice delivery for the splash at one-second intervals, independently of the 30-second diagnostic log cadence. Apply it to both the upgrading client and clients waiting on the startup lock. Extend the existing real startup regression so changing blocker text reaches waiting clients before 30 seconds.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir`. Use the installed `cargo` mbx shim. Run every Cargo test with sandbox escalation and keep all build storage at its configured location. Use the existing test child isolation and explicit `--instance upgrade-test` for real client and daemon commands.

    cargo test -p brokk-mj-core node::tests::
    cargo test -p brokk-mj-controller <affected close or handoff regression>
    cargo test -p brokk-mjolnir splash_refreshes_blockers_each_second_for_replacing_and_queued_clients

Once focused checks pass, run `cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mjolnir` once and `cargo clippy --all-targets -- -D warnings`, plus `cargo fmt --all -- --check` and `git diff --check`.

## Validation and Acceptance


A failed real node command must return all of its large stderr and an unsuccessful exit within a few seconds even though the client keeps stdin open. Resetting a stream must still terminate the process group. A child close blocked on a nonanswering worker must allow daemon admission to close, end promptly at shutdown without removing its journal, and finish through the replacement's existing recovery using the same delegation request. A waiting startup client must display a newly reported age within approximately one second on the splash; noninteractive stderr and diagnostic logs remain bounded. Existing isolated migration, daemon upgrade and terminal draft preservation tests must continue to pass.

## Idempotence and Recovery


All tests use temporary configuration and data stores, never the host default instance. Child close remains restartable from its persisted `Closing` or `Destroying` record. A worker's storage is removed only after its final conversation is durable and its owning process has stopped. No database schema or wire-format changes are planned. Stage only changed files; push rejection is resolved by fetching and merging origin/master without rebasing.

## Artifacts and Notes


The observed upgrade accepted handoff after 289,969 ms, immediately after the old 300-second hello timed out and the child close completed. Both displayed labels belonged to that one close.

The first restart-regression run exposed a fixture requirement: the recorded worker completes Close only when its late-journal notification mode is enabled. Without it the fixture remained Closing forever. Enabling that existing mode made both recovery and same-request replay complete in under a second; the full Cargo invocation of the corrected test passed in 1.19 seconds.

## Interfaces and Dependencies


Reuse Tokio futures and the existing node HTTP/2 stream, lifecycle control, upgrade admission Work guard, durable delegation effect and close recovery. No new crate, dependency, migration or protocol revision is required unless implementation evidence establishes a missing durable boundary.

Revision note (2026-11-04): Record the three user-requested fixes and the stream ownership cause before implementation.

Revision note (2026-11-04): Record passing node regressions, the fully resumable child close, and the serialized recovery/replay decision.

Revision note (2026-11-04): Record passing focused restart and notice regressions, the fixture correction, and the full validation commands now running.

Revision note (2026-11-04): Keep stream reset detection owned by the output forwarder through child exit, including when stdout and stderr close early; extend and pass the process-group regression before final validation.

Revision note (2026-11-04): Record successful final full validation and complete the implementation record. Commit and push this validated change on the current branch, then dispatch nightly.yml on master for the platform-sensitive process-stream change; delivery results are reported with the commit.

Revision note (2026-11-04): Merge concurrent container-native startup changes without conflicts. Their handoff now asks Identity before PrepareUpgrade, so teach the notice regression's daemon fixture that response. The merged child-close regressions, corrected notice regression, all 18 real startup regressions, and workspace Clippy passed; evidence is in `target/responsive-upgrade-merge-tests.log`, `target/responsive-upgrade-merge-notice-tests.log`, `target/responsive-upgrade-merge-startup-tests.log`, and `target/responsive-upgrade-merge-clippy.log`. Push this fixture follow-up and replace the cancelled original platform dispatch.
