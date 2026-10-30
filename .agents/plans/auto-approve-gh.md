# Automatically approve GitHub CLI shell commands

This ExecPlan is maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Supported Mjolnir harnesses should run `gh` shell commands without asking a person for tool approval. Native command rules should authorize them before the harness asks Mjolnir wherever that mechanism exists. ACP, the Agent Client Protocol, carries permission requests from harnesses to the worker; a shared worker decision provides approval for harnesses without native rules. Ordinary permission handling remains available for other commands, compound scripts, and plans.

## Progress

- [x] (2026-10-30) Map all eight supported harnesses and current approval paths.
- [x] (2026-10-30) Assign disjoint shared-ACP implementation and Claude native metadata changes.
- [x] (2026-10-30) Add native Claude allow metadata on all session opening paths, including sessions without MCP servers.
- [x] (2026-10-30) Verify pinned Codex native rule discovery; assign session-private worker startup rule installation.
- [x] (2026-10-30) Confirm Kimi/Grok TOML rule syntax, OpenCode ordered permission-array overlay, MiMo ordered JSONC rules, and DSH callback-only permissions.
- [x] (2026-10-30) Integrate worker startup preparation for Kimi/Grok and actual Codex session ID for local/remote/container private profile recognition.
- [x] (2026-10-30) Finish OpenCode/MiMo configuration implementation, private-layer normalization, durable interruption recovery, and startup integration.
- [x] (2026-10-30) Prepare and review a separate Muse adapter change for opt-in command prefixes before its automatic reviewer; protocol and Rust suites pass.
- [x] (2026-10-30) Finish Muse 0.20.0 release preparation and required local checks; preserve both reviewable patches.
- [ ] Obtain the separate repository's required publication/branch authorization before publishing and pinning the Muse adapter.
- [x] (2026-10-30) Review implementation and focused ACP integration/security tests, including DSH command/args and Kimi streamed input.
- [x] (2026-10-30) Run the full worker crate suite: 762 tests passed, 10 ignored; exit 0.
- [x] (2026-10-30) Finish workspace Clippy with all targets and warnings denied; worker formatting and diff checks pass.
- [x] (2026-10-30) Commit runtime changes as `f16d774e3`; merge current upstream cleanly as `48531b782` after the rejected push, and push to origin/master.
- [x] (2026-10-30) Verify agent-dev image publication for `48531b782`: run `41090818137` succeeded on amd64, arm64, manifest publication, and image inspection.

## Surprises & Discoveries

There is no existing shared executable allow list. `mj-worker/src/acp/permissions.rs::OwnedMcpApprovals` handles trusted Mjolnir MCP tools for DSH, while `drive.rs` handles the common permission callback. Kimi can supply command input through previously streamed tool content instead of `rawInput`; `permission_tool_input` already resolves that distinction. The shell parser in `mj-transcript` produces presentation summaries and must not be treated as authorization evidence.

Both pinned Codex and Muse perform automatic review before ACP approval delivery. Codex's native prefix rules avoid that review for covered commands. No supported seeding mechanism for Muse's persisted native command grants was established; globally disabling its reviewer would affect unrelated permissions and is outside this design.

Independent review found that the existing remote/container private profile check followed a linked `profiles` parent. The shared predicate now checks that directory itself, and an existing security test covers the exact escape without disallowing linked ancestors above a real profiles directory. The original helper commit is `a602a316a5`.

The actual Codex 0.165.0 executable accepts the generated rule for `gh issue view 123` and bare `gh`, and does not match `gh-evil`. Its execpolicy CLI checks argv directly; its runtime separately parses shell wrappers, so the CLI wrapper probe does not establish runtime wrapper behavior.

## Decision Log

The user requested all `gh` subcommands, so approve an executable named exactly `gh` with arbitrary literal arguments. Keep other executable names and independent shell execution subject to existing approvals. Use the existing Bash grammar dependency rather than build a shell lexer. Structured argument vectors must retain their boundaries. Recognize only bounded, unambiguous shell wrappers used by supported harnesses. Plans remain subject to consent. These decisions preserve the requested trust boundary without broadening approval to unrelated commands. Date: 2026-10-30.

