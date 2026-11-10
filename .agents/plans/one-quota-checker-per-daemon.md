# Give each daemon one quota checker

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture


Quota checks are repeatedly being rate limited. The user expects one checker per daemon (the Mjolnir background server). Three independent production paths currently construct managers that contact providers, so ordinary background polling, utility-model selection and automatic continuation can duplicate requests and ignore one another's provider-requested waiting periods. Issue #1487 tracks consolidating those paths under the existing daemon checker.

## Progress


- [x] (2026-11-10) Claim #1487 and delegate production-owner mapping and live rate-limit evidence separately.
- [x] (2026-11-10) Identify background, utility and continuation manager constructors; utility excludes Claude but can duplicate OpenAI probes.
- [x] (2026-11-10) Finish call graph: three manager construction sites; no independent UI/setup checker. Recovery bypasses the poller hold, utility excludes Claude, and web per-profile refresh currently refreshes every profile.
- [x] (2026-11-10) Settle a smaller correction: recovery reads full existing owner reports plus saved resets; utility reads persisted owner reports. Neither requests or performs a new probe.
- [x] (2026-11-10) Remove continuation manager/lanes/cache and consume the existing full owner report plus stored reset evidence; add scoped/stale-report recovery regression.
- [x] (2026-11-10) Finish utility persisted-report reader and missing/stale-cache subprocess regression.
- [ ] Run focused utility/recovery tests, one full controller suite, workspace clippy and formatting; review in parallel as previously requested.
- [ ] Validate, review, commit, push and install this follow-up to the same credential/quota line of work.

## Surprises & Discoveries


`mj-controller/src/utility_llm.rs::UtilityLlmService::quotas` reads a separate in-memory cache and successful stored reports, then constructs another `QuotaManager` when missing or older than twenty minutes. Concurrent callers can independently observe that missing entry. `mj-controller/src/daemon/continuation/quota.rs::Service::refresh` constructs a manager behind a separate thirty-second cache. Neither is the background poller's in-flight owner or rate-limit hold.

One Claude provider poll is a GET to `/api/oauth/usage?cedar_ember=1`. When paid credits are enabled it also reads `/api/oauth/profile` and the organization's wallet. Those are three HTTP requests within one provider probe, distinct from three independent quota-checking owners. A rejected access token can add refresh and a repeated usage read; a 429 is returned to scheduling without an immediate retry in the Claude helper. Do not assert undocumented provider limits or infer unlogged request counts from warning counts.

## Decision Log


Keep one checker per daemon, as the user requested. Reuse the existing quota snapshot and refresh-request ownership. Do not introduce a host-wide leader, a parallel cache, a guessed rate cap or a fallback provider probe. The checker alone decides whether a probe is due, held after a provider rejection, or already in progress. Other consumers read its published state and, when fresh data is required, request work from that owner.

Keep provider I/O off UI and request event loops. A caller waiting for a shared probe must not own its lifetime: cancellation must not cancel another caller's accepted request. Independent profiles remain concurrent. Multiple daemon instances remain independent.

## Outcomes & Retrospective


The duplicate constructors are confirmed. Both source slices are implemented; focused validation is starting. There is now one production QuotaManager construction site, in the daemon poller. Recent logs establish quota/rate-limit warnings but lack per-request traces, so no exact provider request count was measured. The prior shared-login fix is already installed at `918080749` across six daemons. Its cross-process token locks address refresh ownership, not duplicate quota scheduling.

## Context and Orientation


`mj-controller/src/pollers/quota.rs::spawn_quota_refresher` owns the regular provider checker and its due/hold decisions. `mj-controller/src/quota.rs::QuotaManager` retains reports and persistent Codex app-server clients, and invokes provider probes. The daemon publishes quota snapshots through its runtime feed and accepts manual refresh requests through an existing sender. The exact handle wiring will be recorded after mapping.

`mj-controller/src/utility_llm.rs` chooses models for internal LLM work. `mj-controller/src/daemon/continuation/quota.rs` decides quota recovery after a session exhausts allowance. Their independent managers must be removed. Utility also runs outside daemon startup, so it reads the instance store rather than depending on an attached runtime feed. Missing or older-than-twenty-minute utility reports remain unknown, preserving the existing freshness threshold without probing elsewhere. Recovery needs full model-scoped reports: the RuntimeState surface snapshot strips some scopes and must not be used as its data source.

## Plan of Work


