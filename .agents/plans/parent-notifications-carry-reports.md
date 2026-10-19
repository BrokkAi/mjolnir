# Deliver child reports through the durable mailbox outbox

This living ExecPlan follows `.agents/PLANS.md`. The parent replaced the original queued-prompt design on 2026-10-18 while implementation validation was running.

## Purpose / Big Picture

Parents receive finished child reports as messages written by Mjolnir. Reports arrive between tool calls while the parent works and start a new turn when it is idle. Parents do independent work and end their turn when none remains. No parent tool blocks or arms a wait.

## Progress

- [x] (2026-10-18) Removed wait and await_children tools, blocking execution, wait-specific interruption, rollback and continuation overrides. Retained historical request and saved-data decoding and immediate legacy-worker guidance.
- [x] (2026-10-18) Removed the superseded queued-report code, per-parent locks and retry scheduler.
- [x] (2026-10-18) Investigated mailbox opt-out and old-worker capabilities after the replacement design arrived.
- [x] (2026-10-18) Parent resolved compatibility: deliver PlainText to all protocol-33–40 workers, regardless of launch settings. Hookless workers receive reports after their current turn.
- [x] (2026-10-18) Added SubagentFinished and atomic enqueue/mark reconciliation; the outbox owns delivery and retries.
- [x] (2026-10-18) Mailbox hooks and delivery are unconditional; no_github_events excludes only GitHub watcher targets, while github.watch.enabled and jev.enabled gate the GitHub source.
- [x] (2026-10-18) Added atomic rollback, stale/idempotent reconciliation, legacy PlainText delivery, no_mailbox and hookless wake regressions. Updated the existing golden and e2e scenarios.
- [x] (2026-10-18) Regenerated mailbox/settings goldens. Fixed wake eligibility for semantic waiting and event-key retries across PlainText/structured upgrades.
- [x] (2026-10-18) End-to-end recovery exposed startup cleanup settling after initial reconciliation. The existing five-second mailbox sweep now also reconciles durable finishes.
- [x] (2026-10-18) Renamed the CLI/API opt-out and Rust session field; retained the physical database column and legacy saved-session JSON decoding. Moved the source switch to github.watch.enabled with silent legacy folding and config version 18.
- [x] (2026-10-18) Full seven-crate validation passed: 4,416 tests, zero failures, 32 ignored; workspace Clippy and formatting passed. Browser and documentation checks passed.
- [x] (2026-10-19) Final binaries and isolated delegation recovery scenario passed, including startup-cleanup report recovery and 0.395-second daemon handoff.

- [x] (2026-10-18) Review repairs: atomically transfer legacy wait reports into the outbox before deleting the effect; select batches per target; reject symlinks throughout Claude settings staging; render child reports as readable session messages.
- [x] (2026-10-18) Repaired build passes workspace Clippy excluding desktop with warnings denied and formatting. Corrected a new test fixture to use existing database test functions without widening writer visibility.
- [x] (2026-10-18) Finished all seven crate suites: 4,418 passed, 1 known deadline failure, 32 ignored; the failed mbx release test passes serially, covering 4,419 unique passing tests. Earlier daemon reuse and missing-doctest-artifact failures pass in the final suite and targeted rerun.
- [x] (2026-10-18) Copied-binary isolated delegation retry passed, including report recovery and 0.357-second daemon handoff. Current logs are in `.mj/agents/83d9ed261525fb2a56ca86d5ea90d85f/`.

## Surprises & Discoveries

Older workers accept DeliverMailboxEvent independently of hook policy. However, false agent_mailboxes_enabled removes both Claude PostToolBatch and Codex PostToolUse hooks. A disabled older worker therefore cannot automatically drain between tool calls. RelayOperationalState does not publish the captured launch flag. Current daemon settings are not authoritative for a worker started before a configuration change. Installed launch.json is refreshable through worker_launch_refresh_plan, so reading that file does not by itself prove the running process's captured flag.

The superseded prompt design used a relay ledger retaining only the newest 512 completed commands. Mailbox event keys and atomic enqueue-and-mark eliminate the prompt admission/database-write ownership split. Existing mailbox drains batch events; queued ordinary prompts are promoted separately.