Claude receives `Bash(gh:*)` in its existing configured-approval `allowedTools` metadata on new, load, and resume requests. The shared worker callback is also used for Claude requests that reach ACP. YOLO already authorizes ordinary tool requests and needs no additional native allow rules. Date: 2026-10-30.

The user clarified that native approval before asking Mjolnir is preferred and specifically requested Codex coverage. Codex supports `prefix_rule(pattern=["gh"], decision="allow")` in a `.rules` file under its active user configuration layer. Verify the managed profile staging path and add native rules for other harnesses that offer them; retain ACP for missing native capabilities. The official rule documentation is at `https://learn.chatgpt.com/docs/agent-configuration/rules`. Date: 2026-10-30.

Install the Codex rule during worker-side private profile preparation in `mj-worker/src/worker_runtime/subagents.rs::configure_codex_profile`, before role-dependent MCP early returns. This covers remote/container workers, recovery, and worker upgrades without depending on controller restaging. Preserve the existing canonical-directory guard so legacy symlinks/original user profiles receive no writes. The existing controller profile transport already copies Codex rules. Date: 2026-10-30.

Use the existing `private_staged_home(root, session_id, home)` as the authoritative predicate for private profiles, including local `root/profile` and remote/container `profiles/session-id`. Codex and other native profile writers share this helper. Kimi/Grok edits live in `worker_runtime/gh_rules.rs`; OpenCode/MiMo edits live in `worker_runtime/json_gh_rules.rs`. The parent retains only shared startup wiring and final integration because these depend on all slices. Every filesystem edit runs in the existing supervised, bounded blocking preparation task. Date: 2026-10-30.

OpenCode v2.2.1 supports ordered native `permissions` arrays through `OPENCODE_CONFIG_CONTENT`. MiMo v0.1.16 deep-merges overlays in a way that loses scalar Bash defaults; edit its private JSONC configuration instead and preserve key order using the existing jsonc-parser package's concrete syntax tree. Do not globally enable serde_json insertion-order maps. Date: 2026-10-30.

Muse's host exposes native persistent grants through interactive choices but no supported configuration schema or seeding command was found. Prepare an opt-in argv-prefix allow facility in the separate BrokkAi/muse-acp adapter, using complete structured shell approval stages and an offered one-time approval before auto-review. Do not guess native policy JSON or disable unrelated reviews. Review the concrete adapter patch and its publication requirements before consuming a new bridge pin. Date: 2026-10-30.

The Muse patch is prepared in `/mnt/optane/tmp/mj-gh-muse-acp-55224` on unchanged master. Its interface is `MUSE_ACP_ALLOW_COMMAND_PREFIXES='[["gh"]]'`; it accepts only an unambiguous complete single shell stage and an offered one-time approval, before review. The separate repository's `RELEASING.md` requires a release topic PR and a distinct publication phase, which conflict with this task's prohibition on branch/PR creation without explicit instructions. Complete its version/changelog and required checks, then ask for that explicit external delivery authorization. Do not pin an unpublished adapter version in Mjolnir. Date: 2026-10-30.

MiMo's private permission layers are consolidated using native ordered merge/evaluation semantics to preserve other commands' decisions. Record the complete set of output files in `.mj-gh-permissions.pending.json` before replacing any configuration file; replay and complete that batch before reading layers on retry. This prevents an interruption from reinterpreting a mixture of old and new permission layers. Only the three known native configuration filenames are accepted, with regular-file guards. OpenCode's managed allows must be the final ordered suffix, since any earlier identical allow can be shadowed. Date: 2026-10-30.

## Outcomes & Retrospective

Six harnesses have native GitHub CLI rules: Codex, Claude, Kimi, Grok, OpenCode, and MiMo. DSH uses the common ACP approval path. The shared path also handles eligible requests from other harnesses that reach the worker, without approving compound scripts or plans. Runtime changes are committed as `f16d774e3` and pushed through clean upstream merge `48531b782`. Full worker tests pass (762 passed, 10 ignored); workspace Clippy with all targets and warnings denied, worker formatting, and diff checks pass. Upstream added checkpoint and session GitHub-credential work without changing these permission sources; successful validation was reused. Agent-dev publication succeeded at `https://github.com/BrokkAi/mjolnir/actions/runs/41090818137`, including both architectures and published image inspection.

