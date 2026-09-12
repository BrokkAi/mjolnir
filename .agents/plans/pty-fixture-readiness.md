# Make concurrent PTY tests reliable

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Keep real pseudo-terminal (PTY) coverage for terminal restoration and prompt exit while removing startup races. Parallel tests must reach an explicit ready screen before starting their five-second interaction and termination deadlines. Failures must include child status and daemon log tails.

## Progress

- [x] Read fixture and daemon startup paths; identified database ownership being mistaken for readiness.
- [x] Measured six concurrent worker snapshots at 3107–3426 ms each.
- [x] Implemented separate startup readiness, child-exit diagnostics, bounded log tails, PTY ownership, descriptor close-on-exec, and an explicit failing fixture worker. Added immediate startup-failure regression.
- [x] Eight tests passed in five repeated runs with eight parallel threads (5.47–6.86 seconds); strict clippy and formatting passed.
- [x] Full pre-merge workspace tests passed; committed PTY changes as `8c5f30c0`.
- [x] Merged upstream `f23c1196` and resolved its old module references against the ownership refactor.
- [x] Merged-tree cargo test and strict clippy passed; interactive-terminal PTY run passed all eight cases in 5.74 seconds. Merge resolution is ready for commit and push to origin/master.

## Surprises & Discoveries

The daemon locks its store before copying and hashing worker binaries and publishing its endpoint. The native development worker is 96 MB. Measured snapshot time alone was 3107–3426 ms per daemon across six parallel dashboards. Baseline PTY tests failed six of seven cases; separate readiness and terminal ownership passed all seven in 8.68 seconds before the fixture worker isolation was added.

## Decision Log

Keep the five-second interaction/shutdown requirement. Give fixture setup a separate bounded readiness phase that drains terminal output and answers device queries. Do not serialize tests or remove PTY coverage. User explicitly authorized pushing. Use a tiny explicitly failing worker in these lifecycle tests: successful worker execution is outside their scope, and selecting installed development artifacts makes initialization vary by host. Bound fixture Tokio and Rayon pools to two threads each to avoid host-sized pools multiplying across subprocesses. Remove redundant one-second exit speed assertions; all paths retain the shared five-second exit deadline and terminal-state/order checks. Keep production snapshot immutability unchanged; retain one structured snapshot-duration log for diagnostics.

## Outcomes & Retrospective

All existing terminal behaviors and the new startup-exit regression pass concurrently. Fixture worker snapshots dropped to tens of milliseconds. The pre-merge full workspace suite passed. Upstream branch-export authentication commit `f23c1196` required adapting `hel` module references to `mj_core`, `worker_runtime`, and the controller targets module. Combined-tree cargo test and strict clippy passed. An additional interactive-terminal run passed all eight PTY tests in 5.74 seconds. No implementation work remains; publish the validated merge to origin/master.

## Context and Orientation

`mj-cli/tests/termination_pty.rs` launches isolated dashboard processes connected to real PTYs. `mj-cli/tests/common/mod.rs` owns temporary daemon storage and stops writers before removing files. `mj-controller/src/controller/worker_binary.rs` snapshots workers before the daemon accepts requests. Store ownership only protects cleanup; the dashboard screen proves end-to-end readiness.

## Plan of Work

Measure snapshot duration with structured tracing and preserve failure logs before fixture cleanup. Introduce a fixture readiness method using a separate startup deadline, child-exit polling, and terminal query responses. Leave the deliberately early teardown test able to stop after store acquisition. Address measured fixture dependencies without changing production worker immutability guarantees.

## Concrete Steps

From `/home/jonathan/Projects/hel`, run elevated `cargo test -p brokk-mjolnir --test termination_pty`, repeat with parallel test threads, then elevated `cargo test` and `cargo clippy --all-targets -- -D warnings`. Format with cargo fmt. Commit only changed files on the current branch and push its upstream.

## Validation and Acceptance

All seven existing PTY behaviors must pass concurrently over repeated runs. Early teardown still removes storage only after stopping writers. Termination remains bounded by five seconds. The default workspace suite and strict clippy must pass.

## Idempotence and Recovery

Tests own isolated temporary stores. Preserve existing cleanup ordering. Never remove a running daemon's storage or touch user sessions. Repeating tests is safe.

## Artifacts and Notes

Diagnostic output is in `target/pty-baseline.log`, not versioned. Prior runs failed before emitting any terminal output.

## Interfaces and Dependencies

Reuse existing PTY drain and terminal protocol constants, `std::process::Child::try_wait`, and the common daemon-storage guard. No additional crate or dependency is required.

Initial plan records investigation and validation scope.

Revision: measured worker snapshot overhead and recorded the first successful concurrent readiness run; added fixture worker isolation to remove the unrelated host dependency.

Revision: recorded five parallel passes, strict checks, and the additional upstream merge needed before publication.

Revision: full pre-merge validation passed; recorded upstream authentication merge and the required post-merge checks.

Final revision: recorded successful full merged-tree validation and the interactive-terminal regression run.
