# Refresh discovered profile capabilities when a harness home is edited

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document follows `.agents/PLANS.md`, which is checked in at that path and is the canonical rules document for ExecPlans.

## Purpose / Big Picture

When a person edits a model-provider file inside a harness profile's home directory — for example changing `model =` in a Codex profile's `config.toml` — every surface that shows models (`mj models`, the Settings and Move model pickers, automatic reviewer selection) keeps answering with the pre-edit discovery for as long as the daemon runs. After this change, the daemon's profile catalog notices such an edit by itself within one poll interval, re-probes only the profiles that use that home, and publishes the new answer; readers keep the old answer until the re-probe succeeds, so a transient probe failure never blanks the pickers. No filesystem watching or TTL is involved: a slow stat poll of three well-known files per distinct home, with content hashing only when a file's stat changes.

The observable behavior: edit `model = "deepseek-v4-pro"` to `model = "deepseek-flash"` in a Codex profile home, wait at most one poll interval (30 seconds), and `mj models --profile <id>` reports `deepseek-flash` without restarting the daemon. Editing the home of one profile never re-probes profiles with other homes. Editing a home shared by several profiles re-probes each of them.

## Progress

- [x] (2026-11-06) Inspected the catalog owner (`mj-controller/src/server_runtime/profile_catalog.rs`), the probe/cache layer (`mj-controller/src/controller/profile_config.rs`), the profile identity (`mj-core/src/config/harness.rs`), and the two prior fixes (55b8ea778 hashed home files into the cache key; b39c0e69c removed that and made the cache key definition-only). Confirmed the fix belongs in the catalog owner as a background stat poll, per the issue's proposed fix.
- [x] (2026-11-06) Implemented the home watch: `HOME_CATALOG_INPUTS`/`HOME_WATCH_INTERVAL` constants, `FileStat`/`WatchedInput`/`HomeFiles` state, `poll_home_files` (stat, hash on stat change), the `watch_homes` tick with first-sight baselining and launch-gated baseline advance, the `start_watching_homes` loop, a key-scoped `refresh` returning launched keys, and `attempt_refresh` so re-probes bypass the process-global cache via a new `refresh` parameter on `discover_for`.
- [x] (2026-11-06) Replaced `harness_home_files_do_not_invalidate_configured_capabilities` with the acceptance-rule tests: edited-home re-publishes within one tick and re-probes only that home (plus metadata-only rewrite and baselining assertions), shared-home re-primes each profile and no others, a new catalog generation discovers the current home (stopped-daemon rule), an edit detected during an in-flight re-probe is not lost, and `home_watch_digest_tracks_content_not_metadata` for the stat/hash rule. Focused suites pass: 34 profile_catalog tests, 12 profile_config tests.
- [x] (2026-11-06) Committed the implementation as ae57c376f and merged origin/master (advanced to 552633a03 with the settings-golden refresh f97dcce09 and review-host changes) cleanly as b351954b6. Requested review of the merge commit against the merge base (request 820747a2).
- [ ] Full workspace suite and clippy on the merged tree, then address the review verdict, push, open the PR for issue 1337, merge when green.
- [x] (2026-11-07) Review round 9 found a still-Pending entry kept its identity stale (re-probing healthy siblings every window), and the digest-blind retry window held a fresh edit for up to five minutes after an earlier failure. Pending entries are excluded from the settle check like Failed ones, and the window is now armed per digest — a new current digest bypasses it (`cdbf77bec`). 42 profile_catalog tests and 13 profile_config tests pass, three repeated runs stable, clippy clean; round 10 review requested with the controller suite rerunning.

## Surprises & Discoveries

- Observation: Capability entries live only as long as the daemon process. The in-memory cache (`CHOICES` in `mj-controller/src/controller/profile_config.rs`) is a process-global map, and the `ProfileCatalog` entries are per-daemon objects; nothing loads capability rows from the `profile_config_cache` SQLite table at startup (that table is used today only for fetched provider catalog bodies). A daemon restart therefore re-probes every profile and reads the current home files. The issue's suggestion to "store the home-input digest with each persisted entry" has nothing to persist into: edits made while the daemon is stopped are picked up by construction, and the plan proves it with a two-generation catalog test.
  Evidence: `CHOICES` is a `static LazyLock<Mutex<BTreeMap<ChoiceKey, ProfileConfig>>>` in profile_config.rs with the comment "They live only as long as the daemon, so a restart always discovers again."

