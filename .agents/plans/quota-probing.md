# Probe quota once, only when stale, and honour rate limits

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir shows each agent profile's subscription quota (how much of the 5-hour and weekly allowance is left) in the dashboard's Profiles pane, on the web quota page, and in a few internal decisions (which profile a sub-agent or a utility model uses, when a session that ran out of quota may resume). Today several processes ask the provider's usage endpoint for the same fact, each on its own schedule. The daemon (`mj daemon-run`) polls every profile every ten minutes. Every dashboard process (`mj` with no arguments) also polls every profile, at its own start, every ten minutes, on Refresh, on a profile rename, and when the profile set changes. Every probe covers every profile. On 2026-09-29 the Claude endpoint answered HTTP 429 (too many requests) all day, the Profiles pane showed `unavailable`, and the web page showed something different, because two pollers had asked at different times (finding I1-10).

After this change three things are true, and each can be seen.

One process probes. Only the daemon asks a provider for quota. The dashboard reads the daemon's report, so the Profiles pane and the web quota page always agree. The explicit Refresh (`prefix+shift+r`, the Profiles menu's Refresh, the web page's refresh button) asks the daemon to probe. To see it: start two dashboards on one instance and count usage-endpoint requests in the daemon log; there is one poll cycle, not three.

A probe happens only when the last report is stale. At daemon start, and after a configuration reload, a profile whose stored report is younger than the poll interval (ten minutes) is published as it is and its next probe is scheduled at `refreshed_at + interval`. To see it: restart the daemon twice within ten minutes; the second start sends no usage request for a profile that the first start read.

A 429 is respected. When a provider answers 429 (or sends a `Retry-After` header), that profile is not probed again until the given time, or for 15 minutes when no time is given. The last good report stays published with its age. The row reads `rate limited · retry in N min` in the dashboard and on the web page, not `unavailable`, and the daemon's information log line names the hold.

## Progress

- [x] (2026-09-29) Read AGENTS.md, PLANS.md, the campaign runbook's quota paragraphs, F23's commit `ea443194`, and the quota code named under Context.
- [x] (2026-09-29) Wrote this plan.
- [x] (2026-09-29) Commit (a): one prober in the daemon. Tests: `a_quota_change_travels_as_a_metadata_delta`, `metadata_from_a_daemon_that_published_no_quota_decodes_with_an_empty_snapshot` (mj-client); `a_quota_refresh_request_wakes_the_daemons_poller_and_fails_without_one`, `published_quota_reaches_an_attached_client_through_the_runtime_feed` (daemon); `the_profiles_pane_shows_the_daemons_report_and_probing_set_and_nothing_older` (mj-tui); `a_manual_quota_refresh_completes_when_the_daemon_finishes_a_later_cycle` (pollers). The dashboard's `quota_batch_asks_about` and its test were removed; the profile-set-changed probe is the daemon's (covered again in commit b).
- [ ] Commit (b): probe only when stale. Failing tests first: a fresh cached report is published without a probe and the next probe is scheduled at `refreshed_at + interval`; a stale one is probed; a reload keeps fresh reports.
- [ ] Commit (c): honour 429. Failing tests first: a 429 (with and without `Retry-After`) produces a hold; a held profile is not probed by the schedule or by Refresh; the last good report survives with the hold; TUI and web render `rate limited · retry in N min`.
- [ ] Final validation: `cargo test -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui -p brokk-mj-client`, web unit tests, clippy, fmt.

## Surprises & Discoveries

- Observation: the dashboard and the daemon run the same function, `spawn_quota_refresher` in `mj-controller/src/pollers/quota.rs`; the daemon's copy lives in `Policy::new` in `mj-controller/src/daemon/delegation/policy.rs`.
  Evidence: `grep -rn spawn_quota_refresher --include=*.rs` lists `mj-cli/src/dashboard.rs:1535` and `policy.rs:61`.
- Observation: the `quota_reset_cache` store row already holds a whole `ProfileQuota` as JSON (`database/quota_cache.rs` serializes the merged report into `body`), not only the reset times. No schema change is needed for commit (b).
  Evidence: `save()` in `mj-controller/src/database/quota_cache.rs`.
