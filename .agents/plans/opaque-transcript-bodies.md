# Hold transcript payload JSON as opaque text in the live window

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

The daemon's largest heap owner is the decoded live transcript windows: about 440–470 MB for 60 running sessions (jemalloc heap profile, 2026-11-12, `mj_controller::database::materialized::decode_transcript_body`), although the same items are only about 100 MiB of JSON. The cause is the representation. Tool calls and other ACP payloads are held as `serde_json::Value`, and `agent-client-protocol` enables serde_json's `preserve_order`, so every JSON object is an `IndexMap` with its own hash table; small nested objects cost ten or more times their JSON size. After this change those payloads are held as their JSON text and decoded only where a reader needs them, cutting the windows by about 4.5x. A later milestone may compress the text with zstd and a trained dictionary for about another 4x (measured on live windows: 67.0 MiB raw → 16.4 MiB with a 110 KiB dictionary; 26.3 MiB per-item without one). Users see no change.

## Progress

- [x] M0: audit every reader of the opaque candidates and measure a prototype on real windows (memory and CPU of the hot paths).
- [x] M1: introduce the opaque type for `TranscriptBody::Tool.call`, convert readers through accessors and cached metadata, no behavior change (b51237b39).
- [x] M2: not needed. Agent and Thought chunks and User content total about 10 MiB in the measured windows (see Surprises).
- [x] M3: one compressed in-memory text type, applied to the tool-call JSON, terminal output text and the presentation source (approved by the user 2026-11-12, "yes to zstd"; scope and dictionary decisions below).
- [x] Review, push, install, heap profile for M1 (decoded windows 439 → 206 MB with the halved tail; 184 MB at the next profile).
- [ ] Review, push, install, heap profile for M3.

## Surprises & Discoveries

- After M1, the 184 MiB of decoded windows (heap profile of 891d942a9 at 15 minutes) split as 54.6 MiB tool-call JSON text, 5 MiB cached tool metadata, and about 121 MiB allocated while serde buffers the internally tagged body. Those buffered strings are moved into the final fields, so they are retained. Their nesting matches `terminal_outputs[].output` (55.8 MiB), one-level object strings such as `presentation` and `record.output` (17.5 MiB), and `System.text` (4.5 MiB). This attribution is inferred from nesting, not from type names.
- Bytes in the store for the latest 2,000 items of 60 live sessions: `tool.call` 49.0 MiB, `tool.presentation` 19.9 MiB, `tool.terminal_outputs[].output` 13.9 MiB, Agent chunks 6.8 MiB, System 6.6 MiB, Message events 3.7 MiB, Thought chunks 2.8 MiB, User content 0.3 MiB. `presentation.source` holds whole command texts (median presentation 317 bytes, p90 1,384, max 12,027 in one session); it is the only copy once retention trims a finished call's raw input.
- zstd level 3 on tool-call JSON (scripts and results in `/mnt/optane/tmp/zstd-m3/`): no dictionary 2.3x; a 7 KiB raw-content dictionary from the public ACP fixtures 2.5x; a 110 KiB dictionary trained on the user's own older items 3.8x (training takes 0.2 s on 6 MiB of samples). Terminal output compresses 2.5x with or without a dictionary. Per item: compression about 5–7 µs, decompression about 2 µs.

## Decision Log

- Decision: start with `TranscriptBody::Tool.call` (tool JSON is ~56 of ~67 MiB of the measured window tails); other payloads follow only if measured worthwhile.
  Rationale: largest share for the least churn.
  Date/Author: 2026-11-12, parent agent with user approval ("go ahead and start opaque body work").
- Decision: the opaque type lives in `mj-core` beside `TranscriptBody`, holds the JSON text (e.g. `Box<serde_json::value::RawValue>`; `raw_value` is already enabled by the ACP crates), and serializes and deserializes transparently, so store rows, relay/wire messages and goldens stay byte-identical. Deserializing from the store copies the text without building a `Value` tree.
  Rationale: one representation change with no format or protocol change; also removes parse cost on window loads.
