# Pre-approve Mjolnir MCP tools in Guardian mode

This living ExecPlan follows `.agents/PLANS.md`. Update Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as work proceeds.

## Purpose / Big Picture

In Guardian mode, Codex and Claude should run Mjolnir's memory, delegation, and review MCP tools without asking a model to approve each call. Other tools retain their existing approval policy. Yolo mode stays unchanged. MCP is the protocol by which these harnesses call worker-owned tools; ACP is the protocol between a Mjolnir worker and its harness bridge.

## Progress

- [x] (2026-09-28) Investigated launch paths, pinned bridges, and harness approval settings; claimed #1176.
- [x] (2026-09-28) User confirmed both harnesses and Guardian-only changes, and authorized publishing the required codex-acp release.
- [x] (2026-09-28) Implemented bridge metadata; typecheck, 784 tests, build, and packaged live checks passed. Committed 77a24e3 and pushed authorized v1.13.5; publication succeeded.
- [x] (2026-09-28) Implemented Guardian-only registrations and Claude rules; focused worker matrix and final full workspace tests passed.
- [x] (2026-09-28) Packaged Codex and pinned Claude isolated bridge probes passed Guardian/yolo fresh and resumed calls. Claude debug logs confirm only third-party calls invoked its classifier.
- [x] (2026-09-28) Final binaries passed actual Codex and Claude owned-tool calls and suspend/resume in instance issue-1176; both sessions were destroyed, the daemon stopped, and copied credentials removed.
- [x] (2026-09-28) Published v1.13.5, verified its release workflow, and updated matching registry lockfile, runtime identity, and container pin.
- [x] (2026-09-28) Final full cargo test, clippy with warnings denied, formatting, and diff checks passed; validated Mjolnir changes prepared for the required local commit.

## Surprises & Discoveries

The pinned Codex bridge converts ACP server registrations to Codex connection configuration but drops metadata. Its saved session state already retains full server objects across provider restart. Claude already stages role-specific sub-agent allow rules; its bridge accepts SDK `allowedTools` through session metadata. Claude review servers are staged in the profile rather than sent over ACP. Reviewer configuration rejects unknown fields, so adding a serialized approval flag would break old workers; the final implementation retains the wire shape. The running executable may have a canonical path different from its configured path, so worker-local approval ownership is resolved from the configured worker root.

## Decision Log

Decision: Only Guardian receives new approval configuration. Yolo retains existing configuration and behavior. Rationale: explicit user requirement. Date/Author: 2026-09-28, user and Codex.

Decision: Carry approval with the owned registration, not a name-prefix match or global setting. Identify the private review dispatcher by the same worker executable, exact server name, and worker review-mcp subcommand. Rationale: third-party analyzers can share a session with the owned review dispatcher, and existing reviewer wire configuration must remain readable by older workers. Date/Author: 2026-09-28, Codex.

Decision: Release the bridge through its normal BrokkAi tag workflow after validation. Rationale: the user explicitly authorized publication; Mjolnir needs a reproducibly installed dependency. Date/Author: 2026-09-28, user and Codex.

## Outcomes & Retrospective

The bridge is implemented and committed as 77a24e3 (v1.13.5). Both real harnesses demonstrated owned pre-approval and third-party review in Guardian; yolo remained unrestricted. Publication succeeded at https://github.com/BrokkAi/codex-acp/actions/runs/36431473741. The registry lockfile, runtime identity, and container pin now all use 1.13.5. Initial full cargo test and final-tree clippy passed. The final pinned binaries passed actual named-instance Codex and Claude calls and suspend/resume. A final review found that a serialized approval marker would break older workers that reject unknown fields. That marker was removed in favor of exact private invocation recognition, resolved once from the configured worker root. The final full suite and clippy passed, as did formatting and diff checks. Implementation is complete and committed locally with this plan; Mjolnir has not been pushed.

## Context and Orientation

`mj-worker/src/acp/launch.rs` constructs new/load/resume requests and their harness-specific metadata. `mj-core/src/worker_launch.rs` defines persisted review launch configuration. `mj-controller/src/controller/reviewer.rs` constructs the owned review dispatcher and stages Claude review servers; `mj-review/src/bifrost.rs` constructs third-party analyzers. Claude's existing sub-agent registration and permissions are in `mj-controller/src/controller/worker_binary/staging.rs`.

The downstream bridge is the separate existing checkout `/home/jonathan/Projects/codex-acp`, on its current branch. Its `src/CodexAcpClient.ts` converts server registrations for new/load/resume/fork; `src/CodexAcpServer.ts` retains them for provider restart. Follow that checkout's AGENTS.md, run-codex skill, and docs/RELEASES.md. Mjolnir pins the bridge in `mj-core/src/harness_runtime.rs`, `mj-worker/assets/harnesses/codex/`, and `containers/Containerfile.agent-dev`.

## Plan of Work

### Milestone 1: Bridge contract

Add optional per-server `_meta.codex.defaultToolsApprovalMode`, accepting the native enum values auto, prompt, writes, and approve. Forward it as `default_tools_approval_mode` for supported transports, reject malformed metadata, and leave omitted values absent. Test observable thread configuration and saved-server restart behavior. Document the field in readme-dev.md. Do not infer approval from server names.

### Milestone 2: Owned registrations

Attach approve metadata at the memory and sub-agent registration construction sites only for Codex with ConfiguredApprovals. Keep ReviewMcpServer serialization unchanged. Its shared `is_review_dispatch(worker_executable)` method recognizes the exact owned executable, server name, and private subcommand already carried in saved registrations. Resolve that identity once against the configured worker root into a worker-local ReviewerMcpServer wrapper; do not compare against current_exe, which may canonicalize a symlinked path. Retain reviewer registrations in launch state even when their connection configuration is delivered through the staged profile. For Claude Guardian, derive allowedTools from attached owned memory, role-specific sub-agent tools, and the identified private review dispatcher. Keep existing staged sub-agent rules and all existing goal, model, sandbox, and delegation metadata intact. New/load/resume must use the same policy derivation; omitted memory must not gain permissions.

