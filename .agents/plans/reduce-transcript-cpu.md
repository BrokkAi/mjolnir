# Reduce repeated presentation work

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Reduce CPU spent keeping the terminal session list and transcripts current. Preserve session ordering, live tool output, and transcript contents while reusing unchanged derived data.

## Progress

- [x] (2026-09-11) Profiled the running UI and daemon and inspected hot paths.
- [x] (2026-09-11) Cache visible session ordering against actual grouping and timestamp inputs; parse once per visible session on a cache miss.
- [x] (2026-09-11) Reuse tool summaries and eliminate redundant JSON decoding, including output updates whose command comes from unchanged input.
- [x] (2026-09-11) Reuse transcript entry conversion in background dashboard preparation, sharing retained entries with the snapshot and checking diff-stat changes.
- [x] (2026-09-11) Run behavior tests, full test coverage, and clippy. Commit prepared.

## Surprises & Discoveries

The profiler captured 4,762 samples with partial unwinding and 14% loss. Timestamp parsing was the main UI thread's hottest leaf function. Source inspection found sorting before workspace filtering, repeated tool-call decoding, and full transcript conversion during otherwise incremental preparation.

## Decision Log

Cache derived values using their actual inputs so direct state changes cannot leave stale UI data. Reuse existing transcript interpretation helpers rather than inventing a second decoder. Keep preparation in its existing background tasks.

## Context and Orientation

`mj-tui/src/lib.rs` orders sessions for the terminal. `src/hel_projection.rs` applies durable tool updates in the daemon (the background controller process). `mj-tui/src/ingest.rs` prepares dashboard snapshots; `mj-chat/src/hel_chat/transcript.rs` converts stored transcript items into display entries. A projection is the current state derived from recorded events.

## Plan of Work

First cache ordered visible session IDs against workspace, visibility, creation time, and project grouping inputs. Then avoid regenerating tool presentation when its inputs are unchanged and serialize an updated call only once. Finally retain converted entries in the existing per-session preparation cache and reuse entries whose stored source and diff statistics agree.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Inspect and edit the files above with apply_patch. Run `cargo fmt --all`, focused `cargo test` filters, then `cargo test` and `cargo clippy --all-targets -- -D warnings`. Run every test outside the sandbox. Review `git diff --check` and commit only this task's files on the current branch.

## Validation and Acceptance

Tests must preserve ordering after workspace, timestamp, visibility, and project changes; preserve tool summaries through output updates while updating changed commands; and match fresh transcript conversion after edits and appends. Reuse checks should prove unchanged expensive work is skipped. All required Cargo checks must pass. Live CPU improvement requires launching the rebuilt executable; do not restart the user's active processes implicitly.

## Idempotence and Recovery

Changes affect only in-memory caches and require no database migration. Checks can be repeated. Preserve unrelated working-tree files and do not push.

## Artifacts and Notes

The diagnostic capture is `/tmp/mj-cpu-profile-2.data`. Its sample loss prevents exact attribution of CPU shares.

## Interfaces and Dependencies

Use existing Rust standard library containers, the current transcript source equality helper, and the current tool presentation builder. Add no dependencies or crates.

## Outcomes & Retrospective

All three implementation changes are complete. Validation passed: `cargo test --quiet -- --skip npm_upgrade_restarts_after_the_running_package_is_removed` covered the remaining suite, and `cargo test --quiet npm_upgrade_restarts_after_the_running_package_is_removed -- --test-threads=1` passed the excluded test in isolation. Two unfiltered runs encountered that pre-existing executable-copy ETXTBSY race. `cargo clippy --all-targets -- -D warnings` passed. Chat passed 521 enabled tests, core 949, dashboard 433, and the remaining packages and integration tests also passed. No live process was restarted and no CPU reduction percentage is claimed.

The initial chat/dashboard pass passed 519 chat and 432 dashboard tests. Added regression tests now cover transcript edit/append reuse, diff-stat addition/removal, visible ordering invalidation, and summary reuse through large output updates. The first full run found non-exhaustive ACP test fixture construction; corrected fixtures to assign fields on default instances. Full validation is running.

Initial plan recorded before implementation.

Implementation update: retained converted entry vectors use Arc so the snapshot and preparation cache share storage. Changed snapshots still clone reusable display entries, but skip their JSON decoding and text reconstruction. Browser server projection is outside this dashboard cache and remains a possible follow-up optimization.

Validation update: the new background cache checks complete stored source equality when pointers differ, since a timestamp alone cannot prove unchanged content. The regression covers edits sharing the same timestamp. All 521 enabled chat tests passed. The full suite encountered the known unrelated npm self-update ETXTBSY race (774 controller tests passed); a complete rerun is in progress. Clippy completed successfully.

Final validation update: the executable-copy test failed again under concurrent suite load and passed in isolation; every remaining enabled test then passed. This supersedes the in-progress validation notes above. The implementation is ready to commit on the current branch.
