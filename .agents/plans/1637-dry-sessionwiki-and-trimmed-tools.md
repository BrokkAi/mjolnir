# DRY SessionWiki and Mjolnir Transcript Readers with Turn-End Tool Trimming and On-Demand Expansion

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Mjolnir currently duplicates native harness transcript parsing between `mj-import` and `brokk-sessionwiki`, duplicates bulky tool-call payloads inside `materialized_transcript_items` in its main SQLite store and inside every suspend snapshot archive (`canonical/session.json`), and loses SessionWiki tool line counts, failure excerpts, and edit provenance whenever tool calls are compacted. Meanwhile, Mjolnir already links `brokk-sessionwiki` 0.34.27, which contains native adapters for all eight harnesses (including Antigravity, which Mjolnir currently skips), and measured data shows nearly 90 percent of live transcript bytes in SQLite come from tool calls, oversized cached presentations, and embedded terminal outputs rather than conversation text.

After this work completes, Mjolnir and SessionWiki share one native parser layer per harness (`brokk-sessionwiki` adapters, including Antigravity and paginated Codex `event_msg.item_completed`), eliminating Mjolnir's duplicate transcript and edit-target parsers in `mj-import`. Mjolnir's `materialized_transcript_items` table keeps all conversation text plus one compact, precomputed tool summary that satisfies UI list views, SessionWiki summaries and provenance, and GitHub watch in a single pass. Whenever an agent turn settles, Mjolnir spills heavy settled tool-call and standalone terminal payloads out of SQLite into a per-session Zstd-compressed cold detail file (`<data_dir>/tool-details/<session_id>.mjtd`) addressed directly by byte offset and length from the SQLite row. Clicking a trimmed tool call in the terminal UI or web viewer fetches and renders its full inputs, outputs, diffs, and terminal logs asynchronously in under a millisecond—working identically across all eight harnesses, offline targets, suspended sessions, client-hosted ACP terminals, and Grok sessions whose native history was overwritten on compaction—while keeping the main SQLite database and suspend snapshots ~85–90 percent smaller and reclaiming cold tool details instantaneously via `unlink` on `mj destroy`.

This plan resolves issue #1637 with a deliberately simpler architecture than native-file pointers and supersedes issue #1636 by absorbing its SessionWiki metadata requirement and eliminating live tool-output accumulation at turn end.

## Progress

- [x] (2026-11-22 15:46Z) Audited native storage formats across all eight harnesses, mapped all `materialized_transcript_items` writers and consumers, and mapped worker RPC and `.hel.zip` archive layout.
- [x] (2026-11-22 16:07Z) Cloned and code-dived `pingdotgg/t3code` (`2c4310eb`) to test whether native-backed transcript timelines exist in practice and evaluate transferable paging, scanning, and payload-trimming ideas.
- [x] (2026-11-22 19:32Z) Cloned and audited `jbellis/sessionwiki` branch `brokk-release-0.34.27` (`e23c1fe`, byte-identical to pinned `brokk-sessionwiki 0.34.27`), verifying Antigravity support, Grok in-place compaction loss, shared streaming scanners, and exact `{mj, sw}` overlap.
- [x] (2026-11-22 19:46Z) Audited every `mj-import` caller and requirement, proving turn-boundary scaffolding and most strict transcript refusals are unnecessary once `sessionwiki` adds Codex `item_completed` plus raw `cwd` and literal `native_id` accessors.
- [x] (2026-11-22 20:10Z) Audited click-to-expand across TUI/Web/API and ACP `toolCallId` correspondence across all eight harnesses, establishing that native-only tool-call expansion fails on Codex, Kimi, Grok, and Antigravity and selecting daemon-local `.mjtd` cold detail files instead.
- [x] (2026-11-22 20:15Z) Authored initial ExecPlan in `.agents/plans/1637-dry-sessionwiki-and-trimmed-tools.md`.
- [ ] Milestone 1: Enhance `brokk-sessionwiki` (`jbellis/sessionwiki`) with Codex `event_msg.item_completed`, raw `cwd` / literal `native_id` accessors, and Grok thought marker; publish next crate version, update Mjolnir `Cargo.toml`/`Cargo.lock` and `docs/src/content/docs/sessions.md`, and register `Antigravity` in `mj-sessionwiki/src/sessionwiki.rs`.
- [ ] Milestone 2: Replace `mj-import`'s duplicate transcript and edit-target parsers with `brokk-sessionwiki` adapters and direct idle snapshot construction while retaining cheap picker metadata, child ownership filtering, Muse artifact validation, and Git/bundle import checks.
- [x] (2026-11-23 04:27Z) Milestone 3: Extended `ToolCallPresentation` with precomputed SessionWiki summary and provenance fields (`output_line_count`, `output_is_error`, `first_error_line`, `edit_snippets`, `detail_trimmed`, clearing `source` on settlement while keeping `"source":""` for legacy archive compatibility), added standalone `TerminalOutput` compaction, and refactored `MjolnirAdapter` to a single-pass projection over compact items.
- [ ] Milestone 4: Implement per-session `.mjtd` cold Zstd detail files, schema revision 152 (breaking), turn-end spill-and-trim for settled tools and standalone `TerminalOutput` rows, live actor window trimming, snapshot shrinking, destroy cleanup, and restartable backfill for existing stores.
- [ ] Milestone 5: Wire on-demand tool-call detail loading through `mj-store`, `mj-client`, `mj-session-runtime`, `mj-server` HTTP API, `mj-chat` TUI click-to-expand and Alt-T Raw mode, and `mj-server/src/web/viewer.js` click-to-expand.

## Surprises & Discoveries

- Observation: `brokk-sessionwiki 0.34.27` already includes an Antigravity (`agy` and Antigravity IDE) adapter reading `brain/<id>/.system_generated/logs/transcript_full.jsonl` (or `transcript.jsonl`) plus `conversation_summaries.db`, but Mjolnir never registers it.
  Evidence: `jbellis/sessionwiki` `brokk-release-0.34.27` `src/adapters/antigravity.rs:26-121`; Mjolnir `mj-sessionwiki/src/sessionwiki.rs:1235-1294` leaves the `Antigravity` arm empty with a stale comment claiming SessionWiki has no reader.

