# Restore provider-aware Codex mailbox delivery (#1492)

This ExecPlan is maintained according to `.agents/PLANS.md`. A mailbox is the worker's durable queue of messages addressed to a session; a hook is a harness callback after a tool finishes.

## Purpose / Big Picture

Running Codex sessions must receive peer messages at a tool boundary when their provider accepts that history. DeepSeek-backed Codex sessions must continue receiving messages at a turn boundary because DeepSeek rejects the developer message that Codex's post-tool hook inserts between a tool call and its output. Senders must see when delivery waits until the turn ends. The host installation is outside this task.

## Progress

- [x] (2026-11-11) Read #1492 and the journal diagnosis; self-assigned the issue and added `agent-in-progress`.
- [x] (2026-11-11) Updated current master to origin/master 6c8db87a6; confirmed the unconditional PostToolUse suppression from #1277.
- [x] (2026-11-11) Settled status transport: the worker publishes its effective mode in operational state; the daemon reads it without interpreting configuration. The existing staged-home Codex provider resolver identifies the direct DeepSeek service.
- [x] (2026-11-11) Implemented shared delivery mode, provider-aware worker drain, and documentation. Sender response implementation is in integrated review.
- [x] (2026-11-11) Focused isolated E2E `--instance issue1492-mailbox --mailbox-regression-only` passed both provider modes; actual worker configuration and the real hook process were exercised.
- [x] (2026-11-11) Workspace clippy passed with warnings denied. API/controller, chat, core and worker library suites ran; diagnostics #1478 failure passed on targeted rerun.
- [x] (2026-11-11) Completed all touched-crate suites and remaining worker/CLI targets. The initial parallel hook run had one failure and one stall; both passed targeted reruns, and the hook target passed all six cases serially. No production change was made for the transient failures.
- [x] (2026-11-11) Integrated review complete. Commit and push on the current master branch are the final delivery operation; no host installation is requested.

## Surprises & Discoveries

Independent review caught exhaustive relay-state fixture literals missing the new optional field; these are being updated, including the chat fixture. The build of production binaries succeeded. Outbox `accepted_at` proves worker acceptance, not harness consumption; the DeepSeek E2E must wait for worker acceptance before invoking an empty hook and then prove wake-prompt delivery after release.

The starting clone was behind the regression. Fast-forwarding brought in `drain_mailbox`'s unconditional early return for `PostToolUse`. Existing durable leases, acknowledgements, wake turns, and prompt-context delivery remain available and should be reused.

## Decision Log

- Decision: Preserve the existing mailbox delivery and lease mechanism; narrow the provider restriction rather than introducing a second transport.
  Rationale: The journal diagnosis identifies suppression as the failure, and existing Codex hooks previously delivered messages successfully. Investigation will confirm how to resolve the provider once.
  Date/Author: 2026-11-11, Codex.

- Decision: Publish effective mailbox delivery mode in worker operational state and reuse it in sender responses.
  Rationale: A daemon reading profile configuration separately could disagree with an already running worker. Only the worker can report the policy it enforces. This additive runtime field needs no migration or relay protocol bump.
  Date/Author: 2026-11-11, Codex.

- Decision: Restrict only `CodexProviderKind::DeepSeek`, resolved from the effective staged Codex home, and retain tool-boundary delivery for other providers.
  Rationale: The observed failure is the direct DeepSeek API; reusing the existing service classification avoids speculative restrictions based on model/profile names. A DeepSeek model through an unrecognized proxy is outside this classification.
  Date/Author: 2026-11-11, Codex.

## Outcomes & Retrospective

Implementation and the paired isolated provider regression are complete. OpenAI-backed Codex retains its active prompt while the hook returns the peer message; DeepSeek returns an empty hook and receives the same message in a wake prompt after release. The sender sees tool_boundary or turn_boundary and conditional deferral text. Workspace clippy passed. All touched-crate validation completed; no host installation will be performed.

## Context and Orientation

`mj-worker/src/relay/commands.rs::drain_mailbox` serves the Codex `PostToolUse` and Claude `PostToolBatch` hooks. The worker leases messages before responding and acknowledges them after output delivery. `mj-core/src/mailbox.rs` owns shared event representations. `mj-controller/src/server_runtime/api/subagent_input.rs` routes peer and parent messages through durable mailbox rows or legacy turns. `mj-worker/src/subagent_mcp.rs` defines the sender-facing tool and its description. `mj-core/assets/skills/mj/SKILL.md` describes delegation delivery. Existing process-level hook coverage is in `mj-worker/tests/mailbox_hook.rs`; controller API and relay tests cover routing and lifecycle behavior.

## Plan of Work

