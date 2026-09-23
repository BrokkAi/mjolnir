# Recover unused native sessions after upgrades

This ExecPlan follows `.agents/PLANS.md` and remains a living record of implementation and validation.

## Purpose / Big Picture

An upgrade must reopen an empty session even when its original worker predates the saved evidence that the native agent session was never used. Codex does not save a native conversation until its first prompt. An empty session may therefore legitimately have no native history to load. Recover evidence from the retained worker journal before deciding whether a missing native session can safely be replaced. Never discard a used conversation or infer emptiness from missing evidence.

Recovery must also resolve and install the replacement worker before publishing a newer launch configuration, including for local containers running a different architecture from their controller.

## Progress

- [x] (2026-09-22) Read the screenshot, controller logs, and read-only worker metadata. Confirmed the original 2.13.0 launch mismatch and the current 2.17.0 recovery failure for a never-prompted Codex session.
- [x] (2026-09-22) Identified the missing `native_session_opened_ordinal` in an older snapshot whose complete retained journal includes its original non-resumed `SessionOpened` event.
- [x] (2026-09-22) Reproduced the old-snapshot failure, implemented bounded journal-based recovery, and passed the full worker suite: 549 library tests, 8 binary tests, and 7 integration tests (9 existing library tests ignored).
- [x] (2026-09-22) Deferred source resolution for every recovery target. Regressions pass for missing-source preservation, retry after source installation, and architecture detection inside a local container.
- [x] (2026-09-22) Full `env -u NO_COLOR cargo test`, `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks passed on the dev profile. Changes reviewed and prepared for the required current-branch commit.

## Surprises & Discoveries

The screenshot's launch error is dated September 18. On September 22 the controller successfully replaces the old worker with 2.17.0, which then repeatedly exits on a missing Codex rollout. Read-only journal metadata shows an original non-resumed session opening and no queued or dispatched prompts. `DurableRelay::open_with_mode` replays only events beyond the saved snapshot, so fields added since that snapshot was written stay unknown forever.

Local recovery also silently omitted binary replacement when source selection failed, while still replacing the launch configuration. It selected local-container workers using the controller architecture rather than the container architecture. Both decisions belong in the background recovery task already used for remote targets.

## Decision Log

Reconstruct only the missing history evidence, using validated retained journal events. Preserve the saved operational snapshot, command identities, queued work, and recovery frontiers. A retained journal that cannot prove the native session was locally created and unused remains conservative. Do not change the live store or restart installed processes for validation. The test environment must be isolated.

Extend deferred worker-source resolution to every target. A missing source becomes a recovery error before any launch replacement or restart, and the same recovery plan can succeed after the source becomes available. Preserve explicit prepared plans for callers that already have a source. This also removes binary copying and download/cache resolution from event-loop plan construction.

## Outcomes & Retrospective

The worker migration passes its regressions, including fake Codex and Claude native resume with queued work after upgrading an old snapshot. Used, archived, and corrupt histories remain conservative. Local recovery now resolves its worker for the actual target before replacing launch configuration; a missing source leaves both installed files unchanged and is retried without restarting the controller. Full workspace tests, clippy, formatting, and diff checks passed. Validation logs are `target/upgrade-native-history-tests.log` and `target/upgrade-native-history-clippy.log` (untracked build artifacts). The installed application and live data were not modified; the source fix requires installation to affect existing sessions.

## Context and Orientation

The worker process owns an agent session and records observations in a durable relay journal. `mj-worker/src/relay.rs` opens its saved `RelaySnapshot` and the journal. `mj-worker/src/relay/journal.rs` validates and replays journal files, including compressed sealed segments. `mj-worker/src/relay/replay.rs` supplies validated journal traversal. `mj-core/src/relay/snapshot.rs` defines `native_session_used` and `native_session_opened_ordinal`; older snapshots omit both. `mj-worker/src/acp/session.rs` attempts native resume and permits a replacement only if the relay proves that the missing session was never used. Tests are colocated in the relay and worker runtime modules.

## Plan of Work

The first milestone adds a regression that models an old saved snapshot, with its session opening outside the recent in-memory journal window. Reopening must prove that a locally created, never-prompted session is empty while preserving queued work and frontiers. Negative cases cover dispatched work, agent conversation content, externally resumed identities, a recovery floor that removed the opening, and invalid journal evidence.

The second milestone reconstructs missing evidence through the existing validated journal reader. Reuse shared interpretation of ACP content and command delivery. Persist the recovered evidence so normal subsequent startup avoids repeatedly scanning old history. Do not replay historical state over the current snapshot or run scans on controller/UI loops.

The third milestone runs focused tests, the full default workspace tests, formatting, and clippy. Review the final diff and commit only task-owned files on the current branch.

Before final validation, change `mj-controller/src/controller/worker_binary/upgrade.rs::worker_binary_refresh_plan` to defer source resolution for all targets through `DeferredWorkerBinaryRefresh`, defined in `mj-controller/src/session_manager/types.rs`. The recovery dispatcher in `session_manager/recovery.rs` invokes `refresh_target_worker_binary_if_stale` before touching launch configuration. An isolated regression in `controller/worker_binary/tests.rs` must first fail with a missing source without changing either installed file, then succeed using the same plan once the source is created.

## Concrete Steps

Work in `/Users/ryansvihla/code/mjolnir`. Run focused tests with `cargo test -p brokk-mj-worker` filters for the added regressions, then `env -u NO_COLOR cargo test` and `cargo clippy --all-targets -- -D warnings`. All Cargo tests run with sandbox escalation. Run `cargo fmt --all -- --check` and `git diff --check`. If exercising a CLI or daemon, use `--instance upgrade-native-history-test` with isolated configuration and data directories. Never point a test executable at the default instance.

## Validation and Acceptance

The regression must fail before the fix because the old snapshot is treated as potentially used, then pass after recovering its original opening from retained history. Evidence of transmitted prompts or conversation content must continue to prohibit replacement. Missing, truncated, corrupt, or archived evidence must never grant permission to replace history. Existing native-session replacement tests must still preserve queued prompts. All required Rust checks must pass or any environmental blocker must be reported precisely.

Observed: the old-snapshot regression failed on the original implementation with `assertion failed: !relay.native_session_may_have_history()`, then passed after the migration. One new architecture test initially used a container name that did not pass the existing ownership guard; the fixture now uses `targets::resource_name`, and the regression passes. The implementation did not weaken that guard.

## Idempotence and Recovery

The migration is safe to repeat and writes through the existing atomic snapshot mechanism. It does not modify journal events, their digests, native session identity, or command state. The installed application and live data are inspected only, never upgraded by the test build. Temporary test data belongs to existing isolated test helpers.

## Artifacts and Notes

Read-only live evidence: installed worker reports 2.17.0; its exit reason says Codex has no native history for a session considered already used. The retained journal contains a non-resumed opening at ordinal 4, no prompt commands, and a recovery floor of zero. The saved snapshot has neither history-evidence field.

## Interfaces and Dependencies

Use the existing `DurableRelay`, `RelaySnapshot`, `RelayObservation`, journal validation APIs, and ACP content predicate. No new crate or database schema migration is required. Keep new single-use migration logic near relay startup or its journal helpers.

Revision: 2026-09-22 — Created after identifying the actual current worker failure behind the stale screenshot diagnostic.

Revision: 2026-09-22 — Added binary/config pairing after confirming that local recovery could silently skip the binary refresh; recorded successful worker-suite validation.

Revision: 2026-09-22 — Recorded passing final workspace validation and the remaining installation boundary.
