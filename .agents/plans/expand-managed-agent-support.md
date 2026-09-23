# Expand managed coding-agent support

This ExecPlan is a living document governed by `.agents/PLANS.md`. Keep its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections current during implementation.

## Purpose / Big Picture

Mjolnir currently manages Codex, Claude Code, Kimi Code, Grok Build, and Muse Code. A person who primarily uses OpenCode, Gemini CLI, GitHub Copilot CLI, or Cursor CLI cannot select that agent as a Mjolnir profile and use Mjolnir's session lifecycle. Add these agents in that order, one independently usable integration at a time. For each, a person should be able to configure or discover an account, start a session, exchange prompts and approval requests, detach and return, and recover work through Mjolnir's supported continuity path. A name in a picker or a process that merely starts is not completion.

The first milestone delivers OpenCode as a real managed agent. The later milestones apply the same acceptance standard to Gemini, Copilot, and Cursor. The order is an implementation hypothesis based on documented Agent Client Protocol (ACP) support, not a claim about user demand. Revisit it when user requests or protocol probes justify doing so.

## Progress

- [x] (2026-09-23 12:24Z) Audited the five existing harness kinds, managed runtime pins, worker launch, profile documentation, and continuity code.
- [x] (2026-09-23 12:24Z) Verified that OpenCode, Gemini CLI, Copilot CLI, and Cursor CLI publish ACP stdio launch modes in their official documentation.
- [ ] Probe OpenCode's pinned ACP build in an isolated instance and record its actual session, authentication, permission, model, and resume behavior.
- [ ] Complete and validate the OpenCode integration across configuration, installation, runtime, UI, and continuity.
- [ ] Complete and validate Gemini CLI with the same user-visible acceptance criteria.
- [ ] Complete and validate Copilot CLI with the same user-visible acceptance criteria.
- [ ] Complete and validate Cursor CLI with the same user-visible acceptance criteria.
- [ ] Update user documentation and comparison, perform all required checks, and commit each validated integration on the current branch.

## Surprises & Discoveries

- Observation: Harness support is currently a closed five-variant `HarnessKind` enum. It reaches credential detection, execution policy, runtime installation, checkpoint capture and restore, native import, SessionWiki, quota reporting, and the UI. Adding an ACP launch command alone would leave major behavior incomplete.
  Evidence: `mj-core/src/config/harness.rs`, `mj-core/src/harness_runtime.rs`, `mj-worker/src/worker_runtime/harness.rs`, and `mj-checkpoint/src/checkpoint/native_scan.rs` all match on harness kind.
- Observation: The four candidate agents advertise ACP over standard input and output, but their advertised capabilities and authentication flows differ. Treat ACP compatibility as the starting point for a probe, not as evidence of full Mjolnir compatibility.
  Evidence: official launch commands are `opencode acp`, `gemini --acp`, `copilot --acp`, and `agent acp`.

## Decision Log

- Decision: Make OpenCode the first full vertical integration, followed by Gemini CLI, Copilot CLI, and Cursor CLI. Do not add every candidate to the picker before its own behavior passes acceptance.
  Rationale: OpenCode documents model and effort selection, prompt content, and session options over ACP, making it a useful first test of the existing abstractions. A sequence of complete increments avoids implying that an untested agent has Mjolnir's normal durability and safety behavior.
  Date/Author: 2026-09-23 / Codex.
- Decision: Preserve explicit per-agent capability reporting and fail visibly when a required lifecycle operation is unavailable. Do not silently run a different CLI, skip a checkpoint, drop an approval, or claim native restore without evidence.
  Rationale: Session recovery and approvals are part of Mjolnir's product contract. An unsupported feature must be named to the person using it.
  Date/Author: 2026-09-23 / Codex.
- Decision: Treat quota, external native-session adoption, and use as a utility or review profile as separately earned capabilities.
  Rationale: ACP launch support does not provide provider quota endpoints, another tool's native history format, or reliable review output. Each can be added after the core session behavior, with its absence visible in the UI and docs.
  Date/Author: 2026-09-23 / Codex.

## Outcomes & Retrospective

Planning is complete; no harness implementation has begun. The plan identifies a first working increment and the integration surfaces that must be checked. Update this section after each agent lands, including any capability that remains unsupported and the observed reason.

## Context and Orientation

