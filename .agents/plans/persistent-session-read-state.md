# Preserve session read positions and distinguish interrupted work

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Reading a conversation or marking a workspace read must survive restarting the UI. Successful idle worker restarts remain transcript history without demanding attention. Actual interrupted work remains unread until acknowledged. Unresolved questions, failures, and unread replies may notify again after reopening.

## Progress

- [x] Inspected receipt storage, runtime snapshots, transcript projection, and notifications.
- [x] Implement monotonic read positions, asynchronous durable receipt tracking, and shutdown draining.
- [x] Implement interruption-only attention in full transcripts and startup summaries.
- [x] Add behavior regressions and pass the complete workspace suite and required Clippy check.

## Surprises & Discoveries

Read receipts already persist in SQLite. `apply_runtime_records` replaces locally acknowledged positions with incoming records. Prepared transcript results carry unread counts computed against an older read position. Every restart currently counts as unread, even when it interrupted nothing.

## Decision Log

On 2026-09-19 the user selected alerts only for interrupted work and startup reminders for unresolved items. Reuse existing receipt storage and system transcript items; do not introduce a notification-history store or database migration. Ordinary historical restart markers become informational. Explicit interrupted turn outcomes remain eligible for attention.

## Outcomes & Retrospective

Read positions now remain monotonic across runtime, lifecycle, and configuration reloads. Prepared summaries recompute unread state using the current acknowledgement. Receipt writes run independently per session, coalesce newer acknowledgements, retry failures with a short delay, and drain before normal exit. Failed receipts remain available for a final shutdown retry and are reported if unconfirmed.

Ordinary restart history no longer generates unread attention. Interrupted prompted and autonomous work does, with distinct notification text. Existing explicit interrupted-turn outcomes are recognized without a schema migration. The complete workspace test suite and Clippy pass; final delivery commits on `hel4` and pushes to `origin/master` as authorized.

## Context and Orientation

`mj-cli/src/dashboard.rs` acknowledges visible conversations and dashboard mark-read actions through asynchronous daemon requests. `mj-cli/src/dashboard/drains.rs` applies runtime records. `mj-tui/src/ingest.rs` builds and applies transcript summaries. A read position is the largest acknowledged event ordinal, a monotonically increasing session event number. SQLite stores both session-wide and client-specific positions through `mj-controller/src/database/client_state.rs`.

`mj-transcript/src/projection/observation.rs` converts durable relay events into transcript items. `mj-core/src/transcript.rs` defines item identity helpers. `mj-controller/src/database/materialized.rs` builds startup summaries without loading full transcripts. `mj-tui/src/notify.rs` turns attention into notification text.

## Plan of Work

First preserve the maximum local and incoming read position and recalculate unread counts when a prepared result is applied. Track each session's in-flight and queued receipt independently, retry failures a bounded number of times, and block normal exit until writes are acknowledged or reported failed. Keep failed desired positions available for subsequent retry. All persistence remains off the render loop.

Then replace restart-unread accounting with interrupted-work accounting. Use a distinct system item prefix for newly projected interruptions, retain restart transcript markers, and derive older explicit interruption evidence from stored turn outcomes. Include prompted work and autonomous turns interrupted by restart. Startup SQL summaries and full projections must produce the same interruption positions. Notifications describe interrupted work rather than an old agent reply.

## Concrete Steps

Work from `/home/jonathan/Projects/hel4`. Add colocated behavior tests for read-position races, receipt queuing and failures, restart classification, summary parity, and persisted receipts loaded by a fresh client. Check changed Rust sources with `rustfmt --edition 2024 --config skip_children=true --check`, run `env -u NO_COLOR cargo test --quiet`, and run `cargo clippy --all-targets -- -D warnings` on the dev profile. Every Cargo test runs outside the restricted sandbox. Use normal build storage and isolated test databases. Unrelated pre-existing formatting differences in two files were left unchanged.

## Validation and Acceptance

Reading and marking all read remain read after reopen; stale snapshots and delayed projections cannot revive unread activity. New agent content remains unread. Independent session receipts save concurrently and immediate normal quit drains all pending saves. Idle restarts do not notify; interrupted prompted and autonomous work does until read. Startup summaries and live projections agree, and unresolved questions and failures still notify after reopen. Required checks must pass before committing.

## Idempotence and Recovery

Receipt writes use maximum positions and can safely repeat. Do not alter the live store for tests. No database schema change is intended. Stage only files changed for this task and commit on the current branch. The user subsequently authorized pushing the completed commit to `origin/master`; the current branch is `hel4` and its starting commit matches `origin/master`.

## Artifacts and Notes

The initial workspace test run exposed inherited `NO_COLOR=1`, which disables colors expected by the TUI assertions. Subsequent runs use `env -u NO_COLOR cargo test`. A new idle-restart fixture initially used the shared running-session default and was corrected to explicitly use idle execution. Test output is retained in `/mnt/optane/hel4-read-state.6aAyr1/`.

Final validation on 2026-09-19: the complete `env -u NO_COLOR cargo test --quiet` invocation exited 0; controller tests passed 1,473, TUI tests passed 659, worker unit tests passed 484, and the remaining workspace, integration, and documentation suites passed. `cargo clippy --all-targets -- -D warnings` exited 0. Changed-source rustfmt checks and `git diff --check` passed.

## Interfaces and Dependencies

Keep the existing daemon receipt API and client identity semantics. Add a shared transcript interruption classification helper and rename internal restart-unread summary fields to interruption fields. Use existing critical asynchronous operation tracking for saves. Do not create a crate or add dependencies.

Revision note: implementation also preserves read positions across configuration and lifecycle reloads, and freezes new read acknowledgements during shutdown so streaming output cannot prolong cleanup. Push instructions reflect the user's follow-up authorization.
