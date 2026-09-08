# Own test daemons through fixture teardown

This ExecPlan follows `.agents/PLANS.md` and will be implemented after the Muse work is pushed.

## Purpose / Big Picture

Running Mj's tests must not leave detached controller processes behind or delete their databases while they are still writing. A daemon is the background `mj daemon-run` process that owns a test's store. Each test fixture must stop that process and wait for it to exit before removing its temporary directory, including when an assertion panics.

## Progress

- [x] (2026-09-08) Diagnose host task exhaustion and terminate 90 confirmed stale test daemons after the user stopped old hel processes.
- [x] (2026-09-08) Trace the systematic leak to the logging integration test and identify stop-without-wait and PTY field-order races.
- [x] (2026-09-08) Push Muse as d324aaa0, then implement shared fixture ownership, process-exit completion for CLI stop, and bounded stop acknowledgement.
- [x] (2026-09-08) Focused logging (normal and panic cleanup), concurrent-start/store-divergence, and PTY tests pass.
- [x] (2026-09-08) Complete tests and Clippy pass, including bounded stop acknowledgement; final focused tests cover real PTY panic unwinding. Three repeated runs of each panic regression pass.
- [ ] Commit, merge upstream, and push the cleanup fix separately.

## Surprises & Discoveries

WSL's init.scope had a 32,768-task limit and recorded 5,554 denied creation attempts. Old hel/mj daemons accounted for approximately 27,400 threads. Remaining mj daemons used temporary test homes and deleted daemon.log descriptors; some had recreated their data directories after the fixtures removed them. Cleanup reduced the task count from roughly 17,400 to 6,800.

The logging test runs `checkpoint`, which calls `connect_or_start` and double-forks a daemon. Its plain TempDir then removes the store without stopping the daemon. The idle-exit hint does not cover this test because no client attachment ever occurred. Existing PTY and concurrent-start fixtures call `mj daemon stop`, but that command currently acknowledges cancellation without waiting for process exit. The PTY fixture also declares storage before the dashboard child, so automatic field destruction can remove storage before terminating the dashboard.

## Decision Log

Use the existing `ManagementClient::stop_and_wait` for the CLI stop command, and bound its stop acknowledgement as well as its process-exit wait. Keep the management wire protocol unchanged. Reuse existing process liveness and subprocess helpers instead of duplicating operating-system interpretation.

Use one integration-test storage guard for fixtures that can implicitly start daemons. Its destructor stops and waits before dropping the TempDir. A cleanup failure retains the directory and fails the test without causing a second panic during unwinding. Stop PTY children before their storage guard runs, and clean every fixture rather than relying on idle shutdown. Explicit daemon children in store-divergence tests already have child reaping and should retain that ownership.

Do not increase host task limits or change normal runtime worker counts to hide leaked ownership. Validation commands may bound their own concurrency with CARGO_BUILD_JOBS=4, RUST_TEST_THREADS=4, and TOKIO_WORKER_THREADS=4.

## Context and Orientation

`mj-cli/tests/logging.rs` contains the leaking implicit startup. `mj-cli/tests/termination_pty.rs` owns dashboard children and temporary storage. `mj-cli/tests/store_divergence.rs` contains both correctly reaped explicit daemon children and a concurrent-start fixture with a stop-only destructor. `mj-cli/src/main.rs` dispatches the stop CLI command, and `mj-cli/src/daemon.rs` already implements management stop-and-wait with process identity/liveness handling. `src/hel_subprocess.rs` provides shared child-process operations.

## Plan of Work

First make successful `mj daemon stop` mean the daemon has exited, with a bounded error when it cannot finish. Next introduce shared fixture storage ownership and apply it to implicit-start tests, ensuring child-before-storage destruction in PTY fixtures. Finally add behavior coverage that starts the same failing checkpoint command and verifies that normal scope exit and panic unwinding both leave no daemon or metadata, while retaining fixture data on cleanup failure.

## Concrete Steps

Work in `/home/jonathan/Projects/hel3` on the existing branch. Run focused logging, store-divergence, and PTY integration tests, plus management timeout tests. Inspect process state before and after repeated fixture runs. Then run bounded `cargo test` and `cargo clippy --all-targets -- -D warnings` outside the restricted sandbox. Run rustfmt and review the diff before committing only the cleanup change and this plan. Push to origin/master as authorized.

## Validation and Acceptance

The failed checkpoint still creates its private logs and reports the expected unknown-session error. Leaving its fixture scope, including through a caught panic, must stop the daemon before the temporary root disappears. PTY failures must reap the dashboard before storage cleanup. Existing concurrent-start and store-divergence behavior must continue passing. Repeated focused runs must not add deleted-fixture daemon processes.

## Idempotence and Recovery

Tests use isolated temporary homes. Preserve those homes when cleanup cannot prove the writer has stopped; report the path and failure. Never delete a live writer's files to stop it. Do not signal unrelated user daemons or workers. Keep this fix separate from the preceding Muse commit and release.

## Outcomes & Retrospective

Diagnosis and authorized host cleanup are complete. Muse was pushed separately as d324aaa0. The source fix passes complete tests, Clippy, focused integration checks, and three repeated logging/PTY panic-cleanup runs. A read-only review confirmed fixture ownership, PTY destruction order, and portability of the lock/process checks. Seven additional confirmed deleted-fixture daemons were cleaned up, bringing the cleanup total to 97; task usage was roughly 7,600 afterward. The fix is ready for its separate commit and push.
