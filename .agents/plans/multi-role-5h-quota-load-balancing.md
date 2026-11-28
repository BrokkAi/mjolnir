# Multi-Role, 5h-Aware Supply-Demand Quota Load Balancing and Burn-Rate Visibility

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

After this change, Mjolnir's automatic profile load balancer coordinates quota across all configured roles (`primary`, `straightforward`, `subtle`, `oracle`, mandatory `all-of` `[review].reviewers`, and `[review].brief_writer`) and across both 5-hour and weekly quota windows without manual per-profile overrides. Specifically:

1. **Retire conflicting manual pins (`[quota] prefer`):** Remove the strict lexicographic `[quota] prefer` override from scheduling and TUI controls while preserving backward-compatible config deserialization (including `RETIRED_KEYS` so in-place TOML saves strip it) and a no-op HTTP 200 handler on `POST /api/v1/terminal/quota/preferences`.
2. **Fix immediate scheduler defects, unify paid tier strictly, and add decision observability (Stage 1):**
   - Fix `calibrated_prior_scale` so cross-model calibration weights samples equally in benchmark-task units (`sum(obs_usd / bench) / sum(n)`) rather than letting 1 Kimi sample (`$24.25/bench`) outweigh 53 DeepSeek samples (`$0.456/bench`).
   - Fix the post-reset `t_left_hours == 0.0` blind spot with cycle alignment across repeatedly elapsed resets, zero expired-window velocity, inclusive post-reset reservation cutoff, and a 90-minute fallback clamp when `second_to_last` is absent.
   - Reorder tiers cleanly: Safe subscription (`0`) > `Unknown` (`1`) > Unified Paid tier (`2`, combining allowed `Paid` overage credits and `UsagePriced` API ranked **strictly** cheapest-first by real per-task `est_usd`, not probabilistic `1/est_usd`) > 5h-unsafe subscription (`3`) > Weekly-unsafe / sub-reserve subscription (`4`).
   - Record every placement's winning tier, reason, and binding metric (structured tracing + in-memory per-reason paid-spill counters) and fix reviewer reservation `session_id` commit deduplication where feasible.
   - Compute per-profile quota burn-rate projections **once at telemetry refresh** in `QuotaTelemetrySnapshot` (using 24h/7d `quota_history` for weekly burn rate — never extrapolating 60-minute velocity over days — and reporting unknown supply as unknown), and surface them consistently in `GET /api/v1/usage/report` -> TUI "Usage by session" and in `mj quota`.
3. **Production-fidelity simulation before Milestone 3 (Stage 2):** Before shipping cross-role weekly reserves and 5h admission, re-evaluate both mechanisms under production information (10-minute polled quota and estimated mean-cost reservations for every policy, zero oracle leakage), replayed against real placement traces (25,312 real placements with bursty VMR=46 correlated arrivals), live multi-account topology (6 Codex + 7 Claude + 2 Gemini + Grok + Kimi + DSH with staggered resets and `straightforward` overlapping reviewers on Codex), biased cost estimates, volume-over-horizon-with-refills LP (verified against `scipy.optimize.linprog`), rolling-window envelope reserve vs fixed `k_safe`, and 5h volume admission vs exponential-decay rate limiting. Ship cross-role reserve and 5h admission as separate, Advisor-reviewed milestones (Stage 3).

## Progress

- [x] Stage 1 and transient 429 feedback: Retry-After seconds and HTTP dates, prose classification, and immediate deduplicated fault-driven quota probes respecting provider holds.

