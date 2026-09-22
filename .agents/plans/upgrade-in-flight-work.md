# Preserve in-flight work during automatic upgrades

This living ExecPlan follows `.agents/PLANS.md` and extends the startup work recorded in `.agents/plans/seamless-upgrade-startup.md`.

## Purpose / Big Picture


An installed upgrade must wait automatically until replacing each process is safe. Agent turns, steering, answers, background jobs, reviews, and provisioning must continue while the upgrade waits. A timeout is never permission to kill work. Idle workers upgrade independently; a daemon (the persistent controller) must finish its own accepted work before relinquishing the database writer. Existing isolated startup migration and terminal draft preservation remain required.

## Progress


- [x] (2026-09-22) Audit worker leases, relay checkpoint barriers, and daemon Stop. A generic lease currently cancels reviewer tasks before checking idle, and queues every submission, including steering. Automatic daemon replacement currently invokes cancellation immediately.
- [x] (2026-09-22) Add idle-only worker reservation, worker-side atomic idle admission, and regression tests for steering and changing activity.
- [x] (2026-09-22) Separate automatic daemon upgrade from explicit Stop; protect accepted foreground/background operations through handoff and keep controls available while waiting.
- [x] (2026-09-22) Validate compatibility, abandoned reservations, prompt ordering, concurrent startup, and long-running work in isolated named instances. Verify explicitly refused requests retain their command identity across reconnect; ambiguous acknowledgements are not replayed.
- [x] (2026-09-22) Run formatting, full dev-profile Cargo tests and workspace Clippy; include the validated changes in the local commit on the current branch without pushing.

## Surprises & Discoveries


`mj-controller/src/controller/worker_restart.rs::upgrade_session_worker` borrows the managed connection before rechecking idle. `ActorCommand::Lease` in `session_manager/actor.rs` cancels reviewer tasks unconditionally. The existing `IdleWorkspaceLease` already combines a managed connection with a worker checkpoint barrier, including disconnect recovery; reuse that mechanism rather than inventing a second durable barrier format.

Daemon `Stop` cancels lifecycle operations and starts a ten-second process exit watchdog. That is appropriate for an explicit stop, but it cannot implement a safe automatic upgrade. Historical executables cannot acquire a new admission handshake retroactively; compatibility handling must explicitly preserve that limitation rather than silently treating unknown activity as idle.

## Decision Log


Decision: Separate upgrade admission from user-requested cancellation. Rationale: steering and cancellation remain available to the existing turn until it completes; upgrades have no authority to change that turn. Date: 2026-09-22.

Decision: Reuse existing checkpoint barrier persistence and disconnect cleanup for worker reservation. Rationale: command identity, journal replay, and recovery already exist and should have one implementation. Date: 2026-09-22.

## Outcomes & Retrospective


The implementation adds daemon protocol 33 safe handoff and worker protocol 21 atomic idle reservation. Daemon activity ownership follows lifecycle operations, worker activity, review and continuation tasks, completion notifications, recovery, startup queues, finite HTTP responses, and detached web operations. Waiting keeps controls available and has no forced-stop deadline. The idle decision and final admission closure share one mutex. Worker preparation stages a separate executable before reserving the idle worker, then promotes it under the reservation. An abandoned reservation releases through the existing connection-disconnect mechanism.

The first full workspace suite passed. The subsequent suite covering the final retry and staging changes found one new fixture error: the temporary worker directory did not end in the session ID required by existing locator validation. The fixture has been corrected and its focused rerun passed (`target/upgrade-in-flight-staging-test.log`). The workspace Clippy check passed with warnings denied. Logs are in `target/upgrade-in-flight-full-tests.log`, `target/upgrade-in-flight-final-tests.log`, and `target/upgrade-in-flight-clippy.log`. The final suite finished with only the corrected fixture failure; all other tests passed. Formatting and diff checks passed. No live instance was changed.