An agent harness is the program that conducts the coding conversation. ACP is the line-oriented JSON protocol between Mjolnir's worker and that program. A profile selects one harness and one account. A target is the machine or container where the worker runs. A checkpoint is Mjolnir's verified recovery archive containing repository and, when needed, native agent session state. A native session is the agent's own conversation identity and history, distinct from Mjolnir's session record.

`mj-core/src/config/harness.rs` defines `HarnessKind`, profile homes, authentication markers, execution policy, and ACP launch arguments. `mj-core/src/harness_runtime.rs` holds exact managed runtime versions. `mj-worker/src/worker_runtime/harness.rs` installs and leases those versions for bare targets, while `mj-worker/src/acp/launch.rs` opens or resumes ACP sessions. `mj-controller/src/setup.rs` and `mj-controller/src/doctor.rs` discover and diagnose profiles. `mj-controller/src/quota.rs` supplies quota rows. `mj-checkpoint/src/checkpoint/native_scan.rs` and `mj-checkpoint/src/checkpoint/restore.rs` capture and restore native history; `mj-controller/src/import.rs` and `mj-controller/src/sessionwiki/harness_adapters.rs` handle external session discovery. `containers/Containerfile.agent-dev` supplies the published container image. `docs/src/content/docs/profiles.md` states the user-facing support contract.