- Observation: Neither Grok Build nor `brokk-sessionwiki` preserves Grok's pre-compaction transcript on disk.
  Evidence: `mj-import/src/import/grok.rs:266-281,321-327` documents that Grok rewrites `chat_history.jsonl` in place with a `compaction_meta` summary. In `jbellis/sessionwiki` `src/adapters/grok.rs:20-91` and `src/index.rs:1232-1333`, `GrokBuild` only fingerprints `events.jsonl` for freshness and never parses it; when `chat_history.jsonl` shrinks/rewrites, `index_one` deletes the old suffix from `messages` and FTS5. Mjolnir-managed Grok sessions currently preserve pre-compaction history only because Mjolnir projects the live ACP relay into `materialized_transcript_items` and feeds that copy to SessionWiki via `MjolnirAdapter`.

- Observation: Fetching a trimmed tool call's full body from native harness history does not work reliably across Mjolnir's eight harnesses.
  Evidence: Auditing all eight harnesses against installed bridges and `brokk-sessionwiki` adapters showed four concrete failures. Installed `@brokkai/codex-acp` emits app-server `item.id` (`exec-<UUID>`), while Codex rollouts store `payload.call_id`, `mj-checkpoint` omits `thread_history_*.sqlite`, and `sessionwiki` 0.34.27 skips `event_msg.item_completed`. Kimi prefixes ACP IDs (`0:tool_...`), reuses native suffixes across agent wires, and executes Bash via Mjolnir's client-hosted `terminal/create` (`hel-terminal:` synthetic cards). Grok also uses client-hosted `terminal/create` and deletes prior tool calls on compaction. Antigravity brain JSONL has no native call IDs at all, forcing `sessionwiki` to synthesize `<step_index>:<array_index>` and FIFO-pair results, and spills long outputs to separate brain files. Furthermore, `sessionwiki` deliberately caps tool outputs at 8 KiB and redacts them.

- Observation: Measured field splits show nearly 90 percent of live transcript SQLite bytes are tool-call payloads, cached presentations, and embedded terminal outputs, while conversation text is small.
  Evidence: Commit `84aedcb88` recorded one session with 28,066 items and 635 MiB of `body_json`, of which 561 MB was tool-call content. A 60-session live census of the newest 2,000 items (`.agents/plans/opaque-transcript-bodies.md:18-22`) measured 49.0 MiB in `Tool.call` JSON, 19.9 MiB in `ToolCallPresentation` (because `mj-transcript/src/transcript.rs:11-16` retains up to 64 KiB of raw source text per call), and 13.9 MiB in embedded `Tool.terminal_outputs`, versus only 6.8 MiB agent chunks, 2.8 MiB thought chunks, and 0.3 MiB user content (9.9 MiB total conversation text).

- Observation: `brokk-sessionwiki` 0.34.27 silently misses dialogue and file edits in paginated Codex rollouts that use `event_msg.item_completed`.
  Evidence: `mj-import/src/import/transcripts.rs:148-263` reads `event_msg.item_completed` for `UserMessage`, `AgentMessage`, and completed `FileChange`, whereas `jbellis/sessionwiki` `src/adapters/codex.rs:150-214,303-400` handles only `response_item.message` and older `event_msg.{user,agent}_message` records.

- Observation: SQLite `EXPLAIN QUERY PLAN` against `mj-store/src/database/baseline.sql:76-87,320-321` shows backward history paging is already a single indexed seek with no sort, whereas the forward filtered API does two session-range scans, two temp B-trees, and JSON predicates over `body_json`.
  Evidence: Tested on SQLite 3.53.1 against the checked-in DDL. Backward `load_transcript_history` used `SEARCH ... USING INDEX materialized_transcript_position` with no temp sort. Forward `load_materialized_transcript_filtered` (`mj-store/src/database/materialized.rs:847-989`) required two index searches, two temp B-trees, and `json_extract` over `body_json`.

- Observation: Existing post-checkpoint compaction in `mj-store/src/database/materialized.rs:1089-1170` never compacts standalone `TranscriptBody::TerminalOutput` rows and performs a `SELECT` outside the `UPDATE` transaction without checking whether the row changed in between.
  Evidence: Lines 1098-1107 filter strictly on `json_extract(..., '$.kind') = 'tool'`, omitting `terminal_output`. Lines 1097-1135 query `body_json` before opening `TransactionBehavior::Immediate` at line 1142, and line 1162 updates by `(session_id, stable_id)` without guarding `last_changed_at_ms` or `latest_content_event_ordinal`.

- Observation: `pingdotgg/t3code` (`2c4310eb`) does not actually back its UI timeline with native harness files; it stores full message and tool projections inline in SQLite and trims/externalizes heavy blobs (32 KiB diff caps, Git-derived checkpoint diffs, disk attachments, live in-memory command deltas).
  Evidence: Audited `/tmp/t3code/apps/server/src/persistence/Migrations/055_OrchestrationV2.ts:194-275`, `ProjectionStore.ts:3070-3193`, `WireProjection.ts:27-65`, and `CheckpointDiffQuery.ts:139-175`.

## Decision Log

- Decision: Skip #1636's harness compaction-signal eviction machinery and fold #1636's SessionWiki metadata requirement into this plan.
  Rationale: #1636's Claude `system/compact_boundary` / `preservedSegment` mapping and Codex/Muse compaction-signal audits exist solely to delete tool rows mid-session while full tool bodies live in SQLite. Trimming settled tools on turn end into `.mjtd` eliminates live SQLite growth across all eight harnesses without fragile compaction-boundary proofs or permanent loss of click-to-expand detail.
  Date/Author: 2026-11-22, coordinator with user agreement.

- Decision: Keep conversation text and compact tool rows in Mjolnir's `materialized_transcript_items` instead of replacing all transcript bodies with pointers into native harness files.
  Rationale: Primary goal is DRYing `{mj, sw}`; native pointers are only worthwhile if simpler than syncing our own copies. Conversation text is only ~10 percent of stored bytes (~9.9 MiB across 60 live sessions), whereas native harness formats are heterogeneous (SQLite, atomic JSON rewrites, destructive compaction in Grok, unindexed brain files in Antigravity) and would require worker RPCs, archive extraction, and degraded offline timelines for all messages.
  Date/Author: 2026-11-22, coordinator with user agreement.