- Observation: A catalog re-probe can be swallowed by the global cache. `ProfileCatalog`'s probes call `discover_for`, which reads the `CHOICES` global cache first; a row stored minutes ago is "current" for one hour, so a home-change-triggered re-probe would return the stale row without launching the harness. The time-based expiry refresh (every 5 minutes, entries older than 1 hour) never hits this because its rows are already expired.
  Evidence: `resolve_cached` in profile_config.rs serves any cached row for which `is_current` holds; `CHOICES_LIFETIME` is 60 minutes.

- Observation: The 5-minute refresh loop and the new home watch can share one re-probe path. Both are "re-probe now, keep serving the old answer until a probe succeeds" intents, so both go through `ProfileCatalog::refresh` / `drive_refresh`; only the attempt factory differs (refresh probes bypass the global cache, first probes read it).
  Evidence: `refresh` at profile_catalog.rs selects ready entries, launches attempts recorded in `refreshing`, and `drive_refresh` keeps the previous entry on failure.

## Decision Log

- Decision: Detect home edits with a 30-second stat poll (mtime and size; content hash only when a stat changes) of `config.toml`, `models.json`, and `settings.json` directly under each distinct profile home, run by the catalog owner's background task. No filesystem watcher (works on NFS where watchers may not), no TTL.
  Rationale: This is the detection mechanism the issue recommends; it does all I/O off the read paths, one owner decides, and unrelated homes are untouched by construction.
  Date/Author: 2026-11-06, agent.

- Decision: Re-probe through the existing `refresh` path (readers keep the old choices until a probe succeeds), not through the definition-cancellation path from `sync`.
  Rationale: The issue allows "the existing path"; the refresh path cannot strand readers without models while a probe runs, and it already handles in-flight dedup (`refreshing`) and failure (old entry kept).
  Date/Author: 2026-11-06, agent.

- Decision: Refresh probes bypass the global `CHOICES` cache (`discover_from_config` with `refresh = true`); startup/claim/reread probes keep reading it.
  Rationale: Only a deliberately launched re-probe must re-read the home; the global cache still serves its existing purpose for worker-observed choices on machines with no local worker (`reread_failed`) and for non-catalog `discover` callers. Making every catalog probe bypass would re-launch harnesses on every read-join and would ignore worker observations.
  Date/Author: 2026-11-06, agent.

- Decision: The watch advances its per-home baseline only when a re-probe actually launches for a profile of that home; otherwise the change stays pending and is re-detected on the next tick.
  Rationale: If the edit is detected while the profile's entry is still pending its first probe (startup) or failed, no refresh can launch; advancing the baseline would lose the edit. Re-detecting costs one set comparison per tick.
  Date/Author: 2026-11-06, agent.

- Decision: Do not store a home digest with each cache entry and do not re-probe at startup when a digest differs.
  Rationale: Entries are not persisted across daemon processes, so a restart re-probes every profile and reads current files; there is no stored digest that could go stale. The acceptance rule "an edit made while the daemon is stopped is picked up when it starts" is satisfied by construction and pinned by a regression test.
  Date/Author: 2026-11-06, agent.

