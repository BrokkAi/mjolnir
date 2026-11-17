# Cache only reachable browser transcript content

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Reduce the browser projector's retained parsed tool content while preserving every browser output field. Whole-window topology remains authoritative, with cached cheap facts for each source item version and full ChatEntry content only in the renderable suffix.

## Progress

- [x] (2026-11-12) Read existing conversion, topology, and previous blocker evidence; parent authorized cached typed ACP decoding.
- [x] (2026-11-12) Implement shared fact rules and suffix materialization.
- [x] (2026-11-12) Exact reference equality on synthetic topology changes and six live windows; removed the uncommitted probe after measurement.
- [x] (2026-11-12) Required checks passed, including workspace Clippy after a test-only let-chain fix; commit prepared on master without push or review.

## Surprises & Discoveries

Typed ACP validity determines completed-tool membership. Parent explicitly approved using the existing decoder once per item version. Message rows bypass the browser line budget, so the suffix must include the oldest Message.

## Decision Log

Cache source Arcs and scalar topology facts; reuse by Arc identity or full item equality, matching the existing conversion cache. Keep the typed decoder shared with full conversion. Grow suffixes at complete collapse boundaries until rendered lines consume the budget or history is exhausted. Preserve Arc sharing of published browser rows.

## Outcomes & Retrospective

Implementation and equality probes complete. Largest real actor window: 8,730 items, retained converted entries 2,180; approximate display-cache bytes 29,056,561 to 6,621,681 (including 349,200 bytes of facts). Steady thread CPU falls from 15–19 ms to 6–7 ms. Message-heavy windows may retain nearly all content and pay the small facts overhead. Final suites passed: client 57, chat 409 (2 existing ignored), controller server_runtime 164, client integration 1; doctests and formatting passed. Workspace Clippy passed after its single test-style warning was fixed; the affected comparison test and formatting passed again. No implementation or wire contract changes remain pending.

## Context and Orientation

`mj-client/src/transcript.rs` owns materialized conversion, collapse rules, browser clipping and BrowserTranscriptProjector. ChatEntry is the heavy typed display record; TranscriptItem is the source record held in an Arc. EntryCollapse identifies hidden rows and summary groups. BrowserTranscript contains rows, identities, presentation key and cursors.

## Plan of Work

Extract scalar EntryFacts consumed by shared topology functions, while keeping existing public ChatEntry interfaces as adapters. Cache facts per item version. Compute contextual interruption and duplicate suppression over the whole window. Render successively larger aligned suffixes and replace suffix-local topology key and restart reset boundary with whole-window results. Keep a test-only full-window projector as reference.

## Concrete Steps

Work in the repository root. Run focused tests with `TZ=UTC cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-chat -- transcript browser web projection golden`, outside the sandbox. Copy only selected source rows from the live store opened read-only into a scratch database under /mnt/optane/tmp; never copy the database wholesale.

## Validation and Acceptance

Require exact serialized browser output equality over successive projections, synthetic tricky cases and three real large windows. Record approximate owned retained bytes and repeated-projection timings. Finish with workspace clippy, fmt check, full client/chat suites and controller server_runtime tests. Use existing isolated test configuration; never start the default daemon.

## Idempotence and Recovery

Read live data only. Keep probe code uncommitted and remove it after measurements. Commit only changed implementation, tests and this plan. Do not push, request review, or create branches.

## Artifacts and Notes

Evidence logs belong in `.mj/agents/45369a259414ea14903f434dd9c9187e/`. Scratch databases stay outside this report directory.

## Interfaces and Dependencies

Retain public ChatEntry-based APIs and BrowserTranscript wire shape. Introduce private EntryFacts and private fact-based rule functions. Use existing ToolCall::deserialize and item_update_ordinal; no new dependency or persisted format.

Updated 2026-11-12 after implementation and read-only real-window probes. Initial probes exposed redundant suffix decoding; facts now come directly from converted suffix entries.

## Sparse Message refinement (2026-11-12)

The follow-up removes the oldest-Message suffix extension. `BrowserTranscriptProjector` now remembers the suffix boundary separately from its sorted converted cache, which also contains individual older Message items. The converter selects that union. Rendering accepts whole-window collapse decisions translated to selected indices, rather than discovering adjacency across the gaps. The whole-window presentation key and restart boundary remain authoritative. Only rendered suffix lines determine whether suffix growth is finished: older Messages consume the browser budget but cannot justify stopping before the suffix itself fills it.

Messages have role Message and raw_only false: they are neither completed tools nor thoughts, break tool streaks, and break restart runs. They cannot belong to summary groups. `browser_projection_entries` filters each ordinary Message independently by start_seq and raw_only; this projector uses last_compaction_seq zero as before. Mailbox disclosures are derived independently from each row's post-clipping lines. Message lines bypass clipping; durable ids and publication cursors still come from the shared renderer and publication cache. No intervening full content is required. Complete groups intersecting the suffix still require all their members.

Progress: sparse conversion and translated collapse rendering implemented; synthetic comparison extended with dispersed Messages adjacent to initial tool groups and restart markers, with repeated content changes and rebuilt source Arcs. Live read-only probe and final checks complete. Probe source and results belong in `.mj/agents/e7b8d73abe5d5d47698074d459c21d11/`; fixture copies are under `/mnt/optane/tmp/browser-projector-e7b8d73a`.


Sparse-refinement outcome: all 18 live-window comparisons passed. Message-heavy session 49c5f549 retains 899 rather than 3,917 converted entries; estimated display-cache bytes including facts fall from 13,108,659 to 2,417,147 (81.6%), with steady median projection CPU 7,599 to 3,778.5 microseconds. The other two sampled sessions retain the same entry counts. These estimates exclude shared sources and published rows and are not heap census results. Full measurements and probe source are preserved in the follow-up report directory. The probe is removed from source.

Final checks passed: byte-equality regression and all-hidden regression; client 58 tests and integration 1; chat 409 tests with 2 existing ignored; controller server_runtime 164 tests; doctests; workspace Clippy with warnings denied; rustfmt check. Full suite binaries came from the stipulated five-package Cargo selection, avoiding a changed feature graph. All goldens passed under TZ=UTC. The initial overlapping Cargo/rustdoc artifact race is documented in measurements.md; the serialized final run passed. No failing checks remain.
