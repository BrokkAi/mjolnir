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
