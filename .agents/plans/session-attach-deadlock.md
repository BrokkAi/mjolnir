# Keep session opening responsive

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Opening Muse or another conversation must not trap the dashboard. The user must be able to select another conversation, cancel opening with Escape, retry a failed open explicitly, and quit. Failed attachment must not start an unlimited stream of draft writes or consume the threads needed to finish those writes.

## Progress

- [x] (2026-09-07) Inspect the hung dashboard and trace its exhausted blocking pool.
- [x] (2026-09-07) Fix attachment retries, cancellation, deadlines, and draft persistence scheduling.
- [x] (2026-09-07) Focused behavior tests prove bounded attachment, cancellation, explicit retry, stale-result rejection, panic reporting, and progress with one blocking thread. An isolated real PTY proves Escape, retry, and quit in under one second while attachment remains unavailable.
- [x] (2026-09-07) Full `cargo test`, `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks pass. After final selection-race protection, the complete CLI package tests and clippy pass again.
- [x] (2026-09-07) Prepare the validated implementation and this completion record for the required commit on the current branch.

## Surprises & Discoveries

The live Muse session has a native session identifier and running state; startup succeeded. A read-only debugger capture of the installed 2.1.0 dashboard found exactly 512 blocking threads parked in `spawn_detached_session_state_persist`. That function runs asynchronous daemon communication inside `spawn_blocking`; connection metadata itself needs another blocking task. Repeated failed attachment can fill the pool and deadlock those nested tasks. The installed executable differs from this source checkout.

## Decision Log

Use bounded waiting for session-manager adoption within one attachment, suppress automatic retries after failure or cancellation, and let explicit selection/open retry. Give each attachment a generation so stale results cannot overwrite a new request for the same session. Run asynchronous draft/read-receipt writes directly on Tokio tasks with a deadline and reported failures. This fixes the observed dependency cycle without increasing thread limits. Do not alter the user's live agents or installed executable during validation.

Remove the separate startup retry window: adoption is now retried within the single 15-second open deadline. Cancellation aborts supervised futures and queued preparation, while an already-running synchronous database read may finish off the UI loop. Cancelled and failed selections retain their identity; they show no unrelated warm conversation. Resuming or moving a session explicitly reopens its replaced actor. Draft-save failures remain available after terminal teardown so exit does not erase an unconfirmed-save warning.

## Outcomes & Retrospective

Implementation and validation complete. The PTY test uses a durable session with no adoptable worker and requires no provider credentials. Escape leaves the open cancelled across background ticks; Enter retries; Alt-Q exits in under one second and restores terminal flags. The one-thread pool test runs 64 concurrent simulated saves, each requiring a nested metadata read, while unrelated chat preparation still completes. Full suite and clippy pass; tests requiring separately configured real providers retain their existing ignored status. The installed 2.1.0 executable and user's live sessions were not replaced or stopped. A new live provider conversation was not needed to reproduce or verify this dashboard deadlock.

## Context and Orientation

`mj-cli/src/dashboard.rs` owns the terminal event loop and the warm chat (the previous conversation retained in memory). `follow_selected_session` currently retries whenever no open is in flight. `open_chat_session` saves the old draft on every attempt. `mj-cli/src/dashboard/io.rs` runs those saves on blocking threads and applies open completions. `mj-controller/src/hel_session_manager.rs` supplies local handles to sessions mirrored from the daemon, the background process that owns agents. Its adoption wait currently does not bound an individual unresponsive request. The live Muse adapter has already opened its session, so the observed failure is dashboard attachment rather than agent initialization.

## Plan of Work

First add a small attachment state machine beside dashboard code, replacing the queued selection with cancellation and generation tracking. Test same-session stale completions, failed-open suppression, and switching to another conversation while an earlier request hangs. Apply a bounded timeout to waiting for adoption and preparing the chat. Escape cancels only attachment; global quit and session controls remain usable.

Next replace blocking wrappers for draft/read receipt network waits with supervised asynchronous critical work. A timeout must report an unresolved save and release the shutdown blocker. Keep actual synchronous database work on blocking threads. Test these operations using a runtime with one blocking thread and a pending future, proving unrelated blocking work can still run.

Finally run the full Rust tests and clippy, review the diff, update this plan with evidence, and commit only the changed files.

## Concrete Steps

From `/home/ryan/code/mjolnir`, run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Run every Cargo test outside the restricted sandbox. Use focused tests while implementing. Existing PTY termination tests verify terminal restoration and quit behavior; extend the interface tests for opening as necessary.

## Validation and Acceptance

A missing or nonresponsive session must produce one bounded opening attempt, then a visible retry instruction. Repeated render ticks must not create more saves. Selecting another session or pressing Escape must immediately retire the old request; a late completion must not replace the selected conversation. Network writes must not consume a blocking thread while waiting. All required tests and clippy must pass.

## Idempotence and Recovery

Use isolated test state and fake session managers, never the user's real database or running Muse process for mutations. Tests must terminate their own processes. Re-run focused checks after changes. Stage explicit paths and commit on the current branch; do not push.

## Artifacts and Notes

The debugger capture is temporary diagnostic data outside the repository. Its relevant result is 512 identical stacks ending in Tokio's blocking pool, with the instantiated async future resolved to detached draft persistence. No user transcript is needed to reproduce this behavior.

## Interfaces and Dependencies

Use existing Tokio tasks, timeouts, cancellation through abort handles, and the dashboard I/O channel. No new crate or dependency is needed. Attachment result messages need an attempt generation in addition to session identity. The attachment state must own cancellation and suppress automatic reentry after errors.

Revision: initial plan records the live deadlock and the bounded implementation scope.

Revision: implementation replaces automatic startup retries with one bounded adoption wait, adds save-error reporting after terminal restoration, and records the passing terminal regression. PTY assertions use newly written notice suffixes because incremental rendering does not re-emit unchanged characters.

Revision: final review covers a completion arriving between selection input and attachment scheduling; acceptance now checks the current row as well as generation. Final CLI tests (191 unit tests and all four PTY tests) and clippy pass. No release or push is part of this change.