- [x] (2026-11-27 20:47Z) Mapped existing profile scheduler (`mj-accounts/src/profile_scheduler.rs`), quota telemetry (`mj-accounts/src/quota_telemetry.rs`), `/power-ranking.json` integration (`mj-accounts/src/power_ranking.rs`), `~/Projects/bifrost-service` benchmark tables, quota providers, role configs, "Usage by session" surfaces, quota auto-move, and `[quota] prefer` blast radius.
- [x] (2026-11-27 21:09Z) Completed single-role 5h pacing Monte Carlo simulation (`.mj/agents/a55b8944673d8856bb175f75a50c3ef6/quota_lb_results.md`).
- [x] (2026-11-27 21:25Z) Aligned with user on design decisions: skip cross-provider auto-move, retire `[quota] prefer`, keep `Unknown` ahead of paid, unify paid overage credits with usage-priced API by real per-task USD cost, and protect irreplaceable sub-only roles.
- [x] (2026-11-27 22:28Z) Completed initial multi-role Monte Carlo sweep (`.mj/agents/be19347fe2d634324dd69b7aef746f37/fixed_mc.md`) and sent ExecPlan v1 to MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`).
- [x] (2026-11-27 22:46Z) Received MJ Advisor review; fetched Advisor's audit (`findings.md`), code review (`quota-plan-code-review.md`), and real 25,312-placement arrival trace (`scratch.sqlite3`); revised plan into Stage 1 (Milestones 1–2 + tier reorder + placement reason + burn-rate projections), Stage 2 (production-fidelity real-trace simulation + SQL benchmark), and Stage 3 (Milestone 3 cross-role reserve and 5h admission after Advisor sign-off).
- [x] (2026-11-27 22:58Z) Stage 1 / Milestone 1: Retired `[quota] prefer` across config (`RETIRED_KEYS` + `discard_legacy_setting`), scheduler, server, daemon HTTP 200 no-op, TUI, CLI dashboard, and docs/skill refs; targeted tests and all-targets clippy `-D warnings` passed.
- [x] (2026-11-27 23:30Z) Stage 1 / Milestone 2: Fixed `calibrated_prior_scale`, fixed post-reset multi-cycle alignment + inclusive cutoff, reordered tiers with strict cheapest-first Unified Paid tier, added placement decision reason telemetry + paid-spill counters, fixed reviewer reservation `session_id` commit dedup, and added unified refresh-time burn-rate projections to `/api/v1/usage/report`, TUI "Usage by session", and `mj quota`.
- [x] (2026-11-27 23:35Z) Stage 2: Completed polled real-trace simulation + SQLite benchmark (`.mj/agents/915d7fde717e2fae0af46e87ac627305/prod_fidelity_report.md`, KKT residual `<= 9.98e-11`) and received MJ Advisor sign-off: ship Stage 1 now and hold Stage 3a/3b pending one full weekly reset of live Stage 1 telemetry.
- [ ] Stage 3 / Milestone 3a & 3b (held per Advisor verdict): After one full weekly reset under Stage 1, evaluate live telemetry to decide whether to add p90 volume margin inside `five_hour_safe` (3a) and/or cross-role weekly reserve vs shortage-week fallback (3b).
- [ ] Pass pre-push code review (`request_review`) and push commits to `origin/master`.

## Surprises & Discoveries

- Observation: Mjolnir already integrates `BrokkAi/bifrost-service`'s benchmark data via `https://slopcop.com/power-ranking.json` and compiled-in baseline `mj-accounts/assets/power-ranking-baseline.json` (`as_of: 2026-11-19`, `source_commit: a8ed8ccf43`, 47 entries). No `bifrost-service` schema change is needed.
  Evidence: `mj-accounts/src/power_ranking.rs:18-40,433-468`.

- Observation: `calibrated_prior_scale` (`mj-accounts/src/profile_scheduler.rs:535-558`) sums raw USD (`sum(obs.total_api_usd) / sum(n * bench)`), giving a `$24.25/bench` Kimi sample ~53.2x the weight of a `$0.456/bench` DeepSeek sample.
  Evidence: `mj-accounts/src/profile_scheduler.rs:546-552`; Advisor code check `C1`.