- Decision: Store trimmed tool-call and standalone terminal details in a daemon-local, per-session, append-only framed Zstd file (`<data_dir>/tool-details/<session_id>.mjtd`) addressed by `(detail_offset, detail_len)` on the `materialized_transcript_items` row, rather than fetching trimmed tool details from native harness history or copying cold tool details into `.hel.zip` archives.
  Rationale: Our eight-harness audit proved native tool-call lookup fails on Codex, Kimi, Grok, and Antigravity and cannot recover client-hosted ACP terminal stdout or post-compaction Grok calls. A daemon-local `.mjtd` file works identically for all eight harnesses, supports O(1) `pread` click-to-expand in TUI and Web even when targets are offline or sessions are suspended, compresses tool JSON/logs ~8–15x outside SQLite (zero WAL/page bloat, instant `unlink` on destroy), keeps `canonical/session.json` in `.hel.zip` compact (zero network transfer of old tool outputs on Move/Resume because workers never need old expansion details), and preserves exact ACP fidelity. User explicitly confirmed daemon-local storage (no `.hel.zip` copy) on 2026-11-23.
  Date/Author: 2026-11-22 (confirmed 2026-11-23), coordinator with user confirmation.

- Decision: Consume `brokk-sessionwiki`'s existing adapters directly from `mj-import` (after small upstream additions) instead of building a third lower-level decoder crate, and build imported snapshots directly as idle projections rather than synthesizing `PromptAccepted`/`TurnCompleted` relay events.
  Rationale: `mj-import`'s audit showed synthetic turn-boundary events are pure intermediate scaffolding already bypassed by `mj-sessionwiki/src/sessionwiki.rs:2747-2849`. Only literal native IDs, verbatim `cwd`, operational edit paths, and Codex `item_completed` parity are truly needed; adding those to `brokk-sessionwiki` deletes ~900–1,000 lines of duplicate transcript decoders immediately and fixes Codex coverage in SessionWiki itself.
  Date/Author: 2026-11-22, coordinator with user agreement.

- Decision: Keep `mj-store` and SessionWiki's `index.db` as separate SQLite databases, but make `MjolnirAdapter` a single-pass reader over compact items rather than loading raw tool JSON twice.
  Rationale: SessionWiki's `index.db` is shared with the standalone `sessionwiki` CLI across non-Mjolnir tools and retains post-`mj destroy` archives. Once `mj-store` holds precomputed tool summaries, line counts, failure excerpts, and bounded edit snippets, syncing `mj` into `sw` avoids the 150 MB heap spikes recorded in `.agents/docs/daemon-profiling.md:27-33` and removes the redundant JSON/diff walks.
  Date/Author: 2026-11-22, coordinator.

- Decision: Classify the `mj-store` migration from schema revision 151 to 152 as breaking (raising both `SCHEMA_VERSION` and `MINIMUM_COMPATIBLE_VERSION` from 151 to 152 in one transaction).
  Rationale: Adding `detail_offset`, `detail_len`, `item_kind`, `settled`, and `update_ordinal` to `materialized_transcript_items` means an older revision-151 writer overwriting `body_json` would leave stale `.mjtd` offsets and stale metadata columns, and an older revision-151 reader cannot resolve trimmed tool details. Per `AGENTS.md`, any change where older writes would corrupt invariants or drop new semantics must be classified as breaking.
  Date/Author: 2026-11-22, coordinator.

- Decision: Adopt only the evidence-backed parts of `t3code`'s read path—indexed metadata columns (`item_kind`, `settled`, `update_ordinal`) for the forward filtered API and startup unsettled scan—and skip two-phase key-then-body hydration for backward history paging.
  Rationale: `EXPLAIN QUERY PLAN` proved backward paging is already a single index seek with no sort, so two-phase hydration saves only one lookahead row while adding round-trip complexity. Indexed metadata columns directly eliminate the two temp B-trees and `body_json` JSON scans in `load_materialized_transcript_filtered` and actor startup.
  Date/Author: 2026-11-22, coordinator.

## Outcomes & Retrospective

Plan drafted on 2026-11-22 after completing codebase, SessionWiki, `t3code`, UI expansion, and eight-harness fidelity audits. Implementation started on 2026-11-22; this section will be updated at the completion of each milestone.

## Context and Orientation

Mjolnir is split into a control plane and a data plane. The daemon (`mj daemon-run`, implemented in `mj-daemon`, `mj-controller`, `mj-session-runtime`, and `mj-store`) is the control plane: it owns the SQLite store (`<data_dir>/mjolnir.db`), session records, lifecycle operations, the HTTP server (`mj-server`), SessionWiki indexing (`mj-sessionwiki`), and client attachments. Each live session also has a worker (`mj-worker`), which is the data plane running locally, over SSH, in a container, or on EC2; the worker drives the coding agent harness over the Agent Client Protocol (ACP, a JSON-RPC protocol for prompts, streamed text chunks, tool calls, and client-hosted terminals) and writes sequenced events into a durable relay journal.

As worker relay events arrive at the daemon, `mj-session-runtime/src/session_manager/standalone.rs` applies them in pages through `mj-transcript` into the daemon's materialized projection: the SQLite table `materialized_transcript_items` defined in `mj-store/src/database/baseline.sql` (lines 76–87, currently at `SCHEMA_VERSION = 151` in `mj-store/src/database.rs:34` and `MINIMUM_COMPATIBLE_VERSION = 151` in `mj-store/src/database/schema.rs:14`). Each row stores one logical item keyed by `(session_id, stable_id)`, ordered by `(position, stable_id)`, with `latest_content_event_ordinal`, `created_at_ms`, `last_changed_at_ms`, and `body_json` encoding `mj_core::transcript::TranscriptBody` (`mj-core/src/transcript.rs:328-397`). A `TranscriptBody::Tool` item currently contains the full ACP `OpaqueToolCall`, attached `terminal_outputs`, `terminal_refs`, and an optional `ToolCallPresentation` (`mj-core/src/transcript.rs:170-180`), whose `source` field retains up to 64 KiB of raw tool input (`mj-transcript/src/transcript.rs:11-16`). Standalone terminal records that were not attached to a tool call are stored as `TranscriptBody::TerminalOutput`.

When a session suspends, `mj-controller/src/controller/checkpoint/` packages a content-addressed `.hel.zip` snapshot archive containing `manifest.json`, `canonical/session.json` (serialized from `materialized_transcript_items`), the harness's native session files under `native/...` (collected by `mj-checkpoint/src/checkpoint/native_scan.rs`), and origin-delta Git payloads. Only after a suspend checkpoint is verified does `release_projection_behind_checkpoint` (`mj-controller/src/controller/checkpoint/staging.rs:641`) call `compact_materialized_transcript_all_through` (`mj-store/src/database/materialized.rs:1029`), which invokes `mj_transcript::transcript::compact_tool_call_for_retention` (`mj-transcript/src/transcript.rs:28`) to strip raw fields from `>4 KiB` tool rows. Because periodic checkpoints were removed in #1206, live sessions never trim tool rows until suspend, and `canonical/session.json` inside every suspend `.hel.zip` still carries the full untrimmed tool payloads.

