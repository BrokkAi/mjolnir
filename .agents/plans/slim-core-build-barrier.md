# Shorten core's build barrier

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

Shorten the compilation barrier imposed by mj-core and stop archive/review implementation edits from rebuilding UI libraries. The user approved three implementation libraries that compile concurrently above core: mj-checkpoint, mj-transcript, and mj-review. Core retains shared contracts and small helpers. Acceptance concerns observed build time and rebuild propagation, not dependency counts.

## Progress

- [x] 2026-09-13: Read repository rules and dependency references; preliminary build measured core 49.1s and total 158s with cached external dependencies.
- [x] 2026-09-13: Added isolated benchmark driver; baseline at c52742b1 started with warmup plus three measured runs.
- [x] 2026-09-13: Extracted checkpoint implementation; 134 checkpoint tests and 8 worker capture tests passed, all-target compilation and boundary/version checks passed.
- [x] 2026-09-13: Extracted shared transcript implementation; 80 replay/formatting tests, all-target consumer check, boundary and version checks passed. Worker depends on transcript only in unpublished test fixtures.
- [x] 2026-09-13: Extracted turn-review engine; 49 behavior tests and all-target consumer checks passed. Publication order, licenses and boundary rules include all three independent libraries.
- [x] 2026-09-13: Completed all twelve release packages, extracted-library and desktop checks, full serial cargo test, strict all-target Clippy, three measured builds per revision, and all three rebuild-isolation probes. Recorded results for the final commit.

## Surprises & Discoveries

The full parallel test run hit ETXTBSY while the unchanged npm update fixture executed a freshly copied test binary. The fixture passed in isolation; the complete serial full-suite rerun passed, including the controller, worker, PTY/integration and doc-test groups. No fixture behavior or tests were changed to suppress this failure.

The first benchmark hit the mbx compiler cache and was discarded. The driver now invokes rustup-selected Cargo directly with compiler wrappers disabled. Direct Cargo exposes normal metadata/codegen pipelining, so metadata readiness is measured separately from full compilation.

Projection has production consumers in both controller and chat's second-opinion flow. It cannot move directly into controller. Worker uses projection in integration-style unit tests. Worker and controller already have versionless cross-runtime dev dependencies.

## Decision Log

2026-09-13: Dependency count is not an optimization target: independent third-party builds already overlap. Split substantial implementation units and measure their effect on the critical path.

2026-09-13: Preserve core Rust paths for shared types, but update implementation imports directly. No core facade may depend back on extracted libraries. Preserve serialized formats, digest domains, subprocess and runtime behavior. Leave the separate second-opinion workflow in core during this pass.

## Outcomes & Retrospective

All requested boundaries are implemented and validated. Core has 25,227 Rust lines including tests (48,294 before), approximately 18,843 non-test lines (32,148 before). The checkpoint, transcript, and review libraries depend only on core among production workspace dependencies. No existing test functions were removed.

Three measured builds per revision, excluding warmups, reduced median core metadata-stage duration from 15.66s to 9.58s and median full build completion from 89.38s to 64.93s. Core's full compile duration fell from a median 20.93s to 11.85s. All three after builds completed sooner than every before build. Host CPU load varied between runs, so these are observed local measurements rather than a controlled estimate of the refactor's exact percentage effect.

The timing trace confirms the intended overlap: in after run 3, transcript starts at 10.03s, review/checkpoint at 10.04s, while core code generation continues until 12.30s. The three implementation libraries finish at 11.06s, 10.65s and 12.69s respectively. The extra units do not create a sibling dependency chain.

Rebuild probes changed actual implementation in the isolated snapshot. Changing archive compression rebuilt checkpoint/worker/controller while core, transcript, review, client, chat and TUI stayed fresh. Changing summary bounds and its parser cache version rebuilt transcript/client/chat/TUI/controller while core/checkpoint/review/worker stayed fresh. Changing reviewer prompt text rebuilt review/worker/controller while core/checkpoint/transcript/client/chat/TUI stayed fresh. Every temporary edit was restored and rebuilt successfully.

Full `RUST_TEST_THREADS=1 cargo test` and `cargo clippy --all-targets -- -D warnings` exited zero. Packaging assembled all twelve packages with their LICENSE files, and extracted core plus all three new libraries passed all-target checks with the matching packaged core supplied as a local registry patch. Desktop all-target compilation passed using the existing local GTK/WebKit sysroot. Runtime formats, behavior, compiler profile and branch remain unchanged; no push was performed.

## Context and Orientation

The root is a virtual Cargo workspace. mj-core is the common prerequisite of client, worker, controller, chat and TUI. Its archive/checkpoint/native/resources modules contain filesystem and compression implementation; transcript mixes shared types with shell parsing and formatting; projection folds relay events into materialized sessions; review mixes shared status with the turn-review state machine and prompt construction. Controller already compiles for longer than core, so do not dump substantial implementation into controller.

## Plan of Work

First create mj-checkpoint with archive codecs, Git snapshot operations, checkpoint capture/restore, native formats and resource packaging. Retain canonical types and archive constants in core. Worker-only review Git orchestration moves into worker using mj-checkpoint's Git APIs; pure delta computations remain temporarily in core until review extraction.

Next create mj-transcript with projection, canonical/materialized conversion, tool-summary parsing and substantial formatting. Keep shared transcript data, simple constructors, validation, terminal sanitization and text helpers needed by core in core. Update imports in every consumer, including tests, rather than adding duplicate implementations or upward dependencies.