- Observation: MJ Advisor's audit uncovered critical fidelity gaps in the v1 simulator (`scheduler_mc.py`) and plan:
  1. `scheduler_mc.py` gave flow policies true quota (`true_rem_pct`) and true remaining in-flight cost (`task.remain`) while `master` read 10-minute polls — invalidating the v1 comparison until re-run under identical polled quota and mean-cost reservations.
  2. Real arrivals in `scratch.sqlite3` (25,312 placements) are strongly non-Poisson: 10,611 reviewer legs in 5 days (63/h mean, peak 245/h, hourly Fano/VMR = 46, 2–3 same-second legs, 0.68–0.96 daily correlation with `subtle` and `straightforward`), and live config has 6 Codex + 7 Claude profiles with `straightforward = [gpt-6-luna, grok-4.7]` (no paid fallback, overlapping reviewers on Codex).
  3. A rate-based weekly LP fails on staggered multi-account resets (e.g. account A at 2% resetting in 12h vs account B at 80% resetting in 100h) because it ignores A's refill at 12h. The weekly LP must operate in volumes over each pool's horizon and credit other pools' refills, protect only sub-only roles (eliminating Big-M, dollar terms, and the unsound `min_paid_usd` clause), use staged objectives without Phase 1 (all-slack basis is already feasible), and run once per telemetry refresh rather than per draw.
  4. On front-loaded bursts that fit inside 5h (`B6`: 6–12 tasks at 48% of 5h, 47% of weekly), a 5h rate limiter still falsely spilled 28–31% to DSF and cut Kimi weekly fill from 52–53% to 34–41%. Volume-based 5h admission (admitting while high-quantile reservations fit) must be tested side-by-side against decay rate limiting under polled information.
  5. `skip_serializing` alone does not remove `[quota] prefer` from existing `config.toml` files because Mjolnir edits TOML in place (`mj-core/src/config/document.rs:133-135`); `"quota.prefer"` must be added to `RETIRED_KEYS` (`mj-core/src/config.rs:1485-1501`), and live references also exist in `docs/src/content/docs/cli-reference.md:389`, `mj-core/assets/skills/mj/references/configuration.md:635`, and `mj-client/src/daemon.rs:1856`.
  6. `qualifying_placements` uses strict `>` (`quota_telemetry.rs:272-284`), so an exact `reset` timestamp misses placements created in the reset second, and a stale dated reset can elapse multiple cycles; cycle-aligning `reset` and using `>=` (or `cycle_start - 1`) fixes both.
  7. `session_contexts` has no `created_at` index (`mj-store/src/database/baseline.sql:10-15,314-326`), so a 7-day query must be timed on real data before assuming zero-migration latency.
  8. `review_host/selection.rs:79,108` calls `reservation.commit(None)` (generating `__unkeyed-*`), preventing `session_id` dedup against SQLite.
  9. `1 / est_usd` probabilistic weights in Paid tier still send draws to `$15` overage credits alongside `$0.12` DSF; strict cheapest-first ordering is required.
  Evidence: MJ Advisor review (`91233b0efdd193066e7b4eeb46c8db10`), `findings.md`, `quota-plan-code-review.md`, `topology.txt`.

## Decision Log

- Decision: Split delivery into Stage 1 (Milestones 1 & 2 + tier reorder + decision telemetry + burn-rate projections, shipping immediately), Stage 2 (production-fidelity real-trace simulation + SQL benchmark), and Stage 3 (Advisor-signed cross-role reserve and 5h admission as separate commits).
  Rationale: Directly adopts MJ Advisor's verdict so immediate high-confidence fixes (retiring `prefer`, fixing 53x prior bias, fixing post-reset blind spot, spilling unsafe subscriptions to strict cheapest-first Paid, recording placement reasons, and showing burn-rate forecasts) land cleanly without blocking on Milestone 3 calibration, while Milestone 3 is validated under honest polled production conditions before any code is written.
  Date/Author: 2026-11-27 / MJ Advisor + Parent agent.

- Decision: Order tiers strictly as Safe Subscription (`0`) > `Unknown` (`1`) > Unified Paid (`2`, `Paid` overage credits with `may_consume_credits = true` and `UsagePriced` API ordered **strictly** by ascending real per-task `est_usd`, falling back to descending `tasks_per_200_api`) > 5h-unsafe Subscription (`3`) > Weekly-unsafe / sub-reserve Subscription (`4`).
  Rationale: Honors user decisions (subscription before paid except when spilling early avoids 5h/runway mid-turn stranding or protects an irreplaceable sub-only role, `Unknown` ahead of paid, cheapest real dollar within paid) and eliminates probabilistic leakage onto `$15` overage credits when `$0.12` DSF is available. Note that Tiers 3/4 reflect the single-task runway/stranding gate (`rem_eff >= v_60m * min(runway_duration_hours, t_left)` and `1%` reserve floor), not the deferred Stage 3b multi-day cross-role reserve.
  Date/Author: 2026-11-27 / User + MJ Advisor + Parent agent.

- Decision: Compute per-profile burn-rate projections once at telemetry refresh inside `QuotaTelemetrySnapshot` using trailing 24h/7d `quota_history` consumption for weekly windows (never extrapolating 60m velocity over days) and trailing 60m velocity for 5h windows, with unknown supply preserved as unknown.
  Rationale: Single owner for pace projections shared by `/api/v1/usage/report` (TUI "Usage by session") and `mj quota`, avoiding request-time drift and noisy 60m-to-7d extrapolation.
  Date/Author: 2026-11-27 / MJ Advisor + Parent agent.

