# Separate command results, turn results, and session faults (#1170)


This ExecPlan is maintained according to `.agents/PLANS.md`. It is a living document; update Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture


An event consumer must distinguish an interrupted checkpoint barrier from a failed agent turn without reading diagnostic prose or command ID prefixes. The worker is the process that runs the agent and owns its journal; the daemon owns session records and lifecycle operations. Each reports only the outcome it owns. Successful barrier cleanup is not evidence that a checkpoint archive was saved.

The user expressly forbids touching the live database before installation. Do not install this build, operate the default instance, or replace its daemon or workers. Every application invocation uses `--instance issue-1170-events`; database and upgrade tests retain their isolated directories. Cargo builds retain the configured mbx storage.

## Progress


- [x] (2026-09-27) Inspected issue, projections, checkpoint producers, event persistence, and migration rules; claimed issue #1170.
- [x] (2026-09-27) Defined shared normalized outcomes and typed worker causes.
- [x] (2026-09-27) Projected terminal command/turn events and persisted session/checkpoint results.
- [x] (2026-09-27) Added breaking migration 57 and protocol 24, preserving cursors and old journal digests.
- [x] (2026-09-27) Validated behavior, upgrades, replay, CLI decoding, and documented the event contract.
- [x] (2026-09-27) Required dev-profile tests, Clippy, formatting, diff review, and named-instance smoke passed.
- [x] (2026-09-27) Prepared the validated implementation for its delivery commit on master; origin/master matches the parent commit, allowing the requested fast-forward push.

## Surprises & Discoveries


Generic error events are also emitted by SQL triggers on `sessions.last_error`; changing only the transcript projector would leave ambiguous failures. Relay digests include serialized observations, so added optional metadata must be omitted when absent to preserve old hashes. Successful relay completions currently omit command kind. The existing internal `Completed` turn outcome means the harness returned and can contain a provider error; the public result needs semantic normalization.

## Decision Log


Decision (2026-09-27): replace new generic errors rather than preserving their wire shape. The user prioritizes a clear event contract over existing clients. Retain old unclassifiable facts as `legacy_notice`, not guessed failures or cleanup.

Decision (2026-09-27): keep raw internal turn evidence and add one shared normalized interpretation for API, wait, and rendering. This preserves historical journals and avoids independently interpreting the same completion on multiple surfaces.

Decision (2026-09-27): no new severity, retry policy, alert engine, or crate. Outcomes and typed reasons describe facts. Existing task ownership and upgrade admission continue to govern execution.

Decision (2026-09-27): the final user instruction explicitly authorizes pushing the completed changes to origin/master. This supersedes the initial no-push scope; installation remains unauthorized.

## Outcomes & Retrospective


Implementation and documentation are complete. The full dev-profile suite (`cargo test -- --test-threads=4`) and `cargo clippy --all-targets -- -D warnings` passed, including isolated automatic-upgrade regressions. An earlier concurrent-start timing failure passed on focused retry and in the final full suite; no timeout was changed. Formatting and whitespace checks passed. The separate `issue-1170-events` instance listed sessions, reported this build, and stopped cleanly. Its temporary store reported schema revision 57, minimum compatible revision 57, and integrity check `ok`. No application build has been installed and no live database has been opened or migrated. Delivery is the commit containing this completed plan, followed by the explicitly authorized fast-forward push to origin/master.

## Context and Orientation


`mj-core/src/storage.rs` defines the public event envelope and payload. `mj-core/src/state.rs` contains raw turn outcomes and the shared harness completion classifier. `mj-core/src/relay/snapshot.rs` defines hashed relay observations. `mj-worker/src/relay/commands.rs`, `journal.rs`, and runtime dispatch produce terminal observations. `mj-transcript/src/projection/api_events.rs` builds durable public events alongside transcript mutations; observation rendering lives next to it. `mj-controller/src/database/events.rs` persists and pages events; schema migrations are in `database/schema.rs`. Database session writes and checkpoint lifecycle code own daemon results. The CLI decodes SSE (server-sent event) frames in `mj-cli/src/api_client/events.rs`.

## Plan of Work


First establish normalized result types in mj-core and typed causes in relay terminal observations. Emit `turn_ended` for prompts, with outcomes completed/input_required/cancelled/rejected/interrupted/failed, preserving diagnostics, usage, identity and raw stop reason. Emit `command_ended` for non-prompt commands, including owner, ID, kind and succeeded/cancelled/rejected/failed result. Failed results carry typed reason and readable message. Successful cancel commands succeed; their targets are cancelled. Shell outcomes follow their structured results.