The relay's display activity can be Expecting after the parent ends its turn. Mailbox wakes now use the existing activity::can_submit predicate, so a semantic wait does not strand a report. A retained command ID may also refer to an admitted PlainText report from an older worker; retries compare the mailbox event key across old and new representations.

## Decision Log

Review repairs preserve a legacy wait report even when its old model may already have seen it. Retirement reads the durable effect, inserts matching marked reports using the common finish key, and deletes the effect in one transaction. The old worker receives the immediate compatibility answer. Outbox selection limits each target separately so one target cannot consume another target's batch; within-target order and bounded concurrent delivery remain intact. Claude startup repair checks root, profile, .claude, and settings.json with symlink_metadata before reading or writing. Child report text is escaped like session text and remains outside the external-source wrapper, with explicit provenance and no new authorization.

The parent now requires mailbox delivery only: one SubagentFinished event per finish; its key derives from parent ID, child ID and finish identity. Insert the outbox event and update reported_finish in one database transaction. The outbox then owns retries, including a reconciliation scan on its existing five-second sweep. Remove prompt submission, per-parent prompt locks and the prompt retry scheduler. Keep historical decoding and do not add a schema migration. Add relay protocol 41 for the new body, render PlainText for older supported workers, and retain rows for workers that cannot receive them.

A later parent instruction makes mailbox machinery unconditional in new workers. The final policy correction limits the session opt-out to GitHub watcher target registration; it does not affect peer messages, tool availability or the events API. Global github.watch.enabled and jev.enabled gate only GitHub event production. Config version 18 protects the moved watcher switch from older writers. Loading silently folds the old mailbox.enabled into the new value with logical AND; saving emits only the new key. The final user naming decision uses --no-github-events and no_github_events in creation APIs, without aliases. The sessions.no_mailbox database column remains and maps to SessionRecord.no_github_events. The flag has exactly one effect: the GitHub watcher does not register the session as a delivery target. Children inherit it. The final parent decision delivers to all older workers at or above protocol 33 as PlainText; hookless workers receive the report after their current turn ends. No launch-time capability inference is needed, and only workers below protocol 33 remain pending.

## Outcomes & Retrospective

Review repairs are complete and validated. Legacy wait retirement preserves reports transactionally; per-target outbox batches prevent blocked parents from consuming other targets' selection; Claude settings repair rejects staging symlinks; reports render as readable session messages. Full seven-crate suites covered 4,419 unique passing tests after the known mbx deadline test passed serially, with 32 ignored. Clippy excluding desktop and formatting pass. Delegation passes from copied binaries in named instance parent-report-repair-83d9-retry with 0.357-second daemon handoff. Earlier mixed Cargo invocations caused a missing doctest dependency and a daemon-reuse failure; both pass in final validation. The first end-to-end attempt timed out during the delayed child fixture and later accepted its exact report; the copied-binary retry passes. Detailed logs and counts are in `.mj/agents/83d9ed261525fb2a56ca86d5ea90d85f/`.

Wait removal and its mailbox replacement are implemented. Full Rust validation passed across all seven touched crates, with 4,416 passing tests, zero failures and 32 ignored. Workspace Clippy excluding brokk-mj-desktop, formatting, browser checks and documentation builds passed. The first end-to-end run needed an idle-parent file-injection fixture update; the next exposed a missing report after asynchronous startup cleanup, repaired by the existing outbox sweep. The final binary build and isolated recovery rerun passed, including report recovery and a 0.395-second daemon handoff. No commit or installation was performed.

## Context and Orientation

`mj-core/src/mailbox.rs` defines mailbox bodies and rendering. `mj-controller/src/database/mailbox.rs::enqueue_mailbox_event_with` inserts an event using a supplied database connection. `mj-controller/src/mailbox_outbox.rs` delivers durable rows to relay workers. Child completion reconciliation lives in `mj-controller/src/server_runtime/api.rs::reconcile_parent_reports`; daemon startup and child completion call it through `mj-controller/src/daemon/delegation.rs` and the runtime wrapper in `daemon/subagent_park.rs`.

