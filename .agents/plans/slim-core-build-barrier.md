# Shorten core's build barrier

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

Shorten the compilation barrier imposed by mj-core and stop archive/review implementation edits from rebuilding UI libraries. The user approved three implementation libraries that compile concurrently above core: mj-checkpoint, mj-transcript, and mj-review. Core retains shared contracts and small helpers. Acceptance concerns observed build time and rebuild propagation, not dependency counts.

## Progress

- [x] 2026-09-13: Read repository rules and dependency references; preliminary build measured core 49.1s and total 158s with cached external dependencies.
- [x] 2026-09-13: Added isolated benchmark driver; baseline at c52742b1 started with warmup plus three measured runs.
- [x] 2026-09-13: Extracted checkpoint implementation; 134 checkpoint tests and 8 worker capture tests passed, all-target compilation and boundary/version checks passed.
- [x] 2026-09-13: Extracted shared transcript implementation; 80 replay/formatting tests, all-target consumer check, boundary and version checks passed. Worker depends on transcript only in unpublished test fixtures.
- [ ] Extract and validate turn-review engine; commit checkpoint.
- [ ] Complete package integration, full tests, strict Clippy, performance comparison, rebuild isolation, and final commit.

## Surprises & Discoveries

The first benchmark hit the mbx compiler cache and was discarded. The driver now invokes rustup-selected Cargo directly with compiler wrappers disabled. Direct Cargo exposes normal metadata/codegen pipelining, so metadata readiness is measured separately from full compilation.

Projection has production consumers in both controller and chat's second-opinion flow. It cannot move directly into controller. Worker uses projection in integration-style unit tests. Worker and controller already have versionless cross-runtime dev dependencies.

## Decision Log

2026-09-13: Dependency count is not an optimization target: independent third-party builds already overlap. Split substantial implementation units and measure their effect on the critical path.

2026-09-13: Preserve core Rust paths for shared types, but update implementation imports directly. No core facade may depend back on extracted libraries. Preserve serialized formats, digest domains, subprocess and runtime behavior. Leave the separate second-opinion workflow in core during this pass.

## Outcomes & Retrospective

Implementation and controlled performance measurements are in progress.

## Context and Orientation

The root is a virtual Cargo workspace. mj-core is the common prerequisite of client, worker, controller, chat and TUI. Its archive/checkpoint/native/resources modules contain filesystem and compression implementation; transcript mixes shared types with shell parsing and formatting; projection folds relay events into materialized sessions; review mixes shared status with the turn-review state machine and prompt construction. Controller already compiles for longer than core, so do not dump substantial implementation into controller.

## Plan of Work

First create mj-checkpoint with archive codecs, Git snapshot operations, checkpoint capture/restore, native formats and resource packaging. Retain canonical types and archive constants in core. Worker-only review Git orchestration moves into worker using mj-checkpoint's Git APIs; pure delta computations remain temporarily in core until review extraction.

Next create mj-transcript with projection, canonical/materialized conversion, tool-summary parsing and substantial formatting. Keep shared transcript data, simple constructors, validation, terminal sanitization and text helpers needed by core in core. Update imports in every consumer, including tests, rather than adding duplicate implementations or upward dependencies.

Then create mj-review with TurnReviewDriver, prompt builders, verdict parsers, Bifrost handling and pure delta summarization. Keep shared review configuration, request/status and persisted evidence types in core; retain second-opinion workflow. The three new libraries depend on core but not each other in production.

At each milestone update workspace/dependent manifests, boundary checks, release version synchronization, package assets and publication ordering. New registry packages use brokk-mj-* and matching workspace versions. Move existing behavior tests with implementation; use targeted tests before committing coherent milestones. Finish full workspace verification and performance comparison.

## Concrete Steps

Run from /home/jonathan/Projects/hel. Baseline is `python3 scripts/measure-crate-builds.py --revision c52742b1 --label before --directory target/core-build-bench`; after validated commits run the same command with the new revision and label after. The script snapshots committed files without changing branches, uses a private local build directory, warms external dependencies, and removes only workspace products before three measurements. Its logs and timing reports live under target/core-build-bench. Record toolchain and compiler environment there.

Run focused elevated `cargo test -p brokk-mj-checkpoint`, `cargo test -p brokk-mj-transcript`, and `cargo test -p brokk-mj-review` after the respective extractions. Final verification is elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `node scripts/check-crate-boundaries.mjs`, `node scripts/release-version.mjs check v2.6.4`, and `node scripts/sync-package-assets.mjs check`. Verify standalone libraries and extracted package builds, following current CI packaging conventions.

## Validation and Acceptance

Archive round trips, verified streaming, native relocation, transcript replay/formatting, and review cancellation/resume retain their existing expectations. All required tests and Clippy must pass. Compare median core blocking time and total build time across three controlled before/after builds. Timing reports must show sibling compilation overlap and repeatable reductions; revise the boundary if an added chain erases the gain.

In a benchmark source snapshot, make a temporary implementation edit in archive packing, shell-summary parsing, and review prompt construction, rebuilding and restoring between scenarios. Archive and review edits must leave core/client/chat/TUI library compilation fresh; final binary relinking is allowed. Capture Cargo freshness evidence. No serialized-format, compiler-profile, or runtime-topology changes are part of acceptance.

## Idempotence and Recovery

Benchmark labels deliberately refuse to overwrite an existing source snapshot. Use a new label when retrying. Never clean the user's entire target directory or alter unrelated untracked files. Stop subprocess owners before removing any scratch files. Commit only files changed for this task, on the current branch; no push is authorized.

## Artifacts and Notes

Preliminary timing evidence: core starts 3.64s, compiles 49.07s; client/worker start 52.66s; controller starts 60.88s and compiles 76.66s; CLI completes near 158s. Controlled measurements replace this single observation as acceptance evidence.

## Interfaces and Dependencies

New library Rust names are mj_checkpoint, mj_transcript, and mj_review; dependency keys use hyphens. Shared canonical/session/transcript/review types remain in mj_core. The boundary checker must prohibit production sibling dependencies between new libraries and prohibit UI dependencies on checkpoint and turn-review implementation. Implementation APIs preserve signatures where possible with import-path changes; shared formats remain byte-compatible. Existing shared subprocess helpers remain authoritative.

Revision note: created from the approved plan before implementation, with reproducible benchmark and packaging steps.

Revision note: checkpoint extraction validated; benchmark corrected to exclude compiler cache hits.

Revision note: transcript extraction validated and worker production dependency kept limited to core/checkpoint.
