# Reliable steering and truthful subagent lifecycle

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

Escape with a queued prompt must request steering, never silently cancel. Pending delivery must remain visible, repeat presses must be harmless, and failed steering must offer an explicit cancellation choice. A completed subagent turn must not be confused with an unavailable subagent. After moving a parent to another profile, the UI must distinguish current work, reusable agents, and historical agents whose availability is unknown or unavailable.

## Progress

- [x] (2026-09-20) Investigated composer, relay, ACP cancellation, native replay, and subagent counting; agreed on behavior with the user.
- [x] (2026-09-20) Implement explicit steering, durable pending/uncertain outcomes, and queue safeguards.
- [x] (2026-09-20) Implement TUI/web feedback and explicit cancellation/retry choices.
- [x] (2026-09-20) Implement native activity/availability reconciliation and truthful subagent presentation.
- [x] (2026-09-20) Validate isolated behavior tests, full dev-profile tests, clippy, and web checks; prepare the reviewed implementation for the current-branch commit.

## Surprises & Discoveries

The old ACP `settle_steer` cancels on every response other than `injected`. A second cancel request while steering is pending also falls into cancellation. The composer removes operation feedback after submission, not application. Native-agent counts include all retained agents. Native replay reconstructs historical turns and reports only activity plus cancel/close capabilities, not resumability. The observed 23 agents were 17 completed and six disconnected, with none reported running; that does not prove their saved context is unavailable.

## Decision Log

2026-09-20: The user pulled upstream and explicitly requested conflict resolution and push. Preserve upstream suspension/destruction and pinned-session behavior alongside typed steering. Upstream already owns migration 42 and daemon protocol 30; retain that migration unchanged, append the steering migration as 43, and advance the combined daemon protocol to 31. Validate using `MJ_INSTANCE=merge-validation` (the environment form of `--instance`) and isolated config/data roots; no live instance operations.

2026-09-20: The user selected an explicit offer to cancel after steering failure. Escape dismisses that offer and never confirms it. Repeated Escape cannot escalate a pending operation.

2026-09-20: Completion describes a turn, not an agent lifetime. Both Codex and Mjolnir-managed children can receive further work using prior context. Retain histories; never infer availability from replay or opaque identifier suffixes.

2026-09-20: Use a 30-second steering deadline to report unconfirmed delivery, continuing to observe late responses. Preserve the 60-second explicit-cancel recovery bound. Never automatically resend uncertain input. Pending/uncertain input must survive reconnect and remain held if delivery cannot be established.

2026-09-20: Checkpoint admission and dispatch refuse held steering, including checkpoints queued before delivery became uncertain. Canonical restore seeds cannot preserve this hold, so moving must wait for an explicit resolution. When delivery is uncertain, cancelling stops the original turn but retains the hold; the user then chooses whether to risk retrying or remove the prompt.

2026-09-20: Daemon protocol advances to 29, relay protocol to 17, relay snapshots to 9, and the database migration/read-write floor to 42, because old clients must not decode new turn-control commands.

2026-09-20: Native replay stages identities as well as transcripts. Replayed running states become disconnected; only fresh state notifications or negotiated current activity can establish working status. Committing a replay replaces replayed children and retains absent historical children with unknown availability. Browser history loads from the durable native projection in supervised background work, including stopped/moved owners.

2026-09-20: The user also reported a transient missing `daemon.json` refresh warning. Read-only source inspection shows metadata is atomically published and removed during daemon shutdown. Harness restart does not remove it. This establishes a daemon discovery interruption, not its cause; do not claim the steering fix resolves it or restart the live daemon to investigate.

## Outcomes & Retrospective

Implementation and validation are complete. The full default-member dev-profile Cargo suite and final affected-package recheck passed. This plan accompanies the implementation commit on the current branch. No live sessions, stores, configuration, or installed adapters have been changed. Older adapters lack the negotiated inventory extension, so retained native agents correctly show availability unknown. The extension requests `_session/subagents/availability` only after initialization advertises `_meta.nativeSubagentAvailability.supported`; the response contains `agents` (opaque `session_id`, optional `stable_id`, availability, optional current activity `state`, and reason) and an explicit `complete` flag. Invalid or failed inventories do not establish availability.

## Context and Orientation

`mj-core/src/relay/snapshot.rs` defines commands, journal observations, and operational state. `snapshot/apply.rs` deterministically replays the journal. `mj-worker/src/relay/commands.rs` admits and dispatches commands; `mj-worker/src/acp/session.rs` drives the agent connection and `drive.rs` settles steering. These worker files own durable execution, independently of UI lifetime.

`mj-chat/src/chat/active/dispatch.rs` submits composer actions through `chat/remote.rs`. The dashboard and web viewer consume controller snapshots. `mj-core/src/native_agent.rs` is the shared native-child representation; `mj-controller/src/database/native_agents.rs` materializes replay atomically. `mj-tui/src/native_agents.rs` currently counts all children. Managed children are separately provisioned session records, unlike native children sharing their owner's harness.