Current official ACP launch instructions are: OpenCode `opencode acp` (https://opencode.ai/v2/docs/cli/acp/), Gemini CLI `gemini --acp` (https://geminicli.com/docs/cli/acp-mode/), GitHub Copilot CLI `copilot --acp` (https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server), and Cursor CLI `agent acp` (https://prod.cursor.com/docs/cli/acp). Copilot's ACP support is labeled public preview by GitHub. These links are sources to recheck when implementation begins; the commands and caveat are recorded here so the plan remains usable without them.

## Plan of Work

Start with an OpenCode protocol probe against one exact version. In an isolated `--instance` and separate agent home, verify ACP initialize, authentication, session creation, prompt streaming, permission requests, model and effort selectors, cancellation, native identity, `session/load` or `session/resume`, and operation after the worker process is replaced. Capture the actual capability response and a short scrubbed event trace in this plan. Confirm how OpenCode scopes its credentials and whether its ACP process and private server can use Mjolnir's staged home without reading an unintended global account. Do not infer this from its CLI docs. If native history cannot be copied and relocated safely, design an explicit Mjolnir-owned transcript handoff for suspend and move before declaring OpenCode supported; retain the original archive for recovery and report any loss of exact native context.

Then add one `HarnessKind` through the existing control path. Update the kind's identity, environment, account discovery, authorization evidence, install pin, entrypoint, ACP arguments, and execution policy in the files above. Make authentication evidence an explicit per-harness capability if a provider does not have a stable credential file; avoid a fake file marker. Update credential synchronization and target staging using an allowlist of files that the probed version actually requires. Install the pinned version atomically in the existing managed cache and include the identical version in `containers/Containerfile.agent-dev`. Keep the worker's active lease and background installation behavior. A missing prerequisite or failed login must show a concrete error without changing an active worker.

Wire the session's ACP capabilities through the worker and client. Test prompts, streamed output, tools, attachments if advertised, approval questions, cancellation, model changes, and detach/reattach. Decide from observed behavior whether a raw target can preserve configured approvals and whether an isolated target can reliably use Mjolnir's unconstrained policy. If either cannot be enforced, disable that target and show why before launch. Verify Mjolnir's project-memory MCP tools; if the agent cannot accept them, show that limitation and exclude it from utility and reviewer selection rather than pretending the tools are installed.

Implement the native continuity path using the actual agent history layout and session identity. Extend checkpoint scan and restore with bounded, path-safe capture, then prove suspend/resume and same-harness move in an isolated instance. Extend external import and SessionWiki only when their native-history parser is verified on fixtures from the pinned agent version; until then, those commands must say the agent is unsupported for external adoption. A new agent must never make older archives unreadable. Any database schema change needs a new migration revision with a compatible or breaking classification beside it; use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` for breaking migration tests.

Once OpenCode passes, repeat this vertical sequence for Gemini, Copilot, and Cursor. Probe each before choosing its version, credential allowlist, approval policy, and continuity format. Copilot's preview ACP status deserves an explicit pin and compatibility test. Cursor documents blocking `cursor/ask_question` and `cursor/create_plan` methods; answer these through Mjolnir's existing structured-input route or refuse launch with an explanatory limitation until they are supported. Do not copy OpenCode's credential or native-history assumptions into another integration.

## Milestones

The OpenCode milestone produces a profile that can be selected and used from the terminal and web surfaces. A small isolated session demonstrates a streamed answer, a tool request, a person answering an approval, an interruption, and a restored conversation after suspend/resume. Its install and image versions match, and failure cases report why they fail. Commit that validated increment before beginning another agent.

The Gemini milestone adds the same usable lifecycle for `gemini --acp`, including its own authentication and history evidence. Complete its isolated behavior demonstration and commit it separately.

The Copilot milestone adds `copilot --acp` with the same lifecycle and a regression fixture for the pinned preview protocol. Complete its isolated behavior demonstration and commit it separately.

The Cursor milestone adds `agent acp`, including its blocking question and plan requests, with the same lifecycle. Complete its isolated behavior demonstration and commit it separately. If a candidate cannot satisfy required lifecycle guarantees, record the blocker and keep it unavailable instead of shipping partial support.

## Concrete Steps

Run commands from the repository root. First inspect `mj-core/src/config/harness.rs`, `mj-worker/src/worker_runtime/harness.rs`, and the ACP and checkpoint modules named above. Search every exhaustive `HarnessKind` match with `rg -n 'HarnessKind::|Self::Codex|Self::Claude' mj-* containers docs/src/content/docs` before editing. Choose a single pinned upstream version after a successful protocol probe; record the exact version, install source, and checksum or lockfile evidence in this plan.

For each integration, add focused behavior tests beside changed Rust modules, then run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. Run every `cargo test` invocation with elevated permissions outside the restricted sandbox, as `AGENTS.md` requires for socket tests. Run all new-build daemon, CLI, TUI, and end-to-end trials with a separate named instance such as `mj --instance harness-opencode`; never use the live default instance. Exercise launch, prompt, approval, detach, restart, suspend, and resume with a disposable repository and test credentials. Do not redirect build output to `/tmp`.

Before each milestone commit, inspect `git diff --check`, the task-file diff, the container pin parity test, and the applicable documentation. Stage only files changed for that integration and commit on the current branch. Do not push unless the user explicitly requests it.

## Validation and Acceptance

For OpenCode, `mj --instance harness-opencode doctor --json` should report the configured profile ready or a specific fixable authentication or runtime error. A new session using that profile must visibly stream a reply. An agent tool requiring approval must appear as a pending request, accept a chosen response, and continue. `mj --instance harness-opencode wait --session <id> --json` must report the completed turn; `mj --instance harness-opencode transcript --session <id> --json` must contain its content. After suspend and resume, another prompt must retain the previous conversation context and repository changes. Repeating the scenario after a daemon restart must not lose a pending question or running turn. Perform the equivalent scenario for the other three agents using their own named instances.

Negative tests must prove that wrong credentials, a missing pinned executable, failed runtime installation, unsupported approval enforcement, and an unavailable native-history path produce explicit failures. No test should declare an agent ready solely because `initialize` succeeded. Verify older five-harness sessions and archives remain usable after each addition. If a capability such as quota, external import, or review is absent, the UI and docs must say so; the core acceptance above is still required.

## Idempotence and Recovery

Managed runtime installation already stages under a versioned cache and leases active versions. Reuse it; retry a failed install by rerunning the isolated launch after correcting the cause. Never overwrite an existing agent's global home during protocol probes. Keep profile staging limited to documented credentials and settings. Archive capture must validate paths and leave the source history untouched. If a continuity test fails, retain the checkpoint and active worker, surface the error, and fix the path before enabling that harness in a release.

## Artifacts and Notes

At each milestone, add a concise sanitized ACP capability result, a test transcript excerpt showing one turn and one approval, the pinned upstream version, and the observed restore result here. Do not include credentials, tokens, callback URLs, or private conversation content.

## Interfaces and Dependencies

Reuse the existing `HarnessKind`, `HarnessPin`, managed installer, ACP worker, checkpoint archive, and API routes. Keep small harness-specific decisions near the existing match arms, but introduce a shared capability type only when two or more new harnesses actually need the same interpretation; search for an existing helper first. Do not create a new workspace crate solely to organize integrations. Preserve existing serialized harness identifiers and archive formats. Public UI and API capability reporting must distinguish supported, unsupported, and temporarily unavailable operations so scripts cannot mistake a hidden fallback for success.

Revision note (2026-09-23): Created after reassessing Herdr and Mjolnir. The plan prioritizes complete managed-agent support over generic terminal or pane features, and uses OpenCode as the first independently verifiable integration.