- Decision: Detection and settlement are digest-per-identity, recorded at probe success from a digest captured when the probe started. Every attempt reads the home's three catalog inputs before staging; `drive` and `drive_refresh` record `home_settled[identity]` only on success. The watch compares each identity's settled digest against the current files and re-probes mismatches. There is no per-home baseline to advance: a probe that staged the pre-edit home records the pre-edit digest, a failed probe records nothing, and a pending probe records nothing yet, so all three stay pending for the next 30-second pass. This is the issue's "store the home-input digest with each persisted entry", kept in memory because entries already live only as long as the daemon.
  Rationale: The first design advanced a per-home baseline when a re-probe launched; review found it could settle on a failure (silent for up to an hour) or on a still-pending probe that read pre-edit files (restart-and-edit window, shared homes). Recording what a probe actually read removes both races structurally instead of gating the advance.
  Date/Author: 2026-11-07, agent. Superseded the same day: the digest is now per entry, not per identity (round 3), and is recorded only when the harness actually ran, not on a cache-served answer (round 4); a failed re-probe re-arms the retry window, and a success discharges it only when the identity rests at one digest (rounds 6-7). Failed entries are excluded from the settle check (round 5).

- Decision: A failed edit re-probe is retried after `HOME_EDIT_RETRY_WAIT` (5 minutes), matching the expiry refresh cadence, instead of every pass.
  Rationale: Review accepted settling-on-success only with bounded retries; every-pass retries would violate this file's rule that persistent failures (missing login, full disk) are retried rarely rather than every minute.
  Date/Author: 2026-11-07, agent.

- Decision: An input whose mtime is within `RECENT_EDIT_WINDOW` (60 seconds) of now is re-hashed on every pass even when its stat matches the stored one.
  Rationale: Filesystems with one-second mtime granularity (the NFS case the poll was chosen for) can hide a same-length rewrite inside an unchanged (mtime, size) pair; the recency read catches it on the next pass at the cost of hashing recently-written files for a minute. A rewrite with a restored old mtime (cp -p) remains undetected until some other change; reading every file every pass is the only complete fix and was rejected with b39c0e69c.
  Date/Author: 2026-11-07, agent.

- Decision: The watch re-hashes a home in full when the stat poll saw no change yet a settled digest disagrees with the polled digest.
  Rationale: A probe reads the whole home while the poll may reuse a hash taken before a timestamp-preserving rewrite (`cp -p`) hid the edit from the stat; settled and polled digests can then disagree on every pass and churn an unbounded re-probe, because the stat never changes for the poll to catch up. The disagreement proves the poll is behind — a full read and a stat-gated hash can only differ if the content changed after the hash was taken — so one full read reconciles the home and the identity converges. Steady state and normal detection passes still read no files.
  Date/Author: 2026-11-07, agent.

- Decision: The watch does not settle a detected change while any entry selected for it is already re-probing (`refresh` reports those keys as in-flight); the home's baseline advances only once re-probes launched for it and nothing about it is in flight.
  Rationale: A probe already running when the edit lands staged the pre-edit home and will publish that answer; advancing the baseline against it would re-introduce the old state and leave it until the one-hour expiry refresh. Re-detecting on the next pass costs one tick.
  Date/Author: 2026-11-06, agent. Superseded on 2026-11-07 by the settle-on-success digest design above, which handles the same race structurally; the in-flight regression test is kept.

- Decision: Replace `harness_home_files_do_not_invalidate_configured_capabilities` with new tests rather than keep it.
  Rationale: The issue's acceptance criteria say it is replaced by tests for the new rules. The property it guarded (the cache key stays definition-only and does no home-file I/O on read paths) remains guarded by `discovery_fingerprint_ignores_creation_defaults_and_unrelated_profile_preferences`, `setup_token_rotation_preserves_configured_capabilities`, and the b39c0e69c concurrency tests, all of which must keep passing unchanged.
  Date/Author: 2026-11-06, agent.

## Outcomes & Retrospective

Not yet complete.

## Context and Orientation

Mjolnir is a Rust workspace; the daemon is the long-lived controller process (`mj-controller`). It discovers which models and effort levels each configured harness profile offers by staging the profile's home directory into a private directory and launching a throwaway worker probe (`probe_profile` in `mj-controller/src/controller/profile_config.rs`). Probe results are cached in two places: a process-global map `CHOICES` keyed by (profile id, model, definition fingerprint), and the daemon-owned `ProfileCatalog` in `mj-controller/src/server_runtime/profile_catalog.rs`, which is the single owner that Settings, Move, `mj models`, and reviewer selection read from. The fingerprint (`HarnessProfile::capabilities_key` in `mj-core/src/config/harness.rs`) covers only definition fields: harness kind, home path, environment sources, the Codex profile overlay, the managed install pin, and inherited environment names. It deliberately does not cover what the home contains — that was the b39c0e69c design, which fixed #1028's re-probe storms but removed the only way a home edit reached the catalog.