- Observation: a report with an error is never written to that row, so the row is always the last good report.
  Evidence: `if report.error.is_some() { return Ok(()); }` in `save()`.

## Decision Log

- Decision: The dashboard receives the daemon's quota through the existing runtime feed (`RuntimeMetadata` in `mj-client/src/runtime_feed.rs`), as a new field `quotas` of type `QuotaSnapshot` with `#[serde(default)]`; the explicit Refresh is a new `DaemonAction::RefreshQuota`; `PROTOCOL_VERSION` moves from 42 to 43.
  Rationale: the feed is how the dashboard already reads every other daemon-owned fact (config, reviews, notices), and c8d27f3c moved credential sync to the daemon the same way. The web page's `ControllerAction::RefreshQuota` is an HTTP action of the web server and is not reachable from the terminal client; a daemon action is the terminal client's equivalent. A new action variant is a protocol change, so the version moves.
  Date/Author: 2026-09-29, fix agent Q1.
- Decision: `QuotaSnapshot` carries the reports, the set of profiles being probed right now, and a counter of finished probe cycles.
  Rationale: the dashboard's "refreshing…" cell and its "Targets and quotas refreshed." notice both need to know when the daemon is busy and when a refresh ended. A counter is enough for the notice: the dashboard remembers the counter when it asks and completes the notice when the counter is larger. A cycle already in flight can end first and complete the notice a few seconds early; this is cosmetic and accepted.
  Date/Author: 2026-09-29, fix agent Q1.
- Decision: The daemon does not persist holds or the probing state. Only the existing `quota_reset_cache` row (last good report) survives a restart.
  Rationale: the repository owner's rule is simplicity over durability; a cache is fine because it is rebuildable. A daemon restart during a 429 hold loses the hold and costs one probe per profile, which either succeeds or starts a new hold.
  Date/Author: 2026-09-29, fix agent Q1.
- Decision: The explicit Refresh respects a hold. A held profile is skipped and its row keeps reading `rate limited · retry in N min`.
  Rationale: probing an endpoint that just said 429 makes the limit last longer, and the row already tells the person the answer. Refresh still probes every profile that is not held.
  Date/Author: 2026-09-29, fix agent Q1.
- Decision: The utility model's ranking (`utility_llm.rs`) reads the stored report before it probes; the quota auto-resume probe (`daemon/continuation/quota.rs`) is unchanged.
  Rationale: the instructions say to touch these only to make them read the same reports. The auto-resume already reads the stored row; the utility ranking now does the same and probes only when there is no report younger than its own freshness limit.
  Date/Author: 2026-09-29, fix agent Q1.

## Outcomes & Retrospective

(To be written when the three commits have landed.)

## Context and Orientation