First trace the resolved Codex provider from profile configuration to worker startup and the sender response. Choose one shared capability owner, avoiding separate worker and daemon interpretations. Then allow the existing post-tool drain for compatible Codex providers while retaining the DeepSeek restriction. Extend the message response with an explicit delivery timing where deferred. Update the tool description and skill to explain timing without promising universal mid-turn delivery. Extend existing regressions to cover a message arriving during an active long turn, its acknowledgement before completion for OpenAI, and preservation until completion for DeepSeek.

## Milestones

### Provider policy and observable delivery

Resolve the configuration and wire path, implement the provider-aware drain, and expose deferral in the sender response. Acceptance is a running compatible Codex recipient consuming a message at its next hook and a DeepSeek recipient leaving the same message pending until the turn ends.

### Regression evidence and delivery

Use existing process-level and isolated end-to-end harnesses. Run focused tests during development; fix failures and rerun only failing tests. After the last change, run full suites of touched crates and workspace clippy once. Review the final diff and push the committed result on master.

## Concrete Steps

All commands run at the repository root. Cargo test commands run outside the restricted sandbox with normal mbx storage. Do not change target directories. Use `--instance codex-mailbox-1492` for a test binary invocation and separate configuration/data directories. Focused regression filters are `mailbox_post_tool_use` and `send_message_to_` with the stable package set `-p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mjolnir`. Build dev binaries with `cargo build -p brokk-mjolnir -p brokk-mj-worker`. Run the isolated daemon/worker fixture with `python3 tests/e2e/delegation.py --mj <target>/debug/mj --worker <target>/debug/mj-worker --instance issue1492-mailbox`. The fixture drives the real hook process at a simulated Codex tool boundary while the fake ACP turn is held. Final validation is `cargo test -p <touched-crates>` and `cargo clippy --all-targets -- -D warnings` on the dev profile. Stage only task files, commit on master, and push to origin/master; merge upstream if the push is rejected.

## Validation and Acceptance

The OpenAI regression must fail against the unconditional suppression and show delivery before the recipient's active turn ends after the fix. The DeepSeek control must show an empty hook response, an event remaining pending, and eventual delivery at the next turn boundary. Sender status must describe deferred timing. Existing mailbox acknowledgement and duplicate-prevention checks must continue passing. Keep all daemon/store tests isolated from live host state.

## Idempotence and Recovery

Investigations are read-only. Existing mailbox event keys and acknowledgements retain their retry semantics. Do not replay acknowledged messages or install the build on the host. Stop test process groups before deleting their temporary data.

## Artifacts and Notes

The original read-only diagnosis lives in the coordinator clone at `.mj/agents/9dd354ecd31b49eb0576b1c4ae63c62e/notes.md`. It documents accepted worker journal events with no post-restart hook leases. Child investigation and validation reports for this task live in this clone's `.mj/agents/` directories.

## Interfaces and Dependencies

Reuse the existing relay and mailbox protocol, configuration resolver, and subprocess helpers. Introduce no database migration or new crate unless investigation proves one necessary. Add `mj_core::mailbox::MailboxDeliveryMode::{ToolBoundary, TurnBoundary}` with snake_case serialization. `RelayOperationalState.mailbox_delivery: Option<MailboxDeliveryMode>` is process-local projected state, absent for old workers. `SessionMessageResponse.mailbox_delivery` projects the published mode; a turn-boundary response explicitly explains deferral. Preserve legacy response routing fields. The worker decides from its staged effective Codex configuration and publishes the same mode it uses to gate hooks. An unresolved worker mode or a mode absent from an older worker must not be advertised as tool-boundary delivery. This does not impose an OpenAI allowlist: the direct DeepSeek restriction uses the existing provider classification. The deferral note is conditional on the recipient still working, since an idle recipient wakes immediately.

Revision note (2026-11-11): Created the plan after confirming #1492 and dispatching provider-policy and regression investigations.

Revision note (2026-11-11): Recorded worker ownership and the additive delivery-mode interface; dispatched disjoint core/worker, API, and documentation slices.

Revision note (2026-11-11): Recorded passing provider E2E and clippy, isolated diagnostics rerun, and outstanding hook/recovery validation. Final E2E evidence is under target/reliability-artifacts/delegation-seed-1171-620426; validation logs are in .mj/agents/3e3aeea4dec11853aa4a3d2632a6237f/.

Revision note (2026-11-11): Final validation passed: chat 405, core 646, worker 821, CLI 298; controller 2,530 plus the diagnostics test on a targeted rerun. Hook serial rerun passed 6/6; API 227/227 and relay 407/407 passed. Workspace clippy passed. Initial diagnostics flake and parallel hook failure/stall remain recorded as validation limits, with successful focused reruns. Host installation is unchanged.
