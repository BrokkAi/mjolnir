# Preserve goal controls across session loading and configuration changes

## Purpose

Reopened Codex sessions must advertise `/goal` and retain goal controls after model or other configuration changes. Loading old conversation history must not duplicate that history. This work separates adapter settings from goal state and stops treating current session metadata as replayed conversation content.

## Progress

- [x] 2026-09-28: Trace both failures in session 51a8864993190bfa7f45b7711daf1d70 and inspect the installed adapter.
- [x] 2026-09-28: Implement typed projected configuration, shared replay classification, and worker-owned live controls.
- [x] 2026-09-28: Add behavior and compatibility regressions; run full tests, strict Clippy, formatting and diff checks. Two existing subagent wording assertions fail; all other tests pass.
- [x] 2026-09-28: Review and record the implementation on the current branch (hel4), without pushing.

## Surprises & Discoveries

The worker journal has goal capability at ordinal 181425, native replay begin/commit at 181426/181578, and configuration replacement at 181581. The latter erases projected goal state. The installed Codex adapter publishes commands before its load response; the worker drops all session updates until that response. The worker's operational goal state remains intact.

The first full test run exposed stack overflow in four isolated move tests when GoalState was embedded inline in projected configuration. Its optional field is now boxed to retain a small session/future footprint. A checkpoint reuse fixture also relied on configuration replacement erasing synchronized goal state; its archive now includes the fake live worker's synchronized empty goal. Production archive comparisons remain strict.

## Decision Log

2026-09-28: Preserve existing JSON and checkpoint representations, but use a typed configuration with separate adapter values and optional goal state in Rust. Adapter configuration replacement owns only adapter values. This prevents deletion of goal state by construction without adding database migrations.

2026-09-28: Share the classification of session-state announcements versus native history. Allow state announcements during load, suppress history until the existing load boundary, and retain late historical-tool filtering. Live chat controls read the worker's operational goal state rather than the independently projected database copy.

## Outcomes & Retrospective

Both bugs are fixed. Adapter settings and boxed typed goal state have separate owners, current session announcements survive history loading, and live controls use worker-owned goal state. Existing JSON and checkpoint formats are unchanged. The isolated adapter-loading regression passed, including pre-response commands and goal metadata, suppressed old tools/usage/history, and more than 64 KB of intact live output. Full validation passed all new, recovery and upgrade tests. The only full-suite failures are two existing literal subagent-instruction assertions, verified against unchanged HEAD source. Strict Clippy, rustfmt and diff checks passed. No live session or database was changed.

## Context and Orientation

`mj-core/src/state.rs` defines the materialized session, the database-backed conversation view. Its configuration map currently includes both adapter settings and JSON-encoded goal state. `mj-core/src/storage.rs` defines projection mutations, and `mj-core/src/archive.rs` defines canonical checkpoint state. All three must use the same typed configuration while retaining their current serialized form.

`mj-transcript/src/projection/` folds journal observations into that materialized state. Both SessionConfigured and ConfigOptionUpdate currently replace the entire configuration map. Goal updates, restart and context clearing encode/decode goal JSON in the same map.

`mj-worker/src/acp/drive.rs` receives adapter notifications. Its load gate currently suppresses every ordinary session update. `mj-worker/src/acp/session.rs` opens the gate after load, or before new/resume paths that do not replay. The shared native-history classification is in `mj-core/src/acp.rs`.

`mj-chat/src/chat/active.rs` consumes a managed snapshot containing both operational worker state and the materialized transcript. Live goal controls should use the former; historical views retain the projected goal state.

## Plan of Work

Milestone one introduces shared typed configuration with adapter values and optional GoalState. Keep adapter keys flat and the goal field named mj_goal_state in JSON. Update database, mutation, checkpoint and projection consumers. Full adapter configuration replacement still removes old settings; goal state remains untouched. Replace repeated JSON interpretation with typed reads and updates.

Milestone two narrows suppression to native history. Commands, config, mode and session metadata pass during loading. Messages, tools, plans, usage and unknown future variants remain suppressed during replay. Rename the gate to express that it controls history. Preserve native-subagent replay and late-tool filtering. Live chat initial attachment and refresh both take goal state from the operational snapshot, without transcript refresh overwriting it.

Milestone three proves behavior with a fake adapter that publishes metadata before its load response, interleaved with more than 64 KB of replayed content. Verify metadata survives, old content stays suppressed, and new content flows. Add projection, legacy serialization/checkpoint and chat-control regressions, including an already-damaged projected goal. Reuse the existing isolated worker replacement/upgrade tests.

## Concrete Steps

Work from /home/jonathan/Projects/mjolnir4. Retain the existing mbx/Cargo storage configuration. Run focused package tests while implementing, then cargo test and cargo clippy --all-targets -- -D warnings on the dev profile. Every cargo test runs with elevated permissions outside the restricted sandbox. Run cargo fmt --all -- --check and git diff --check. All executable integration invocations use --instance goal-metadata-regression or an existing test's isolated configuration/data directories.

## Validation and Acceptance

Reopening a fake native session must retain its pre-response command advertisement and goal state without replaying old messages, tools or usage. Both configuration update forms must preserve goal accounting, execution, capability and recovery decisions while removing obsolete adapter settings. An attached chat must accept goal objectives and supported controls using operational state even if projected metadata is missing, and reject unknown slash commands. Legacy configuration JSON and checkpoints round-trip without format changes. Existing clear, restart, and isolated upgrade tests remain passing.

## Idempotence and Recovery

Do not mutate the reported live session, change adapter pins, redirect build output, or force a busy worker restart. Existing clients gain live goal controls from worker state; missing advertisements refresh on normal safe worker replacement. No new schema, wire or checkpoint format is intended. Stage only this task's files and commit on the current branch; do not push.

## Artifacts and Notes

Initial cargo test --no-run passed after the type conversion. The focused command cargo test -p brokk-mj-worker loading_native_history_preserves_current_commands_and_goal_metadata -- --nocapture passed. Final full test and Clippy evidence is in target/goal-metadata-tests-final.log and target/goal-metadata-clippy-final.log. The test command was cargo test --no-fail-fast; it completed every target and reported only subagent_mcp::tests::instructions_collect_results_through_wait_and_never_promise_a_push and subagent_mcp::tests::the_tools_explain_parked_children_and_the_live_child_limit as failing. target/goal-metadata-baseline-check.log records that their source is identical to HEAD and the asserted phrases were already absent. The tests were not weakened or skipped. No generated logs belong in product documentation.

## Interfaces and Dependencies

Add a shared SessionConfiguration type in mj-core with separate values and optional goal fields, keeping the old flat JSON representation. Use it for MaterializedSession.configuration, MaterializedSessionMutation.configuration, canonical checkpoint configuration, and database configuration decoding. Keep the shared GoalState transition implementation and existing relay operational goal field as the authority for live controls. No new dependencies or crates are required.

2026-09-28 implementation note: SessionConfiguration uses serde flattening for adapter values and the existing mj_goal_state key for its optional typed goal. Only the legacy WorkerSnapshot boundary still decodes goal JSON. Live chat passes the operational GoalState explicitly when applying its materialized transcript; historical callers select the projected state.

2026-09-28 validation revision: The first full run passed chat, checkpoint, and client suites, then reported four move-test stack overflows and the stale checkpoint reuse fixture. Clippy passed for that revision. Boxing the optional goal and correcting the archive fixture address those findings; validation is repeated for the final revision.
