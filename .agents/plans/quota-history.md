# Retain quota observations for subscription cost analysis

This ExecPlan is maintained according to `.agents/PLANS.md`. Its progress, discoveries, decisions, and outcomes must stay current.

## Purpose / Big Picture

The user wants to estimate the effective discount of coding subscriptions against API pricing. The daemon (the process that coordinates sessions and owns the SQLite writer) already probes enabled profiles every ten minutes and retains per-turn token usage and model attribution. Retain those quota observations in `quota_history` so an analyst can compare allowance consumption with the existing usage records. A read-only SQLite query should show chronological observations, including all quota windows and reset times. This change collects evidence; it does not calculate a discount or change pricing.

## Progress

- [x] (2026-11-02) Inspected quota refresh ownership, database migrations, retained token usage, and test isolation.
- [x] (2026-11-02) Added compatible revision 103 and atomic history/cache persistence for successful and failed refresh attempts.
- [x] (2026-11-02) Added migration/rollback coverage and extended existing cache, HTTP-reader, and writer-ownership integration cases.
- [x] (2026-11-02) Documented querying and interpretation; exercised the exact documented SQL against an isolated in-memory schema with multiple windows, unknowns, and a failed probe.
- [x] (2026-11-02) Focused history tests passed; full controller suite passed (2,228 passed, 18 ignored), including the direct-upgrade interruption matrix. Workspace clippy, rustfmt, and diff checks passed.
- [x] (2026-11-02) Committed the feature as f37c2812c and merged origin/master as b0e72ad49 without conflicts. Final quota tests passed against ACP SDK 5 (70 passed, 1 ignored); merged workspace clippy and formatting checks passed. The current master is prepared for the authorized push to origin/master.

## Surprises & Discoveries

The reset cache merges missing reset times from older observations. History must capture the incoming report before that merge, otherwise a missing reset could masquerade as newly observed data. Failed and rate-limited probes reach persistence before the manager substitutes older windows for display; capture them there to avoid recording stale data as fresh consumption.

Some provider readers (notably Muse) cache recent provider responses themselves. A row records a refresh attempt's normalized report, not a guarantee of a new HTTP measurement. Documentation explicitly distinguishes this from the daemon adopting a stored report at startup, which adds no row.

## Decision Log

- Decision: Store one row per fresh probe, including identical observations and failures, with indexed profile/time metadata and the complete serialized `ProfileQuota` in `body`.
  Rationale: SQLite JSON functions expose every window without losing banked resets, errors, amounts, unknown values, or model-specific allowances. Reusing the existing report shape avoids a second interpretation of the same data. Replaying cached reports at startup must not append observations.
  Date/Author: 2026-11-02 / Codex.
- Decision: Append history and update the successful-report cache in one writer transaction; keep history independent of session and profile deletion and do not backfill or prune it.
  Rationale: A failed transaction must not leave the two records disagreeing. Historical estimates need a long-lived series, and cached reports cannot reconstruct earlier readings. Failed probes append history but leave the last successful cache intact.
  Date/Author: 2026-11-02 / Codex.
- Decision: Keep the ten-minute probe schedule and provide SQL documentation, without adding UI/API surfaces or changing live data.
  Rationale: The requested result is a history table for later analysis. Provider throttling can make intervals longer; increasing probes would change account behavior.
  Date/Author: 2026-11-02 / Codex.

## Outcomes & Retrospective

The feature and documentation are implemented, committed, merged with upstream, and validated. History retains normalized observations for correlation with retained model/token usage; rounding, resets, incomplete usage, and activity outside Mjolnir remain explicit estimation limits. Collection begins when an updated daemon runs. No live store, installation, or release was changed. Publication uses the repository's ordinary `git push origin HEAD:master` workflow.

## Context and Orientation

`mj-controller/src/pollers/quota.rs` schedules real probes. `mj-controller/src/quota.rs::refresh_profile_with_cache_root` builds a `ProfileQuota` (defined in `mj-client/src/quota.rs`) and saves it on a blocking task when the process owns the database writer. `mj-controller/src/database/quota_cache.rs` currently stores one merged report per hashed configuration identity. `mj-controller/src/database.rs` declares schema revision 102; `mj-controller/src/database/schema.rs` runs forward migrations and records a minimum compatible revision of 102. Revision 103 will add an independent table without changing that minimum: older readers ignore it and older writers never delete or update it. Existing token records in `session_turn_usage` contain completion timestamps and usage JSON; `session_turn_selections` supplies profile and model attribution. Human-facing documentation belongs in `docs/src/content/docs/profiles.md`.