- Decision: hot paths read small typed metadata computed once, in one constructor, from the parsed value whenever a call is created or updated: at least the decoded status (with today's typed-decode rule: malformed fields read as Pending), the tool-call id, the terminal ids named in content, and a digest of the raw result used by raw-output terminal matching. Cold paths parse on demand through one accessor. Mutations (projector merges of tool updates) parse, modify, re-serialize and recompute metadata through one function.
  Rationale: without metadata, raw-output matching would parse every tool in the window on each terminal event; one constructor keeps metadata and text from disagreeing.
- Decision (amends the type decision): the opaque type deserializes through a transient `serde_json::Value` and then stores the JSON text; no custom deserializers in M1. A raw fast path at the store boundary is optional (M1b) and only if measured load CPU matters.
  Rationale: `TranscriptBody` is `#[serde(tag = "kind")]`; serde buffers internally tagged enums through `Content`, which cannot hand `RawValue` the original text (`invalid type: newtype struct, expected any valid JSON value`), and transcript bodies also cross other tagged envelopes. The transient decode keeps the whole retained-memory benefit.
  Date/Author: 2026-11-12, parent agent after the implementer's M0 reproducer.
- Decision: cached metadata also carries the fallback-terminal classification, and keeps the two existing status rules as separate named fields: the raw stored status string (window trim) and whether the typed ACP decode succeeded (renderers read Pending when it did not).
  Rationale: terminal events typed-decode every tool through `fallback_tool_item` today; the two status rules are existing behavior and are kept explicit rather than silently merged.
  Date/Author: 2026-11-12, parent agent.
- Decision: compression is a separate milestone and a separate decision after M1 is measured live.
  Rationale: compression adds per-access decompression and a dictionary lifecycle (training, versioning, embedding); measure the raw-text win first.
- Decision (M3 scope): one immutable text type in `mj-core` holds either the plain text or its zstd frame, whichever is smaller. It replaces the JSON text inside `OpaqueToolCall`, `TerminalOutputRecord::output` and `ToolCallPresentation::source`. Store rows, relay and wire messages, and goldens stay byte-identical: the strings serialize as strings and the call as raw JSON.
  Rationale: these three fields are most of the decoded windows (Surprises); one type keeps one interpretation of compressed text.
  Date/Author: 2026-11-12, parent agent after the user approved zstd.
- Decision (M3 dictionary): no dictionary for now. A dictionary trained on transcripts must never be compiled into the published binary, because it would contain text from private sessions. A dictionary from public fixtures gains almost nothing (2.5x against 2.3x). Only a dictionary trained by each daemon on its own store reaches 3.8x, about 10 MiB more at the measured load, and it needs a startup training step that must finish before windows load. The representation keeps a variant tag, so a per-daemon dictionary can be added inside the type later without touching its readers.
  Date/Author: 2026-11-12, parent agent.
- Decision (M3 representation): zstd level 3 with the content size in the frame and no checksum. Compression and decompression reuse thread-local contexts, because creating a level-3 context per item allocates far more than the item. The original length is readable without decompressing (terminal matching compares lengths first). Equality compares text. Byte equality of two compressed values is exact, because compression is deterministic within one build. A value that compression does not shrink stays plain.
  Date/Author: 2026-11-12, parent agent.

## Outcomes & Retrospective

M3 uses `PackedText` for call JSON, terminal output and presentation source. ACP terminal metadata appends chunks (`mj-transcript/src/projection/terminals.rs`); it keeps an owned plain buffer until exit. Client-owned terminal observations arrive after reaping/release and pack immediately. No persisted or wire representation changes.

Temporary dev-profile field-load probe on the three M0 JSONL copies (removed before commit): retained text allocations 5,539,640 → 1,435,740; 4,946,754 → 1,427,217; 4,167,870 → 1,405,879 bytes. JSON-string decode including compression: 22.7 → 82.6, 17.2 → 80.5, 12.6 → 66.0 microseconds per field. Packed access: 15.1, 15.8, 13.1 microseconds per field. These measure the selected fields, not the entire decoded window or allocator overhead; dev profile and concurrent host load affect CPU numbers.

## Context and Orientation

`TranscriptBody` (`mj-core/src/transcript.rs`) is the stored and in-memory form of each transcript item; `TranscriptBody::Tool { call: serde_json::Value, terminal_outputs, terminal_refs, presentation }`. The daemon's live window of each session (`ProjectionWindow`, `mj-core/src/state.rs`; loader `load_materialized_actor_projection_from`, `mj-controller/src/database/materialized.rs`) holds `Arc<TranscriptItem>` values shared with runtime-feed tails (`mj-client/src/runtime_feed.rs` `SessionTail`) and the browser projector. The projector (`mj-transcript/src/projection/*`) applies relay events to items, including tool-call updates merged into `call`, terminal matching (`projection/terminals.rs`: `uniquely_matching_raw_tool`, `uniquely_matching_raw_terminal`, `tool_call_terminal_ids`), and the window trim reads tool status. Readers that render items (mj-client transcript conversion, mj-chat, summaries, SessionWiki, handoff, archive export, GitHub watch) read `call` fields.

## Plan of Work

M0: list every read and write of `Tool.call` (and of the other `Value` payloads) with their frequency (per event, per projection, per request, cold). Prototype the opaque type on a copy of three large live windows (store opened read-only, rows copied to `/mnt/optane/tmp`) and report retained bytes for `Value` vs opaque text, plus the CPU of window load, a tool-update merge, a terminal-output event and a browser projection.

M1: add the type and metadata; convert all readers and writers; keep exactly one interpretation of each field. Existing goldens, store round-trip and projector equality tests are the acceptance tests; add a byte-identity round-trip for a tool item through store and wire if none exists.

## Validation and Acceptance

- Existing transcript, projection, browser, TUI and golden tests pass unchanged (TZ=UTC).
- Store and wire JSON for tool items is byte-identical before and after.
- A live heap profile after install shows `decode_transcript_body` reduced roughly in proportion to the measured prototype.

## Idempotence and Recovery

No schema, store or protocol change; reverting the commits restores the `Value` representation and reads the same stored text.

M3 validation: all touched-crate suites pass (chat 409, controller 2570, core 638, transcript 98; integration/doc checks also pass), workspace clippy with warnings denied passes, and x86_64-unknown-linux-musl core check passes. Existing goldens unchanged.