- Decision: Record placement decision reason and binding metric on every automatic draw (and aggregate paid-spill counters by reason inside `QuotaTelemetryStore`).
  Rationale: Essential operational observability before shipping mechanisms that spend paid dollars while subscription quota remains, so production spills can be audited and tuned from evidence.
  Date/Author: 2026-11-27 / MJ Advisor + Parent agent.

- Decision: Skip cross-provider automatic Move for now; retain lossless same-provider native-continuity Move.
  Rationale: Confirmed with user; parent agents and human operators handle the rare residual tail cleanly without introducing cross-harness handoff churn.
  Date/Author: 2026-11-27 / User + Parent agent.

## Outcomes & Retrospective

Stage 1 (Milestones 1 and 2) is implemented and validated without SQLite migrations, config format breaks, or wire-protocol bumps:
- **Milestone 1 (`461d013aa`):** Retired `[quota] prefer` across scheduler, server, daemon, TUI, CLI dashboard, and docs, retaining backward-compatible `discard_legacy_setting` + `RETIRED_KEYS` config handling and an HTTP 200 no-op on `POST /api/v1/terminal/quota/preferences`.
- **Milestone 2:** Fixed `calibrated_prior_scale` cross-model USD dominance (`sum(obs_usd / bench) / sum(n)`), fixed post-reset multi-cycle alignment + inclusive `>= cycle_start` cutoff + 90m fallback bound + zero expired velocity, reordered tiers (`Safe sub > Unknown > strict cheapest-first Unified Paid > 5h-unsafe > Weekly-unsafe`), added `PlacementDecision` structured logging + `PaidSpillCounters` (including empty draws and unsafe fallbacks by category, plus `estimated_cost_usd`), carried `ReviewReservation` through child registration so reviewer/brief-writer reservations commit with `Some(child_session_id)`, and computed unified refresh-time `ProfileQuotaProjection` (weekly from 24h/7d measured coverage, 5h from clean trailing 60m intervals) exposed on `/api/v1/usage/report` (TUI "Usage by session") and `mj quota`.