## Plan of Work

Add revision 103 with `quota_history(id INTEGER PRIMARY KEY, identity TEXT NOT NULL, profile_id TEXT NOT NULL, harness TEXT NOT NULL, sampled_at_epoch_seconds INTEGER NOT NULL, body TEXT NOT NULL CHECK(json_valid(body)))` and a profile/time/id index. Keep `body` as the unmerged report and reuse its refresh timestamp as the sampling time. Rename the persistence entry point to `save_quota_report` and its private connection-taking helper to `save_report`; append first, then merge and upsert the cache only for successful reports, all in one transaction. In the refresh function persist every outcome, including errors, on the existing blocking task and report any failure with the profile ID. Do not persist reports merely adopted from the cache.

Extend the existing cache integration test to compare historical raw JSON against merged cache state across reopening, including identical same-second probes, failures, model scopes, and unknown values. Force a cache-write failure with a SQLite trigger and prove history rolls back too. Add migration coverage for atomic rollback, unchanged compatibility, preservation of cache/session data, and older writer behavior. Exercise a real HTTP quota reader against a local fixture through the process-wide writer in an isolated child test.

## Concrete Steps

Work from the repository root using the existing mbx-wrapped Cargo, without changing target directories. Run focused checks while developing:

    cargo test -p brokk-mj-controller quota_history
    cargo test -p brokk-mj-controller quota_cache

Run every Cargo test elevated. Final checks are:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    git diff --check

Only isolated temporary stores and the existing test-owned configuration directories are permitted. Never launch a new binary against the live/default instance. Stage only owned files; commit on the current branch and push `HEAD:master`. Fetch and merge upstream if the push is rejected.

## Validation and Acceptance

A fresh isolated database has revision 103 and minimum compatibility 102. Applying the migration to a revision-102 store preserves its reset cache and session rows; an aborted migration leaves revision 102 and no partially created history table. Real probe results append in completion order; repeated values still produce observations, failure bodies contain no substituted old windows, and cached reports do not create readings. Cache merge behavior remains intact, but historical bodies retain the provider's missing fields. A failed cache write cannot leave an appended history row. Documentation includes an executable SQLite JSON query and explains how to compare the observations with token usage and API-equivalent estimates without adding overlapping allowances.

## Idempotence and Recovery

The migration runs once and is transactional. Failed writes roll back as a unit and log their failure; subsequent scheduled probes continue. History is append-only and intentionally retains distinct probes even if timestamps match. No cached data is backfilled. Tests can be rerun with their own temporary databases. Do not install, release, or migrate the live store as part of this task.

## Artifacts and Notes

The SQL example in profile documentation is the analysis entry point. Its isolated manual check passed: the LEFT JOIN preserves failed probes and missing percentages alongside multiple windows. With the unchanged mbx Cargo configuration, focused history tests passed (2/2), the full controller suite passed (2,228 passed, 18 ignored, 267 seconds), workspace clippy passed, and formatting/diff checks passed.

After merging the upstream ACP SDK update, `cargo test -p brokk-mj-controller quota` passed (70 passed, 1 ignored), workspace `cargo clippy --all-targets -- -D warnings` passed again, and rustfmt passed. Only upstream's unrelated changes were added by the merge; the quota implementation required no conflict resolution.

## Interfaces and Dependencies

Use existing `rusqlite`, `serde_json`, and the shared database writer; add no dependencies or crates. Keep `load_quota_cache` unchanged. Add `pub(crate) fn save_quota_report(identity: &str, report: &ProfileQuota) -> Result<()>` in `mj-controller/src/database/quota_cache.rs` and call it only from the existing supervised blocking persistence task.

Revision note: initial plan records raw-observation semantics, compatible migration classification, existing ten-minute cadence, SQL access, and isolated validation.

Revision note: implementation and documentation are ready for validation. Clarified provider-reader caching so observations are not overstated as fresh upstream measurements.

Revision note: full validation passed. Origin/master advanced to 7cbd8a4fd with an ACP SDK update and unrelated lifecycle/release changes; none modify the quota implementation or migration ladder. Commit this validated checkpoint, merge upstream on the current master branch, then exercise quota tests and workspace clippy against the resulting dependency versions before pushing.

Revision note: upstream integration and final validation are complete. The final documentation commit records the successful merged checks before the authorized publication.