Next make worker producers record controller_disconnected and owner_lost_on_restart for the two cleanup cases, and distinguish explicit cancellation, rejection, runtime loss and restart at their actual producers. Add command kind to successful observations instead of reconstructing it from a transient projection cache. Share interpretation with transcript and wait. Do not emit duplicate prompt command events. Keep expected cleanup out of the transcript but retain actionable failures.

Then replace generic session errors with `session_fault` at authoritative persistence boundaries, atomically with state changes and without duplicate repeated-save events. Record daemon-owned requested checkpoints with durable operation identity and related worker barrier commands; recovery ends abandoned operations once. Success requires a verified durable archive; cleanup cannot mask transfer, export or verification failures. Busy deferrals are rejections. Reuse supervised lifecycle work and admission.

Finally introduce a breaking forward schema migration and protocol revision. Convert existing generic errors to legacy_notice, and normalize stored turn events using known evidence, preserving row sequence, timestamps and correlation. Old optional relay fields must deserialize absent and remain omitted when reserialized, preserving hashes. Older busy workers remain attached until ordinary idle replacement; unknown causes stay unknown. Update API docs, CLI fixtures, and isolated upgrade regressions.

## Concrete Steps


Work from `/home/jonathan/Projects/mjolnir`. Use focused dev-profile Cargo checks while implementing, then run `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings`. Do not redirect target output or alter mbx configuration. Run application checks only with `--instance issue-1170-events`; automated tests must use their existing temporary config/data directories. Never run install commands. Stage only task files and commit the validated changes on master, then push to origin/master as the user explicitly requested. Do not install the build.

## Validation and Acceptance


Behavior tests reproduce both ownerless checkpoint cases, checking cancelled command results, no failed turn, and queue progress. Test real checkpoint failure and success independently of barrier cleanup. Exercise successful, awaiting-input, cancelled, rejected, quota/provider-failed and runtime-interrupted prompts, requiring one turn result each. Exercise shell exit failures and successful cancel commands. Verify startup faults, repeated saves, projection rollback, page coalescing, replay/reopen and reconnect. Migration tests preserve cursors and unclassified history; digest tests use old observation fixtures; isolated upgrade tests preserve active older workers and enforce the new compatibility floor. Required Cargo commands must exit zero.

## Idempotence and Recovery


Migrations are forward-only and transactional; never edit applied revisions. Event insertion shares the owning state transaction, and relay replay uses its existing ordinal gate. Checkpoint terminal recording consumes durable operation identity so retries and startup recovery cannot report it twice. Stop any test-instance processes before removing their files. No live-store reset or mutation is permitted during this task.

## Artifacts and Notes


The issue is https://github.com/BrokkAi/mjolnir/issues/1170. Unrelated untracked files present at start are `.agents/plans/fix-subagent-tool-results.md`, `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, and `mj.sqlite3`; do not stage or modify them.

## Interfaces and Dependencies


The event envelope remains seq/session_id/recorded_at_ms/type/data. Replace generic error with turn_ended, command_ended, session_fault, and legacy_notice for older unknown facts. Command results identify worker or daemon ownership. Keep all other event families. Use existing serde, rusqlite transactions, journal digest helpers, subprocess helpers and supervised tasks; no new dependency is needed. Public classification is pure and deterministic; messages and IDs never determine it.

Initial plan recorded on 2026-09-27 from the approved design, including the explicit live-instance isolation constraint.

Revision (2026-09-27): terminal completions now include optional barrier identity so successful release can end the barrier once. Checkpoint admission and terminal recording use a durable operation ID in the same writer transaction as lifecycle/metadata updates, preventing stale completions from consuming another operation. Unexpected runtime failure is recorded at the failing runtime owner; ordinary `Stopped` notifications remain non-faults. Historical adapter events default their absent cause to explicitly unknown. Added projection, rollback, stale-owner, archive-preservation, digest and migration regressions.

Revision (2026-09-27): recorded passing final validation and the separate-instance smoke result, and updated delivery to include the user-requested push. Test evidence is in `/tmp/1170-final-tests.log`, `/tmp/1170-clippy.log`, and `/tmp/1170-smoke.log`; the smoke store is under `/tmp/mj-issue-1170-events-kam0ms0v` and its daemon is stopped.