Per MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) review of Stage 2 (`.mj/agents/915d7fde717e2fae0af46e87ac627305/prod_fidelity_report.md`) and user follow-up:
- **Ship Stage 1 now; hold Stage 3a/3b pending one full weekly reset of live Stage 1 telemetry** (and offline simulator calibration against real per-cell costs, durations, starting `quota_history` balances, and 0% provider rejection).
- **Stage 3a (5h admission):** Drop the exponential-decay 5h rate limiter (it falsely spilled 40/60 tasks on `B6` bursts that fit 5h, leaving most of Kimi's 5h quota unused). If live mid-turn 5h interruptions remain material after Stage 1, implement **p90 quantile volume admission** as a risk margin inside `five_hour_safe` (which preserved 0/60 false spills on `B6` and full Kimi utilization while cutting `M6a` peak overdraw).
- **Stage 3b (cross-role weekly reserve):** Protection scope confirmed by user as **no-paid-fallback (sub-only) roles only**. Evaluate after one full weekly cycle using four live metrics (failed/unsafe draws by sub-only role, paid spill USD by reason, forecast vs actual role demand, and predicted vs observed burn). Drop the rolling-window envelope (no better than fixed `k`); evaluate `k = 1.0–1.25` against the user-level alternative of giving reviewers a fallback only in shortage weeks.
- **Stage 3c:** Failed-turn 429 and overload classification includes prose and Retry-After metadata, including RFC 2822 dates; rate and quota faults schedule immediate deduplicated probes subject to provider holds. Startup failures remain visible to the parent under the failed-child contract. Active-turn same-provider Move for worker-owned `RetryProvider` when quota is not exhausted remains separate design work: durable `capacity_retry` and Move need a single arbitration owner to avoid executing both.

## Context and Orientation

Mjolnir's automatic profile scheduling converges on `mj-accounts/src/profile_scheduler.rs` and `mj-accounts/src/quota_telemetry.rs`, backed by `/power-ranking.json` (`mj-accounts/src/power_ranking.rs`) and SQLite usage queries (`mj-store/src/database/usage.rs`). Callers include subagent/primary/oracle resolution (`mj-server/src/server/api/subagents.rs`), review host selection (`mj-daemon/src/review_host/selection.rs`), and same-provider quota auto-move (`mj-daemon/src/daemon/continuation/quota.rs`).

Background telemetry refresh runs in `mj-daemon/src/daemon/delegation.rs` (`spawn_quota_telemetry_refresh`: 10-minute quota/placement tick and hourly category/cell-cost tick). The TUI "Usage by session" modal (`mj-tui/src/session_usage_report.rs`) fetches `GET /api/v1/usage/report` (`mj-core/src/usage/report.rs`, `mj-server/src/server/api/turns.rs`, `mj-daemon/src/server_runtime/api.rs`). `mj quota` (`mj-cli/src/api_commands.rs`) reads live quota snapshots from the runtime feed.

Config lives in `mj-core/src/config.rs` with in-place TOML edits handled by `mj-core/src/config/document.rs` and `RETIRED_KEYS` (`mj-core/src/config.rs:1485-1501`).

## Plan of Work

### Stage 1 / Milestone 1: Retire `[quota] prefer` cleanly

1. `mj-core/src/config.rs`:
   - Change `QuotaConfig.prefer` to `#[doc(hidden)] #[serde(skip_serializing, deserialize_with = "discard_legacy_setting")] pub prefer: ()`.
   - Add `"quota.prefer"` to `RETIRED_KEYS` (`mj-core/src/config.rs:1485-1501`) so in-place TOML saves strip `[quota] prefer` cleanly from existing files.
   - Update `mj-core/src/config/tests.rs` to verify a legacy config with `[quota] prefer = ["codex"]` loads cleanly and drops `prefer` on save.
2. `mj-accounts/src/profile_scheduler.rs` & `mj-accounts/src/quota_telemetry.rs`:
   - Remove `prefer: &'a [String]` from `SchedulingContext`, `prefer_pos` from `ScoredPool` and sort comparators, and `prefer: &[String]` from `QuotaTelemetryStore::draw_and_reserve`.
3. `mj-server/src/server/api/subagent_backend.rs`, `mj-server/src/server/api/subagents.rs`, `mj-daemon/src/server_runtime/api.rs`, `mj-daemon/src/daemon/continuation/quota.rs`:
   - Remove `SubagentBackend::quota_preferences` and its pass-throughs.
   - Remove `load_balancing_preferences_lead_among_quota_equals_in_list_order` and preference fields from test fakes in `subagents.rs`.
4. `mj-client/src/http/setup.rs`, `mj-server/src/server/api/terminal_operations/setup.rs`, `mj-daemon/src/daemon/terminal_setup.rs`, `mj-client/src/daemon.rs`:
   - Keep `POST /api/v1/terminal/quota/preferences` registered as a backward-compatible HTTP 200 no-op (`#[serde(default)]` on `prefer` in the request body) so older TUI clients talking to an upgraded daemon do not fail mid-upgrade.
   - Clean up the comment/reference at `mj-client/src/daemon.rs:1856-1858`.
5. `mj-tui/src/lib.rs`, `mj-tui/src/pane_controls.rs`, `mj-tui/src/dashboard_sessions.rs`, `mj-cli/src/dashboard/{actions.rs, io.rs, io/spawn.rs}`:
   - Remove `PreferLoadBalancingProfile`, `ClearLoadBalancingPreferences`, `PreferProfile`, `ClearLoadBalancing`, their quota-row menu items, their save/IO plumbing, and `prefer` tie-breaking in `dashboard_sessions.rs`. Update `pane_controls.rs` tests.
6. Docs/skills:
   - Update `docs/src/content/docs/cli-reference.md:389` and `mj-core/assets/skills/mj/references/configuration.md:635` to remove `[quota] prefer` documentation. Do not edit historical Jev fixtures or `go/`.

### Stage 1 / Milestone 2: Core Scheduler Fixes, Strict Paid Tier, Decision Telemetry, and Burn-Rate Projections

1. **Cross-model `calibrated_prior_scale` fix (`mj-accounts/src/profile_scheduler.rs`):**
   - Replace raw-USD summing (`observed_usd / benchmark_usd`) with sample-weighted benchmark-task units (`sum(obs.total_api_usd / bench) / sum(obs.sample_count)`), updating the corresponding unit test expectation and adding a mixed DSF+Kimi regression check.
2. **Post-reset window alignment and reservation cutoff fix (`mj-accounts/src/profile_scheduler.rs`, `mj-accounts/src/quota_telemetry.rs`):**
   - Extend `WindowEval` with `reset_elapsed: bool` and `cycle_start: Option<i64>`.
   - When `reset <= now_seconds`, compute window duration `d_w` (`duration_seconds`, else `5 * 3600` for `Short5h` or `7 * 86_400` for `Long`), advance `cycle_start = reset + ((now_seconds - reset) / d_w) * d_w` so repeatedly elapsed stale resets align to the current cycle, set `remaining_pct = 100.0`, `h_w = (100.0 - RESERVE_FLOOR_PCT) / 100.0`, `u_w = 1.0`, `t_left_hours = (cycle_start + d_w - now_seconds) as f64 / 3600.0`, and `reset_elapsed = true`.
   - In the runway gate: when `ev.reset_elapsed`, set `v_60m = 0.0` (do not carry expired-window velocity into the new window) and filter placements with inclusive `created_at_epoch_seconds >= cycle_start` (taking the stricter of `second_to_last` strict-`>` and `cycle_start` inclusive-`>=`).
   - Bound `qualifying_placements` fallback when `second_to_last == None` to `now_seconds - HISTORY_LOOKBACK_SECONDS` (90m) so future widening of placement history cannot inflate reservations.
3. **Tier reorder and strict cheapest-first Unified Paid tier (`mj-accounts/src/profile_scheduler.rs`):**
   - Tiers: `0` Safe subscription (`all_above_reserve && weekly_safe && five_hour_safe`), `1` `Unknown`, `2` Unified Paid (`Paid` overage credits with `may_consume_credits = true` and `UsagePriced` API), `3` 5h-unsafe subscription (`all_above_reserve && weekly_safe && !five_hour_safe`), `4` Weekly-unsafe or sub-reserve subscription.
   - Within Tier 2 (Unified Paid), rank **strictly** by ascending real per-task `est_usd` (computed with the same empirical/calibrated benchmark estimator, falling back to descending `tasks_per_200_api`, then `tasks_per_200_sub`) rather than probabilistic weighted drawing — so `$0.12` DSF deterministically beats `$15` Claude overage credits. (Equal-cost Paid profiles break ties randomly/deterministically via the existing draw tie-break.)
4. **Placement decision reason telemetry (`mj-accounts/src/profile_scheduler.rs`, `mj-accounts/src/quota_telemetry.rs`, `mj-daemon/src/review_host/selection.rs`):**
   - Return structured `PlacementDecision` metadata alongside draw ordering (chosen profile, tier, `PlacementDecisionReason` — `SafeSubscription`, `UnknownQuota`, `PaidSpill { sub_5h_unsafe, sub_weekly_unsafe, sub_exhausted_or_none }`, `UnsafeSubscriptionFallback`, plus the binding profile/window/headroom numbers).
   - Emit a compact `tracing::info!` / `tracing::debug!` structured event inside `draw_and_reserve` and maintain atomic per-reason paid-spill counters on `QuotaTelemetryStore` so every paid spill can be audited against subscription state.
   - Where `review_host/selection.rs` commits reviewer/brief-writer reservations before the child session ID exists, attach a stable pre-allocated or correlated key on child registration (or update pending reservation key when the child session ID is assigned) so pending review reservations deduplicate cleanly against SQLite rows instead of double-counting under `__unkeyed-*`.
5. **Unified refresh-time burn-rate projections (`mj-accounts/src/quota_telemetry.rs`, `mj-core/src/usage/report.rs`, `mj-server/src/server/api/turns.rs`, `mj-daemon/src/daemon/delegation.rs`, `mj-daemon/src/server_runtime/api.rs`, `mj-tui/src/session_usage_report.rs`, `mj-cli/src/api_commands.rs`):**
   - During the 10-minute `QuotaTelemetryStore` refresh, compute `Vec<ProfileQuotaProjection>` once from the live `ProfileQuota` reports + `quota_history`:
     - Weekly windows: compute `observed_pct_per_hour` from trailing 24h/7d `weekly_consumption` over observed coverage (reporting unknown when coverage is absent; **never** extrapolating 60-minute velocity over days).
     - 5h windows: compute `observed_pct_per_hour` from `trailing_60m_pct_per_hour`.
     - Compute `sustainable_pct_per_hour = remaining_pct / t_left_hours` when `resets_at > now_seconds`, `pace_ratio = observed / sustainable`, and `projected_exhaustion_epoch_seconds = now + (remaining_pct / observed) * 3600` when `pace_ratio > 1.0`.
   - Expose additive `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub quota_projections: Vec<ProfileQuotaProjection>` on `SessionUsageReport` (zero `PROTOCOL_VERSION` bump) and expose the same snapshot projections to `mj quota` (human table + `--json`).
   - Render a concise **Quota Pace & Forecast** section in TUI "Usage by session" (`mj-tui/src/session_usage_report.rs`) whenever any profile is burning over pace (`pace_ratio > 1.0`), leaving quiet-report goldens unchanged and adding a golden case in `mj-tui/src/dialogs/tests.rs` + `mj-tui/tests/golden/dashboard-dialogs.txt`.

### Stage 2: Production-Fidelity Milestone 3 Simulation & SQLite Query Benchmark

Run in parallel with Stage 1 (read-only on repository, artifacts in report directory):
1. Time the 7-day `session_contexts` demand query and inspect `EXPLAIN QUERY PLAN` on `scratch.sqlite3` (25,312 real placements) and synthetic 200k-row scale to verify whether a compatible `CREATE INDEX IF NOT EXISTS` migration is needed before Stage 3.
2. Build a strictly fair, zero-oracle-leakage simulator replaying the 25,312 real placements (`scratch.sqlite3`) plus synthetic stress scenarios (biased cost estimates `0.5x` and `2x`, staggered 6-Codex + 7-Claude + 2-Gemini + Grok + Kimi + DSH resets, `straightforward = [codex/luna, grok]` overlapping `reviewers` on Codex, two paid-capable roles, front-loaded bursts `B6` and `M7`, no-API `M6b`):
   - Every policy (`master`, `stage1_spill_only`, `5h_volume_admission`, `5h_decay_pacing`, `volume_refill_lp + envelope`, `volume_refill_lp + fixed_k`) sees **identical 10-minute polled quota** and **estimated mean-cost reservations from qualifying placements since `second_to_last_clean_ts`**.
   - Implement the **volume-over-horizon-with-refills LP** protecting ONLY sub-only roles, with staged objectives (no Big-M, no Phase 1), solved once per 10-minute telemetry refresh, and verified numerically against `scipy.optimize.linprog` on every solve.
   - Compare 5h volume admission (high-quantile reservation fit) against continuous-time exponential-decay pacing (subtracting cross-role inflow on shared pools), and compare rolling-window envelope reserve (`max` demand seen in any past window of length `t_left`) against fixed `k_safe * max(lambda_24h, lambda_7d) * t_left`.
3. Send Stage 2 simulation results and the finalized Stage 3 (Milestone 3a cross-role reserve + Milestone 3b 5h admission) spec to MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) for review before coding Stage 3.