Worker hook installation is in `mj-worker/src/worker_runtime/subagents.rs`; launch preparation in `worker_runtime/unix.rs` installs hooks independently of the historical peer-tool launch flag. The relay always understands mailbox admission and drain requests, but older disabled workers have no harness hook to trigger a busy-turn drain.

## Plan of Work

Use SubagentFinished with task name, child ID, state, parked status, login failure and repair hint, pending messages and delivery outcomes, bounded report text and report_dir. Omit a running-child list. Render explicit Mjolnir provenance and child-data framing. Enqueue and mark through one database transaction using a deterministic per-finish event key; no relay acceptance controls reported_finish.

Replace ensure_parent_wait_prompt and its scheduler with startup and child-completion reconciliation. Extend outbox delivery to translate new events to PlainText for eligible older workers. New workers always install Claude and Codex mailbox hooks. Keep GitHub target policy distinct from mailbox delivery and peer-tool availability. Use the final CLI/API names; retain the old physical database column and saved-session JSON decoding. Update the shared instructions and configuration documentation.

## Milestones

The first milestone removes parent wait tools while retaining old persisted shapes and immediate guidance for older workers. MCP contract and legacy replay tests verify this boundary.

The second milestone atomically enqueues structured child reports and records ownership in the database. The mailbox sweep reconciles durable finishes and retries delivery every five seconds; the delegation scenario verifies recovery after startup cleanup and daemon replacement.

The third milestone makes report delivery independent of GitHub event selection, renames the user controls and preserves old configuration files. Config round trips, creation API/CLI contracts, browser goldens and the full touched-crate suites verify the result.

## Concrete Steps

Work from the repository root with ordinary Cargo through the existing mbx cache. Do not redirect target storage, install, commit or push. Run focused failing tests during edits. Final commands are `cargo test --no-fail-fast -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir -p brokk-mj-client -p brokk-mj-chat` outside the sandbox, `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Include full suites of any newly touched crate. Logs belong in `.mj/agents/1956d35bf77e693744f2f3e279a46dd8/`.

## Validation and Acceptance

Regressions must prove atomic finish marking and outbox insertion, idempotent reconciliation including unreported old finishes, PlainText compatibility, and report receipt with no_github_events and github.watch.enabled=false. Retain legacy decode and immediate old-worker wait/await_children regressions. Verify mailbox pending-at-turn-end wake and authorization handling follow the existing GitHub/peer event paths. The live driver is now `tests/e2e/parent_report_notifications.py`; retain its other child-owned changes. Update delegation.py, which has no separate recordings.

## Idempotence and Recovery

The database transaction is the only owner of reported_finish. An outbox row and marker either both commit or neither commits. Stable per-finish keys make reconciliation repeatable; the outbox keeps delivery pending until accepted. No report rollback, epoch increment, prompt fallback or per-parent delivery lock remains in the final design.

## Artifacts and Notes

Initial investigation is in findings.md in the report directory. The original test run and clippy logs describe the superseded design, not final acceptance of the mailbox replacement. Keep historical subagent-wait authorization exclusions if needed to decode old history; assess whether any can be removed safely.

## Interfaces and Dependencies

Reuse the existing mailbox outbox, relay DeliverMailboxEvent, mailbox drain hooks and idle wake. Add a relay protocol constant after RELAY_TURN_CONTROLS_PROTOCOL=40 and return it from the new body's minimum_relay_protocol. Retain old schema-84 request enums, protocol-39 SetSubagentWait as a replay no-op, awaiting_children_since_ms, and old queue delivery fields as decode-only. No new crate or schema migration is planned.

Plan rewritten after the parent's delivery design and unconditional-mailbox policy changes; records the legacy-worker fact needed before implementation continues.

Plan updated after the parent accepted idle-wake compatibility and mailbox implementation replaced prompt delivery. The database also checks retained event keys before updating a marker, so a stale reconciliation cannot move the latest marker backward.

Plan updated after review to cover legacy report preservation, outbox fairness, staging safety, and readable reports. Final repair validation is recorded in the progress and outcomes sections; earlier validation records describe the previous source.
