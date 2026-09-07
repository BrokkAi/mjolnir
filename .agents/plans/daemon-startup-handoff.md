# Coordinate daemon startup and tolerate concurrent log pruning

This ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Starting a development client during daemon replacement should reuse the winning replacement and wait for the old controller to release its database ownership. It must not launch a doomed child and report missing daemon.json. Log cleanup must not abort startup when another process removed the same expired log.

## Progress

- [x] Confirmed fatal concurrent log deletion and controller-lock contention in daemon.log.
- [x] Implemented a cross-process startup lock and explicit controller ownership probe.
- [x] Add deterministic concurrency regressions and review log-pruning fix.
- [x] Run Cargo tests, Clippy, formatting, and host build; commit validated changes.

## Surprises & Discoveries

The daemon removes discovery metadata early in shutdown but retains its controller lock through database-writer cleanup. Clients previously treated missing metadata as permission to launch immediately. Startup polling only retried connections after launching one child, so a child rejected by the controller lock could never recover. Concurrent pruning separately treated a missing expired log as fatal.

## Decision Log

Serialize clients using an advisory daemon-start.lock held through replacement and readiness. Probe the existing controller lock before launching; if occupied, keep checking for a ready daemon or ownership release under the existing stop budget. Keep the ready-start budget separate. Perform lock probes and child creation in background tasks and use asynchronous polling. Preserve fail-fast ControllerStoreGuard::acquire for existing callers, adding a typed optional probe instead of matching error text. Delegate only the independent logging change.

## Context and Orientation

mj-cli/src/daemon.rs discovers the daemon through daemon.json, replaces development executables, and launches detached daemon-run children. mj-controller/src/hel_controller.rs owns the exclusive controller.lock file. mj-cli/src/logging.rs prunes startup logs. mj-cli/tests/store_divergence.rs already exercises real daemon startup in isolated storage and will also cover ownership handoff.

## Plan of Work

First make controller ownership contention an optional result. Add the startup guard and recheck metadata inside it so concurrent clients join a single winner. Before launch, wait until the old writer releases ownership, returning an already ready daemon if one appears. Keep errors contextual and waits bounded. Then cover two concurrent CLI starts while an old owner still holds controller.lock and verify both succeed once ownership is released. Independently cover concurrent log removal without ignoring permission or other I/O errors.

## Concrete Steps

From /home/jonathan/Projects/hel run cargo fmt --all, cargo test, cargo clippy --all-targets -- -D warnings, and git diff --check. Run Cargo tests escalated outside the restricted sandbox. Tests use isolated temporary storage and must not stop live session workers.

## Validation and Acceptance

A held controller lock with absent metadata must delay startup instead of creating a child that exits with ownership contention. Releasing ownership lets simultaneous clients successfully connect to one daemon. Startup lock cancellation releases ownership. A concurrently removed log does not fail pruning, while genuine filesystem errors remain visible. Full tests and Clippy must pass before commit.

## Idempotence and Recovery

Lock files remain on disk intentionally; operating-system advisory ownership is released on close/process exit. Never delete a lock file to recover startup. No database migration, session rewrite, or live worker restart is needed.

## Outcomes & Retrospective

Startup coordination, ownership handoff, cancellation coverage, and concurrent pruning are complete. The real two-client ownership test passed. Chat (409), controller (675), TUI (332), worker (108 plus integrations), and CLI (183 plus integrations) passed. Core had 830 passing tests and one unchanged fake-worker launch failure (Text file busy); that test passed independently. Clippy, formatting, diff checks, and the host binary build passed. No live worker was restarted.

## Artifacts and Notes

Observed errors: remove expired Mjolnir log: No such file or directory; another Mjolnir controller is already using the store; parent then reports read daemon.json: No such file or directory.

## Interfaces and Dependencies

Add ControllerStoreGuard::try_acquire() -> Result<Option<Self>>. Use std file advisory locks, Tokio spawn_blocking and sleep, and existing shared subprocess spawning. No new dependencies.