Mapping is complete. Add a read accessor on ApiBackend that clones a profile report under the existing reports Arc mutex, already shared with the poller policy and profile catalog. Recovery reads that full report, supplements missing reset times from the existing identity-keyed reset cache and current provider diagnostic, and truthfully marks retained data as cached. Delete its Service, per-profile lanes and thirty-second cache. Utility deletes its quota cache and transient manager; it reads existing identity-keyed persisted reports on a blocking task and retains its existing twenty-minute selection freshness rule. No new actor, waiter, generation or request queue is needed because these consumers already support unknown/retained evidence. The ordinary daemon poller alone keeps provider data current.

Add or extend integration and ordering regressions showing these consumers use the published or stored report and never launch a provider probe on a cache miss, stale result or recovery event. Preserve scoped recovery/reset behavior and current-message reset evidence. Reuse existing fake subprocess, database and daemon report seams. Do not add ordinary happy-path unit examples. Manual refresh coalescing and its existing all-profile web behavior are observed separate issues, not new automatic checker owners; do not expand this correction into request scheduling redesign.

## Concrete Steps


Work on the current branch and preserve normal mbx-managed Cargo storage. Every Cargo test invocation runs elevated. During development run only affected/new/failed filters, using a stable touched-package selection. After the last implementation round run full suites for touched crates and `cargo clippy --all-targets -- -D warnings` once in the dev profile. Run formatting and diff checks. Use stable package selection `-p brokk-mj-controller` for this controller-only change; filters will be recorded in implementation handbacks. No core/worker source change is planned.

Commit only task-owned files, request repository push review and push to `origin/master` after acceptance. If upstream moved, merge rather than rebase. The user's install authorization for this credential/quota line of work persists; after delivery use `scripts/install.sh` from a frozen checkout and verify the active daemon handoffs. Do not edit, commit or merge while installation builds run.

## Validation and Acceptance


There must be one production owner of provider probes in each daemon. Utility selection and automatic continuation must share its reports and provider-requested waiting period. Concurrent readers must use the owner result rather than create another manager. No extra provider work is requested by utility selection or recovery. UI clients read the daemon result. A missing report cannot silently re-enable an independent provider query. Live logs may corroborate fewer duplicate probes but do not by themselves establish a provider's exact quota limit.

## Idempotence and Recovery


Diagnosis is read-only against live state: inspect whitelisted metadata and existing daemon traces, never print bearer tokens, credential files or environment secrets. Tests use synthetic credentials, isolated directories and named instances. Installation uses normal daemon handoff and idle-only worker replacement, preserving active turns.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1487. Agent reports are under `.mj/agents/`; this plan must retain decisive findings and validation evidence without relying on temporary raw logs.

## Interfaces and Dependencies


Reuse the daemon's published `QuotaSnapshot`, existing refresh sender, watch channels and current request supervision. No new crate, database schema, protocol or external dependency is planned. Expose `ApiBackend::quota_report(&self, profile_id: &str) -> Result<Option<ProfileQuota>>` as an internal accessor over its existing shared full-report authority. Keep the utility reader on the existing `database::load_quota_cache` API; no new persistence or attached-process cache is added.

Initial revision (2026-11-10): record the confirmed duplicate owners, request-count distinction and single-owner correction requirements before implementation.

Design revision (2026-11-10): choose read-only consumers over a refresh-and-await service. Unknown/retained data is already supported, and adding another asynchronous refresh protocol would add state while allowing event traffic to drive extra probes. Preserve full scoped reports for recovery. Delegate utility and recovery in disjoint file slices; another read-only audit now checks which of the six daemons actually carry work, as the user requested.

Activity audit revision (2026-11-10): s48-ds8 and s48-opus have no live lifecycle sessions/workers, clients, queued/unaccepted work, goals, continuations, tasks or restart intents; their rows are suspended history. Other campaign daemons are idle but retain workers (dsh2, judge10, s49 eighteen plus a restart intent). Default is actively busy. The user asked for this inventory; no daemon stop is authorized or performed by that question.

Implementation revision (2026-11-10): remove both extra managers and their caches. Utility constructs cache identity and reads stored reports in parallel blocking tasks; missing/stale data stays Unknown. Recovery reads the existing full report authority and saved reset evidence. Added isolated subprocess/database regressions for both boundaries. Source is frozen for validation; start repository review concurrently with tests under the user's standing request.