Then create mj-review with TurnReviewDriver, prompt builders, verdict parsers, Bifrost handling and pure delta summarization. Keep shared review configuration, request/status and persisted evidence types in core; retain second-opinion workflow. The three new libraries depend on core but not each other in production.

At each milestone update workspace/dependent manifests, boundary checks, release version synchronization, package assets and publication ordering. New registry packages use brokk-mj-* and matching workspace versions. Move existing behavior tests with implementation; use targeted tests before committing coherent milestones. Finish full workspace verification and performance comparison.

## Concrete Steps

Run from /home/jonathan/Projects/hel. Baseline is `python3 scripts/measure-crate-builds.py --revision c52742b1 --label before --directory target/core-build-bench`; after validated commits run the same command with the new revision and label after. The script snapshots committed files without changing branches, uses a private local build directory, warms external dependencies, and removes only workspace products before three measurements. Its logs and timing reports live under target/core-build-bench. Record toolchain and compiler environment there. Final acceptance used `--label before-final --revision c52742b1` and `--label after-final --revision caf6707d --probe-edits` sequentially after all other task-owned compilation finished. Both use the same build directory and rustup-selected Rust 1.96.0/Cargo; compiler wrappers are empty and the default development profile is unchanged.

The full parallel suite initially failed only in the unchanged npm restart fixture with ETXTBSY; the complete passing rerun was `RUST_TEST_THREADS=1 cargo test`.

Run focused elevated `cargo test -p brokk-mj-checkpoint`, `cargo test -p brokk-mj-transcript`, and `cargo test -p brokk-mj-review` after the respective extractions. Final verification is elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `node scripts/check-crate-boundaries.mjs`, `node scripts/release-version.mjs check v2.6.4`, and `node scripts/sync-package-assets.mjs check`. Verify standalone libraries and extracted package builds, following current CI packaging conventions.

## Validation and Acceptance

Archive round trips, verified streaming, native relocation, transcript replay/formatting, and review cancellation/resume retain their existing expectations. All required tests and Clippy must pass. Compare median core blocking time and total build time across three controlled before/after builds. Timing reports must show sibling compilation overlap and repeatable reductions; revise the boundary if an added chain erases the gain.

In a benchmark source snapshot, make a temporary implementation edit in archive packing, shell-summary parsing, and review prompt construction, rebuilding and restoring between scenarios. Archive and review edits must leave core/client/chat/TUI library compilation fresh; final binary relinking is allowed. Capture Cargo freshness evidence. No serialized-format, compiler-profile, or runtime-topology changes are part of acceptance.

## Idempotence and Recovery

Benchmark labels deliberately refuse to overwrite an existing source snapshot. Use a new label when retrying. Never clean the user's entire target directory or alter unrelated untracked files. Stop subprocess owners before removing any scratch files. Commit only files changed for this task, on the current branch; no push is authorized.

## Artifacts and Notes

Preliminary timing evidence: core starts 3.64s, compiles 49.07s; client/worker start 52.66s; controller starts 60.88s and compiles 76.66s; CLI completes near 158s. Controlled measurements replace this single wrapper-enabled observation as acceptance evidence; do not compare its 49.07s directly with the unwrapped final benchmark.

Measured runs (core metadata stage / full core compile / final build unit completion, seconds):

    before-final 1: 17.72 / 22.70 / 80.83
    before-final 2: 15.66 / 19.91 / 89.38
    before-final 3: 15.15 / 20.93 / 89.90
    after-final  1: 11.23 / 14.59 / 67.11
    after-final  2:  9.40 / 11.79 / 56.85
    after-final  3:  9.58 / 11.85 / 64.93

Raw settings, JSON summaries, HTML timing reports, edit freshness and restore logs are under `target/core-build-bench/{before-final,after-final}`. Suite evidence is in `target/core-workspace-tests-serial.log`, `target/core-workspace-clippy.log`, `target/core-package.log`, `target/core-packaged-check.log`, and `target/core-desktop-check.log`. The discarded cache-hit pilot is under before, and the earlier unwrapped pilot that overlapped validation compilation is under before-uncached.

## Interfaces and Dependencies

New library Rust names are mj_checkpoint, mj_transcript, and mj_review; dependency keys use hyphens. Shared canonical/session/transcript/review types remain in mj_core. The boundary checker must prohibit production sibling dependencies between new libraries and prohibit UI dependencies on checkpoint and turn-review implementation. Implementation APIs preserve signatures where possible with import-path changes; shared formats remain byte-compatible. Existing shared subprocess helpers remain authoritative.

Revision note: created from the approved plan before implementation, with reproducible benchmark and packaging steps.

Revision note: checkpoint extraction validated; benchmark corrected to exclude compiler cache hits.

Revision note: transcript extraction validated and worker production dependency kept limited to core/checkpoint.

Revision note: turn-review extraction validated; all 3,218 statically inventoried test functions retained. Controlled final timing will run without other compilation jobs competing for resources.

Revision note: parser cache-version ownership moved into mj-transcript so a rule-version bump does not dirty core. The benchmark now probes archive compression, summary rules plus cache version, and review prompt edits in its private source snapshot and checks Cargo library freshness.

Revision note: final validation and performance acceptance completed; recorded all measured runs, host-load limitations, sibling overlap and successful edit/restore freshness checks.
