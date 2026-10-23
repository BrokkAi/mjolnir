# Forward child questions to their direct parent

This living ExecPlan follows `.agents/PLANS.md`. The parent authorized implementation without commits and owns the separate TUI edits.

## Purpose / Big Picture

A Mjolnir child raising a harness question or permission request that would reach a person should get a cancelled form immediately, while its direct parent receives the question through the existing message mailbox. The parent answers using ordinary send_message. Humans continue answering top-level session questions normally.

## Progress

- [x] (2026-10-23) Traced elicitation, projection, mailbox, authorization and launch ownership.
- [x] Parent resolved cancellation identity race: worker cancels its own ACP responder; controller only forwards durable events.
- [x] Implement generic worker cancellation and direct-parent forwarding in projection transactions.
- [x] Implement child guidance; restore send_message authorization and its test exactly to HEAD because State forbids nested sub-agents.
- [x] Preserve full-access automatic permission approval and its original error path; worker regression passes for every harness and both policies.
- [x] Add compatible migration 93 and durable historical-question suppression; focused migration/projection/review regressions pass (6 tests).
- [x] Workspace Clippy with warnings denied and formatting checks pass.
- [x] Run all five full crate suites and all three separate NO_COLOR-unset theme runs. Full run: 4068 passed, 11 failed, 37 ignored; every initial failure passed targeted retries. Theme comparison: 3/3 passed, plus the full-suite theme failure passed an additional NO_COLOR-unset retry.

## Surprises & Discoveries

Elicitation IDs reset per ACP runtime, so daemon cancellation would race newer questions. Cancellation now stays with the original worker responder. Full-access tool auto-approval must remain exactly as before for children as well as top-level sessions; only human-form paths are cancelled. The worker already receives managed-child identity in WorkerLaunchConfig.environment, but removes that private marker before building the ACP LaunchSpec. Carry it as an internal boolean rather than extending a persisted wire format. Muse's question-routing form is automatically answered before it becomes a user-facing question; preserve that internal routing.

## Decision Log

The parent approved worker cancellation, API-event-sequence deduplication, existing SessionMessage payloads and direct-parent-only forwarding. The requested own-child sender exception proved unreachable: State::validate_subagent forbids a sub-agent parent. The parent therefore decided to retain the no-nesting invariant and restore send_message authorization and its test exactly to HEAD. Older workers may keep their questions pending; the controller does not cancel them. Use existing database projection transactions to atomically persist InputRequired and its mailbox message, preserving a question even when its cancellation is in the same event page. Review found repair_projection can rewind the projection and replay historical observations under new API sequences. Compatible migration 93 records input_required_event_frontier; projection replacement preserves that frontier and replay below it does not insert historical InputRequired events again. The API-sequence mailbox key therefore remains stable, including legacy pending-question recovery. Reconcile only still-pending historical questions on periodic mailbox sweeps, avoiding replay of old resolved requests.

## Outcomes & Retrospective

Implementation is present. Projection regression passes. Authorization validation exposed the existing no-nested-subagents invariant; the parent chose to preserve that boundary and remove the authorization change and nested fixture. Worker ACP regression passes for all harness kinds and both policies. Full validation completed; initial timing/configuration/instruction-budget failures all passed targeted retries. Clippy and formatting pass. The separate theme test passes 3/3 with NO_COLOR unset; inherited NO_COLOR explains its full-suite monochrome failure. No protocol change is needed. Compatible migration 93 adds a durable question-event frontier with read/write floor 92.

## Context and Orientation

`mj-worker/src/acp/drive.rs` translates ACP questions and permission requests into RuntimeEvent elicitations. `LaunchSpec` is the in-process ACP configuration; `WorkerLaunchConfig` already marks managed children. `mj-controller/src/database/events.rs::insert_api_event` writes events within relay projection transactions. A SessionMessage in `mailbox_outbox` is delivered durably and wakes its recipient. Existing send_message authorization remains unchanged.

## Plan of Work

Carry managed-child identity to LaunchSpec. On every user-facing question path, publish the requested and cancelled observations and cancel the original ACP responder without registering an externally answerable form. Preserve Muse's automatic route selection and normal top-level approval behavior. Add a small shared question formatter in `mj-core/src/elicitation.rs`.

After inserting a child's InputRequired API event, enqueue SessionMessage to its direct live parent in that transaction with key child ID plus API seq. Read the captured request, not the final pending state. Reconcile historical pending forms using their most recent matching InputRequired event. No controller cancel RPC exists. Extend the existing mailbox sweep for reconciliation, using blocking database work off its async loop.

Keep managed senders unable to send messages: nested sub-agents cannot exist. Add guidance to HANDBACK_REPORT_RULES.

## Concrete Steps

Edit the named runtime and database modules, keeping other agents' TUI files untouched. Run cargo fmt checks and focused controller/worker integration regressions with their existing isolated directories. Run full suites for brokk-mj-core, brokk-mj-worker, brokk-mj-controller, brokk-mj-tui and brokk-mjolnir, plus cargo clippy --all-targets -- -D warnings in dev profile; every cargo test runs elevated. Keep existing Cargo/mbx settings and output layout.

## Validation and Acceptance

Projection integration tests must show a child question and permission question become exactly one waking parent SessionMessage each even when resolved in the same page, that transaction rollback/replay cannot duplicate or lose them, reused elicitation IDs receive independent keys, and a top-level question stays local. Worker ACP integration must show cancelled responses for child questions/permissions and unchanged pending forms for top-level sessions. Existing authorization tests must continue to refuse sub-agent senders; no nested fixture is introduced. Existing scripted ACP bridge tests can drive these cases without a real provider; report whether a full isolated daemon fake-harness check is available.

## Idempotence and Recovery

The API event, question-event frontier and outbox event commit or roll back together. Projection repair preserves the question-event frontier before rewinding its replay cursor, preventing historical API question insertion after daemon restart. Replay sees the committed projection frontier; the stable seq-based event key provides deduplication. Historical pending forms are retried idempotently by the mailbox sweep. Daemon replacement has no cancellation duty and cannot cancel a reused worker request ID.

The final validation also runs auto_theme_paints_light_on_a_terminal_that_reports_a_white_background in brokk-mjolnir three separate times with NO_COLOR unset. Combined-tree failures must be reported, and only failing tests are rerun.

Final evidence is in `.mj/agents/aed94104d79f29d5e9b3e637ff84cfdf/validation-summary.md`, including exact failing names and retry logs. Existing report-rule wording was shortened without weakening its requirements so the added child guidance fits the unchanged 1800-byte MCP description budget. No commits were made.