A profile is one configured agent login (`[profiles.<id>]` in `config.toml`), for example a Claude account or a Codex account. Its quota is what the provider says is left. `mj-client/src/quota.rs` defines `ProfileQuota` (one profile's report: `windows`, `error`, `refreshed_at_epoch_seconds`, ...) and `QuotaWindow`. `mj-controller/src/quota.rs` defines `QuotaManager`, which probes profiles (`refresh_profiles`) and keeps the latest report per profile in memory, and `QuotaRefreshRequest`, which describes one profile to probe. The Claude probe is `mj-controller/src/claude_usage.rs` (`query_api` sends the HTTP request; its unit tests have a fake usage server `spawn_usage_server`).

The daemon is the long-running process (`mj daemon-run`) that owns sessions and the database. Its quota poller is `spawn_quota_refresher` in `mj-controller/src/pollers/quota.rs`, started by `Policy::new` in `mj-controller/src/daemon/delegation/policy.rs`. `Policy` keeps the daemon's reports (a shared map that the web server also reads), receives `QuotaUpdate` messages (`Refreshing`, `Report`, `Finished`, defined in `mj-controller/src/pollers/types.rs`), and republishes the profile list to the poller whenever the configuration's profiles change (`republish_quota_profiles` in `mj-controller/src/server_runtime/actions.rs`). `QUOTA_REFRESH_INTERVAL` (ten minutes) and `QUOTA_STALE_AFTER` (twice that) are in `mj-controller/src/pollers.rs`.

The dashboard is the terminal UI process (`mj`, code in `mj-cli/src/dashboard.rs` and `mj-cli/src/dashboard/`). It reads the daemon's state through a "runtime feed": the daemon builds a `RuntimeProjection` (`mj-client/src/runtime_feed.rs`, `RuntimeMetadata` holds config, reviews, notices) in `RuntimeState::capture_runtime` (`mj-controller/src/daemon/feed.rs`), and the dashboard's `spawn_remote_dashboard_worker_poller` (`mj-controller/src/pollers/remote.rs`) turns it into watch channels that `DashboardContext` drains (`mj-cli/src/dashboard/drains.rs`). The Profiles pane is drawn by `mj-tui/src/render/quotas.rs` from `DashboardState::quotas` and `quota_refreshing` (`mj-tui/src/ingest.rs`).

The web quota page is `renderQuota` in `mj-controller/src/web/viewer.js`. The server builds each profile's `ViewerQuota` in `mj-controller/src/server_runtime/snapshot.rs` from the daemon's reports. Its unit tests are `tests/e2e/web/viewer.quota.unit.test.mjs` (run with `node --test` in `tests/e2e/web`).

The store row is `quota_reset_cache(identity, body)` (`mj-controller/src/database/quota_cache.rs`); `identity` is `QuotaRefreshRequest::cache_identity()`, a hash of profile id, harness, environment, home, and provider key. `QuotaManager`'s refresh saves each good report there when the process has the database writer (the daemon does; the dashboard does not). Only quota recovery reads it today (`daemon/continuation/quota.rs`).

Terms: a "hold" is a time before which a profile must not be probed. A "cycle" is one pass of the poller over the profiles that are due.

## Plan of Work

Commit (a). In `mj-client/src/quota.rs` add `QuotaSnapshot { reports, probing, cycles }`. Add `quotas: QuotaSnapshot` (`#[serde(default)]`) to `RuntimeMetadata`. Add `DaemonAction::RefreshQuota` and `DaemonClient::refresh_quota`, and bump `PROTOCOL_VERSION` to 43. In `RuntimeState` (`mj-controller/src/daemon.rs`, `daemon/state.rs`) add a small quota board: the current `QuotaSnapshot` plus the sender that wakes the poller; `capture_runtime` copies the snapshot into the metadata, and `publish_quotas` stores a changed snapshot and publishes a revision so attached dashboards wake. `Policy` attaches its refresh sender at construction and publishes the snapshot on every `QuotaUpdate` and whenever it prunes reports for removed profiles. The `RefreshQuota` action handler in `daemon/actions.rs` asks the board to wake the poller. `spawn_remote_dashboard_worker_poller` exposes the snapshot as a watch channel. In the dashboard, delete the local `spawn_quota_refresher` use, `quota_profiles_tx`, `quota_batch_asks_about`, `refresh_quotas_if_profiles_changed` and the startup, rename and lifecycle refresh calls; `DashboardContext::quota` becomes a feed of the snapshot; `request_quota_refresh` sends `RefreshQuota` to the daemon on a background task and reports failure as a notice; `manual_quota_refresh` remembers the cycle counter and completes when the counter grows. `DashboardState` gets `set_quota_snapshot` that replaces `quotas` and `quota_refreshing` with the daemon's.

Commit (b). Replace the "probe everything on every batch and every tick" loop in `spawn_quota_refresher` (now used only by the daemon) with a per-profile schedule. `QuotaManager` gains `seed(report)` and `next_probe_at(report)`; the poller keeps `next_probe: profile -> epoch seconds` and sleeps until the earliest one. For each new profile (or one whose `cache_identity` changed) the poller loads the stored report through an injected loader (the daemon injects `database::load_quota_cache`), seeds the manager with it when its `refreshed_at` is younger than `QUOTA_REFRESH_INTERVAL`, publishes it, and schedules the probe at `refreshed_at + interval`; otherwise the profile is due now. A configuration reload that leaves a profile's identity unchanged keeps its report and schedule. The explicit Refresh probes every profile.

Commit (c). `ClaudeUsageError::RateLimited { retry_after }` is produced by `query_api` for HTTP 429, reading `Retry-After` (seconds, or an HTTP date). Kimi's HTTP status check does the same for 429. `refresh_profile` turns it into a report whose `rate_limited_until_epoch_seconds` is set (new optional field of `ProfileQuota`, default absent) and whose `error` is `rate limited`. `QuotaManager::refresh_profiles`, when it gets such a report, keeps the previous good report (windows, `refreshed_at`) and attaches the hold to it; with no previous good report it publishes the error report with the hold. The default hold is 15 minutes. `next_probe_at` is the later of `refreshed_at + interval` and the hold, and a held profile is skipped by the schedule and by Refresh. The log line names the hold. `mj-tui/src/render/quotas.rs` and `viewer.js` show `rate limited · retry in N min` for a held profile (the last good numbers stay on the row when there are any; the status text carries the hold). `ViewerQuota` gets `rate_limited_until_epoch_seconds`.

The utility ranking change (see the Decision Log) goes with commit (b), because it reads the row that (b) keeps fresh.

## Concrete Steps

All commands run from the repository root (the worktree). Tests run outside any sandbox.

    cargo test -p brokk-mj-client
    cargo test -p brokk-mj-controller quota
    cargo test -p brokk-mj-controller daemon::
    cargo test -p brokk-mj-tui quota
    cargo test -p brokk-mjolnir dashboard
    (cd tests/e2e/web && node --test viewer.quota.unit.test.mjs)

Each layer's new tests are written first and run to see them fail, then the implementation makes them pass. After each commit run the full validation below.

## Validation and Acceptance

Run and expect success:

    cargo test -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui -p brokk-mj-client
    (cd tests/e2e/web && node --test *.unit.test.mjs)
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Behavior, by commit. (a): a dashboard built from this tree has no quota poller of its own; `grep -rn spawn_quota_refresher mj-cli` finds nothing; a `DashboardState` given a `QuotaSnapshot` with `probing = {claude}` draws `refreshing…` for that row only. (b): the `QuotaManager` poller test with a fake usage server counts requests: zero for a profile whose seeded report is one minute old, one for a report eleven minutes old. (c): the fake usage server answers 429 with `Retry-After: 120`; the report keeps the earlier windows, carries a hold about two minutes away, the next probe is not scheduled before it, and the rendered row reads `rate limited · retry in 2 min`.

A real-endpoint look, read only: with a fake lab instance whose `[profiles.nativeclaude]` points at `/home/jonathan/.claude`, run one probe and read the log; do not loop it, because the endpoint is rate limited. Never run `mj` against the default instance.

## Idempotence and Recovery

Every step is a source edit plus tests, so it can be repeated. The stored row is unchanged in shape, so an older daemon reads what a newer one wrote. A newer daemon reads rows written by an older one (they hold the same `ProfileQuota` JSON; the new `rate_limited_until_epoch_seconds` field has a serde default and is skipped when absent). The protocol version moves once, in commit (a); a dashboard and a daemon of different versions refuse each other as they do for any protocol change, and the ordinary upgrade handoff replaces the daemon. To roll back, revert the three commits in reverse order.

## Artifacts and Notes

(To be filled with short test transcripts as the commits land.)

## Interfaces and Dependencies

In `mj-client/src/quota.rs`:

    pub struct QuotaSnapshot {
        pub reports: BTreeMap<String, ProfileQuota>,
        pub probing: BTreeSet<String>,
        pub cycles: u64,
    }

In `mj-client/src/daemon.rs`: `DaemonAction::RefreshQuota` and `DaemonClient::refresh_quota(&mut self) -> Result<()>`.

In `mj-controller/src/quota.rs` (commits b and c): `QuotaManager::seed`, `QuotaManager::next_probe_at`, `QuotaManager::is_held`, `DEFAULT_RATE_LIMIT_HOLD` (15 minutes). In `mj-client/src/quota.rs` (commit c): `ProfileQuota::rate_limited_until_epoch_seconds: Option<u64>` and a display helper that returns `rate limited · retry in N min`.

## Revision notes

- 2026-09-29: first version of this plan (fix agent Q1).
