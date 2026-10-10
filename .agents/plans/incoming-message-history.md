# Record delivered messages as dedicated conversation entries

This ExecPlan is a living document maintained under `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections must reflect the implementation and validation.

## Purpose / Big Picture

Delivered messages from other sessions, parent agents, and users should appear in conversation history with their sender, delivery time, and complete text. The TUI and browser should show each message beside the local user's prompts and the local agent's responses. A message must remain visible after reconnect, daemon replacement, and checkpoint restoration. It must not be represented as a new user instruction merely because it arrived through a mailbox.

## Progress

- [x] (2026-10-09) Inspect delivery ownership, transcript projection, browser identities, schema compatibility, and checkpoint formats; advance the current master to upstream before editing.
- [x] Add a typed message transcript body, its projections, and the required store/checkpoint compatibility gates.
- [x] Project every mailbox delivery route, preserve full content and stable identities, and stop creating blank user rows for mailbox wake turns.
- [x] Render sender-labelled entries in live and earlier TUI/browser history; prevent batched deliveries from sharing a browser row.
- [x] Extend existing goldens and isolated integration regressions; focused delivery, browser, retry, TUI, and archive checks pass.
- [x] Finish full touched-crate suites and Clippy after the final retention fix and test corrections.
- [x] Review the final diff and prepare the validated master commit for the authorized push.

## Surprises & Discoveries

Mailbox delivery already has a durable owner: the worker relay. `MailboxEventsDelivered` records hook acknowledgments and attachment to an existing prompt. The wake route claims its messages in `MailboxWake`'s `CommandQueued` observation, where the relay removes them from the pending mailbox and records their keys as delivered. The transcript projector currently ignores acknowledgments and accumulates truncated summaries in a single old system row.

A delivery can contain multiple messages at the same relay ordinal. The store's filtered transcript pagination already retains an entire equal-ordinal group. The browser currently keys rows and its publication cache by ordinal alone, which would overwrite such messages. Durable transcript stable IDs must identify these rows while ordinals remain the paging cursors.

Wake prompts deliberately project an empty content list. Their later `CommandStarted` currently produces a blank User row; the absence of user content should instead produce the existing autonomous-turn marker. This preserves turn tracking without attributing an incoming message to the local user.

Final validation found that the existing retained-summary selector preserved local user prompts, the final agent answer, and recent tool calls but omitted received messages. The new Message role now participates in retained context and gets the same budget priority as incoming work. The regression verifies message-only handoffs, fragmented large handoffs, escaped peer text, and preserved lack of user authority. Two old migration fixtures also needed their expected compatibility floor advanced from 74 to 78.

## Decision Log

- Decision: Add `TranscriptBody::Message` and its canonical checkpoint counterpart carrying the original structured `MailboxEvent`, plus `ChatRole::Message` and `TranscriptRole::Message`.
  Rationale: The existing event already owns sender identity, full text, and the deduplication key; retaining it avoids inventing another sender representation or parsing display text.
  Date/Author: 2026-10-09 / Codex.
- Decision: Derive entries from the existing relay delivery decisions, using one stable ID per event key and delivery observation timestamps. Keep informational GitHub notices in their existing system presentation.
  Rationale: UI state must reflect the worker's durable decision, and accepted-but-pending mailbox events must not masquerade as delivery. This requires no worker wire or journal change.
  Date/Author: 2026-10-09 / Codex.
- Decision: Introduce breaking store revision 78 and message archive schema 9, while retaining the complete forward migration ladder and reading every previously supported archive version.
  Rationale: Older readers do not understand the new transcript enum value. Explicit gates are required even though SQL tables themselves need no new columns.
  Date/Author: 2026-10-09 / Codex.
- Decision: Leave pre-upgrade summarized notices intact; project new delivery observations into full entries.
  Rationale: Some acknowledged relay history has already been collected or checkpointed without full received-message bodies. The migration must preserve available history and cannot invent lost text.
  Date/Author: 2026-10-09 / Codex.

- Decision: Preserve incoming-message context in handoffs and search, and use the existing mailbox renderer for model-facing provenance.
  Rationale: Adding the durable body without updating these readers would drop received work when changing harnesses. Peer messages must retain the explicit statement that they carry no user authority. Compaction supports message-only conversations by omitting the user block when no local prompt exists. SessionWiki has only user/assistant/tool roles; received context goes in its tool channel with full sender provenance. Advance summary version to invalidate previously derived indexes.
  Date/Author: 2026-10-09 / Codex.

## Outcomes & Retrospective

Dedicated received messages preserve sender data and full text through storage, canonical checkpoints, browser/TUI presentation, search, and model-facing handoffs. Implementation and validation are complete. The complete suites for all seven touched crates ran; the controller's three failures were corrected and passed on focused reruns (1,974 passed in total, ten existing ignored cases). The final transcript suite passed all 81 tests. Browser source tests pass (53), and the deterministic browser suite passes (99; three lab-only cases skipped). Final Clippy, formatting, and diff checks pass. Tests used isolated stores and instances; no test build accessed or migrated the live default store.

The optional desktop build is excluded from the normal default targets and requires GTK/WebKit system libraries unavailable in this container. Existing summarized history remains intact; only newly projected deliveries can retain full message bytes that older summaries discarded. The main lesson is that delivery keys, rather than relay ordinals, must identify message rows and viewport anchors, and every context-retention reader must understand the new role.

## Context and Orientation

`mj-core/src/mailbox.rs` defines structured messages and their sender information. `mj-core/src/transcript.rs` defines the stored logical transcript body and client ChatEntry presentation values. `mj-transcript/src/projection/observation.rs` transforms durable relay observations into stored transcript mutations, using `ProjectionIndex` in `mj-transcript/src/projection.rs` for immutable identities and deduplication. `mj-client/src/transcript.rs` converts those records for the TUI and web viewer. `mj-chat/src/chat/transcript/render.rs` renders TUI entries. `mj-controller/src/web/viewer.js` and `viewer.css` render browser entries and earlier history. Canonical checkpoint conversion is in `mj-transcript/src/projection/materialize.rs`; archive version selection is in `mj-checkpoint/src/archive.rs`. The controller's migration ladder is `mj-controller/src/database/schema.rs` and its current revision is declared in `mj-controller/src/database.rs`.

## Plan of Work

First add the typed message variant, shared sender/text interpretation, role mappings, canonical conversion, and compatibility gates. The migration raises the minimum read/write revision in the same transaction as revision 78. Archives containing typed messages declare schema 9; older payloads continue using their existing versions.

Next have the projector append one message per event key on hook/prompt delivery and wake claims. Close any open anonymous streams at the delivery boundary, skip already-existing message identities, and include these identities in historical-reference loading so replay cannot duplicate or move old entries. Do not add message summaries to the aggregated external-events notice. A mailbox wake with no local user content starts the existing autonomous-turn marker rather than an empty User item.

Finally extend client and UI presentation. Use a dedicated message role, a sender label, the recorded delivery timestamp, and normal wrapped prose. The browser's publication cache and DOM use optional durable stable IDs for message rows while retaining numeric ordinals for cursors and compatibility. Earlier-history text must retain the sender and full content. Where equal-ordinal messages affect saved TUI anchors, preserve identity through a backward-compatible optional field rather than relying on the first matching ordinal.

## Concrete Steps

Run all commands from the repository root. Use the configured mbx Cargo wrapper and normal build storage. Do not change target directories. Every Cargo test runs outside the restricted sandbox. Existing automated tests keep their isolated configuration/data directories; any manual daemon or CLI check uses `--instance incoming-message-history`.

    cargo test -p brokk-mj-transcript golden_mailbox_delivery_transcript
    cargo test -p brokk-mj-chat golden_rich_transcript_tool_presentation
    cargo test -p brokk-mj-controller history_window_deduplicates_deliveries_after_the_message_leaves_the_live_tail
    cargo test -p brokk-mj-checkpoint archive_round_trip_preserves_clear_boundary_and_requires_context_schema
    node --test tests/e2e/web/viewer.unit.test.mjs

Use `MJ_UPDATE_GOLDEN=1` only when deliberately updating affected expected files, then review the complete diff and run the corresponding tests without it. Keep platform-specific TUI golden expectations aligned. After the last implementation changes run complete suites for every touched crate and `cargo clippy --all-targets -- -D warnings`, plus `cargo fmt --all -- --check`. Check browser tests using the existing deterministic fixtures and isolated configuration; no live default instance is needed.

## Validation and Acceptance

The projection golden must show complete messages at delivery, including two messages in one acknowledgment, sender identity, deduplication, and all three delivery routes. A queued-but-undelivered message must have no received-message row. New entries must not change local user-turn attribution or appear as blank You rows. The TUI golden must show a message before the local agent's subsequent response, preserve Unicode and multiline wrapping, and include restored-history and ASCII views. Browser regression coverage must prove two messages with one ordinal survive separate DOM keys and repeated publication.

An isolated database integration test must upgrade a revision-77 fixture, preserve its existing transcript, round-trip a typed message through storage and history paging, and refuse revision-77 readers/writers once the floor advances. Checkpoint regression coverage must prove sender identity and full message bytes survive archive verification and canonical restoration, while old archives remain readable. Full touched-crate tests, Clippy, formatting, and diff checks must pass before committing.

## Idempotence and Recovery

Projection identities derive from mailbox event keys, so repeated delivery observations produce one immutable history row. Migration revision and compatibility floor advance atomically and the ladder retains every shipped revision. New-format tests use their own temporary stores and never start or migrate the default instance. If upstream advances before push, fetch and merge upstream into the current branch under AGENTS.md, resolve conflicts, check affected behavior, and push without rebasing.

## Artifacts and Notes

The user-facing example is:

    ← Message from bifrost3 · 15:28
    │ Full incoming message text
    ● Agent · 15:29
    │ The local agent's response

Build output and build logs remain in normal mbx storage; do not redirect Cargo output into /tmp. Agent-owned design artifacts stay under `.agents/`.

## Interfaces and Dependencies

Reuse `MailboxEvent`, `MailboxEventBody`, `Sender`, `TranscriptSource`, the shared sanitizer, and existing canonical conversion helpers. No new crate, dependency, worker journal event, or relay protocol version is required. The dedicated entry must keep sender identity as structured data, and the existing worker continues to decide delivery.

Initial revision: records the researched implementation boundaries and compatibility requirements before coding.

Revision note (2026-10-09): implemented message projection and all presentation paths; added handoff/search preservation after inspecting downstream summary readers. Focused transcript golden and browser batch/reload regression pass; final Rust validation is running.

Completion note (2026-10-09): implementation and validation are complete. Commit only the reviewed task files on the current master branch and finish with the explicitly authorized `git push` to origin/master.
