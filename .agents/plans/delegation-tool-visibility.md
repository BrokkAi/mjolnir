# Expose delegation tools and align parent guidance

This ExecPlan follows `.agents/PLANS.md` and records the implementation and validation of Claude Code and Codex delegation visibility.

## Purpose / Big Picture

Parents should see Mjolnir delegation as an ordinary way to investigate and complete work. Keep design and acceptance with the parent; encourage broad investigation, parallel independent assignments, and concise evidence. Expose the owned delegation tools without requiring a discovery step, using each harness's existing controls. Keep other MCP servers, permissions, and selected child models unchanged. Delegation remains a model decision, not a harness-enforced checkpoint.

## Progress

- [x] (2026-09-28) Inspected native Claude routing, native sessions, and both pinned harness implementations.
- [x] (2026-09-28) Revised shared parent policy and shared spawn/server routing; configured Claude alwaysLoad and passed initial targeted tests and clippy.
- [x] (2026-09-28) Implemented Codex registration at worker startup, including existing worker upgrades and delegation disable.
- [x] (2026-09-28) Review: trimmed server instructions to mechanics, kept routing beside `spawn`, replaced the three-search rule with a context-fraction rule, removed stale discovery text (Codex keeps a one-line hint for legacy ACP-delivered homes). Format, 106 subagent tests, staging and ACP launch tests, and clippy pass.
- [x] (2026-09-28) Live, isolated instance `delegation-visibility`, bare target: Codex lists the six mj-agents tools in code mode beside the ACP-delivered mj-memory tools, and `list_agents` succeeds with no discovery step; Claude reports every mj-agents tool immediately callable while other MCP tools stay deferred. Both a Codex and a Claude parent spawned a child unprompted on a broad investigation question after one or two local lookups.
- [x] (2026-09-28) Startup step ids are joined with `-` (commit "Join startup step ids with a hyphen so the worker relay admits them"). Repeated live: a Codex session created with a first prompt and `--model` answered; a Claude parent spawned two children in parallel, both worked and handed back, and the parent synthesized their reports with no rejected command ids.
- [x] (2026-09-28) Committed on the current branch and pushed to origin/master.

## Surprises & Discoveries

Claude's native Agent routing disappears when Agent is disabled. MCP alwaysLoad is supported by the pinned SDK 0.3.280, not just the newer locally installed CLI. Codex 0.156.1 supports omit_tools_from, but the pinned ACP bridge 1.13.5 only forwards per-server approval metadata. It cannot forward this exposure setting.

Worker upgrades replace the executable and launch configuration without restaging the private profile. Therefore controller-only Codex registration would break upgraded sessions once ACP registration was removed. The worker must configure its own private profile before starting the bridge. Earlier local sessions can instead name the user's original home, directly or through a compatibility symlink. Canonical path equality and a real directory at the staged path are required before writing. Legacy shared homes retain ACP delivery; a runtime registration flag carries that decision into request construction. Flat dotted CODEX_CONFIG overrides cannot safely complement the bridge's whole mcp_servers override: Codex applies same-layer HashMap entries in unspecified order, allowing the whole table to erase the dotted setting.

Startup follow-up steps are queued with ids such as `api-startup-<hex>:model` and `subagent-spawn-<hex>:model` (`mj-controller/src/daemon/startup_followup.rs`, since 0a98bc4f on 2026-09-27). The worker relay accepts only ASCII alphanumerics, `-` and `_` in a command id (`mj-worker/src/relay/commands.rs`, `validate_identifier`), so every such step is rejected with "invalid command ID" and retried forever. Effects: `mj new` with a first prompt, `--model`, or `--effort` never runs its turn, and every spawned child with a selected model never receives its prompt. master carries the same code. Independent of this task; it blocks live validation of delegation.

## Decision Log