Muse requires the separate adapter release before approval can reliably happen before its internal reviewer. Release 0.20.0 is prepared and locally validated, including full adapter Rust suites, Clippy, protocol, launcher, and npm packaging checks. No adapter remote mutation has occurred. The complete adapter patch is preserved in `.agents/docs/muse-gh-adapter-0.20.0.patch`; its Mjolnir pin/image/environment companion is `.agents/docs/muse-gh-integration.patch`. Both patches pass applicability checks. Do not apply the companion until the adapter has been authorized and published.

## Context and Orientation

`mj-core/src/config/harness.rs` enumerates Codex, Claude, Kimi, Grok, Muse, OpenCode, MiMo, and DSH. `mj-worker/src/acp/drive.rs` owns ACP permission delivery, including plan routing, forms, away-mode decisions, and delegated sessions. `mj-worker/src/acp/permissions.rs` constructs allowing responses and maintains related policy helpers. `mj-worker/src/acp/launch.rs::session_request_meta` sends native Claude `allowedTools` on every session opening operation. `mj-worker/src/acp/plan_tests.rs` contains `PlanProbe`, which exchanges protocol messages with the real worker callback without invoking an external model. `mj-worker/src/acp/tests.rs` checks native session metadata and recorded Kimi tool content. Rust package names have a `brokk-` prefix.

## Plan of Work

First add the native Claude allow entry while preserving its existing Mjolnir MCP entries. Add shared worker authorization before ordinary permission routing, excluding plan approvals. Resolve tool input with the existing extractor and require shell execution identity. Parse shell strings with the pinned tree-sitter Bash grammar; reject malformed input, independent commands, pipelines, substitutions, redirections, environment assignments, and unsupported wrappers. Handle argument vectors without joining them into a script. Bound script size and wrapper recursion. Independently investigate pinned local harness sources to confirm which payloads reach ACP and whether additional native settings are necessary.

Then exercise approval and refusal through ACP requests, checking all harness kinds and recorded payload distinctions. Independently review the resulting security boundary, correct concrete defects, and run final validation. Commit only changed files on the current branch and push to origin/master. If upstream has moved, fetch and merge origin/master before retrying the push.

## Concrete Steps

Run commands from `/home/jonathan/Projects/mjolnir/.mj/clones/55224b4db70fd4f5a2ded170be482077`. Use `cargo test -p brokk-mj-worker <affected-filter>` during development with elevated permissions outside the restricted sandbox. After final edits run `cargo test -p brokk-mj-worker` and add full suites for any other touched crate, followed by `cargo clippy --all-targets -- -D warnings`. Use the default dev profile and existing mbx cache configuration; never redirect target output. Run `cargo fmt --all -- --check` and `git diff --check`. Stage explicit files, commit, and push `HEAD:master`.

## Validation and Acceptance

A Guardian ACP request to execute `gh issue list` should receive its offered one-time allow option immediately, including when the command is supplied through streamed content or a recognized shell wrapper. It should leave no permission form pending. A request to execute `gh issue list && unrelated`, a similarly named executable, or a plan must follow the existing approval flow. Native Claude new/load/resume metadata should contain `Bash(gh:*)` even without memory or delegation. Missing allowing options must not fabricate permission. Final tests and Clippy must pass; any unrelated failure must be investigated and reported with evidence.

## Idempotence and Recovery

All tests keep their existing private data directories. Any live binary invocation must use a separate named `--instance`; do not use the default daemon or live store. No installation or runtime configuration changes are required. Repeating focused tests and metadata generation is safe. Do not change branches or include unrelated working-tree edits.

## Artifacts and Notes

Initial tree was clean on master. No related open issue was found by searches for `gh whitelist`, `gh permission`, or `whitelist`. Child reports live under `.mj/agents/`; their transcripts and raw logs need not be committed.

## Interfaces and Dependencies

Keep the authorization helper private to ACP permissions and reuse `allow_permission_response` for option selection. Use `permission_tool_input` as the sole input extractor. Existing grammar versions are `tree-sitter = 0.25` and `tree-sitter-bash = 0.25.1`; adding direct worker dependencies should reuse the already locked packages. Do not introduce a workspace crate, database migration, or new user-facing setting.

Revision note (2026-10-30): Updated through implementation, independent security and recovery review, full local validation, clean upstream integration, push, and image publication. The only remaining task is the prepared Muse adapter's explicitly authorized release and companion integration.
