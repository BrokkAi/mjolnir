# Keep API activity out of durable event history

This living ExecPlan follows `.agents/PLANS.md`. Update Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation proceeds.

## Purpose / Big Picture

Busy indicators and their changing timestamps currently accumulate permanently in SQLite. One observed store contained 7.64 million API events occupying 3.39 GB including their index; over 98% of two 10,000-event samples were activity notifications. Keep current activity and live updates while retaining only meaningful lifecycle events in the replay log. Existing activity history must be removed automatically in background batches without delaying startup, interrupting workers, changing transcript history, or resetting event cursors.

## Progress

- [x] (2026-10-21) Inspect event writes, HTTP streams, CLI decoding, daemon shutdown ownership, and build configuration.
- [x] (2026-10-21) Implement current-state persistence and transient HTTP activity frames with reconnect snapshots.
- [x] (2026-10-21) Implement cancellable background cleanup through the sole database writer.
- [x] (2026-10-21) Extend SQLite and HTTP/CLI integration regressions, document the contract, and run focused checks: persistence/cleanup, HTTP streams, CLI decoding, and automatic isolated-daemon cleanup all pass; formatting and Astro checks pass.
- [x] (2026-10-21) Run full touched-crate tests and workspace clippy, format and review the final diff, and prepare the complete change for commit on `master`. Controller: 2,020 passed, 14 ignored. CLI: every test passed, with the color-specific PTY test rerun without inherited `NO_COLOR=1`. Workspace clippy passed.

## Surprises & Discoveries

The v1 event stream currently polls the database every 250 milliseconds. Its activity recorder compares full activity objects, including the last activity timestamp. The CLI requires an SSE ID on every event. Removing activity inserts alone would therefore remove live updates. The current checkout is older than the installed release, so testing must use isolated instances and must not touch the live store. Cargo resolves to the existing mbx wrapper; keep the normal target layout. Viewer session arrays use a sorted map internally: a new HTTP regression initially mutated the wrong numeric row after adding a second session. Addressing fixture sessions by ID fixed the test; all seven focused stream tests now pass. The shell inherits `NO_COLOR=1`, so the existing white-background PTY test paints monochrome and fails its RGB assertion. Rerun that color-specific test with `env -u NO_COLOR` rather than changing unrelated terminal code.

## Decision Log

- Decision: Keep `api_session_activity` as the durable current value and stop inserting `ActivityChanged` into `api_events`.
  Rationale: Existing schema already expresses the desired storage boundary; no schema migration or compatibility-floor change is needed.
  Date/Author: 2026-10-21 / Codex.
- Decision: Deliver current activity on connection and coalesced changes from the viewer snapshot watch channel, after catching up durable history. Transient SSE frames have no ID. Their existing JSON `seq` field names the durable resume position at delivery, rather than allocating an event ID.
  Rationale: One durable sequence remains the replay owner; reconnect reconstructs disposable activity from current state. Existing JSON event envelopes remain decodable and numeric IDs continue to mean durable replay positions.
  Date/Author: 2026-10-21 / Codex.
- Decision: Scan historical events in bounded sequence batches and delete only `activity_changed`, preserving `sqlite_sequence`. Run this maintenance beside the daemon, with bounded admission per batch, cancellation between batches, and logged progress/failures.
  Rationale: Startup must serve promptly and handoff must drain. A crash can safely rescan remaining rows because deletion is idempotent. No live default-instance mutation is authorized as a test.
  Date/Author: 2026-10-21 / Codex.

## Outcomes & Retrospective

The storage boundary and background cleanup are implemented and validated. The three focused SQLite/activity-recorder tests, seven HTTP stream tests, two CLI decoder tests, and isolated-daemon cleanup regression pass. The daemon regression removed 2,003 activity records automatically, retained the lifecycle event and transcript, and preserved the sequence frontier. The full controller suite passed 2,020 tests with 14 ignored. All CLI tests passed: its full run passed every test except the color-specific PTY check under `NO_COLOR=1`, and rerunning that check with `env -u NO_COLOR` passed. Workspace clippy with warnings denied passed. `npm run check` in `docs` reports zero errors or warnings, formatting passes, and the final diff is clean. Physical file compaction is separate: deleted pages become reusable, and this work does not run VACUUM or alter the live default database. Installing the updated daemon activates cleanup automatically.

## Context and Orientation