A profile's home is the directory the harness keeps its files in; for the three catalog inputs the files sit directly in it (`config.toml` names the provider and default model for Codex/Kimi/Grok; `settings.json` carries Claude/Muse defaults; `models.json` is the model catalog Mjolnir merges for a custom Codex provider). Staging copies `home/<name>` verbatim, so polling `home/<name>` watches exactly what a probe will stage.

Key files: `mj-controller/src/server_runtime/profile_catalog.rs` (the owner: entries, definitions, the 5-minute expiry refresh loop — all changes live here except one signature change in profile_config.rs), `mj-controller/src/controller/profile_config.rs` (`discover_for` gains a `refresh` flag; the old home-files fingerprint test is removed), `mj-core/src/config/harness.rs` (unchanged; identity stays definition-only).

## Plan of Work

All work is in `mj-controller`.

In `mj-controller/src/server_runtime/profile_catalog.rs`:

Add near the top with the other constants: `HOME_CATALOG_INPUTS` (the three file names) and `HOME_WATCH_INTERVAL` (30 seconds). Add small private types `FileStat` (optional mtime plus size), `WatchedInput` (stat plus content hash, both `None` when the file is absent), and `HomeFiles` (the three inputs plus a digest). The digest is a SHA-256 over each input's name and either an absent marker or its content hash, computed with the workspace `sha2` crate already available to mj-controller.

Add `Inner.home_watches: BTreeMap<PathBuf, HomeFiles>` mapping each watched home to its last observed catalog-input state. It is owned by the watch tick: the only mutator is the tick (under the catalog lock), and it is not part of the published snapshot, so updates never invalidate the publication.

Add `fn poll_home_files(home: &Path, previous: Option<&HomeFiles>) -> Option<HomeFiles>`. It stats each of the three inputs; a hard I/O error (anything but NotFound) makes it return `None` so the home keeps its previous baseline and is retried next tick. When a file's stat equals the previous tick's stat, its stored content hash is reused; otherwise the file is re-read and re-hashed. The digest is recomputed only when a hash (or presence) changed, or there is no previous state.

Add `fn watch_homes(self: &Arc<Self>)`, one watch tick. Under the lock it collects, for every non-cancelled definition, the profile's home (expanded with `mj_core::path_input::expand_local`) grouped by identity, plus a snapshot of `home_watches`. With the lock released it polls each distinct home. For a home whose baseline digest differs from the current digest it collects the identities that use the home. It then launches re-probes through `Self::refresh` restricted to those identities, and finally, under the lock, stores the polled state for every home whose identities launched a probe plus every unchanged home, prunes homes no longer referenced by any definition, and deliberately keeps the old baseline for changed homes that launched nothing. A home with no baseline is stored without triggering (first sight — the startup generation is already discovering).

Add `fn start_watching_homes(self: &Arc<Self>)`, a background loop mirroring `start_refreshing` (weak catalog reference, daemon cancellation token, missed-tick behavior Delay), and call it from `sync` next to `start_refreshing` on first adoption.

Change `fn refresh` to take a predicate over `(&CacheKey, &ProfileConfig)`, and to return the keys it launched so the tick can advance baselines. The 5-minute loop passes `|_, choices| !is_current(choices)` and ignores the return.

Add `fn attempt_refresh(&self, definition: &Definition, model: Option<String>) -> Attempt` used only by `refresh`; `attempt` keeps serving first probes and read-joins. Both funnel into a private builder that passes a `refresh` flag to `discover_for`.

In `mj-controller/src/controller/profile_config.rs`:

Give `discover_for` a `refresh: bool` parameter forwarded to `discover_from_config`. Delete the test `harness_home_files_do_not_invalidate_configured_capabilities` (replaced by the new tests below). Everything else — the definition-only fingerprint, `resolve_cached`, the global cache — stays exactly as it is.