SessionWiki (`brokk-sessionwiki 0.34.27` in `Cargo.toml:60`, developed in the fork `https://github.com/jbellis/sessionwiki` as documented in `.agents/docs/sessionwiki-fork.md`) is an external-content FTS5 search index (`index.db`, schema version 9) shared with the standalone `sessionwiki` CLI. `mj-sessionwiki/src/sessionwiki.rs` feeds Mjolnir's own sessions into it through `MjolnirAdapter` (which currently reads full materialized or checkpoint transcripts and walks them twice—once in `mj-sessionwiki/src/sessionwiki/provenance.rs` for edit evidence and once via `mj-transcript/src/summary.rs` for tool output line counts and failure quotes) and also registers `sessionwiki`'s native adapters for external sessions (currently skipping Antigravity). Separately, `mj-import/src/import/` maintains its own native transcript and edit-target readers for Claude, Codex, Kimi, Grok, and Muse (`transcripts.rs`, `grok.rs`, `muse.rs`, `claude.rs`, `codex.rs`, `kimi.rs`, `native.rs`, `native_import.rs`) to power `mj import`, the TUI Resume dialog's Import tab, and `mj resume --wiki`.

In the terminal UI, `mj-chat/src/chat/transcript/render.rs` renders `ChatEntry` items (`mj-core/src/transcript.rs:690-735`) in Rich or Raw (Alt-T) mode and lets a user click any collapsed tool row to toggle inline expansion via `expanded_tool_calls` (`render.rs:299-309`). Background transcript operations in `mj-chat` run via `mj-chat/src/chat/active/dispatch.rs` calling `mj_client::session::SessionHandle`, whose production backend is `ClientSessionHandle` in `mj-session-runtime/src/session_manager/client_backend.rs`. In the web viewer (`mj-server/src/web/viewer.js`), transcript pages are fetched from `mj-server/src/server/api/routes.rs` and painted in `paintEntry` (`viewer.js:9357-9406`), which currently folds thinking and mailbox blocks but has no click-to-expand fetch path for tool calls.

A cold tool-detail file (extension `.mjtd`, short for Mjolnir Tool Detail) is a new daemon-local, per-session append-only binary file under `<data_dir>/tool-details/<session_id>.mjtd`. Each record in the file is an independent, length-prefixed, CRC32-checked, Zstd-compressed frame containing one item's `stable_id` and full pre-trim `TranscriptBody` JSON. Because the SQLite row stores the exact `(detail_offset, detail_len)` of the current frame, reading a single trimmed tool call requires one `pread`, one CRC32 check, and one Zstd decompression—never scanning the file, opening a `.hel.zip` archive, or contacting a remote worker.

## Plan of Work

The work proceeds in five independently verifiable milestones. Each milestone leaves the repository compiling, tested, and usable on its own.

### Milestone 1: Enhance `brokk-sessionwiki` and register Antigravity in Mjolnir

Begin in a checkout of `https://github.com/jbellis/sessionwiki` (branched from the `brokk-release-0.34.27` release lineage; keep `sessionwiki::index::SCHEMA_VERSION` at 9 so existing user indexes do not rebuild unnecessarily). Make three focused, generally useful additions to the library.

First, extend `src/adapters/codex.rs` so the Codex rollout reader parses `event_msg` records whose `payload.type` is `item_completed`, covering `UserMessage`, `AgentMessage`, and completed `FileChange` items (recording their paths in `touched` and preserving deduplication when a rollout also contains legacy `response_item.message` or `event_msg.{user,agent}_message` records, and updating the selective-retention predicate in `session_lines_keeping` so `item_completed` lines are never dropped on files larger than 256 MiB).

Second, expose two lightweight metadata accessors without changing `index.db`'s SQLite schema: add an optional `raw_cwd: Option<String>` field (default `None`) on `sessionwiki::model::Session` populated verbatim by every adapter before display-label fallback, and add a default-returning method `fn native_id(&self, key: &str) -> Option<String> { None }` on `sessionwiki::adapters::Adapter` implemented for Claude (JSONL stem), Codex (authoritative `session_meta.payload.id` or rollout filename suffix preserving exact case), Grok and Kimi (session directory basename), Muse (parent directory name cross-checked with stream ID), DSH (session stem), OpenCode (native row ID), and Antigravity (conversation directory UUID). Also preserve an optional `is_thought: bool` flag (default `false`) on `sessionwiki::model::Message` so Grok's `reasoning.summary` blocks remain distinguishable from assistant replies for callers that render thoughts separately, while keeping SessionWiki's own indexing behavior identical.

