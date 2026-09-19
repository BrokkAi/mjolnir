# Reliable conversation maintenance (#1100)

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

Users can compact Codex and Claude conversations without hidden context disabling the command, and clear model context without losing workspace files or the visible conversation. Clear requires idle state and creates a new native conversation in the same Mjolnir session, with a durable divider. Kimi support is optional. The user authorized implementation, followed by merging and pushing to origin/master.

## Progress

- [x] (2026-09-19) Inspected pinned adapters and reproduced Codex's first-block parsing failure.
- [x] (2026-09-19) Confirmed idle-only clear and retained visible history with the user.
- [x] (2026-09-19) Implement shared complete-name parsing, canonical command spelling, and compaction context retention.
- [x] (2026-09-19) Implement durable clear admission, native replacement, rollback, settings restoration, and transcript boundary.
- [x] (2026-09-19) Connect UI/API capabilities and protect checkpoint/handoff/review boundaries.
- [ ] Run focused, full, and isolated live validation; commit, merge, and push.

## Surprises & Discoveries

Codex 1.11.4 handles compact but not clear. Claude ACP 0.79.0 explicitly excludes clear. Mjolnir prepends hidden context at `mj-worker/src/worker_runtime/unix/dispatch.rs::acp_command`; the installed Codex parser returns null for that shape. Kimi 2.0.2 advertises compact but returns immediately after starting background compaction.

## Decision Log

Keep Mjolnir identity and old visible history; change native identity and persist a context boundary. This preserves the requested interface without falsifying what the model remembers. Refuse clear while any work is outstanding. Preserve project memory and accepted configuration, but discard pending conversational handoff/shell context. Use shared ACP session creation, never provider-specific textual clear commands.

The clear completion records the new native identity, history boundary, and fresh project-memory context in one journal transition. A pre-adoption crash reports interruption and retains the old identity. Restore all advertised selectors and the execution mode before adoption, including Fast and plan mode. Clear carries an explicit activity timestamp so upgrades cannot interrupt replacement startup. Database migration 41 is breaking because old readers cannot interpret the new persisted command/outcome; relay state is version 8, protocol 15, and archives containing a boundary require schema 5. Kimi clear remains disabled; its existing native background compact behavior is preserved.

## Outcomes & Retrospective

Implementation is complete; validation and integration are in progress. Focused lifecycle tests cover replacement and startup rollback; the full suite also exercises archive preservation, handoff isolation, relay deduplication, and UI capability gating.

## Context and Orientation

`mj-core/src/relay/snapshot.rs` defines durable commands/state; `mj-worker/src/relay/commands.rs` admits and claims them. The coordinator in `mj-worker/src/worker_runtime/unix/dispatch.rs` moves commands to the ACP process and persists runtime results. `mj-worker/src/acp.rs` supervises bridge replacement, while `mj-worker/src/acp/session.rs` creates native sessions and restores settings. A native session is the provider's conversation, distinct from Mjolnir's durable session and transcript. Controller materialization, checkpoint canonical snapshots, and handoff generation must preserve and honor the clear boundary.

## Plan of Work

Milestone 1 adds shared complete-name parsing in core ACP helpers and prevents compact from claiming hidden context. Add behavior tests with large shell output and a subsequent ordinary prompt. Reject unsupported Codex compaction instructions and exclude maintenance turns from automatic review.

Milestone 2 adds a typed ClearContext relay/runtime operation with idle admission and a durable completion boundary. Hold dispatch during replacement, restore accepted selectors, and only publish success after native adoption is persisted. Failed pre-adoption startup resumes the old conversation. Crash recovery before adoption retains the old identity; after adoption it uses the new identity. Reset current goal, plan, context usage, and conversation-specific activity, retaining files, history, settings, and project memory.

Milestone 3 exposes clear in TUI/web help and API routing, gated by worker capability/protocol. Carry the boundary through materialized state and checkpoint restore and filter handoff/review input to the current conversation. Version persisted changes so older binaries refuse unsafe reads/writes; use isolated stores for breaking migrations. Kimi clear is enabled only if the same lifecycle tests pass; do not pretend background compaction has completed.

Milestone 4 validates, commits coherent changes to the current branch, merges to master, and pushes origin/master. No live store is upgraded for validation.

## Concrete Steps

Work from `/home/jonathan/Projects/hel4`. Run focused Cargo tests for changed behavior, then `cargo test` with elevated sandbox permissions, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and relevant web checks. Use isolated MJ_CONFIG_DIR/MJ_DATA_DIR for live sessions and migration tests. Review the final diff before staging only changed files.

## Validation and Acceptance

Compact must reach the adapter as a command while preserving pending startup/handoff/shell context for the next ordinary prompt exactly once. Clear must reject busy/queued/background work and arguments/images, change native identity, preserve files and selectors, append exactly one divider, and survive reconnect/restart. Test duplicate requests and failures around adoption. Checkpoint restore and cross-harness handoff must not resurrect pre-clear context. Test both command surfaces and direct prompt submission, and run isolated authenticated Codex/Claude checks where credentials and tooling permit.

## Idempotence and Recovery

The durable command ID is the retry identity. Adoption and the context boundary must be one durable transition. Before adoption, recovery uses the previous native conversation and reports interruption rather than claiming success. After adoption, recovery uses the replacement. Do not delete provider history or working files. Test migrations only against isolated stores.

## Artifacts and Notes

Authenticated isolated checks (2026-09-19): pinned Codex adapter completed a prompt, `/compact`, and native identity replacement in 26.86s; pinned Claude adapter completed the same sequence in 17.64s. Both used temporary project/config/data directories and left the personal controller database unopened. These checks establish native command completion and replacement; a short Claude conversation may produce its native too-short-to-compact response rather than a summary. The opt-in worker test `live_adapter_compacts_and_replaces_context` reproduces the sequence.

Installed-parser probe: plain `/compact` returned `{name:"compact",rest:""}`; the same command after a hidden context block returned null. This proves command routing fails before model invocation.

## Interfaces and Dependencies

Reuse ACP, existing relay commands/events and subprocess supervision; add no crate. Add ClearContext command/completion and capability, and a durable boundary usable by transcript and archive consumers. Share maintenance parsing between admission, UI validation, and compaction detection. Preserve existing normal prompt and queue semantics.