Tests, all in the `#[cfg(test)]` module of `profile_catalog.rs`, using a fixture probe that reads `model = "..."` out of the profile home's `config.toml` the way the real probe reads the staged copy (fixture choices keep the ancient `observed_at` of `test_choices`, which stands in for the empty process cache a restarted daemon has, so every fixture generation genuinely re-probes):

An edited home re-publishes within one tick and re-probes only that home's profiles. Two profiles with distinct tempdir homes; sync; edit one home; one tick; the edited profile converges to the new model while the other profile's probe count stays at one. A first tick with no edits launches nothing (baselining).

A home shared by two profiles re-probes each of them and no others.

A new catalog generation discovers current home files (the stopped-daemon rule): settle one catalog, edit the home, settle a fresh catalog over the same configuration, and the fresh catalog reports the new model.

The poll helper: writing a file changes the digest; rewriting identical content (new mtime, same bytes) does not; changing content does; deleting does.

The pre-existing tests `concurrent_readers_join_startup_hydration`, `profile_definition_changes_do_not_retire_unrelated_reads_or_publish_old_choices`, `pending_model_efforts_share_one_attempt_and_unchanged_profiles_stay_warm`, and `expired_entries_are_probed_again_and_kept_until_a_probe_succeeds` must pass unchanged: they are the b39c0e69c constraints (shared attempts, no unrelated-profile invalidation, keep-old-on-failure).

## Concrete Steps

Working directory: the repository root. Build and test with the normal Cargo storage (no redirection into /tmp); run cargo test outside the restricted sandbox with elevated permissions per AGENTS.md.

Implement the plan. Then run, from the repository root:

    cargo fmt --all
    cargo test -p brokk-mj-controller profile_catalog
    cargo test -p brokk-mj-controller profile_config
    cargo clippy --all-targets -- -D warnings
    git diff --check

Expect the two focused test runs to pass including the new tests, clippy to report no warnings, and no whitespace errors. Then run the full workspace suite once before delivery:

    cargo test

Expect every workspace test to pass. Stage only the changed files (never `git add -A`), commit on the current branch, request review of the commit, address the findings, push the branch, open the pull request referencing issue 1337, and merge when CI is green.

## Validation and Acceptance

Behavioral acceptance mirrors the issue:

After an edit to `model =` in a Codex profile home, the catalog reports the new model within one 30-second poll interval without a daemon restart, proven by the re-publish test driving `watch_homes` directly.

An edit made while the daemon is stopped is picked up when it starts, proven by the two-generation test (a fresh process has an empty global cache, so its first discovery probes; the fixture's expired timestamps reproduce that condition inside one test process).

Editing one home re-probes every profile that uses that home and no others, proven by the distinct-homes and shared-home tests counting probe calls per profile.

Catalog reads do no home-file I/O: the watch tick is the only code path that stats or reads home files, and the b39c0e69c concurrency tests pass unchanged.

`harness_home_files_do_not_invalidate_configured_capabilities` is gone; its replacement tests encode the rules above.

## Idempotence and Recovery

All tests use temporary directories and the in-process catalog fixture; no live or default instance is touched. The change adds no database migration, no wire-protocol change, and no persisted state; aborting the work only leaves an uncommitted branch. The background loops exit on daemon cancellation and hold no resources beyond a 30-second timer.

## Artifacts and Notes

Branch: agent/profile-home-watch. Issue: 1337. No release implications.

## Interfaces and Dependencies

No new dependencies; mj-controller already depends on workspace `sha2`. No new public API: the watch is private to `ProfileCatalog`. The only cross-module signature change is `pub(crate) fn discover_for(config: Config, id: String, model: Option<String>, cancellation: CancellationToken, refresh: bool)` in `mj-controller/src/controller/profile_config.rs`, with exactly one caller (the catalog's attempt builder). `ProfileConfig`, `capabilities_key`, the global cache shape, and the SQLite schema are unchanged.