## Plan of Work

### Milestone 1: Explicit and durable turn control

Introduce a version-gated steering command identifying the active prompt and queued prompt. Reject stale identities at admission and dispatch. Preserve legacy decoding but never translate a new steering request into legacy cancellation. Journal steering status so pending, failed, and unconfirmed operations survive reconnection. Keep queued input held while delivery is uncertain; late confirmed injection consumes it once. Provide explicit release/removal decisions for uncertain delivery, and do not auto-retry after a bridge restart. Explicit cancellation remains bounded and targets the intended turn.

### Milestone 2: User-visible operation outcomes

Route Escape with queued input through steering, and without it through cancellation. Deduplicate pending requests in the worker and disable repeat presses in the composer. Keep feedback until the outcome is established. On failure or unconfirmed delivery offer Cancel turn and apply queued prompt / Keep queued; on unresolved delivery after disconnect offer Retry queued prompt / Remove queued prompt with duplicate-delivery context. Apply equivalent semantics to the web control surface. All waiting is background work; quit remains responsive.

### Milestone 3: Activity and availability

Add availability (available, unknown, unavailable with reason) independently of native activity. Negotiate read-only availability evidence and stable identity where providers support it; old adapters remain unknown. Invalidate evidence across harness replacement and publish reconciliation atomically. Late old-harness events cannot restore stale availability. Replayed transcript presence is not evidence. Managed children use their own lifecycle/resume information and are not invalidated solely by their parent's move.

Present a working count separately from retained history; keep the subagent control accessible at zero working. Group working, idle/reusable, and historical/unknown records. Preserve transcripts and opaque IDs. Do not add native resume controls or probe by actually resuming a child.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Use focused colocated Rust tests and existing web fixtures while implementing each milestone. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` before completing implementation. Every `cargo test` runs with elevated sandbox permissions. Use the repository's existing web test commands for modified viewer behavior. Stage only changed task files and commit on the current branch; do not push.

## Validation and Acceptance

Controlled ACP fakes must cover successful/rejected/unsupported/delayed steering, repeated Escape, queue mutation, stale turn identities, natural completion, late acknowledgments, reconnect, and explicit cancellation recovery. Uncertain delivery must not silently lose or automatically duplicate input. Composer and web tests must exercise the failure choice and persistent feedback.

Subagent tests cover completed-to-running follow-up, parent moves with independently running managed children, supported/unsupported native availability, failed replay, late old-harness updates, and history preservation. A fixture containing 23 retained agents with none running must display zero working and availability supported by evidence.

## Idempotence and Recovery

No live store upgrades, profile moves, harness restarts, pushes, or adapter publication are authorized by this implementation. Use isolated stores for all migration/restore tests. Preserve existing migrations; classify new migrations and advance read/write compatibility when older readers or writers cannot preserve the new durable semantics. Failed validation must be corrected without deleting working files of running processes. Existing unrelated untracked plans, `1q`, and `mj.sqlite3` remain untouched.

## Artifacts and Notes

Merge validation (2026-09-20): all eight conflicts resolved while retaining both feature sets. The full `cargo test` suite, final interruption-feedback test, `cargo clippy --all-targets -- -D warnings`, formatting, 43 web unit tests, and 88 deterministic browser tests passed (three existing browser skips). Rust validation used `MJ_INSTANCE=merge-validation` with dedicated `/mnt/optane/hel-merge-validation` config/data roots. The user explicitly authorized completing the pulled merge and pushing it.

The first web unit run outside the restricted sandbox passed 43 tests. Browser validation found and corrected a stale count assertion and a native-only navigation guard; the new Escape/reconnect/cancel-choice test passes. Rust validation found and corrected migration ordering, an outdated compatibility-floor assertion, and finishing retained streaming transcript entries on disconnect. `cargo check --workspace --all-targets` includes the optional desktop crate and cannot run on this host without Pango/JavaScriptCore development libraries; required plain Cargo checks use the repository default members, which exclude desktop. The full `cargo test` run passed (zero failures); `cargo clippy --all-targets -- -D warnings` passed. Final viewer checks passed 43 Node unit tests and 85 deterministic browser tests, with three existing browser skips. The last review corrected cancellation capability validation and added an assertion that submission acceptance does not clear turn-control feedback; the affected chat, controller, and worker packages all passed the final recheck. The final all-target clippy and formatting checks also passed. The user's original incident established a cancellation timeout and harness restart, but did not establish which fallback path initiated cancellation.

## Interfaces and Dependencies

Use shared relay types and existing command journals for operation state, shared subprocess helpers, supervised background tasks, and existing native replay transactions. New steering commands require a new relay protocol version. Extend native availability through a negotiated optional interface with unknown defaults for existing adapters. Persisted enum/command additions require explicit format compatibility protection. Do not create a workspace crate.

Plan recorded and updated 2026-09-20 from the approved conversation. Implementation is complete; validation outcomes and the commit checkpoint are maintained above.
