# Global task-type delegation backend

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Users configure two global child model/effort choices, Straightforward and Subtle. Parents choose a task type instead of model selectors. Create and Move choose Native or Mjolnir; Mjolnir requires both global choices and discovery availability. Old durable policy and spawn data remain readable.

## Progress

- [x] 2026-10-11 Read instructions, fixed contract, and consumer map.
- [x] Replace core policy and configuration, retaining legacy decoding.
- [x] Update daemon selection, validation, MCP, REST, CLI and wire.
- [x] Remove obsolete TUI controls, update docs and isolated e2e fixtures.
- [x] Validate build, fmt, owned-crate clippy, six crate suites and both e2e scenarios; workspace lint needs the parent-owned mj-chat fixture edit.

## Surprises & Discoveries

The old policy carries selectors throughout persisted state and launch specifications. Compatibility must be at the policy decoder so all readers agree. The new stored mode and durable task_type require schema revision 81 with a breaking compatibility floor and config version 17 to protect global tables from older writers: older decoders cannot read them. Existing Move preflight only validated a changed policy; the resolved policy must be validated to enforce availability on retained Mjolnir policies.

## Decision Log

The parent fixes the design in `.mj/subagent-redesign-contract.md`: Native/Mjolnir/None policy, global optional choices, required MCP task_type, legacy spawn defaults to Straightforward. No commits are authorized. Browser sources and web tests belong to another implementer.

## Outcomes & Retrospective

Implementation is in the working tree. All owned suites are validated (one controller timing flake passes serially); workspace lint exposed one mj-chat profile fixture outside delegated ownership. The parent-message API refuses child senders, so that boundary question must be delivered in handback unless the parent updates the fixture independently.

## Context and Orientation

`mj-core/src/subagent.rs` defines durable policy and tool actions. `mj-core/src/config.rs` holds global configuration; `config/harness.rs` holds profiles. The daemon in mj-controller selects profiles using discovery and quota, registers children, and owns prepared replay. mj-worker exposes MCP. mj-cli translates flags. mj-client consumes durable types. mj-tui Move must compile with two choices, while a later implementer adds global Settings and Create.

## Plan of Work

First change shared types and compatibility decoders. Then update controller validation and both REST and worker spawn paths to choose the global task type while preserving prepared selections. Remove Jev effort verdict modules and adaptive constants. Update MCP schema, CLI parsers, browser metadata, and the minimal TUI consumers. Replace selector-era coverage with compatibility and task routing regressions. Update assigned docs and Python fixtures.

## Concrete Steps

From the repository root, run `cargo build --workspace --exclude brokk-mj-desktop`, `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings`, `cargo fmt --check`, and `cargo test -p mj-core -p mj-controller -p mj-worker -p mj-cli -p mj-tui -p mj-client` outside the restricted sandbox. Keep mbx storage unchanged. Read the Python test headers and run their isolated commands when prerequisites exist.

## Validation and Acceptance

Legacy single_model/all_models/boolean data loads as Mjolnir/Native, legacy profile config disappears on save, and old durable spawns default to Straightforward. MCP lists no list_profiles and requires exactly the new schema. Spawn uses the configured task model and effort and quota ranking; unconfigured Mjolnir fails Create/Move and spawn. Required checks pass, with baseline failures and evidence explicitly reported.

## Idempotence and Recovery

Only edit owned source files and stage nothing. All live validation uses named instances or existing isolated test directories. Never modify default host data or build storage. Rerun worker load flakes serially.

## Artifacts and Notes

Logs belong in `.mj/agents/25ee9b8caf2c36ecf1b136203c38ee45/`.

## Interfaces and Dependencies

`SubagentPolicy::{Native,Mjolnir,None}`, `SubagentTaskType::{Straightforward,Subtle}`, `SubagentChoice { model, effort }`, `SubagentConfig::{choice,is_configured}`. Parent role is `SubagentMcpRole::Parent` with fixed_parent accepted on read. REST uses optional task_type; durable action uses defaulted task_type. Viewer top-level state uses `subagents_configured: bool`.

Updated during implementation: shared types and all owned consumers are migrated; validation remains in progress.

The legacy save regression exposed comment-preserving TOML keeping deprecated nested keys; the document editor now explicitly removes profile choices and the deprecated global enabled key.

Validation evidence and exact handoff names are recorded in `.mj/agents/25ee9b8caf2c36ecf1b136203c38ee45/validation.md`. No changes were committed.