### Milestone 3: Validation and delivery

Exercise both harnesses in an isolated issue-1176 instance. Prove owned calls run without review, a non-owned approval-requiring tool still uses Guardian, resume preserves policy, and yolo is unchanged. Validate the bridge, bump the next patch version, package and test it, commit on main, and publish to BrokkAi via its release workflow. Then update Mjolnir's exact package pin, lockfile, runtime identity, and container version together. Commit only changed files on the current hel3 branch; do not push Mjolnir without further authorization.

## Concrete Steps

In codex-acp run `npm ci`, `npm run typecheck`, `npm test`, `npm run build`, and `npm pack`, with the run-codex skill and isolated live validation. In Mjolnir run focused tests first, then `cargo test` with elevated permissions and `cargo clippy --all-targets -- -D warnings` in the dev profile, plus `cargo fmt --all -- --check` and `git diff --check`. Preserve normal mbx/Cargo storage. All actual mj CLI/daemon test invocations use `--instance issue-1176` and isolated data/config directories.

## Validation and Acceptance

Behavior tests cover owned and unmarked servers together, memory omitted, all delegation roles, fresh/load/resume, and yolo lacking new approval settings. Bridge tests reject invalid enum values and preserve unrelated configuration. Live evidence records successful owned calls without Guardian review and an unmarked mutation being reviewed. Required commands must exit successfully; record actual results below rather than claiming unrun checks.

## Idempotence and Recovery

Do not modify default-instance sessions or global harness configuration. Keep untracked preexisting bridge tarballs untouched. Stop test processes before removing their directories. Publish immutable version tags only after validation; retry a failed workflow with the same tag. Do not roll back unrelated concurrent changes.

## Artifacts and Notes

Bridge checks: `/tmp/issue-1176-typecheck.log`, `/tmp/issue-1176-bridge-tests.log` (784 passed, 32 skipped), `/tmp/issue-1176-bridge-build.log`, and `/tmp/issue-1176-pack.log`. The packaged probe runs live in `/mnt/optane/issue-1176/`; credentials were copied privately and removed after each probe. Codex Guardian emitted no review for owned calls, and emitted Guardian start/end cards for third-party calls both fresh and resumed. Successive writable grants and an explicitly authorized out-of-sandbox write passed; yolo emitted no reviews. Claude Auto debug logs show ~1ms decisions for owned calls, while both third-party calls invoked `classifier_request_started` and took ~1.2s. Its yolo probe also passed fresh and resume. Rust validation logs are `/tmp/issue-1176-rust-focused.log`, `/tmp/issue-1176-cargo-test.log`, and `/tmp/issue-1176-clippy.log`.

## Interfaces and Dependencies

The only new bridge interface is optional metadata on each ACP MCP registration. ReviewMcpServer gains only an interpretation method, with no serialized field or relay protocol change. This preserves compatibility with older workers that reject unknown fields and recognizes existing saved dispatcher configurations. No database migration is needed. Claude uses its existing `_meta.claudeCode.options.allowedTools` interface. Codex native and Claude bridge versions stay pinned unless validation shows a concrete incompatibility.

Initial plan recorded 2026-09-28 after the user's implementation and release authorization.

2026-09-28 update: recorded implemented policy and live harness evidence; publication is authorized and underway.

2026-09-28 update: release workflow succeeded; npm metadata became visible after its five-minute CDN cache expired. The lockfile records the published tarball integrity, matching the locally tested package. Initial full Rust suite passed; final-tree checks are running after pinning.

2026-09-28 update: `/tmp/issue-1176-live-mj.log` records PASS and RESUMED for Codex session fe2178006a71ce1dadd0010b9ed9b2ca and Claude session 1fdc3c470f30af844110d126677dc4f8 in instance issue-1176. Owned tool transcripts are in `/mnt/optane/issue-1176/mj/codex-tools.json` and `claude-tools.json`. The disposable fixture required a valid Git HEAD, acknowledgment of intentionally unpublished work, and waiting for asynchronous lifecycle transitions; these were test-driver corrections, not product changes. Cleanup verified zero remaining sessions and stopped the daemon. Final clippy exited zero; final cargo test is still running.

2026-09-28 compatibility correction: replaced the planned serialized marker with `ReviewMcpServer::is_review_dispatch(&Path)`. Older workers use serde deny_unknown_fields, so a default on a new field only helps new readers, not old readers. The exact existing dispatcher invocation supplies the ownership information without changing the wire shape. Added a regression proving a saved registration round-trips unchanged and that another executable, subcommand, or server name is not trusted. Validation for this final correction is in `/tmp/issue-1176-cargo-test-compatible.log` and `/tmp/issue-1176-clippy-compatible.log`. The published bridge and the primary-session memory/delegation policy are unchanged.

2026-09-28 path correction: resolve dispatcher ownership into a nonserialized worker-local wrapper using the same worker root that constructed the registration. The policy matrix now proves that a different canonical executable path does not remove approval. Final validation logs are `/tmp/issue-1176-cargo-test-complete.log` and `/tmp/issue-1176-clippy-complete.log`.

2026-09-28 completion: final `cargo test` and `cargo clippy --all-targets -- -D warnings` both exited zero, using the dev profile and normal build cache. Formatting and diff checks passed. No implementation work remains; the required Mjolnir commit includes this completed plan.