`mj-controller/src/database/events.rs` owns API event insertion, replay reads, and latest activity writes. `mj-controller/src/server_runtime/api_activity.rs` consumes viewer snapshots and persists changes using the shared database writer. `mj-controller/src/server/api/events.rs` serves authenticated server-sent events (SSE), a text format that carries a type, JSON data, and optional reconnect ID. `mj-controller/src/server/viewer_types.rs` defines the session snapshots that also drive the web UI. `mj-cli/src/api_client/events.rs` decodes SSE and `mj-cli/src/api_commands.rs` prints events and handles daemon handoff. `mj-controller/src/daemon/process.rs` starts and joins background services. All database mutations must run through the single writer; synchronous calls belong on blocking tasks.

## Plan of Work

First remove durable activity inserts while retaining current-state upserts. Centralize activity interpretation on the viewer session so both recorder and HTTP stream ask the same helper. Subscribe to snapshots before the initial history read. Send a filtered current snapshot after history catch-up, then changes; coalesce slow readers through the existing watch channel and observe shutdown during bounded channel sends. Durable lifecycle frames retain their IDs and cursor order. Update CLI decoding to accept ID-free activity frames while continuing to reject missing or conflicting IDs for durable events.

Next add a database maintenance batch that scans a bounded number of existing sequence IDs, deletes only activity rows, and returns the scan position and removed count. Preserve the AUTOINCREMENT frontier even when the last event was activity. A daemon-owned task runs batches with a pause, refuses new admission during draining, and joins at shutdown. Resume on each startup; existing durable rows remain available during cleanup. Hide legacy activity rows from replay immediately so reconnect behavior does not depend on cleanup progress.

Finally extend existing SQLite tests to verify deletion bounds, partial cleanup/restart, retained lifecycle events, latest activity state, and cursor monotonicity. Extend HTTP stream tests for initial current state, filtering, timestamp-only changes, durable/live interleaving, and handoff. Extend CLI decoder tests for transient frames adjacent to durable frames. Update the human API reference at `docs/src/content/docs/api-reference.md`.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir` on its current branch. Use the existing `cargo` command (mbx wrapper) without overriding its target directory. Run all Cargo tests outside the restricted sandbox. Focused validation used `cargo test -p brokk-mj-controller api_activity`, `cargo test -p brokk-mj-controller event_stream`, `cargo test -p brokk-mjolnir event_decoder`, and `cargo test -p brokk-mjolnir --test daemon_startup daemon_cleans_legacy_activity_history_without_removing_session_transcripts`. Final validation used `cargo test -p brokk-mj-controller -- --test-threads=8`, `cargo test -p brokk-mjolnir -- --test-threads=8`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. The existing color-specific test required `env -u NO_COLOR cargo test -p brokk-mjolnir --test termination_pty auto_theme_paints_light_on_a_terminal_that_reports_a_white_background`. Every actual manual daemon/CLI invocation of a new build must name `--instance transient-api-activity`; automated tests keep their existing named instances and temporary stores (the daemon startup regression uses `upgrade-test`). `cargo fmt --all -- --check`, `git diff --check`, and the documentation's `npm run check` passed. Commit only the changed files.

## Validation and Acceptance

Repeated activity updates must change the latest-state row without adding history rows. Replayed history must retain lifecycle events in order and skip activity rows before and after cleanup. Cleanup must be restartable and bounded, and the resume frontier must not decrease when the newest historical activity row is removed. HTTP subscribers must receive a current activity snapshot and subsequent updates without durable SSE IDs, while lifecycle events and handoff retain correct IDs. Slow readers and shutdown remain bounded. Existing upgrade regressions and touched-crate suites must pass.

## Idempotence and Recovery

Cleanup deletes only explicitly selected activity events and can repeat safely. An interrupted batch rolls back through SQLite; committed deletes remain removed and a new daemon rescans safely. Never rewrite shipped migrations. No deployment, push, automatic VACUUM, or manual alteration of the live default store is part of this implementation. Failures must be logged and reported rather than hidden.

## Artifacts and Notes

The observed dominant storage is full transcripts, which this change preserves. Cleanup reclaims reusable SQLite pages; it does not promise an immediate reduction in the file's apparent size.

## Interfaces and Dependencies

Use existing rusqlite transactions, the shared database writer, tokio watch receivers, cancellation tokens, and upgrade admission. Keep the existing `ApiEvent` envelope and global durable sequence. Add only small local cleanup result types/helpers, without new crates or external dependencies.

Initial plan recorded after tracing the live event and persistence paths. Updated after implementation to record passing persistence/cleanup regressions and documentation validation, then finalized with full validation results and the inherited color-setting explanation. Live activity uses an independent watch notification path and durable polling interval, so frequent snapshots neither poll SQLite on every change nor starve durable delivery.
