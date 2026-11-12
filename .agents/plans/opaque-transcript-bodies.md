# Hold transcript payload JSON as opaque text in the live window

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

The daemon's largest heap owner is the decoded live transcript windows: about 440–470 MB for 60 running sessions (jemalloc heap profile, 2026-11-12, `mj_controller::database::materialized::decode_transcript_body`), although the same items are only about 100 MiB of JSON. The cause is the representation. Tool calls and other ACP payloads are held as `serde_json::Value`, and `agent-client-protocol` enables serde_json's `preserve_order`, so every JSON object is an `IndexMap` with its own hash table; small nested objects cost ten or more times their JSON size. After this change those payloads are held as their JSON text and decoded only where a reader needs them, cutting the windows by about 4.5x. A later milestone may compress the text with zstd and a trained dictionary for about another 4x (measured on live windows: 67.0 MiB raw → 16.4 MiB with a 110 KiB dictionary; 26.3 MiB per-item without one). Users see no change.

## Progress

- [ ] M0: audit every reader of the opaque candidates and measure a prototype on real windows (memory and CPU of the hot paths).
- [ ] M1: introduce the opaque type for `TranscriptBody::Tool.call`, convert readers through accessors and cached metadata, no behavior change.
- [ ] M2: extend to other large `Value` payloads only if M0 measurement justifies it (Agent/Thought chunks, User content, Plan).
- [ ] M3 (separate decision): zstd with a trained dictionary inside the same type.
- [ ] Review, push, install, heap profile.

## Surprises & Discoveries

(none yet)

## Decision Log

- Decision: start with `TranscriptBody::Tool.call` (tool JSON is ~56 of ~67 MiB of the measured window tails); other payloads follow only if measured worthwhile.
  Rationale: largest share for the least churn.
  Date/Author: 2026-11-12, parent agent with user approval ("go ahead and start opaque body work").
- Decision: the opaque type lives in `mj-core` beside `TranscriptBody`, holds the JSON text (e.g. `Box<serde_json::value::RawValue>`; `raw_value` is already enabled by the ACP crates), and serializes and deserializes transparently, so store rows, relay/wire messages and goldens stay byte-identical. Deserializing from the store copies the text without building a `Value` tree.
  Rationale: one representation change with no format or protocol change; also removes parse cost on window loads.
- Decision: hot paths read small typed metadata computed once, in one constructor, from the parsed value whenever a call is created or updated: at least the decoded status (with today's typed-decode rule: malformed fields read as Pending), the tool-call id, the terminal ids named in content, and a digest of the raw result used by raw-output terminal matching. Cold paths parse on demand through one accessor. Mutations (projector merges of tool updates) parse, modify, re-serialize and recompute metadata through one function.
  Rationale: without metadata, raw-output matching would parse every tool in the window on each terminal event; one constructor keeps metadata and text from disagreeing.
- Decision: compression is a separate milestone and a separate decision after M1 is measured live.
  Rationale: compression adds per-access decompression and a dictionary lifecycle (training, versioning, embedding); measure the raw-text win first.

## Outcomes & Retrospective

(none yet)

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