Compatibility limit: protocol versions before 33 can report observed idle state but cannot atomically close admission. Their bootstrap uses an activity snapshot before Stop and never escalates a failed automatic handoff to a signal; it cannot provide the new race-free guarantee retroactively. A continuously busy or unreachable process can defer an automatic upgrade indefinitely. Unknown activity is not evidence of idle. Database revisions and journal persistence formats are unchanged by this implementation.

Scope note: after the user raised the task duration, scope was frozen. Validation and the implementation record are complete; this plan accompanies the required local commit. No further subsystem audit was performed.

## Context and Orientation


The CLI in `mj-cli/src/daemon.rs` serializes startup under a file lock, replaces stale daemons, and waits for database readiness. The daemon serves local protocol requests in `mj-controller/src/daemon/serve.rs`; lifecycle tasks live beyond the request that created them. Session manager actors own each worker connection. Their leases temporarily hand that connection to lifecycle operations. Worker relay commands and requests are defined in `mj-core/src/relay`; worker-side admission runs under the relay mutex in `mj-worker/src/relay/requests.rs`. Checkpoint barriers prevent new execution and are released when their owning controller connection disappears.

## Plan of Work


Milestone one changes managed connection admission so upgrades borrow only an already-idle session without cancelling reviewers. Add a relay request that atomically verifies idle and admits the existing checkpoint barrier, with a protocol capability check. Use the shared idle workspace lease for worker replacement and recheck before stopping. Busy or uncertain state defers replacement and returns the live connection immediately. Tests must prove steering remains addressed to its original turn and replacement cannot use a stale idle observation.

Milestone two introduces an upgrade-specific daemon handoff, independent of explicit Stop. Track accepted operations until completion, including detached lifecycle work, reviews, startup prompt delivery, and recovery work. New clients wait without imposing a destructive deadline. Keep normal controls available while work is active. Preserve command identities and database writer exclusivity through replacement. Audit legacy protocol fallback explicitly; never infer safety from a failed activity query.

Milestone three exercises failure and concurrency with existing real-process and PTY fixtures, retains historical database migration coverage, and documents the final contract in AGENTS.md. Each coherent validated milestone is committed on the existing branch.

## Concrete Steps


From `/Users/ryansvihla/code/mjolnir`, inspect the modules named above, implement focused behavior tests beside the affected code, and run focused Cargo tests with sandbox escalation. Then run:

    cargo fmt --all -- --check
    cargo test --no-fail-fast
    cargo clippy --all-targets -- -D warnings
    git diff --check

Use the dev profile. Keep build artifacts in the normal target directory. Every actual new-build daemon, CLI, TUI, or end-to-end invocation uses `--instance upgrade-in-flight-test` or an existing named isolated fixture and temporary `MJ_CONFIG_DIR` / `MJ_DATA_DIR`.

## Validation and Acceptance


A busy worker receives steering and cancellation while its upgrade remains deferred, regardless of elapsed time. Reviewer or background activity prevents idle reservation. Work appearing between observation and reservation wins. Queued next-turn prompts retain order and identity through an idle replacement. A failed reservation leaves the original worker usable. Daemon handoff waits for an accepted lifecycle operation and does not set its cancellation flag. Multiple upgrading clients converge on one daemon. A communication failure never escalates an automatic upgrade to a signal that kills ongoing work.

## Idempotence and Recovery


Keep the old worker running through all preparation that can fail. Disconnecting an abandoned reservation releases its worker barrier. Preserve existing durable command IDs and restart recovery, and never roll database migrations backward. Tests never modify the default instance. Do not push, release, or switch branches.

## Artifacts and Notes


The preceding startup work is committed as `2cfb84ef`. This plan records its missing in-flight-work protection separately so the advertised invariant is not mistaken for already-validated behavior.

## Interfaces and Dependencies


Use the existing session manager command channel, managed lease, relay request capability negotiation, checkpoint barrier, daemon startup lock, and shared subprocess helpers. No new crate or third-party dependency is required.

Revision note: Initial plan records the user-approved draining design and steering requirement before implementation.

Revision note (2026-09-22): Record implemented ownership and reservation interfaces, the legacy bootstrap limitation, validation results, and the user-directed scope boundary.
