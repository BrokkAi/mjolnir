# Consistent Empirical Session Cost, Unbiased 7-Day 5h Ratio, and Tick-Scale Fix for Quota Load Balancing

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

After this change, Mjolnir's quota load balancer produces accurate per-pool capacity weights and 5h/weekly runway reservations from observed local session costs instead of mismatched static benchmark ratios and category-wide averages, and stops losing or biasing `five_hour_to_weekly_ratio` (`R`) on 5h-capped pools. Specifically:

1. **10x tick-scale fix (Commit 1, shipped `81cd75fe4` / `f643b6f0f`):** `load_category_burn_observations` in `mj-store/src/database/usage.rs:681` divided `session_ticks` by `1e9` instead of `USD_TICKS_PER_DOLLAR` (`1e10`), inflating reservations 10x once a category crossed 200 completed sessions (`subtle` had already reached 231 on the live store).
2. **Unbiased 7-day `five_hour_to_weekly_ratio` (`R`) ledger (Commit 2, shipped `621cf999a` / `e953ce36c`):** `compute_window_telemetry` previously dropped every clean 10-minute pair where integer weekly usage stayed flat while 5h moved (biasing `R` down 2.5x on `claude5` and 1.6x on `kimi`), and `refresh_from_store` computed `R` over only 90 minutes (leaving `R = None` on 91–95% of polls in live-scale simulation). Summing all clean un-reset pairs (including zero-delta pairs) in a single per-`(profile_id, cache_identity)` 7-day ledger cuts L3 5h interruptions in half (`3.31–3.61% -> 0.67–1.84%`) and restores `R` availability to 100%.
3. **Unified per-`(category, model, effort)` empirical cost estimator (Commit 3, Milestone 3):** Today `profile_scheduler.rs` takes `base_capacity` from static benchmark `tasks_per_200_sub` (missing configured efforts like `grok-4.7 / low` and `kimi-code/k3 / high`), while reservations use one category-wide USD average (`$0.12` on `straightforward` vs live `$0.023` Luna and `$0.353` Grok; `$3.47` on `subtle` vs live `$1.41` Sol). Learning `est_usd(category, model, effort)` via arithmetic shrinkage (`K = 3`, capped at 500 terminal sessions per cell) toward a per-draw calibrated benchmark prior — and using that one `est_usd` for both direct capacity (`effective_weekly_budget / est_usd`) and reservations — yields accurate per-pool reservations and capacity, drains Kimi aggressively if Kimi turns out cheaper than its benchmark fallback, and preserves `master`'s relative weights at cold start (including `primary`, where long-lived sessions rarely produce single-effort cells).
   - **Scope honesty on Grok:** At live `straightforward` volume (~600 tasks/week × `$0.353` observed Grok/low mean ≈ `$212`/week vs Grok's `$1,415`/week budget), the entire `straightforward` category is only ~15% of Grok's weekly quota. Milestone 3 improves Grok's share and fixes its reservations, but does **not** drain Grok by itself; draining Grok requires configuring Grok on a heavier category (`subtle` or `primary`).
   - **Trade-off on small 5h-capped pools:** Shifting more traffic onto small 5h-capped pools (when learned cost is lower than static fallback) trades a few additional 5h-interrupted sub-agents per week for substantial quota recovery on those pools (see Decision Log for the L3 gate numbers).

No new scheduler tiers, no exponent change (`PACE_EXPONENT_ALPHA` stays `2.0`), no cross-model effort-slope extrapolation, no SQLite migrations, and no protocol changes are needed. Kimi remains excluded from automatic scheduling on raw (`ConfiguredApprovals`) targets per the native-guardian contract (`HarnessKind::Kimi.guardian() == GuardianReviewer::Mjolnir`); live store data confirms 61.3% of `subtle` spawns run on `Unconstrained` container targets where Kimi is eligible.

## Progress

- [x] (2026-11-23 13:02Z) Audited `master` (`1ab4934ad`), live daemon (`v2.74.0+1ab4934a`), live `mj quota --json`, live `[subagents]` config, and Kimi guardian contract.
- [x] (2026-11-23 14:00Z) Received Advisor review of v1 plan; cut cross-model effort-slope extrapolation and dead `cache_identity` deduplication.
- [x] (2026-11-23 14:02Z) Measured live SQLite target mix (61.3% of `subtle` and 58.3% of `straightforward` spawns on `Unconstrained` containers) and live token/cost/effort coverage; discovered the `1e9` vs `1e10` tick-scale bug in `mj-store/src/database/usage.rs:681`.
- [x] (2026-11-23 14:21Z) Disabled duplicate-login profile `codex-review_arms_rql` (`enabled = false`) in `~/.config/mjolnir/config.toml` per user instruction and confirmed via `mj doctor --json` and `mj quota --json`.
- [x] (2026-11-23 15:15Z) Completed v3/v4 simulations (27 cells × 20 seeds): ruled out behind-pace Tier 0, `h_w_eff`, `alpha > 2`, and `R_cold`.
- [x] (2026-11-23 15:40Z) Received Advisor review of v2 plan: split Milestone 1 (tick fix) and Milestone 2 (unbiased 7-day `R` ledger) to ship immediately; ran live-scale SQL benchmark + 28-day simulation (`m3-live-scale-prep.md`).
- [x] (2026-11-23 15:47Z) **Commit 1 pushed to `origin/master` (`81cd75fe4`, merged `f643b6f0f`):** exported `USD_TICKS_PER_DOLLAR = 10_000_000_000` in `mj-core/src/usage.rs`, fixed `mj-store/src/database/usage.rs:681`, added hard-won regression test `category_burn_observations_scales_usd_ticks_by_ten_billion`.
- [x] (2026-11-23 15:56Z) **Commit 2 pushed to `origin/master` (`621cf999a`, merged `e953ce36c`):** fixed downward `R` bias in `accumulate_ratio_deltas` (including zero-delta clean pairs) and added per-`(profile_id, cache_identity)` 7-day `RatioLedger` with hourly rehydration and incremental 10-minute folding in `mj-accounts/src/quota_telemetry.rs`.
- [x] (2026-11-23 16:05Z) Completed live-scale Milestone 3 prep (`m3-live-scale-prep.md`) and L3 `Arith_K3_AllTerm` gate at live dispersion `σ = 0.9, 1.3` plus live Kimi usage audit (`m3-advisor-gate.md`); received Advisor clearance to proceed with Milestone 3 as specified below.
- [x] (2026-11-23 16:20Z) Implemented and validated Milestone 3 across `mj-store/src/database/usage.rs`, `mj-accounts/src/quota_telemetry.rs`, `mj-accounts/src/profile_scheduler.rs`, and `mj-daemon/src/daemon/delegation.rs` (store `usage` 19/0, accounts `quota_telemetry` 5/0, accounts `profile_scheduler` 13/0, daemon `delegation` 36/0, all-targets `clippy -D warnings` clean).
- [ ] Pass pre-push code review and push Commit 3 to `origin/master`.

## Surprises & Discoveries

- Observation: `load_category_burn_observations` (`mj-store/src/database/usage.rs:681`) divided `session_ticks` by `1e9` instead of `1e10`, and `subtle` had already reached `231 >= 200` completed sessions on the live store.
  Evidence: `.mj/agents/a23f4b46bcbc3a2b4bb64cf335a376aa/advisor-followup-checks.md`. Fixed standalone in Commit 1 (`81cd75fe4`).

- Observation: `compute_window_telemetry` (`mj-accounts/src/quota_telemetry.rs:335-356`) dropped every clean pair where `d_long == 0`, biasing `R` down 2.5x on live `claude5` (`5.51` vs true `13.93`) and 1.6x on live `kimi` (`1.46` vs `2.39`), while the 90-minute lookback left `R = None` on 91–95% of polls.
  Evidence: `.mj/agents/a23f4b46bcbc3a2b4bb64cf335a376aa/advisor-followup-checks.md` and `.mj/agents/a227080b94f5cb694a9459dbb93aee8e/m3-live-scale-prep.md`. Fixed in Commit 2 (`621cf999a`).

- Observation: Within-cell live session cost dispersion is `p90/p50 = 2.23–3.33` (`σ ≈ 0.63–0.94`, `CV ≈ 0.70–1.19`), NOT `CV = 6` (`σ = 1.90`, which came from pooling across categories/models/efforts).
  Evidence: `.mj/agents/a227080b94f5cb694a9459dbb93aee8e/m3-live-scale-prep.md`:
  - `straightforward / gpt-6-luna / high`: `n=157`, mean `$0.0226`, p50 `$0.0135`, p90 `$0.0397` (`p90/p50 = 2.95`)
  - `straightforward / grok-4.7 / low`: `n=27`, mean `$0.3527`, p50 `$0.2825`, p90 `$0.6295` (`p90/p50 = 2.23`)
  - `subtle / gpt-6.2-sol / medium`: `n=233`, mean `$1.4050`, p50 `$0.9418`, p90 `$2.9703` (`p90/p50 = 3.15`)
  - `primary / gemini-4-neon / unknown`: `n=16`, mean `$9.7021`, p50 `$6.1251`, p90 `$20.4190` (`p90/p50 = 3.33`)
  At `σ = 0.65–0.9`, arithmetic mean is 91–97% efficient relative to log-space estimators and stays distribution-unbiased, so `LogRatio` is unnecessary.

- Observation: Rust `UsageScope` (`mj-core/src/usage.rs:11-15`) has only `Turn`, `LastRequest`, and `Unspecified` — there is no `Session` variant and zero cumulative rows exist in the live DB. No speculative cumulative-differencing code is needed.

- Observation: Live Kimi `kimi-code/k3` is already in `mj-core/src/usage/pricing.rs:rates()` and prices cleanly when token counters are present (17 stopped historical sessions, 729 turns; e.g. `$9.87`). Live SQL returned 0 Kimi cells only because 80 of 81 Kimi-model sessions pre-dated `session_turn_selections.category` (`category IS NULL`), and the single categorized session (`a9fea156`) had two `usage: null` outcomes (one provider failure). `kimi-code/k3.1-pro` is not in `rates()` (deferred until configured).
  Evidence: `.mj/agents/ae04c6c9ddab6ffa0be7d0915faa7fac/m3-advisor-gate.md`.

- Observation: On the L3 stress case (Sol + Kimi + `$100` Claude Opus at `R = 15`), the strict no-regression gate `Arith_K3_AllTerm 5h − B_7dR 5h <= 0` **failed** by `+0.453 ± 0.217 pp` (World A, `σ = 0.9`), `+0.198 ± 0.263 pp` (World B, `σ = 0.9`), `+0.592 ± 0.371 pp` (World A, `σ = 1.3`), and `+0.546 ± 0.291 pp` (World B, `σ = 1.3`).
  Evidence: `.mj/agents/ae04c6c9ddab6ffa0be7d0915faa7fac/m3-advisor-gate.md`. Diagnosis: NOT estimation error (`StoppedOnly` had `-0.22%` Opus bias at `σ = 0.9` yet rose `+0.342 ± 0.208 pp`), but load moving onto small 5h-capped pools as true costs are learned (`$100` Opus start share `13.8% -> 16.9%`, Opus stranding `-6.4 to -8.1 pp`; Kimi start share `4.9% -> 7.4%`, Kimi stranding `-25.3 pp` in World A). Against `B0`, `Arith_K3_AllTerm` still cuts 5h interruptions by `-1.77 to -2.48 pp` and cuts World B weekly interruptions 9x (`0.232% -> 0.025%`).

## Decision Log

- Decision: Ship Commit 1 (tick-scale fix) and Commit 2 (unbiased 7-day `R` ledger) standalone ahead of Milestone 3.
  Rationale: Both fixed active bugs in `master` (live 10x reservation inflation on `subtle` and 1.6–2.5x `R` bias / 90m amnesia) independently of Milestone 3.
  Date/Author: 2026-11-23 / Parent agent + MJ Advisor.

- Decision: For Milestone 3, use arithmetic shrinkage with `K = 3`, capped at 500 terminal sessions (`stopped`, `error`, `lost`) per `(category, model, effort)` cell, direct capacity `effective_weekly_budget_usd / est_usd`, per-draw calibrated prior scale with geometric-mean category-default cold-start fallback, and unknown-effort matching (`Some(e)` matches `Some(e)` + `None`; `None` matches all efforts for `(category, model)`).
  Rationale: At live within-cell dispersion (`σ ≈ 0.65–0.95`), arithmetic mean has 91–97% efficiency and is unbiased without assuming lognormality. `K ≈ 3` matches the relative variance ratio (`~1.25 / ~0.5`) and avoids the +24–96% wrong-prior bias seen at `K = 10–40`. Including all terminal sessions (`stopped`, `error`, `lost`) shares one population with `category_burn_observations` and avoids dropping the upper tail entirely on cap-interrupted runs. Direct `budget / est_usd` works cleanly across varying per-draw scales, and the geometric-mean cold-start fallback preserves `master`'s exact relative draw weights while centering cold-start reservations on the category default (critical for `primary`, where sessions rarely stay single-effort).
  Date/Author: 2026-11-23 / MJ Advisor + Parent agent.

- Decision: Accept the L3 strict no-regression gate failure (`+0.20 to +0.59 pp` 5h interruptions vs `B_7dR` alone) without adding a new throttling mechanism.
  Rationale: The gate as written priced shifted traffic onto small 5h-capped pools at zero. At `σ = 0.9` in L3 A (800 tasks/week), `+0.45 pp` is ~3.6 extra interrupted tasks/week in exchange for ~`$147`/week (~42 `subtle` tasks) shifted off exhausted Codex pools onto Kimi and `$100` Claude. Moreover, live `subtle` matches L2 (no `$100` Claude pool in sub-agent draws), where 5h interruptions stay ~`0.10%` (`+0.056 pp`) while Kimi stranding drops `68.7% -> 46.9%`. Compared to `B0` (before today's fixes), 5h interruptions drop `-1.77 to -2.48 pp` and World B weekly interruptions drop 9x.
  Date/Author: 2026-11-23 / MJ Advisor + Parent agent.

- Decision: Log each observed cell's `n`, `observed_mean_usd`, `prior_usd`, and `estimate_usd` at `tracing::debug!` during the hourly refresh so pool-share shifts can be explained directly from daemon logs.
  Rationale: Advisor condition for operational observability without adding new UI/API surface.
  Date/Author: 2026-11-23 / MJ Advisor + Parent agent.

## Outcomes & Retrospective

All three milestones implemented and validated without SQLite migrations, config schema changes, or wire-protocol changes:
- **Commit 1 (`81cd75fe4`, merged `f643b6f0f`):** Fixed the active 10x tick-scale inflation (`1e9 -> 1e10` via `USD_TICKS_PER_DOLLAR`) affecting `subtle` reservations once `N >= 200`.
- **Commit 2 (`621cf999a`, merged `e953ce36c`):** Fixed the 1.6–2.5x downward selection bias and 90-minute amnesia on `five_hour_to_weekly_ratio` via one unbiased accumulator and a per-`(profile_id, cache_identity)` 7-day `RatioLedger`.
- **Commit 3 (Milestone 3):** Unified subscription capacity (`effective_weekly_budget / est_usd`) and runway reservations on one `K = 3` empirical `(category, model, effort)` cost estimator capped at 500 terminal sessions per cell, with per-draw calibrated benchmark priors, geometric-mean cold-start preservation, unknown-effort matching, placement `(model, effort)` persistence, cross-category scale isolation, and hourly `debug` logging of observed cell estimates.

Remaining limitations documented transparently:
- Live `straightforward` volume (~600 tasks/week × `$0.353` Grok/low ≈ `$212`/week) is ~15% of Grok's `$1,415`/week budget; draining Grok requires offering Grok to a heavier category (`subtle` or `primary`).
- `kimi-code/k3` is already in `pricing::rates()`, while `kimi-code/k3.1-pro` is deferred until configured.
- Shifting traffic onto small 5h-capped pools trades a small increase in 5h interruptions vs `B_7dR` alone (`+0.20 to +0.59 pp` on L3 stress; `+0.056 pp` on live-matching L2) for substantial weekly quota recovery, while staying `-1.77 to -2.48 pp` below pre-fix `B0`.

## Context and Orientation

Milestone 3 touches four files:
1. `mj-store/src/database/usage.rs`: turn pricing (`estimate_model_cost`), `RecentPlacementRow` / `recent_placements_since`, `category_burn_observations`, and the new `load_cell_cost_observations`. Re-exported via `pub use usage::*;` in `mj-store/src/database.rs`.
2. `mj-accounts/src/quota_telemetry.rs`: `effective_weekly_budget_usd`, `QuotaTelemetrySnapshot` (`cell_costs`, `cell_cost_observation`, `qualifying_placements`), `RecentPlacement` (adding `model`, `effort`), `refresh_category_stats_from_store`, and hourly debug logging.
3. `mj-accounts/src/profile_scheduler.rs`: `benchmark_task_usd`, per-draw calibrated `prior_scale` with geometric-mean cold-start fallback, `shrink_cell_cost_usd` (`K = 3`), direct `effective_weekly_budget / est_usd` capacity, and scheduler-owned reservation pricing.
4. `mj-daemon/src/daemon/delegation.rs`: pass `state.power_ranking.snapshot()` to `log_cell_cost_estimates` on the hourly `category_tick`.

## Plan of Work

### Milestone 1 (Completed — `81cd75fe4` / `f643b6f0f`)
Exported `USD_TICKS_PER_DOLLAR = 10_000_000_000` in `mj-core/src/usage.rs` and fixed `mj-store/src/database/usage.rs:681` to divide by `USD_TICKS_PER_DOLLAR as f64`, with hard-won regression test `category_burn_observations_scales_usd_ticks_by_ten_billion`.

### Milestone 2 (Completed — `621cf999a` / `e953ce36c`)
Extracted shared unbiased `accumulate_ratio_deltas` (including clean zero-delta pairs while excluding reset-corrupted pairs) and added per-`(profile_id, cache_identity)` 7-day `RatioLedger` (`RATIO_LOOKBACK_SECONDS = 7d`, hourly rehydration, incremental 90m folding without double-counting, quiet-window `Short5h` retention, atomic pruning) in `mj-accounts/src/quota_telemetry.rs`.

### Milestone 3: Unified Empirical Cell Cost (`Arith_K3_AllTerm`)

1. **`mj-store/src/database/usage.rs`:**
   - Extract `turn_cost_ticks(usage: &TokenUsage, model: Option<&str>) -> Result<Option<u64>>`: ignore non-`UsageScope::Turn` rows, prefer non-partial `provider_details.cost.usd_ticks`, else fall back to `estimate_model_cost`. Share between `category_burn_observations` and `cell_cost_observations`.
   - Extract `canonical_observed_category(category: &str) -> Option<&'static str>` mapping `"supervisor"` -> `"orchestrator"`.
   - Add `CellCostKey { category, model, effort }` and `CellCostObservation { total_api_usd, sample_count }`, plus `load_cell_cost_observations(max_per_cell)` / `load_cell_cost_observations_from(path, max_per_cell)`:
     - Scan categorized terminal sessions (`state IN ('stopped', 'error', 'lost')`) newest-first (`sc.created_at DESC, s.session_id DESC`).
     - Require exactly 1 distinct normalized model (`mj_core::usage::pricing::normalize_model`) and `<= 1` distinct normalized non-empty effort across all `session_turn_selections` for that session (`NULL`/empty ignored when checking distinctness; `NULL`-only lands in `effort: None`).
     - Short-circuit `session_turn_usage` lookup when all of a session's category cells have reached `max_per_cell`.
     - Accumulate priced session USD (`ticks / USD_TICKS_PER_DOLLAR`) into each needing cell (up to `max_per_cell = 500` per cell).
   - Add `model: Option<String>` and `effort: Option<String>` to `RecentPlacementRow`, populated in `recent_placements_since` from the latest non-null `session_turn_selections` values for `sc.session_id`.

2. **`mj-accounts/src/quota_telemetry.rs`:**
   - Add `LOCAL_CELL_MAX_SAMPLES = 500`.
   - Extract `pub fn effective_weekly_budget_usd(weekly_budget_usd, p_eff, plan_usd_per_month) -> Option<f64>` and reuse it in `mean_burn_weekly_pct` and `profile_scheduler.rs`.
   - Store `cell_costs: BTreeMap<CellCostKey, CellCostObservation>` in `QuotaTelemetryStoreInner` and `QuotaTelemetrySnapshot`.
   - Add `QuotaTelemetrySnapshot::cell_cost_observation(category, model, effort)`: when `effort` is `Some(e)`, combine `(cat, norm_model, Some(norm_e))` + `(cat, norm_model, None)`; when `effort` is `None`, combine all efforts for `(cat, norm_model)`.
   - Add `model: Option<String>` and `effort: Option<String>` to `RecentPlacement` (populated from the chosen candidate in `draw_and_reserve` and from `RecentPlacementRow` in `publish_store_refresh`).
   - Add `QuotaTelemetrySnapshot::qualifying_placements(profile_id, second_to_last_clean_ts)` so `profile_scheduler.rs` owns reservation pricing.
   - Update `refresh_category_stats_from_store` to load and atomically publish both `category_stats` and `cell_costs`.
   - Add `QuotaTelemetryStore::log_cell_cost_estimates(&self, power_ranking: &PowerRankingSnapshot)` emitting `tracing::debug!` per observed cell (`category`, `model`, `effort`, `n`, `observed_mean_usd`, `prior_usd`, `estimate_usd`).

3. **`mj-accounts/src/profile_scheduler.rs`:**
   - Constants: `COST_PRIOR_WEIGHT_K = 3.0`, `WEEKS_PER_MONTH = 4.345`.
   - `benchmark_task_usd(lookup)`: `(weekly_budget_usd * WEEKS_PER_MONTH) / (tasks_per_200_sub * (plan_usd_per_month / 200.0))`, falling back to `200.0 / tasks_per_200_api`.
   - Per-draw calibrated `prior_scale` over distinct `(norm_model, norm_effort)` candidate cells:
     - If any distinct candidate cell has observations (`n > 0` and `bench_usd > 0`): `sum(obs.total_api_usd) / sum(obs.sample_count * bench_usd)`.
     - Else if any candidate has `bench_usd > 0` (cold start): `cat_stats.mean_api_usd / geom_mean(bench_usd)`. With no observed candidate, relative weights equal `master`'s, and reservations are centered on the category default rather than equal to it per model.
     - Else: `1.0`.
   - `est_usd = ((3.0 * prior_usd) + obs.total_api_usd) / (3.0 + obs.sample_count)` when observed, else `prior_usd` (where `prior_usd = prior_scale * bench_usd`, or `cat_stats.mean_api_usd` if unranked).
   - Direct capacity: `effective_weekly_budget_usd / est_usd` (falling back to `tp200 * (p_eff / 200.0) / (prior_scale * WEEKS_PER_MONTH)` when `weekly_budget_usd` is absent, and `peer_geom_mean_capacity` for unpriced peers, computed in the same units).
   - Reservations: `new_burn` uses `cand_est_usd`; each qualifying placement on `cand.profile_id` uses the same `est_usd` estimator for same-category placements (or `cand_est_usd` if `placement.model` is `None`), and uses the placement's own category's `category_stats.mean_api_usd` as prior shrunk with `K = 3` toward its cell observation for cross-category placements (never applying another category's `prior_scale`).

4. **`mj-daemon/src/daemon/delegation.rs`:**
   - On `category_tick` right after `telemetry.refresh_category_stats_from_store()`, call `telemetry.log_cell_cost_estimates(&power_ranking_snapshot)`.

## Concrete Steps

All commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/86ad42e5c32d5c8c6ff5741425b72742`, outside the restricted sandbox for `cargo test`:

    cargo test -p brokk-mj-store usage
    cargo test -p brokk-mj-accounts quota_telemetry
    cargo test -p brokk-mj-accounts profile_scheduler
    cargo test -p brokk-mj-daemon delegation
    cargo clippy -p brokk-mj-store -p brokk-mj-accounts -p brokk-mj-daemon --all-targets -- -D warnings

## Validation and Acceptance

1. All targeted test filters pass with non-zero matched counts and zero clippy warnings (`-D warnings`).
2. Tricky-condition tests pin:
   - Per-cell capping, terminal-session inclusion (`stopped`/`error`/`lost`, excluding `running`), `provider_details.cost` vs `is_partial` vs `LastRequest` handling, effort normalization (`NULL` + `high` -> `high`, `NULL`-only -> `None`, mixed -> skipped), and `RecentPlacementRow` `(model, effort)` loading.
   - Unknown-effort matching (`Some(e)` matches `Some(e)` + `None`; `None` matches all efforts; provider-qualifier normalization).
   - Cold-start preservation: relative weights match `master`'s `tp200 * (p_eff / 200.0)` and reservations are geometrically centered on the category default.
   - Learned cost unification: capacity weight updates directly as `effective_weekly_budget / est_usd`, reservations use the same `est_usd` (tripping the 5h gate on expensive pools), and cross-category placements do not inherit the current draw's `prior_scale`.
3. No SQLite schema migrations, no config changes, and no protocol changes.

## Idempotence and Recovery

Pure query and in-memory telemetry/scheduler changes. Rollback is a clean `git revert`.

## Artifacts and Notes

- `.mj/agents/ba4387929914ad5ac9e07cf4dee5bd41/live-burn-and-target-mix.md`
- `.mj/agents/24dc7531b56d90175fe77d993d22e4ae/observed-session-cost-coverage.md`
- `.mj/agents/a23f4b46bcbc3a2b4bb64cf335a376aa/advisor-followup-checks.md`
- `.mj/agents/a227080b94f5cb694a9459dbb93aee8e/m3-live-scale-prep.md`
- `.mj/agents/ae04c6c9ddab6ffa0be7d0915faa7fac/m3-advisor-gate.md`