### Stage 3 (Deferred Until Stage 2 Advisor Sign-Off): Cross-Role Reserve & 5h Admission

To be finalized from Stage 2 findings and shipped as two separate, cleanly testable changes:
- Milestone 3a: Refresh-time cross-role weekly volume-with-refills LP + per-group shortfall reporting in `SessionUsageReport` and `mj quota`.
- Milestone 3b: Winning 5h admission mechanism (volume admission vs exponential-decay pacing with cross-role inflow accounting).

## Concrete Steps

All commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/9103481fa841de038a7b6de95e5589d2` (`cargo test` outside restricted sandbox).

Stage 1 / Milestone 1 validation:

    cargo test -p brokk-mj-core config::tests
    cargo test -p brokk-mj-accounts profile_scheduler
    cargo test -p brokk-mj-server subagents
    cargo test -p brokk-mj-tui pane_controls
    cargo clippy -p brokk-mj-core -p brokk-mj-accounts -p brokk-mj-client -p brokk-mj-server -p brokk-mj-daemon -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Stage 1 / Milestone 2 validation:

    cargo test -p brokk-mj-core usage
    cargo test -p brokk-mj-store usage
    cargo test -p brokk-mj-accounts quota_telemetry
    cargo test -p brokk-mj-accounts profile_scheduler
    cargo test -p brokk-mj-server subagents
    cargo test -p brokk-mj-daemon delegation
    cargo test -p brokk-mj-daemon continuation::
    TZ=UTC cargo test -p brokk-mj-tui golden_dashboard_dialogs
    cargo test -p brokk-mjolnir quota
    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-accounts -p brokk-mj-client -p brokk-mj-server -p brokk-mj-daemon -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Commit each validated milestone cleanly on `master`, call `request_review`, and push to `origin/master` once clean.

## Validation and Acceptance

1. **Milestone 1:** Existing `config.toml` containing `[quota] prefer = ["codex"]` loads without error, and an in-place TOML save removes `prefer` via `RETIRED_KEYS`; `POST /api/v1/terminal/quota/preferences` returns HTTP 200 no-op; TUI quota-row context menu no longer shows Prefer/Clear actions; docs and skill references are updated.
2. **Milestone 2:**
   - `calibrated_prior_scale` weights 100 cheap DSF samples and 2 expensive Kimi samples by sample count rather than raw USD.
   - Elapsed dated resets align across multiple elapsed cycles, zero expired velocity, include same-second post-reset placements, exclude pre-reset placements, and bound `second_to_last == None` reservations to 90m.
   - Tier order puts Safe Subscription > `Unknown` > strict cheapest-first Unified Paid (`$0.12` DSF deterministically beats `$15` Claude overage credits) > 5h-unsafe > Weekly-unsafe / sub-reserve.
   - Every draw records its `PlacementDecision` reason and increments paid-spill reason counters; pending review reservations deduplicate cleanly.
   - Refresh-time `ProfileQuotaProjection` computes weekly pace from 24h/7d `quota_history` (never 60m velocity) and unknown supply as unknown; both `/api/v1/usage/report` (TUI "Usage by session" golden) and `mj quota` render over-pace exhaustion forecasts consistently.

## Idempotence and Recovery

Stage 1 introduces no SQLite migrations, no config format breaks, and no `PROTOCOL_VERSION` bumps. Legacy `config.toml` files and older TUI binaries remain compatible. Rollback is a clean `git revert`.

## Artifacts and Notes

- Advisor review evidence extracted to `scratch/advisor-trace/1b3d468b03ee5d82c60743e1d72a3207/` (`scratch.sqlite3` with 25,312 real placements, `tables.txt`, `topology.txt`).
- Advisor audit notes: `/mnt/optane/tmp/mjolnir/91233b0efdd193066e7b4eeb46c8db10/home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/a1d66d4b61913e3d72f1d082fb0fcd3f/findings.md` and `.../7177b99cd7e4a2ab32dd5aeca9010553/quota-plan-code-review.md`.

## Interfaces and Dependencies

No new external crates are added in Stage 1.

In `mj-core/src/config.rs`:

    pub struct QuotaConfig {
        #[doc(hidden)]
        #[serde(skip_serializing, deserialize_with = "discard_legacy_setting")]
        pub runtime: (),
        #[doc(hidden)]
        #[serde(skip_serializing, deserialize_with = "discard_legacy_setting")]
        pub prefer: (),
    }

In `mj-accounts/src/profile_scheduler.rs`:

    pub enum PlacementDecisionReason {
        SafeSubscription,
        UnknownQuota,
        PaidSpill {
            sub_5h_unsafe: usize,
            sub_weekly_unsafe: usize,
            sub_exhausted_or_absent: usize,
        },
        UnsafeSubscriptionFallback,
    }

    pub struct PlacementDecision {
        pub chosen_profile: String,
        pub chosen_tier: u8,
        pub reason: PlacementDecisionReason,
        pub binding_profile: Option<String>,
        pub binding_remaining_pct: Option<f64>,
    }

In `mj-core/src/usage/report.rs`:

    pub struct ProfileQuotaProjection {
        pub profile_id: String,
        pub window_label: String,
        pub remaining_percent: u8,
        pub resets_at_epoch_seconds: Option<i64>,
        pub observed_pct_per_hour: Option<f64>,
        pub sustainable_pct_per_hour: Option<f64>,
        pub pace_ratio: Option<f64>,
        pub projected_exhaustion_epoch_seconds: Option<i64>,
    }

Revision note (2026-11-27): Revised ExecPlan after MJ Advisor (`91233b0efdd193066e7b4eeb46c8db10`) review to stage delivery cleanly (Stage 1: Milestones 1 & 2 + strict cheapest-first Paid tier + decision telemetry + refresh-time burn-rate projections; Stage 2: honest polled production-fidelity real-trace simulation + SQL benchmark; Stage 3: Advisor-reviewed cross-role reserve and 5h admission), fix `RETIRED_KEYS` and unlisted `prefer` references, fix strict-`>` reset cutoff and multi-cycle alignment, switch Paid tier to strict cheapest-first, and compute weekly pace from 24h/7d history rather than 60m velocity.
