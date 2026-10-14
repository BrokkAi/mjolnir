# Persist semantic turn outcomes without changing clients

This ExecPlan follows `.agents/PLANS.md` and remains a living record.

## Purpose / Big Picture

Database analysts must be able to read `session_turn_usage.body` outcome.kind as how a turn ended. Existing clients must receive their exact raw internal values. Store the existing ApiTurnOutcome shape plus an always-present raw_outcome containing the original MaterializedTurnOutcome in both usage history and materialized last-turn columns.

## Progress

- [x] Design investigation and parent approval: semantic converter plus exact raw copy, no inverse.
- [x] Implement shared database serializer/decoder and all writers/readers.
- [x] Add breaking revision 82 and migration/codec regression coverage.
- [x] Validate affected live tables in an isolated copy, then delete copy.
- [x] Run formatting, controller tests, and workspace clippy excluding desktop; report without committing.

## Surprises & Discoveries

The existing semantic converter loses raw absent reasons and may replace messages with diagnostic text. An always-present raw copy avoids that loss. state_io.rs also writes last-turn JSON and must use the shared codec. Historical schema tests use invalid placeholder usage JSON; replace those placeholders with realistic raw turns so strict migration can validate all rows.

## Decision Log

Parent chose persistence-only conversion and then explicitly approved always preserving raw MaterializedTurnOutcome. Keep the semantic ApiTurnOutcome at the top level to preserve the SQL path $.outcome.kind and accepted-ordinal extraction. Do not edit subagent.rs, wait.rs, or server_runtime/api.rs. Do not commit.

## Context and Orientation

The worker records relay events. The controller projects these into MaterializedTurnOutcome. database/materialized.rs and database/state_io.rs serialize that type. database/usage.rs and materialized.rs read it. event_outcome.rs already supplies ApiTurnOutcome::from with shared stop-reason classification. Schema revision advances from 81 to 82 in database.rs; schema.rs owns the migration ladder and compatibility floor.

## Plan of Work

Add database/turn_outcome.rs with serialize_turn_outcome, deserialize_turn_outcome, and migration helper. A stored structure flattens ApiTurnOutcome and contains raw_outcome. The decoder returns raw_outcome directly. Replace all serialization/deserialization sites for both persisted columns. Add revision 82 in one transaction that updates both tables and raises the compatibility floor. Tests must cover raw exact round-trip, cancellation versus relay interruption, absent reasons, diagnostics, legacy migration, unchanged billing and rollback on invalid data.

## Concrete Steps

From this repository run cargo fmt --all, cargo test -p brokk-mj-controller, and cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings. Run all Cargo tests outside the sandbox, retain existing named isolated fixture configuration, and keep current mbx build settings. Use a test-only environment-selected copied DB fixture to run the actual migration helper on small read-only snapshots of live tables. Store logs and before/after counts under .mj/agents/862d5d0331a5fd9fff7279593deae518. Delete the database copy afterward.

## Validation and Acceptance

Raw decoder values must equal every original field including reason=None and diagnostics. SQL outcome.kind must distinguish cancelled, failed and input_required from completed. Schema 81 readers must be refused after migration. Invalid data must roll back both table conversions and schema revision. Cost accounting still counts all turn rows. Real-data conversion must have no failed rows and preserve raw values exactly.

## Idempotence and Recovery

Revision 82 runs only on older stores and commits atomically. Failure leaves revision and data unchanged. Never open the live store read-write; only copy its relevant tables from mode=ro and migrate the report-directory copy. No live daemon invocation is needed.

## Artifacts and Notes

Evidence and test logs go in the delegated report directory. First focused run: exact round-trip and existing projection test passed; rollback fixture initially attempted to insert an already-created materialized session. Fixed the fixture to update that session and started the focused migration rerun. A 6MB mode=ro snapshot contains all 7,712 usage rows, including 26 absent interruption reasons and 337 diagnostics; source revision 81.

## Interfaces and Dependencies

Reuse mj_core::event_outcome::ApiTurnOutcome and serde. Shared database codec returns anyhow::Result. Migration accepts rusqlite::Transaction; every error names table and row identity. No public API or relay type changes.

## Outcomes & Retrospective

Implementation complete. Focused migration tests passed. Live-copy migration verified 10,699 non-null bodies exactly, including 26 usage rows with absent reasons and all 337 diagnostic-bearing usage rows; no conversion failures, both table copies deleted. Full controller tests ran once: 1,985 passed, 4 failed and 11 ignored. The historical event fixture was fixed; that test and all three unrelated full-suite failures passed on targeted reruns. Workspace clippy passed after the new-test slice assertion was corrected. Final formatting and controller doc-test command passed (zero doctests).

Live-copy evidence: `.mj/agents/862d5d0331a5fd9fff7279593deae518/real-db-check/before-after-counts.md`. Source had 7,712 usage rows; billing coverage and all full-turn token counters were unchanged. Updated after completing live-copy acceptance.

Validation updates: historical event migration fixture now restores legacy body shapes before lowering its schema; unrelated transfer/profile failures passed isolated reruns. See validation-summary.md in the report directory for names, reasons and logs. No commits.

Completed outcome: schema and compatibility floor 82; both persisted columns now expose semantic kinds while retaining exact raw turns for unchanged internal/v1 readers. All change-specific tests, live-copy verification, formatting and clippy passed. Full-suite concurrency failures passed targeted reruns; no broad suite repetition. No commits and no live store modifications.
