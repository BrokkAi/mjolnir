# Preserve delegation accounting and make startup cleanup truthful

This ExecPlan is maintained according to `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections must be updated throughout implementation.

## Purpose / Big Picture

Issues #1204, #1203, and the remaining #1192 gaps are one authorized task. Parents should receive concise, complete delegation tool guidance, reuse useful children, and supply assignments through instructions alone. Failed startup must release processes without hiding teardown failures. `mj usage --parent ID` must report the parent's whole tree, including cleaned-up descendants, with separate model and effort totals and honest coverage.

## Progress

- [x] (2026-09-30) Read the issues and inspected existing startup, accounting, projection, and MCP paths. User selected a combined implementation, existing readiness design, and whole-tree reporting by model.
- [x] (2026-09-30) Implement durable accounting and tree reporting; focused accounting and CLI tests passed. Historical migration fixtures and final full-suite validation passed; included in the accounting/cleanup checkpoint.
- [x] (2026-09-30) Implement restartable startup cleanup, truthful capacity and status, poller exclusion, and bounded shutdown joining. Final full-suite validation passed; included in the accounting/cleanup checkpoint.
- [x] (2026-09-30) Simplify spawn arguments and parent guidance. All 31 MCP contract tests and the accepted legacy prompt regression passed on final code. Committed checkpoint 04067804 on master.
- [x] (2026-09-30) Complete dev-profile validation passed: cargo test --quiet -- --test-threads=8, cargo clippy --all-targets -- -D warnings, rustfmt, and diff checks. All 11 isolated daemon startup/upgrade tests and final named-instance delegation passed. Results recorded; final accounting/cleanup checkpoint completes this plan.

## Surprises & Discoveries

The branch already implements much of #1192: control socket publication at acceptance, full JSON worker probes, progress-aware readiness, and failed-launch stop attempts. Teardown nevertheless receives the possibly cancelled launch executor, and failed teardown can incorrectly settle a child as Error, releasing its live-child slot.

Usage rows reference `materialized_sessions`, whose deletion cascades from `sessions`; child relationships also reference operational sessions. `session_contexts` is the existing durable identity independent of operational deletion. Projection pages coalesce configuration changes, so model/effort must be captured while applying individual turn-start events, not from the final page configuration.

Historical migration fixtures often downstamp a current store rather than building a shipped schema. They must remove the newly introduced accounting tables and startup state before replaying earlier migrations. When a fixture edits sqlite_schema on the same connection, writable_schema=RESET refreshes SQLite’s parsed table cache; merely switching writable_schema off leaves ALTER TABLE using obsolete offsets. The existing true historical-revision test passed before these fixture corrections.

The child handback description initially measured 1,898 bytes because the complete shared report rules were repeated after a verbose introduction. Its introduction was shortened without changing those rules.

Root rollback previously attempted checkout deletion even when process termination failed, and the checkout helper could reset an exhausted cleanup deadline. Rollback now proceeds to checkout removal only after confirmed stop. The lifecycle owner supplies the single fresh budget, and checkout cleanup preserves that budget; behavior tests cover both failed stop and exhausted cleanup.

Accepted worker-wire requests previously allowed instructions up to the handoff byte limit, independently of HTTP prompt character validation. Legacy execution keeps that original contract, including attachment-free requests. New MCP assignments trim and reject empty instructions before dispatch; new HTTP assignments use their existing prompt validation.

## Decision Log

Decision: retain accounting under session contexts rather than copying it during cleanup. Rationale: one durable owner removes deletion and snapshot-copy races. Date/Author: 2026-09-30 / Codex.

Decision: keep whole-turn totals separate from partial reports; use provider model breakdowns and preserve unknown attribution. Rationale: different models and missing reports must not produce misleading totals. Date/Author: 2026-09-30 / user and Codex.

Decision: new MCP/HTTP spawn requests reject files and context, but accepted legacy wire requests retain execution support. Rationale: worker queues and database requests survive upgrades and cannot silently lose accepted content. Date/Author: 2026-09-30 / Codex.

Decision: share the existing independent 30-second failed-unpark cleanup executor with initial startup and recovery, and allow bounded cleanup to join during shutdown. Rationale: cancellation must not disable teardown or close its durable store before the result is recorded. Date/Author: 2026-09-30 / Codex.

Decision: commit accounting and cleanup together after validation because the two numbered migrations share one compatibility floor and fixture changes. Keep the MCP contract as a separate validated checkpoint. Date/Author: 2026-09-30 / Codex.

## Outcomes & Retrospective

Implementation and historical fixture corrections are complete. All 2,074 controller tests passed, including migration, cancellation, cleanup capacity, shutdown joining, and accounting behavior tests. The isolated delegation run passed with model/effort attribution after reuse, legacy attachments, cleanup recovery after restart, and identical accounting after deleting parent and children. The complete dev-profile workspace suite passed with zero failures, including all 2,074 controller tests, 695 worker tests, 254 CLI tests, and 11 isolated daemon upgrade tests. Final clippy, rustfmt, and diff checks passed. The MCP checkpoint is 04067804; accounting, cleanup, their migrations, and the final validation artifacts are committed together as the second checkpoint. Earlier runs encountered existing worker timing/error-wording failures. Both tests passed unchanged in the final full workspace run; the empty-session recovery regression also passed separately. Previously deleted usage cannot be reconstructed reliably; migration preserves surviving evidence only.

## Context and Orientation

The daemon in `mj-controller` owns SQLite and lifecycle operations. Workers in `mj-worker` own harness processes and durable relay journals. `mj-controller/src/database/usage.rs` reads recorded turn consumption; `materialized.rs` applies worker observations atomically with their event frontier. `mj-core/src/storage.rs` defines usage responses. CLI usage commands are in `mj-cli/src/api_commands.rs`, with HTTP client routes in `api_client.rs` and server endpoints in `mj-controller/src/server/api/turns.rs`.

Child registration is in `mj-controller/src/database/sessions.rs`. Controller startup is in `controller/provisioning.rs`; recovery and serialized lifecycle ownership are in `daemon/` and `recovery/`. `mj-core/src/state.rs` defines persisted session states. Parent MCP schemas, descriptions, and initialization instructions are in `mj-worker/src/subagent_mcp.rs`; HTTP spawn types and prompt rendering are in `mj-controller/src/server/api/types.rs` and `subagents.rs`. Legacy wire actions live in `mj-core/src/subagent.rs`.

## Plan of Work

### Milestone 1: durable accounting and whole-tree queries

Introduce a numbered breaking migration after current revision 66. Rebuild usage/cost tables against session_contexts without changing their existing payloads and replay keys. Add durable child accounting relations, populated atomically at registration and backfilled from surviving subagent_sessions. Capture turn selections under a private table keyed by session and command ID in projection transactions. Capture configuration per event so multiple selections within a page remain distinct. Existing historical selections remain unknown unless reliable evidence exists.

Add UsageSelection and UsageModelTotal response types, model/effort totals and per-turn selections to UsagePage, and a UsageTree response containing root ID, per-session accounting summaries, coverage, and grouped totals. Keep per-session pagination; tree responses contain summaries rather than every turn. The tree endpoint is GET /sessions/{id}/usage/tree. CLI --session and --parent are exclusive; --parent includes the root and all descendants. Tree queries use one read snapshot and never call a live worker. Provider breakdowns are counted instead of, not in addition to, aggregate counters; unattributable residuals remain unknown. Provider cost remains per session with currency, without estimated model prices.

### Milestone 2: startup cleanup

Add a subsequent breaking migration and StartupCleanup persisted state, and advance daemon protocol compatibility. Persist the launch failure and placement before teardown. A single lifecycle owner stops the process group with an independent 30-second executor. StartupCleanup retains live-child capacity, refuses work, and is recovered as teardown rather than startup. Confirmed termination settles Error and archives failed children while retaining bounded diagnostics and accounting. Failed cleanup remains visible and retryable, with bounded backoff and daemon recovery. Keep existing readiness budgets and socket publication behavior. Use shared subprocess and target-stop helpers.

### Milestone 3: concise delegation contract

Remove obsolete arguments from new strict MCP and HTTP requests. New prompt construction trims and validates instructions with the existing 256 KiB limit, without attachment reads. Preserve legacy accepted requests and historical summary parsing. Move delegation policy to parent-only initialization instructions, preserving both parent modes and Codex hints. Add reuse and file-pointer guidance. Rewrite all descriptions with mechanics first and a maximum of 1,800 UTF-8 bytes. Child handback instructions remain independent.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir` on the current branch. Claim the three issues with the agent-in-progress label. Run focused tests outside the sandbox as each milestone completes. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` in the dev profile before final delivery. Preserve normal Cargo/mbx storage; do not redirect target output. Use named instance `issue-1204` for every CLI/daemon/delegation invocation of the new build.

## Validation and Acceptance

Prove usage survives close, profile move, failed startup, forced cleanup, parent/child deletion, reopening, and duplicate projection replay. Test configuration changes and multiple turns within one page, provider breakdowns, unknown selections, missing/partial usage, and equality of tree/session totals. Test new CLI argument exclusivity and text/JSON model distinctions.

Prove cancelled launch still runs teardown; failed stop retains capacity and does not relaunch; daemon restart resumes cleanup; confirmed stop releases capacity exactly once. Preserve existing isolated migration and upgrade regressions. Extend `tests/e2e/delegation.py` and invoke it with a named instance and the dev binaries.

Exercise real MCP initialize and tools/list output for both parent roles and Claude/Codex, enforce the description budget, reject obsolete new arguments before socket dispatch, and replay a legacy accepted attachment request without losing content. Expected checks pass with zero clippy warnings and no formatting diff.

## Idempotence and Recovery

Migrations run transactionally once per new revision and raise the compatibility floor; never rewrite shipped migrations or upgrade the live store with a test build. Accounting keys prevent duplicate replay charges. Failed cleanup remains durable until confirmed termination and can be retried after restart. Do not delete working files before stopping their owning process group. Stage only changed files and commit validated checkpoints on the current branch; do not push.

## Artifacts and Notes

Record focused test results and full-check logs here as milestones finish. Remove agent-in-progress when the task is finished or stood down. No live host instance should be touched during verification.

## Interfaces and Dependencies

Use existing Rust, rusqlite, serde, Axum, Clap, and subprocess helpers. Do not add crates or dependencies. Preserve the existing relay turn/outcome payloads; turn selection belongs to daemon accounting projection. The public additions are UsagePage accounting metadata, UsageTree, HTTP usage/tree, CLI usage --parent, and the StartupCleanup state with coordinated daemon compatibility.

Revision note (2026-09-30): initial implementation plan records the approved combined scope and current repository evidence.

Revision note (2026-09-30): implementation is recorded with focused results, historical-fixture corrections, MCP byte-budget evidence, and the bounded shutdown ownership adjustment. Full validation and commit checkpoints remain pending.

Revision note (2026-09-30): recorded successful isolated delegation verification, process-before-checkout ownership, bounded cleanup deadlines, and preserved accepted-wire validation.

Validation artifact: final named-instance delegation run passed at `target/reliability-artifacts/delegation-seed-1171-4093265`; the earlier successful run is at `target/reliability-artifacts/delegation-seed-1171-3749920`. Both use isolated configuration and data.

Revision note (2026-09-30): all final validation passed. The implementation is delivered as two local commits on the current master branch; issue labels are removed after the final checkpoint. No push or release was requested.
