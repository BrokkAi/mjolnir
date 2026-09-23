# Registry-backed ACP agent support

This ExecPlan is a living document governed by `.agents/PLANS.md`. Keep its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections current as work proceeds.

## Purpose / Big Picture

Mjolnir should let users select and run compatible agents from the [ACP registry](https://agentclientprotocol.com/get-started/agents), including OpenCode and Antigravity, without adding a new agent category in code for every agent. ACP, the Agent Client Protocol, is the message format between a worker and an agent. The registry is a catalog of ways to download and start agents, not proof that an agent supports Mjolnir's session lifecycle or authentication needs. Show only entries that can run on the worker's platform and pass the relevant capability checks. Keep Gemini out of the default catalog; allow an explicit advanced search for users with enterprise or paid API access.

The result should feel like a managed Mjolnir session: installation, launch, approvals, checkpoints, reconnection, and failure reporting work through existing worker and daemon ownership. Unsupported lifecycle actions must fail clearly before stopping accepted work.

## Progress

- [x] (2026-09-23) Reassess the earlier per-agent plan against the registry and current product availability.
- [ ] Build a cached, filterable registry catalog and a durable selected-agent profile.
- [ ] Run one registry agent, starting with OpenCode, through a generic ACP worker path.
- [ ] Validate capabilities and continuity across daemon and worker lifecycles.
- [ ] Add Antigravity through its official ACP server; evaluate other candidates afterward.
- [ ] Complete isolated behavior tests, full Rust checks, and documentation; commit each validated milestone.

## Surprises & Discoveries

- The prior plan treated OpenCode, Gemini CLI, Copilot, and Cursor as separate harness implementations. Mjolnir previously consumed the ACP registry for Thor; the reusable catalog approach is a better starting point, though today's worker lifecycle requires additional validation.
- The [registry entry for Antigravity](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json) supplies a separate `agy_acp_server` binary, not the `agy` CLI. It currently has platform downloads but no published SHA-256 in its manifest.
- [Google announced](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/) the end of Gemini CLI consumer free/Pro/Ultra access after June 18, 2026, while enterprise and paid API access remain. Gemini should therefore be hidden by default, not described as unavailable to everyone.
- The registry describes launch artifacts. It does not certify authentication, approvals, session restore, checkpointing, quota visibility, or review integration.

## Decision Log

- Decision: Replace the per-agent roadmap with a registry-backed ACP path while preserving the five built-in harnesses.
  Rationale: One launch and protocol path can cover registry agents, but existing sessions must keep their stored identities and behavior.
  Date/Author: 2026-09-23 / Codex.
- Decision: Treat registry entries as opt-in choices, not installed worker inventory. Pin selected identity, version, platform artifact, and launch arguments.
  Rationale: Catalog updates must not silently change an existing session.
  Date/Author: 2026-09-23 / Codex.
- Decision: Filter Gemini from ordinary discovery, while allowing explicit advanced search with an enterprise/paid-access qualification; treat Antigravity as a separate entry.
  Rationale: Google's changed consumer access makes Gemini a poor default, but it remains usable through some accounts.
  Date/Author: 2026-09-23 / Codex.
- Decision: Gate lifecycle features on observed behavior and tested adapters.
  Rationale: Registry presence alone does not establish managed-session continuity, approvals, or authentication.
  Date/Author: 2026-09-23 / Codex.

## Outcomes & Retrospective

Planning revised; implementation has not begun under this revision. Update this section after each milestone with observed behavior and any changes to scope.

## Context and Orientation

A harness is the agent program used by a session. The current five harness categories are closed in `mj-core/src/config/harness.rs`; avoid adding one category per registry entry. A profile records the selected harness and account. A daemon is Mjolnir's long-running control process; a worker is the per-session process that runs the agent. Runtime selection and installation live in `mj-core/src/harness_runtime.rs`, launch construction in `mj-core/src/worker_launch.rs`, and worker-side harness behavior in `mj-worker/src/worker_runtime/harness.rs`. A checkpoint is a verified archive used to recover repository and agent state; related assumptions appear in `mj-checkpoint/src/checkpoint/native_scan.rs`. Before adding interpretation of registry commands or paths, find and reuse existing helpers.

Historical commits `77165172` and `7ca7652d` contain the former registry catalog and setup flow; use them as reference, not as code to restore unchanged. The authoritative feed is `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`. Follow `.agents/PLANS.md` and update this plan as the design is tested.

The daemon owns catalog selection, durable session records, and lifecycle orchestration. Workers own agent processes, turns, relay journals, and pending questions. Catalog refresh, downloads, probes, and installs must run in supervised background tasks rather than UI event loops. Independent requests should remain concurrent, cancellable where rollback is possible, and have visible errors.

## Plan of Work

### Milestone 1: Catalog and durable selection

Fetch and cache the registry with an explicit refresh status and last successful update. Serve cached results during a fetch failure with their age visible. Parse distribution commands and binary artifacts through one shared representation. Filter by the **worker** operating system and architecture, since a local controller may launch remote Linux workers. Keep unsupported distributions visible only when they can be explained accurately.

Apply product filters separately from platform filters. Hide `gemini` in normal discovery; expose it only through explicit advanced search with its access qualification. Do not conflate `antigravity-acp` with Gemini CLI. Selection pins the registry entry/version, artifact identity, and arguments in durable configuration. Define how users deliberately upgrade a pin. Preserve reads and writes of the five existing harness kinds and classify any database migration under repository policy.

### Milestone 2: Generic ACP runtime, first exercised with OpenCode

Build a generic ACP installation and launch path from a selected pinned entry. Use existing subprocess helpers. Cache downloads by identity and version; verify provided checksums and reject mismatches. For archives, reject path traversal and unsafe links before extraction. For entries without a checksum, use a documented trust decision tied to the official publisher or require a user-provided local executable; never silently fall back to a different version or source. Keep installed artifacts leased while workers use them.

Probe ACP initialization and session creation, then map prompts, streaming output, tool calls, permissions, cancellation, and errors into existing worker events. Authentication failures must remain actionable and must not be mistaken for capability absence. Mark features such as review, external import, project memory, and quota visibility available only after an adapter or behavior test demonstrates them. Do not create a new workspace crate solely for this work.

### Milestone 3: Continuity and lifecycle gates

Prove that an active generic ACP turn survives daemon replacement because its worker and relay journal remain authoritative. Test client detach and reattach, replay, pending approval, cancellation, and worker failure. For worker replacement, suspend, and move, support tested native session restoration with captured native state or an explicitly labeled transcript handoff with a verified repository checkpoint. If neither route is safe for a selected agent, reject that lifecycle action before stopping accepted work. Make limits visible in the session UI and API.

### Milestone 4: Antigravity and later candidates

Exercise the official Antigravity ACP server on a supported platform using its own registry entry and distribution. Resolve the missing manifest checksum through the trust rule from Milestone 2. Validate login, approvals, turn streaming, and continuity before marking it managed. Then evaluate Copilot and Cursor registry entries with the same capability matrix. Add agent-specific adapters only for demonstrated gaps that cannot be expressed in the generic ACP path. Gemini has no implementation milestone unless access or user demand changes.

## Concrete Steps

Run commands from the repository root, `/Users/ryansvihla/code/mjolnir`. Start with `rg -n 'HarnessKind::|Self::Codex|Self::Claude' mj-*` and `rg -n 'acp|ACP' mj-worker/src mj-core/src` to find the current profile, worker, and bridge paths. Read the files named above, then use `git show 77165172:src/registry.rs` to inspect the historical registry parser. Record the shared data shape and migration classification in this plan before editing. Re-run these searches after each milestone to find new integration points.

Implement the milestones in order. For each Rust milestone, run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile; run `cargo test` outside the restricted sandbox with elevated permissions as required by `AGENTS.md`. Use `mj --instance registry-acp` for every new-build CLI invocation and the same named instance for daemon, TUI, and end-to-end trials. Never use the host's default session data. Before each commit, run `git diff --check`, review the changed files, stage only those files, and commit on the current branch. Do not push without an explicit request.

## Validation and Acceptance

- A user can find an eligible registry entry, select a pinned version, see its capability and trust status, and start an OpenCode session through the generic ACP path. In an isolated disposable repository, `mj --instance registry-acp doctor --json` should report either a ready profile or an actionable installation or authentication error. A started session should stream a reply and persist it in the transcript.
- Default discovery omits Gemini. An explicit advanced search can display it with the access qualification. Antigravity appears only on platforms with a supported artifact.
- A registry refresh failure retains a clearly aged cache; an install or probe failure reports the actual error without substituting another agent or version.
- A live generic ACP turn, pending approval, and relay replay survive daemon replacement. After reattaching, a user can answer the pending approval and see the same turn complete. Unsupported worker continuity actions reject before accepted work is stopped.
- Existing harness sessions and stored profiles continue to work across any schema change. New migration revisions and isolated migration tests follow `AGENTS.md`.
- For Rust changes, run focused behavior tests, then `cargo test` outside the restricted sandbox with elevated permissions and `cargo clippy --all-targets -- -D warnings` on the dev profile. Do not substitute release-profile checks. For this plan-only revision, review the diff and run `git diff --check`.

## Idempotence and Recovery

Catalog refresh and artifact downloads should be retryable and keyed by pinned identity. A partial installation cannot become selectable. An interrupted probe or failed launch leaves a useful status and does not corrupt a stored profile. Daemon restarts reconstruct in-flight control-plane status from durable state; workers remain the source for active turns. Re-running migration or selection steps must not duplicate records or silently upgrade a pin.

## Artifacts and Notes

Keep this ExecPlan in `.agents/plans/`. Put any internal capability matrix or agent research in `.agents/docs/`, not `docs/`. Record test commands and results here as milestones complete. Relevant sources: [ACP registry](https://github.com/agentclientprotocol/registry), [Antigravity registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json), and [Google's Gemini CLI transition](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/).

## Interfaces and Dependencies

Expect a catalog fetch/cache interface, a pinned registry-agent profile, a generic ACP worker launch specification, and a capability report consumed by both API and UI. Keep platform artifact selection at the worker boundary. Any new persisted shape or wire field must be versioned and remain compatible with existing harness profiles or have an explicitly breaking migration. The registry feed is an external advisory dependency; selected sessions must continue to start from their pinned, installed artifact when the feed is offline.

Revision 2026-09-23: Replaced the per-agent roadmap after the user proposed restoring ACP registry support. Gemini is filtered from default discovery, Antigravity is treated as its own ACP server, and implementation remains stopped pending a separate request.