Third, add unit and fixture tests in `sessionwiki` proving paginated Codex `item_completed` rollouts emit messages and touched paths (including under the >256 MiB selective-retention filter), proving `raw_cwd` and `native_id` preserve exact spelling and case across all eight harnesses, and proving `SCHEMA_VERSION` remains 9. Publish the updated crate as `brokk-sessionwiki 0.34.28` (coordinating with the user's release credentials as noted in `.agents/docs/sessionwiki-fork.md`), then update Mjolnir's root `Cargo.toml`, `Cargo.lock`, and `docs/src/content/docs/sessions.md` in one commit. In that same milestone, wire `sessionwiki::adapters::Antigravity` in `mj-sessionwiki/src/sessionwiki.rs` (inside `native_adapters` around lines 1235–1294, replacing the empty `HarnessKind::Antigravity` arm and stale comment) and add an integration test in `mj-sessionwiki/src/sessionwiki/native_tests.rs` verifying Antigravity native sessions are indexed and searchable through Mjolnir's SessionWiki sync.

### Milestone 2: Collapse `mj-import`'s duplicate transcript and edit-target parsers onto `brokk-sessionwiki`

Refactor `mj-import/src/import/` so `brokk-sessionwiki`'s adapters become the single native transcript and edit-target decoder shared by both SessionWiki and Mjolnir import, deleting Mjolnir's duplicated parsers.

Introduce a shared adapter-to-snapshot translator inside `mj-import/src/import/` that calls the matching `brokk-sessionwiki` adapter (`ClaudeCode`, `Codex`, `KimiCode`, `GrokBuild`, `Muse`) on the located native session key, validates `session.raw_cwd` as an absolute path, extracts `edited_paths` from the adapter's operational touched paths (preserving Claude child-log edits, Kimi multi-wire edits, Codex completed `FileChange` edits, Grok `search_replace`, and Muse committed writes), and builds an explicitly idle canonical projection directly from `session.messages`—reusing the direct idle-snapshot construction already proven in `mj-sessionwiki/src/sessionwiki.rs:2747-2849` instead of synthesizing dummy `PromptAccepted`/`TurnCompleted` relay events. Map `Message.is_thought` to `TranscriptBody::Thought` so Grok thought rendering stays intact. If a native history starts with a compaction summary (no pre-compaction user prompt), skip synthetic compaction prompts and record a visible `TranscriptBody::System` notice stating that earlier turns were compacted natively, rather than refusing the import; likewise rely on `sessionwiki`'s tolerant JSONL parsing rather than aborting display decoding on a single truncated trailing line.

Delete the duplicated full-file decoders in `mj-import/src/import/transcripts.rs` (Claude, Codex, Kimi transcript and edit-target parsers), the transcript parsing tail of `mj-import/src/import/grok.rs`, and the duplicate transcript/edit projection in `mj-import/src/import/muse.rs`. Keep the small operational layers that belong strictly to Mjolnir import: cheap picker metadata and child-ownership classification in `NativeScanCache` (`native.rs`, `claude.rs`, `codex.rs`, `kimi.rs`, `grok.rs`, `muse.rs`) so opening the Resume dialog never parses full logs; Codex's `history_mode == paginated` policy gate; Muse's native-artifact coherence validation close to `mj-checkpoint/src/native/muse.rs` (deriving display projection from the privately captured copy so preview and archive cannot race); and `native_import.rs`'s path-containment, Git repo discovery, bundle resolution, and checkpoint publication. Update `mj-sessionwiki/src/sessionwiki.rs:2879-2912` (`wiki_continuation`) to obtain literal native IDs through `sessionwiki`'s new `Adapter::native_id` accessor rather than duplicating path-inversion helpers.

### Milestone 3: Unified compact tool presentation and single-pass `MjolnirAdapter`

Eliminate the oversized raw source stored inside `ToolCallPresentation`, capture SessionWiki's tool summary and provenance inputs once when a tool settles (absorbing #1636 scope item 1), and replace `MjolnirAdapter`'s double walk over raw tool JSON with a single pass.

In `mj-core/src/transcript.rs`, define a compact `ToolEditSnippet` struct (`path: String`, `kind: ToolEditKind`, `snippet: String` capped at 512 bytes) and add `#[serde(default)]` fields to `ToolCallPresentation`: `output_line_count: Option<u64>`, `first_error_line: Option<String>` (capped at 256 bytes), `edit_snippets: Vec<ToolEditSnippet>`, and `detail_trimmed: bool`. Keep `source: PackedText` with `#[serde(default)]` so existing serialized rows deserialize cleanly, and add a helper on `mj-transcript/src/transcript.rs` that computes `output_line_count`, `first_error_line`, and `edit_snippets` from a full `TranscriptBody::Tool` (including attached `terminal_outputs` and diffs) and clears `presentation.source` to an empty `PackedText` whenever a tool call is compacted or trimmed. Keep `presentation.source` populated only while a tool call is unsettled (so partial ACP updates can still re-summarize incrementally).

Update `mj-transcript/src/transcript.rs::compact_tool_call_for_retention` so it calls that summary-enrichment helper before stripping `rawInput`, `rawOutput`, `_meta`, non-diff `content`, diff patch text, and `terminal_outputs`, sets `detail_trimmed = true`, and clears `presentation.source`. Also extend `compact_tool_call_for_retention` (or a sibling `compact_transcript_body_for_retention`) to compact standalone `TranscriptBody::TerminalOutput` items by keeping exit code, signal, truncation status, output line count, and a bounded first-error/summary line while dropping the raw 256 KiB output buffer and marking `detail_trimmed = true`.

Refactor `mj-sessionwiki/src/sessionwiki.rs` (`tool_messages`, `tool_output`) and `mj-sessionwiki/src/sessionwiki/provenance.rs` (`Evidence::observe`) so both consume the precomputed `ToolCallPresentation` metadata first (falling back to raw `call`/`terminal_outputs` only for unsettled or legacy uncompacted rows) and run in a single pass over `materialized_transcript_items`. Verify with regression tests that a compacted tool call retains identical SessionWiki line counts, first-line failure quotes, and file-edit provenance after compaction.

### Milestone 4: Cold `.mjtd` tool-detail store, schema 152, and turn-end trimming

Create `mj-store/src/database/tool_details.rs` owning the per-session cold detail file at `<data_dir>/tool-details/<session_id>.mjtd`. Every appended record uses a self-describing binary frame: 4-byte magic `b"MJTD"`, 1-byte format version (`1`), 2-byte big-endian `stable_id` byte length, UTF-8 `stable_id` bytes, 4-byte big-endian IEEE CRC32 over the compressed payload, 4-byte big-endian compressed byte length, and the Zstd-compressed UTF-8 JSON of the full pre-trim `TranscriptBody`. Provide three operations: `append_tool_detail(data_dir, session_id, stable_id, body_json) -> Result<(u64, u32)>` (opens or creates the per-session file with append semantics, writes the complete frame, flushes to OS, and returns `(offset, total_frame_len)`), `read_tool_detail(data_dir, session_id, expected_stable_id, offset, len) -> Result<TranscriptBody>` (opens read-only, performs a single positional read of `len` bytes at `offset`, verifies magic, version, `expected_stable_id`, and CRC32, and Zstd-decompresses the payload—failing visibly with a structured error on missing file, truncation, ID mismatch, or checksum mismatch), and `remove_session_tool_details(data_dir, session_id)` (unlinks the `.mjtd` file cleanly when `delete_session` or `force_destroy_session` runs, ignoring `NotFound`).

Advance `mj-store` in a single breaking migration from `SCHEMA_VERSION = 151` to `152` and `MINIMUM_COMPATIBLE_VERSION = 151` to `152` (`mj-store/src/database.rs:34`, `mj-store/src/database/schema.rs:14`, `baseline.sql`, and `legacy_schema.rs`). Add five columns to `materialized_transcript_items`: `detail_offset INTEGER`, `detail_len INTEGER`, `item_kind TEXT NOT NULL DEFAULT ''`, `settled INTEGER NOT NULL DEFAULT 1`, and `update_ordinal INTEGER NOT NULL DEFAULT 0`. Populate `item_kind`, `settled`, and `update_ordinal` (`COALESCE(latest_content_event_ordinal, position)`) in the migration transaction from existing rows, add covering index `(session_id, update_ordinal, position, stable_id)` plus partial index `(session_id, position, stable_id) WHERE settled = 0`, and maintain all five columns on every upsert in `mj-store/src/database/state_io.rs:474-550` (importantly clearing `detail_offset = NULL, detail_len = NULL` whenever a late relay update replaces `body_json` on an existing row so a pointer can never go stale). Rewrite `load_materialized_transcript_filtered` and actor startup's unsettled scan (`mj-store/src/database/materialized.rs:847-989,1181-1285`) to query `item_kind`, `settled`, and `update_ordinal` directly instead of running `json_extract` over `body_json` and temp sorts.

Replace the unsynchronized `SELECT`-then-`UPDATE` loop in `compact_materialized_transcript_batch_in` (`mj-store/src/database/materialized.rs:1089-1170`) with a race-free spill-and-trim routine `spill_and_trim_settled_tools_through(session_id, frontier_ordinal)` serialized on `submit_database_write`. Inside the serialized write pass, select rows for `session_id` with `position <= frontier_ordinal`, `item_kind IN ('tool', 'terminal_output')`, `settled = 1`, `detail_offset IS NULL`, and `length(body_json) > RETENTION_BODY_FLOOR_BYTES`. Never trim `Pending` or `InProgress` tool calls, and never trim a standalone `TerminalOutput` that can still be claimed by an unsettled tool call in the same session. For each candidate row, compact `TranscriptBody` via Milestone 3's helper (skipping if it does not shrink), append the original full `body_json` to `<session_id>.mjtd` first, and then commit the SQLite `UPDATE` setting `body_json = compacted, detail_offset = offset, detail_len = len` guarded by `AND last_changed_at_ms = ? AND latest_content_event_ordinal IS ?`. Because appending to `.mjtd` precedes the SQLite commit, a crash before commit leaves SQLite intact with full inline JSON (the next turn-end pass retries cleanly), and a crash after commit leaves SQLite pointing to an already-flushed `.mjtd` frame.

Invoke `spill_and_trim_settled_tools_through` from `mj-session-runtime/src/session_manager/standalone.rs` whenever a committed relay page leaves the session in `MaterializedExecutionState::Idle` (and on suspend via `release_projection_behind_checkpoint` to catch any remaining settled rows). Return the trimmed `(stable_id, compacted_body)` pairs to the session actor so it updates its live in-memory `self.materialized` window immediately, shrinking daemon RAM alongside SQLite and ensuring every subsequent suspend/Move `canonical/session.json` automatically omits bulky tool outputs. Keep `mj-store/src/database/materialized.rs` backward-compatible with pre-152 `.hel.zip` archives, and add a bounded, restartable backfill pass (triggered on daemon startup or idle maintenance in small batches, participating in daemon handoff cancellation per `AGENTS.md` without holding upgrade admission) that spills and trims legacy untrimmed settled tool rows in existing databases without blocking active sessions.

### Milestone 5: On-demand click-to-expand tool detail across API, TUI, and Web

Add `load_tool_call_detail(session_id, stable_id) -> Result<ToolCallDetailResult>` in `mj-store/src/database/materialized.rs`. It reads `(body_json, detail_offset, detail_len)` for `(session_id, stable_id)` in one primary-key lookup and returns one of three typed outcomes: `Full(TranscriptBody)` directly from `body_json` when the row is still inline/untrimmed; `Full(TranscriptBody)` from `tool_details::read_tool_detail` when `(detail_offset, detail_len)` is present; or—if the row is marked trimmed with `detail_offset IS NULL` (for example, a pre-152 post-suspend compacted row) or `.mjtd` is missing—attempts a verified read of that single item from the session's latest legacy checkpoint `canonical/session.json` before returning `Unavailable { summary: Option<TranscriptBody>, reason: String }`.

Expose this lookup across all control-plane boundaries without blocking UI loops. In `mj-core/src/transcript.rs`, add `stable_id: Option<String>` and `tool_detail_trimmed: bool` to `ChatEntry` (populated in `mj-client/src/transcript.rs` whenever a tool or standalone terminal has `detail_trimmed = true`). Add `fn tool_call_detail(&self, stable_id: String) -> BoxFuture<'_, Result<ToolCallDetailResult>>` to `mj_client::session::SessionHandleBackend` (`mj-client/src/session.rs`) and implement it in `ClientSessionHandle` (`mj-session-runtime/src/session_manager/client_backend.rs`) via `spawn_blocking`. Add HTTP routes `GET /api/v1/sessions/{session_id}/items/{stable_id}/detail`, `GET /api/v1/sessions/{session_id}/native-agents/{child_id}/items/{stable_id}/detail`, and browser route `GET /api/sessions/{session_id}/items/{stable_id}/detail` in `mj-server/src/server/api/routes.rs`, `turns.rs`, `subagent_backend.rs`, and `mj-server/src/server/routes.rs` (running `load_tool_call_detail` in `spawn_blocking` and returning structured JSON with hydrated `ChatEntry`-equivalent detail fields, raw `call`, `terminal_outputs`, and explicit `unavailable_reason` when absent).

Wire the terminal UI in `mj-chat/src/chat/transcript/render.rs` and `mj-chat/src/chat/active/dispatch.rs`. When the user clicks a collapsed tool row whose `ChatEntry` has `tool_detail_trimmed == true` and whose `stable_id` is not yet in `ChatState.hydrated_tool_details`, mark it expanded immediately, display a muted `"Loading tool details…"` line under the header, and spawn an async task calling `session.tool_call_detail(stable_id)` that delivers `ChatIoUpdate::ToolCallDetail { stable_id, result }` on `chat_io_tx`. On success, cache the hydrated detail in `ChatState` (bounded per-session LRU or cleared on session switch) and re-render `ToolView::Expanded` identically to pre-trim behavior. On `Unavailable` or error, display the reason inline beside the retained summary and allow retrying on collapse/re-expand. When the user switches to Raw mode (`Alt-T`), lazily request details for trimmed tool rows currently in the visible viewport rather than hydrating the entire session eagerly.

Wire the web viewer in `mj-server/src/web/viewer.js` (`paintEntry`, around lines 9357–9406) so tool entries with trimmed or expandable details render an interactive `<details class="block-fold tool-detail-fold">` (matching the existing fold state preservation in `openFolds`) whose `toggle` event fetches `/api/sessions/<id>/items/<stable_id>/detail` once on first open, shows `"Loading tool details…"` while in flight, renders input, output lines, diff patches, terminal outputs, and errors upon arrival, caches the response on the entry, and renders an explicit inline error if unavailable.

## Concrete Steps

All Mjolnir commands run in `/home/jonathan/Projects/mjolnir/.mj/clones/52f10ee41f9c61aff134b426ad72a9d1` using the dev profile (never `--release`, so `debug_assert!` and overflow checks run) and outside the restricted sandbox so loopback sockets and SQLite tests work cleanly. Never point test binaries at the host default instance; use `--instance <test-name>` for any manual CLI/daemon check and stop the test daemon afterward.

For Milestone 1 (`jbellis/sessionwiki`), work in a clean checkout of `jbellis/sessionwiki` (branched from `origin/brokk-release-0.34.27`), implement the Codex `item_completed`, `raw_cwd`, `native_id`, and `is_thought` additions, and validate in that checkout with:

    cargo test
    cargo clippy --all-targets -- -D warnings

After publishing `brokk-sessionwiki 0.34.28` (and updating `Cargo.toml`, `Cargo.lock`, `docs/src/content/docs/sessions.md`, and `mj-sessionwiki/src/sessionwiki.rs`), validate Milestone 1 in Mjolnir with:

    cargo test -p brokk-mj-sessionwiki native_tests
    cargo clippy -p brokk-mj-sessionwiki --all-targets -- -D warnings

For Milestone 2, after replacing `mj-import`'s duplicate transcript/edit decoders and direct idle snapshot creation, validate with:

    cargo test -p brokk-mj-import -p brokk-mj-sessionwiki
    cargo clippy -p brokk-mj-import -p brokk-mj-sessionwiki --all-targets -- -D warnings

For Milestone 3, after enriching `ToolCallPresentation` and converting `MjolnirAdapter` to single-pass reading, validate with:

    cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-sessionwiki
    cargo clippy -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-sessionwiki --all-targets -- -D warnings

For Milestone 4, after adding `tool_details.rs`, migration 152, turn-end spill-and-trim, and destroy cleanup, validate schema migrations (including isolated upgrade fixtures), `.mjtd` framing/crash recovery, and snapshot size with:

    cargo test -p brokk-mj-store -p brokk-mj-session-runtime -p brokk-mj-controller -p brokk-mj-checkpoint
    cargo clippy -p brokk-mj-store -p brokk-mj-session-runtime -p brokk-mj-controller -p brokk-mj-checkpoint --all-targets -- -D warnings

For Milestone 5, after wiring API routes, TUI `mj-chat` async click-to-expand, and Web `viewer.js`, run the targeted crate tests (including `mj-chat` golden tests under `TZ=UTC` so local-time goldens remain deterministic) and lint:

    TZ=UTC cargo test -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-server
    cargo clippy -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-server --all-targets -- -D warnings

For final live verification before committing and requesting review, run an isolated instance smoke check (starting a test daemon with `--instance 1637-smoke`, running a session with a >64 KiB tool output, confirming `materialized_transcript_items` trimmed the row at turn end while `<session_id>.mjtd` holds the compressed frame, clicking to expand the tool call in TUI and via `GET /api/v1/sessions/<id>/items/<stable_id>/detail`, suspending and confirming `canonical/session.json` stays compact while click-to-expand still succeeds, destroying the session and confirming `.mjtd` is unlinked, and stopping the daemon with `mj --instance 1637-smoke daemon stop`).

## Validation and Acceptance

Acceptance is verified through observable behaviors and targeted tests matching `AGENTS.md` (golden, end-to-end, integration, and tricky-condition tests only—no trivial getter unit tests):

1. **DRY native parsing and Antigravity indexing (Milestones 1–2):** A paginated Codex rollout containing dialogue and `FileChange` records exclusively in `event_msg.item_completed` is searchable in SessionWiki and imports cleanly in `mj-import` with exact `cwd`, literal `native_id`, and edited repository roots. An Antigravity (`agy`) native session under `brain/<id>/` is discovered and searchable via Mjolnir's SessionWiki sync. A native session starting with a compaction marker imports cleanly with a visible compacted-prefix notice instead of failing.

2. **SessionWiki preservation across trimming (Milestone 3):** After a tool call with multi-line output, a failure message, and a file edit is compacted by `compact_tool_call_for_retention`, `MjolnirAdapter` produces the exact same SessionWiki output line count, first-line failure quote, and file-edit provenance as before compaction, while `ToolCallPresentation::source` is empty and serialized size drops below 1 KiB.

3. **Storage reduction and tricky-condition durability (Milestone 4):** A large-corpus integration test driving >64 KiB tool outputs, 128 KiB diffs, 256 KiB standalone `TerminalOutput` records, and 64 KiB tool inputs across multiple turns proves that after turn end: total `materialized_transcript_items.body_json` bytes grow only with conversation text and compact tool metadata (at least an 80 percent reduction on tool-heavy corpora), `canonical/session.json` in the resulting suspend `.hel.zip` reflects the same reduction, and `<session_id>.mjtd` is unlinked on `delete_session`. Tricky-condition tests specifically prove: (a) a still-`InProgress` background tool call is NOT trimmed when a turn ends; (b) a simulated crash after `.mjtd` append and before SQLite commit leaves full inline `body_json` intact and succeeds on retry; (c) a late tool update arriving after turn-end trim clears the stale `(detail_offset, detail_len)` and re-spills cleanly on the next settlement without losing the late update; (d) a corrupt CRC32 or mismatched `stable_id` in `.mjtd` fails visibly rather than returning wrong content; (e) `EXPLAIN QUERY PLAN` / integration query tests confirm `load_materialized_transcript_filtered` no longer scans `body_json`; and (f) an isolated schema-151 database upgrades cleanly to 152 and rejects downgrade to 151.

4. **Click-to-expand UX across TUI, Web, and API (Milestone 5):** Clicking a trimmed tool call in `mj-chat` (TUI) shows a non-blocking loading indicator, hydrates full `tool_input`, `tool_content`, diffs, terminal outputs, and errors via `SessionHandle::tool_call_detail`, and renders `ToolView::Expanded` identically to an untrimmed call (verified by a `TZ=UTC` `mj-chat` rendering/interaction test and golden check). Clicking a trimmed tool call in the Web viewer fetches `/api/sessions/<id>/items/<stable_id>/detail` once, renders full detail, and caches it. Removing `<session_id>.mjtd` causes both surfaces to render a clear inline unavailable-detail notice beside the retained summary rather than crashing or hanging.

## Idempotence and Recovery

Every milestone is safe to repeat or interrupt. In Milestone 1–2, `brokk-sessionwiki` schema version remains 9, so existing SessionWiki indexes never lose user tags, notes, or archives. In Milestone 4, `.mjtd` writes always precede the SQLite row update, and the SQLite update is guarded by the row's `(last_changed_at_ms, latest_content_event_ordinal)`: an interrupted turn-end trim or interrupted legacy backfill leaves the untrimmed inline row untouched in SQLite, and rerunning the trim pass appends a fresh frame and commits cleanly. Orphan trailing bytes in `.mjtd` from a mid-append crash are unreachable by any SQLite `(detail_offset, detail_len)` pointer and are reclaimed when the session is destroyed. If a `.mjtd` file is ever lost or deleted externally, `materialized_transcript_items` retains the complete conversation text and compact tool summaries, and click-to-expand degrades visibly to the retained summary with an explicit reason.

## Artifacts and Notes

Measured baseline evidence motivating the design:

    Commit 84aedcb88 single-session SQLite bloat:
      28,066 items -> 635 MiB body_json (561 MB tool-call content = 88.3%)

    60-session live newest-2,000-items census (.agents/plans/opaque-transcript-bodies.md:18-22):
      Tool.call JSON:            49.0 MiB  (52.9%)
      Tool.presentation:         19.9 MiB  (21.5%)  [64 KiB raw source cap]
      Tool.terminal_outputs:     13.9 MiB  (15.0%)  [4 MiB ACP display-terminal cap]
      Agent chunks:               6.8 MiB  ( 7.3%)
      Thought chunks:             2.8 MiB  ( 3.0%)
      User content:               0.3 MiB  ( 0.3%)
      Total tool data:           82.8 MiB  (89.3%)
      Total conversation text:    9.9 MiB  (10.7%)

Binary frame layout for `<data_dir>/tool-details/<session_id>.mjtd`:

    [0..4]                       magic: b"MJTD"
    [4]                          version: u8 = 1
    [5..7]                       stable_id_len: u16 (big-endian)
    [7..7+id_len]                stable_id: UTF-8 bytes
    [7+id_len..11+id_len]        crc32: u32 (big-endian IEEE CRC32 of zstd_payload)
    [11+id_len..15+id_len]       payload_len: u32 (big-endian)
    [15+id_len..15+id_len+plen]  zstd_payload: Zstd-compressed TranscriptBody JSON

## Interfaces and Dependencies

In `jbellis/sessionwiki` (`brokk-sessionwiki 0.34.28`):

    // src/model.rs
    pub struct Session {
        // existing fields...
        pub raw_cwd: Option<String>,
    }

    pub struct Message {
        // existing fields...
        pub is_thought: bool,
    }

    // src/adapters/mod.rs
    pub trait Adapter: Send + Sync {
        // existing methods...
        fn native_id(&self, _key: &str) -> Option<String> {
            None
        }
    }

In `mj-core/src/transcript.rs`:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ToolEditKind {
        Edit,
        Write,
        MultiEdit,
        NotebookEdit,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ToolEditSnippet {
        pub path: String,
        pub kind: ToolEditKind,
        pub snippet: String,
    }

    pub struct ToolCallPresentation {
        pub summary: String,
        #[serde(default)]
        pub source: PackedText,
        pub source_kind: ToolSummarySourceKind,
        pub tool_kind: ToolKind,
        #[serde(default)]
        pub summary_version: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output_line_count: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub first_error_line: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub edit_snippets: Vec<ToolEditSnippet>,
        #[serde(default, skip_serializing_if = "is_false")]
        pub detail_trimmed: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    pub enum ToolCallDetailResult {
        Full {
            body: TranscriptBody,
        },
        Unavailable {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            summary: Option<TranscriptBody>,
            reason: String,
        },
    }

In `mj-store/src/database/tool_details.rs` and `mj-store/src/database/materialized.rs`:

    pub fn append_tool_detail(
        data_dir: &Path,
        session_id: &str,
        stable_id: &str,
        body_json: &str,
    ) -> anyhow::Result<(u64, u32)>;

    pub fn read_tool_detail(
        data_dir: &Path,
        session_id: &str,
        expected_stable_id: &str,
        offset: u64,
        len: u32,
    ) -> anyhow::Result<mj_core::transcript::TranscriptBody>;

    pub fn remove_session_tool_details(data_dir: &Path, session_id: &str) -> anyhow::Result<()>;

    pub fn spill_and_trim_settled_tools_through(
        session_id: &str,
        frontier_ordinal: u64,
    ) -> anyhow::Result<Vec<(String, mj_core::transcript::TranscriptBody)>>;

    pub fn load_tool_call_detail(
        session_id: &str,
        stable_id: &str,
    ) -> anyhow::Result<Option<mj_core::transcript::ToolCallDetailResult>>;

In `mj-client/src/session.rs` and `mj-server/src/server/api/routes.rs`:

    // SessionHandleBackend trait method
    fn tool_call_detail(
        &self,
        stable_id: String,
    ) -> BoxFuture<'_, anyhow::Result<Option<mj_core::transcript::ToolCallDetailResult>>>;

    // HTTP endpoints (returning 200 JSON ToolCallDetailResponse, 404 if session/item missing)
    // GET /api/v1/sessions/{session_id}/items/{stable_id}/detail
    // GET /api/v1/sessions/{session_id}/native-agents/{child_id}/items/{stable_id}/detail
    // GET /api/sessions/{session_id}/items/{stable_id}/detail

Dependencies: Mjolnir already depends on `zstd` (via `mj-checkpoint`/`mj-core`) and `crc32fast` (or standard CRC32 already in the workspace dependency graph—verify exact crate name before adding any new dependency in `mj-store/Cargo.toml`). No new workspace crate is created.

Revision note (2026-11-22): Initial ExecPlan authored after completing the eight-harness native tool-call fidelity audit, UI expansion audit, `t3code` code dive, and `brokk-sessionwiki 0.34.27` audit. Recorded the decision to use daemon-local `.mjtd` cold Zstd files for click-to-expand instead of native harness lookup, and unified the `{mj, sw}` reader consolidation with turn-end tool trimming.

Revision note (2026-11-23): Recorded user confirmation that `.mjtd` cold tool-detail storage is daemon-local only (not copied into `.hel.zip` archives) and handed off the plan to session `52f10ee41f9c61aff134b426ad72a9d1` for joint `sessionwiki` + `mjolnir` implementation.