- Decision: Use Claude's staged server alwaysLoad and Codex's native per-server omit_tools_from = [deferred]. Rationale: supported controls keep other tools' search and code-mode behavior intact. Date: 2026-09-28.
- Decision: Configure the complete Codex server atomically in the worker's private config.toml before every worker bridge initialization; omit its ACP duplicate. Rationale: one complete configuration preserves transport, visibility, approvals, and upgrades without relying on bridge deduplication or override order. Date: 2026-09-28.
- Decision: Share routing text between spawn and MCP initialization, while keeping the longer parent policy readable. Rationale: the tool should explain when to delegate, with matching guidance for both harnesses. Date: 2026-09-28.

## Context and Orientation

The controller stages session-private harness profiles. Claude's server lives in mj-controller/src/controller/worker_binary/staging.rs. Codex previously received its server over ACP in mj-worker/src/acp/launch.rs. Worker initialization in mj-worker/src/worker_runtime/unix.rs knows the resolved private credential home, worker executable, socket, role, and execution policy. mj-worker/src/worker_runtime/subagents.rs owns the local delegation endpoint. The shared parent policy is mj-core/assets/subagent-delegation.md; MCP descriptions are mj-worker/src/subagent_mcp.rs.

## Milestones and Plan of Work

First align shared guidance with focused native task routing: narrow lookups locally, broad searches delegated before reading, independent assignments together, no duplicate investigation, and optional context reuse. Keep parent design ownership and existing handback/wait semantics.

Then configure Claude's owned server upfront. For Codex, add the workspace's existing toml dependency to the worker, parse the private profile structurally, replace only the owned mj-agents registration, retain other servers, and write atomically. Remove the owned entry when delegation is disabled. Preserve the configured-approval policy and leave native Agent suppression unchanged. Keep registrations for other harnesses over ACP.

Finally validate the private-profile configuration and lifecycle paths. Commit and push only this task's files.

## Concrete Steps

Work in /home/jonathan/Projects/mjolnir4. Use the normal mbx-backed Cargo setup, with elevated execution for tests. Run cargo fmt --all -- --check; cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller --lib subagent; focused worker profile and ACP lifecycle tests; controller staged-profile and delegation-guidance tests; and cargo clippy -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller --all-targets -- -D warnings. Logs live under /tmp/mj-delegation-*. Do not run a test daemon against the live default instance.

## Validation and Acceptance

The Codex profile tests must cover all parent/child roles and both execution policies, replacement of an obsolete registration, preservation of other servers and model settings, repeated configuration, disabling delegation, absent config, and invalid config without overwriting it. ACP request tests must cover new/load/resume so the profile-backed servers are not injected twice and other owned servers retain approval policy. Existing staging tests prove Claude's roles and upfront setting; existing instruction tests prove both harness profiles receive the policy once without altering source instructions. Tests must pass with formatting and clippy before push. A live cost or delegation-rate improvement is not established by these mechanical tests.

## Idempotence and Recovery

Profile writes are atomic and repeated registration has the same result. Configuration errors propagate before harness launch. This changes no durable database schema or launch format. Existing workers with private homes converge when the new worker initializes their profile. Legacy shared homes continue using ACP without modification and gain upfront loading after normal restaging. Keep the user's original profile and other servers intact. Do not redirect build storage.

## Artifacts and Notes

Initial validation passed 103 subagent tests, two Claude staging tests, one shared-policy staging test, and clippy before the Codex implementation. An isolated pinned Codex 0.156.1 app-server config/read probe accepted omit_tools_from = [deferred] and default_tools_approval_mode = approve without starting a model turn. Final validation is recorded below when complete.

## Interfaces and Dependencies

Use the workspace's existing toml crate for structured private-profile changes. configure_codex_mcp takes the worker root, configured home, optional role, and execution policy. It derives its executable and socket from the running worker and returns the resolved profile-registration decision. The role is computed once in worker startup and reused for profile registration and socket exposure. No MCP protocol, model selector, native tool suppression, or user-facing configuration option is added.

## Outcomes & Retrospective

Both harnesses expose the delegation tools upfront in live sessions, parents delegate on their own, and after the startup step id fix children start and hand back end to end.

Revision note: records the worker-owned Codex configuration design and the upgrade/override-order constraints that require it.
