# Generalized Subagent Categories, Supervisor Role, and Velocity/Value Load Balancing

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This document lives at `.agents/plans/generalized-subagents-and-value-load-balancing.md` and must be maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture


Today Mjolnir maps each subagent category (`straightforward`, `subtle`, `oracle`) to a single `(model, effort)` pair and maintains a separate `[subagents.yolo.*]` table for unconstrained targets (containers and SSH targets with `permissions = "yolo"`). Its profile scheduler ranks candidates for that single model using a coarse pace ratio (`remaining / pace`, capped at 4.0) multiplied by plan list price, a binary cumulative 5-hour burn flag rather than recent burn velocity, no cross-harness task-value normalization, no runway check against aggregate burn and pending placements, no raw-target guardian filter, and a failed-probe bug where transient network errors erase known `0%`-exhausted quota readings into `Unknown`. In addition, child sessions are hard-capped at depth 1: no subagent can delegate to worker subagents.

After this change, a user can configure multiple equal-peer `(model, effort)` pairs for each of four categories — **Primary and supervisor** (`primary`), **Straightforward** (`straightforward`), **Subtle** (`subtle`), and **Oracle** (`oracle`) — in one unified `[subagents]` section with `[subagents.yolo.*]` removed. Top-level sessions on New and Move dialogs gain a **Supervisors** checkbox (mirrored on the CLI and HTTP API) that lets a primary session spawn depth-1 supervisor subagents drawn from `primary`; those supervisors can spawn depth-2 non-supervisor workers (`straightforward` and `subtle`) and hand integrated results back to the primary, while depth-2 workers remain leaves. Primary top-level sessions can also launch automatically against `primary` without pinning a profile. Across all automatic scheduling paths (primary auto-launch, model-first `mj new`, worker/supervisor spawn, oracle, reviewers, brief writer, and quota automove), Mjolnir draws over quota pools using linear capacity weighted by `tasks_per_200` from `brokkai/bifrost-service`'s static JSON endpoint (cached 24 hours or until a new `mj` release, backed by a compiled-in baseline), a self-correcting pace exponent (`u^alpha`) with a 1% reserve floor, a single trailing-60-minute velocity and durable reservation runway gate for both 5-hour and weekly windows, value-ordered Paid and API tiers, preserved last-good quota across probe failures, and native-guardian filtering on raw guardian targets.

A user will be able to verify this end to end by configuring heterogeneous `(model, effort)` pairs under `[[subagents.primary]]` and `[[subagents.straightforward]]` in Settings (TUI or Web), launching a session with Supervisors enabled, watching it spawn a supervisor that waits on depth-2 workers without prematurely parking and refuses `handback` while depth-2 workers are live while refusing depth-3 spawns, confirming non-guardian harnesses (Kimi, OpenCode, DSH) are excluded automatically on raw guardian targets while allowed on container/YOLO targets, and running the new integration and tricky-condition tests covering failed probes after exhaustion, burst placements on a stale reading, moved deadlines, usage top-ups, and supervisor tree teardown.

## Progress


- [x] (2026-11-21 15:35Z) Completed initial codebase reconnaissance of `[subagents]`, `profile_scheduler.rs`, quota windows, `quota_history`, guardian rules, and `mj-agents` signaling.
- [x] (2026-11-21 16:06Z) Completed deep reconnaissance of depth-1 subagent gates, New/Move dialogs, Settings array editors, other-harness `monthly_price_usd` availability, `brokkai/bifrost-service` power ranking, and `session_turn_usage` / `quota_history` burn queries.
- [x] (2026-11-21 16:15Z) Drafted initial ExecPlan and submitted to Advisor (`91233b0efdd193066e7b4eeb46c8db10`) for review.
- [x] (2026-11-21 16:42Z) Received Advisor review with live-store empirical measurements (40,975 quota rows; 180,417 turns / 50,192 children), verified `ChildCompletion`, `ChildSwitch`/park cap behavior, failed-probe `Unknown` path, and `7d911d2ef`/`da321f4b6` tier history, and revised this ExecPlan accordingly.
- [x] (2026-11-21 19:04Z) Locked the three user product decisions: (1) retire legacy top-level 5h tier in favor of velocity runway gate, (2) bare `mj new` automatically load-balances from `[[subagents.primary]]` when configured, (3) restrict to a single model per provider (`QuotaProvider`) per subagent category.
- [x] (2026-11-21 19:20Z) Milestone 1 (`BrokkAi/bifrost-service`, commit `d8ab267` on `main`): Published static `/power-ranking.json` with separate `model`, `effort`, `harness`, `tasks_per_200_sub`, `tasks_per_200_api`, `weekly_budget_usd` (recorded `B_plan_weekly_usd` or derived from plan price × subscription multiple), `plan_usd_per_month`, `plan_assumed`, `basis`, `minutes`; 450 client vitest tests and production Next.js build passed.
- [x] (2026-11-21 20:06Z) Milestone 2A: Compatible SQLite migration 146 (`category` column on `session_turn_selections` and `subagent_accounting`, `minimum_compatible_version` kept at 145), failed-probe `Unknown` fix (`QuotaReportPublication.last_good` + `CandidateQuota::from_publication` + `billing(harness)`), and `PowerRankingStore` with compiled-in baseline (`d8ab267`) and 24h/`CARGO_PKG_VERSION` disk cache.
- [x] (2026-11-21 21:37Z) Milestone 2B: Multi-pair `[subagents]` categories (`primary`, `straightforward`, `subtle`, `oracle`), single-model-per-provider validation (`model_catalog::model_provider`), drop `[subagents.yolo.*]` and `use_same_on_all_targets` (`CONFIG_VERSION = 24`), Settings array editors, CLI/TUI/Web updates and goldens, multi-choice expansion, bare `mj new` `primary` LB, plus both Phase 2A review fixes (log refresh cache-write errors, expire undated exhausted `last_good` readings).
- [x] (2026-11-21 22:40Z) Milestone 2C (Phase A.5 + Phase B): Raw-target guardian filter (`ConfiguredApprovals` -> `GuardianReviewer::Harness`), pool-first `tasks_per_200` value draw (`alpha = 2.0`), retired legacy top-level 5h tier & dead scheduler paths, Paid/API tier scoring, trailing-60m velocity (`QuotaTelemetryStore` with generation-guarded refresh), durable session-derived mean reservations across all placement paths (including `start_session`, `spawn_subagent`, `oracle`, `review_host`, and quota automove), exact `cache_identity` matching, pre-draw `unhealthy` filtering, bare `mj new` `primary` effort forwarding, unified 5h/weekly runway gate (`p90` durations), and local quantile switchover at `N >= 200`.
- [x] (2026-11-22 01:30Z) Milestone 4 (Mjolnir Phase C): Supervisor role (depth-2 delegation, `SubagentMcpRole::Supervisor`, `ChildCompletion::AwaitingChildren`, `handback` live-children refusal, `live_supervisors < max_concurrent / 2` anti-starvation rule on spawn + unpark, breaking store migration 147, `PROTOCOL_VERSION` bump, New/Move Supervisors checkbox + working `"Auto (Primary)"` across CLI/TUI/Web, tree-wide occupancy/suspend/Move walks), atomic select-and-reserve with RAII `PlacementReservation` and `session_id`-deduplicated `refresh_from_store`, plus end-to-end live `tmux` verification on real Gemini (`antigravity`) models.

## Surprises & Discoveries


- Observation: Live-store measurements across 2026-11-03..21 (40,975 `quota_history` rows; 180,417 turns; 50,192 children) showed that the workload is heavily capacity-saturated, not lightly utilized: 23 of 27 unscoped profile-weeks ended at `0%` and sat at `0%` for a median 124 hours before reset; only 4 of 27 stranded quota. Active Claude $200 and Codex $500 pools burn at 6 %/h median, 12 %/h p90 (max 36 %/h), exhausting a fresh weekly pool in roughly a day. Consequently, Paid and API tiers serve traffic for most of each week.
  Evidence: Advisor empirical reports `5edc633dc75943375e020bbe3050d6fd/report.md` and `c4ecd0f90d14c80a4f0d25104e36eebe/report.md` from session `91233b0efdd193066e7b4eeb46c8db10`.

- Observation: Five-hour windows appeared only on Claude $100, Kimi, Antigravity, and Muse (never on Codex, and not on Claude $200 after Nov 12). On active 5-hour windows, steps are 2 points median, 6 p90, 39 max per 10-minute reading; 15% of 5-hour buckets reached `<= 1%` with a median 1.1 h left. Observed 5h-to-weekly capacity ratios vary drastically across providers: 2.5x (Kimi), 3.2x (Antigravity), and 9x–32x (Claude). Usage can drop within a single deadline (top-ups up to 100 points), Codex deadlines shift by hours or days, and Antigravity deadlines advance while unused.
  Evidence: Advisor quota-history analysis (`5edc633dc75943375e020bbe3050d6fd/report.md`).

- Observation: Probe errors affect 3–48% of Claude readings (`claude6` 48%, `claude4` 13%) and 2.3% of Codex readings, and error rows carry empty windows (0 of 3,527). While `QuotaManager::probe` (`mj-accounts/src/quota.rs:400-413`) retains previous good windows on 429 via `hold_report`, any non-429 network error overwrites `publication.report` with empty windows and `error: Some(...)`, which causes `ProfileQuota::billing_class()` (`mj-client/src/quota.rs:177-186`) to return `Unknown`. Exhausted 0% pools therefore bounce into `Unknown` and outrank weekly-risk, paid, and API profiles. In addition, `CandidateQuota::from_report` does not fall back to `CandidateQuota::billing(harness)` for usage-priced harnesses without reports.
  Evidence: Verified in `mj-accounts/src/quota.rs:225-237, 400-413, 1059-1076`, `mj-store/src/database/quota_cache.rs:59-96`, `mj-client/src/quota.rs:177-186`, and `mj-accounts/src/profile_scheduler.rs:22-50`.

- Observation: Burst placements between 10-minute polls are large (12 median, 31 p90, 56 p99, 90 max new Codex child sessions per 10-minute gap), and task costs are extremely heavy-tailed (p90/p50 is 11–13x, p99/p90 is 6–8x). Measured API USD (mean / p50 / p90 / p99) and wall duration (p50 / p90): straightforward Luna/high workers $0.12 / $0.02 / $0.24 / $2.01 (4 / 71 min); Sol workers $3.47 / $0.74 / $8.41 / $50.20 (16 / 166 min); reviewers $0.22 / $0.06 / $0.75 / $1.63 (`<= 6` min). On a Codex $500 week, a single task's p90 burn is tiny (0.008% for Luna, 0.28% for Sol, 0.025% for reviewer), so aggregate burn rate — not a single-task p90 threshold — is what binds weekly exhaustion.
  Evidence: Advisor workload analysis (`c4ecd0f90d14c80a4f0d25104e36eebe/report.md`).

- Observation: For predicting 5-hour velocity, trailing 60-minute difference, 30/60/120-minute EWMA kernels, and linear fits all achieve nearly identical MAE (7.7–7.9 percentage points, vs 8.5 for cumulative pace and 17.4 for zero-burn), whereas projecting `v * t_left` 3–5 hours to window end has 16.9-point MAE and only 55–65% exhaustion precision. Projecting over `min(d_category, t_left)` using plain trailing-60-minute difference plus recent placement reservations is simpler, more accurate (3.7-point MAE at 0.5–1h), and avoids double counting.
  Evidence: Advisor velocity backtest on live `quota_history` (`5edc633dc75943375e020bbe3050d6fd/report.md`).

- Observation: Online per-profile `pct_per_api_usd` calibration is biased because the user runs multiple daemons/hosts sharing the same logins (`quota_history` reflects all consumers while SQLite `session_turn_usage` sees only one daemon), and several models (Fable, Muse, Gemini) lack pricing or token counters. Meanwhile, `brokkbench26/bt/ranked.csv` already carries `B_plan_weekly_usd` (Claude Max 20x $2,320 API-equivalent/week; ChatGPT Pro $200 $1,185/week — matching the 2026-11-10 offline fit $2,281), which can be emitted directly in `/power-ranking.json`.
  Evidence: Inspected `BrokkAi/bifrost-service` `client/data/power-ranking/ranked.csv` columns and Advisor review §3.

- Observation: Offline 20-seed × 80-config Monte Carlo simulation (`simulate_lb.py`, report `f2f5e83d632c67114f5e91f70cb700ac/lb_sim_report.md`) confirmed that durable mean-burn reservations (`reserved = on`) and the unified runway gate cut mid-task exhaustions across both saturated (1.79x load) and moderate (0.80x load) regimes with <=0.30% premature API spill. Comparing top configs: `alpha = 2.0, d = p90, reserved = on` achieved the lowest moderate mid-task exhaustion (0.27% vs 0.44% at `alpha = 1.5 / p75` and 0.85% with no gate), fewest 5-hour exhaustions (20.9 saturated / 5.1 moderate vs 22.2 / 7.2 at `1.5 / p75`), tighter $20 vs $500 pace tracking, and near-zero sensitivity to 2x price belief errors on an unpriced harness (-0.04 pp saturated exhaustion), with saturated exhaustion within noise of rank 1 (4.40% vs 4.25%, seed SD 0.71%).
  Evidence: `/var/lib/hel/profiles/811f71013aa31834aae114b250c70c8b/antigravity-cli/brain/53afbfe3-2691-4c27-9bb1-3bc358cd9779/scratch/simulate_lb.py` and `.mj-agents/f2f5e83d632c67114f5e91f70cb700ac/lb_sim_report.md`.

- Observation: `B_plan_weekly_usd` in `ranked.csv` is populated on the 14 `plan`/`observed` rows and blank on the 33 `model-measured`/`model-pooled`/`model-inferred` rows (such as `opus-5.5/xhigh` and `gpt-6.1-sol/high`), which carry `subscription_value_multiple` instead. Milestone 1 (`d8ab267` in `BrokkAi/bifrost-service`) derives `weekly_budget_usd = round(plan_usd_per_month * (tasks_per_200_sub / tasks_per_200_api) * (7 / 30.4375), 2)` whenever `B_plan_weekly_usd` is blank and all three inputs are positive, filling 42 of 47 rows (`opus-5.5/xhigh` → $2,281.06, `gpt-6.1-sol/high` → $887.76, matching the 2026-11-10 offline fit).
  Evidence: `BrokkAi/bifrost-service` commit `d8ab267` (`client/app/power-ranking.json/route.test.ts`, 450 vitest tests + production Next.js build passing).

- Observation: Only Claude (`rateLimitTier` → $20/$100/$200 in `mj-accounts/src/claude_usage.rs:882-891`) and Codex (`planType` → $8/$20/$100/$200/$500 in `mj-accounts/src/codex_usage.rs:700-718`) have clean plan-to-price fields. Antigravity, Grok, Kimi, Muse, and Z.ai expose no closed plan-to-price mapping.
  Evidence: Audited every quota parser and credential reader in `mj-accounts/`.

- Observation: `ChildCompletion` in `mj-daemon/src/daemon/delegation.rs:614-788` only exempts a child from handback reminder, parent finish notice, and parking when `child_review().holds()` (`AwaitingReview`) or a handback is queued (`AwaitingHandback`). Without an `AwaitingChildren` state, a supervisor whose turn ends while waiting on depth-2 workers is reminded and parked — and `enqueue_subagent_reports` (`mj-store/src/database/delegation.rs:421-428`) writes `SubagentFinished` with `unpark: false`, so the supervisor would never wake when its workers finish.
  Evidence: Verified in `mj-daemon/src/daemon/delegation.rs:19-49, 455-458, 614-788`, `mj-daemon/src/server_runtime/api.rs:972-983, 1607-1631, 2249-2252`, and `mj-store/src/database/delegation.rs:421-428`.

- Observation: `Plan::ChildSwitch` (`mj-daemon/src/daemon/subagent_park.rs:277-410`) stops and unparks only the child's own worker root while preserving its session ID, relay journal, borrowed container target, and all `subagent_sessions` links, leaving depth-2 workers undisturbed on their own worker roots. Its unpark bypasses `max_concurrent`, whereas `send_message` to a parked child enforces `ensure_subagent_slot_available` (`api.rs:1703-1722`) and returns `slot_refusal` when full.
  Evidence: Verified in `mj-daemon/src/daemon/subagent_park.rs:277-410` and `mj-controller/src/controller/subagent_park.rs:396-566`.

- Observation: Commit `ef74ec25c` (2026-11-20) replaced `HarnessKind::supports_guardian_approvals()` with `HarnessKind::guardian()` (`mj-core/src/config/harness.rs:1350-1357`: `Harness` for Codex, Claude, Grok, Muse, Antigravity; `Mjolnir` for Kimi, OpenCode, DSH). The scheduler currently receives no target or `ExecutionPolicy` and applies no guardian filter at all. Separately, commit `7d911d2ef` (revised by `da321f4b6` on 2026-11-18) intentionally placed top-level profiles with an active 5-hour window into Tier 2 (below unwindowed subscriptions and `Unknown`, above Paid and API) to save 5-hour-windowed quota for subagents.
  Evidence: Verified via `git show ef74ec25c`, `git show 7d911d2ef`, `git show da321f4b6`, and `mj-accounts/src/profile_scheduler.rs:278-300`.

## Decision Log


- Decision: Lock `alpha = 2.0`, runway duration quantile `p90` (`71 min` straightforward, `166 min` subtle, `6 min` reviewer, `15 min` oracle, `180 min` primary/supervisor), and durable mean-burn reservations (`reserved = on`) based on the Milestone 3.1 20-seed offline simulation.
  Rationale: `alpha = 2.0, p90, reserved = on` achieved the lowest moderate mid-task exhaustion (0.27%) and fewest 5-hour exhaustions (20.9 sat / 5.1 mod) while keeping saturated exhaustion (4.40%) within 0.15 pp of `alpha = 1.5 / p75` (4.25%, seed SD 0.71%), tightening $20 vs $500 pace balance, and halving distortion from 2x unknown-price errors.
  Date/Author: 2026-11-21 / Coordinator

- Decision: Exclude proactive handoff-before-weekly-exhaustion from this plan; retain existing same-harness quota automove (`Plan::Move` and `Plan::ChildSwitch`) and let parents restart subagents as needed.
  Rationale: User decision (item 5). Keeps scope focused and avoids adding cross-harness handoff machinery while existing automove continues to handle same-harness weekly stops.
  Date/Author: 2026-11-21 / User & Coordinator

- Decision: Generalize `[subagents]` to four equal-peer `Vec<SubagentChoice>` categories (`primary`, `straightforward`, `subtle`, `oracle`), drop `[subagents.yolo.*]` and `use_same_on_all_targets` on load (printing one upgrade notice when legacy `yolo` entries are dropped), and bump `CONFIG_VERSION` from `23` to `24`.
  Rationale: User decisions (items 1, 2, 7) plus Advisor note on emitting a single visible notice when old `yolo` config keys are dropped.
  Date/Author: 2026-11-21 / User, Advisor & Coordinator

- Decision: Draw over **quota pools** `(profile_id, binding_window_scope)` rather than over `(profile, model, effort)` tuples, using linear capacity and putting the exponent strictly on the pace ratio: `weight = tasks_per_200 * (P_eff / 200) * u_long^alpha * min(1, u_5h)` (starting with `alpha = 2.0`, validated via offline simulation before locking).
  Rationale: Advisor review §1 proved that raising `(R_tasks / t_left)` to `alpha > 1` raises capacity to `alpha` too, starving small plans ($20 vs $500) and low-`tasks_per_200` models while holding the largest pools pinned at the exhaustion edge; and drawing over `(profile, model, effort)` triples a profile's weight when three of its models are listed. Linear capacity keeps equal-pace traffic proportional to real task capacity; `alpha = 2` makes equal pace self-correcting, drains expiring quota toward the 1% floor, and halves sensitivity to uncertain capacity estimates on unpriced harnesses.
  Date/Author: 2026-11-21 / Advisor & Coordinator

- Decision: Replace EWMA velocity, online `pct_per_api_usd` calibration, Empirical Bayes blending, and `n_inflight * 0.5 * b_p90` with: (a) plain trailing-60-minute `quota_history` difference (dropping usage drops and shifted deadlines), (b) durable session-derived recent placement reservations (sum of mean burn for this daemon's placements newer than the second-to-last clean reading), (c) one unified runway gate `rem_eff >= v * min(d_category, t_left)` for both 5-hour and weekly windows, and (d) `B_plan_weekly_usd` from `/power-ranking.json` plus live-store measured category defaults (switching to local store quantiles only once a category accumulates at least 200 completed tasks).
  Rationale: Advisor review §§2–3 proved from live-store data that EWMA gives no accuracy gain over trailing-60-min diff (7.7–7.9 MAE), online `pct_per_api_usd` is biased by shared multi-daemon logins, summing p90 across 12–90 placements over-reserves by ~5–10x whereas summing the mean is unbiased, and deriving reservations from durable session records obeys AGENTS.md's control-plane rule (survives daemon restarts with zero ephemeral memory state).
  Date/Author: 2026-11-21 / Advisor & Coordinator

- Decision: Fix the failed-probe → `Unknown` bug by retaining `last_good: Option<ProfileQuota>` on `QuotaReportPublication` (seeded on startup from `quota_reset_cache`) until its windows reset, and route usage-priced harnesses without reports to `UsagePriced` via `CandidateQuota::billing(harness)`. Score Paid and API tiers using `tasks_per_200_api` (and credit balance for Paid).
  Rationale: Advisor review §4 and code verification showed that 3–48% of Claude probes fail with non-429 errors, turning 0%-exhausted pools into `Unknown` where they outrank Paid and API profiles — and Paid/API tiers handle most of each week's workload in this capacity-saturated regime.
  Date/Author: 2026-11-21 / Advisor & Coordinator

- Decision: Retire the legacy top-level 5-hour tier rule (`da321f4b6` / `7d911d2ef`) in favor of the unified velocity runway gate and `min(1, u_5h)` damping.
  Rationale: User decision (2026-11-21: "1. velocity based"). Removes the separate top-level active-5h demotion tier so top-level and child placements share one velocity- and value-driven tier ladder.
  Date/Author: 2026-11-21 / User & Coordinator

- Decision: When `[[subagents.primary]]` is configured, bare `mj new` (no `--profile` and no `--model`) automatically load-balances across `[[subagents.primary]]` (and TUI/Web New dialogs offer `"Auto (Primary)"` as the load-balanced default when `primary` is non-empty; explicit `--profile` pins and explicit `--model` launches keep working unchanged).
  Rationale: User decision (2026-11-21: "2. lb"). Makes "Primary and supervisor" immediately serve top-level primary sessions via load balancing without extra flags.
  Date/Author: 2026-11-21 / User & Coordinator

- Decision: Restrict each subagent category to at most a single model per provider (`QuotaProvider`: Anthropic, OpenAI, Google, xAI, Moonshot, DeepSeek, Z.ai, Meta, or profile's `quota_provider()` for custom models), enforced in `Config::validate` and Settings validation, while keeping pool-first drawing in `profile_scheduler.rs`.
  Rationale: User decision (2026-11-21: "3. restrict to single model per profile per subagent type" clarified to "model per provider per sub type, that is"). Eliminates ambiguity about choosing between e.g. Opus and Sonnet on Claude or Sol and Luna on Codex within one category, lets `Config::validate` catch conflicts immediately without waiting for discovery, and naturally allows pairing e.g. Anthropic + OpenAI + Google models across profiles (including disjoint Antigravity `gemini` vs `claude,gpt` pools). If a multi-provider pool ever offers two entries from distinct providers, the pool keeps one draw weight and selects the higher `tasks_per_200` entry.
  Date/Author: 2026-11-21 / User & Coordinator

- Decision: Keep `monthly_price_usd` inference limited to Claude and Codex; fall back to `power_ranking.plan_usd_per_month` (then geometric mean capacity of priced peers in the draw, never a hardcoded $100), and surface assumed prices in selection diagnostics. Compile a baseline power-ranking snapshot into `mj` alongside the 24h / `CARGO_PKG_VERSION` disk cache.
  Rationale: User decision 3a/3b and Advisor review §5.
  Date/Author: 2026-11-21 / User, Advisor & Coordinator

- Decision: Specify the Supervisor lifecycle completely before implementing Milestone 4: add `ChildCompletion::AwaitingChildren` (preventing premature reminders, parent notices, and parking while depth-2 children are live, rebuilt deterministically from SQLite on daemon restart), refuse supervisor `handback` while direct children are live (naming them), cap live supervisors at `max_concurrent / 2` (requiring `max_concurrent >= 2`) inside the container-wide `max_concurrent` worker cap to prevent nested slot starvation, hide the New/Move Supervisors checkbox whenever no `primary` entry can delegate on that target, and default `SessionRecord.supervisors` to `false` (remembered per profile).
  Rationale: Advisor review §5 and `verify-advisor-gaps` confirmed that without `AwaitingChildren` and the supervisor sub-cap, supervisors park and stall permanently (because `SubagentFinished` has `unpark: false`) or deadlock their own depth-2 workers at a full cap.
  Date/Author: 2026-11-21 / Advisor & Coordinator

- Decision: Enforce raw-target guardian filtering in the scheduler using today's native-guardian predicate (`harness.guardian() == GuardianReviewer::Harness`, excluding Kimi, OpenCode, and DSH whenever `ExecutionPolicy::ConfiguredApprovals` applies) across all automatic scheduling paths, while allowing explicit user-pinned `--profile` launches.
  Rationale: User decision (item 6: "same rule as today, for now (mj guardian is super half baked)").
  Date/Author: 2026-11-21 / User & Coordinator

- Decision: Adopt the four-milestone phased order: (1) `bifrost-service` `/power-ranking.json`; (2) Mjolnir Phase A (categories, YOLO drop, compatible `category` column so data starts accruing immediately, power-ranking baseline + cache, `Unknown` fix, guardian filter, pool-first value draw, Paid/API tier scoring); (3) Mjolnir Phase B (offline simulation + trailing-60m velocity + durable reservations + unified runway gate + local quantile switchover at `N >= 200`); (4) Mjolnir Phase C (Supervisor role, `AwaitingChildren`, breaking store migration, New/Move checkbox, tree-wide walks).
  Rationale: Advisor review suggested order: delivers high-value scheduling and category improvements immediately, starts accruing `category` data ahead of the breaking supervisor migration, validates `alpha`/duration thresholds against real distributions before locking them, and isolates the highest-risk state-machine work (supervisors) into its own clean milestone.
  Date/Author: 2026-11-21 / Advisor & Coordinator

## Outcomes & Retrospective


All four milestones are implemented, validated across unit/integration/tricky-condition/golden/Playwright suites, and verified live in `tmux` using real Gemini (`antigravity`) profiles:

- **Milestone 1 (`BrokkAi/bifrost-service`, commit `d8ab267` on `main`):** Published static `/power-ranking.json` with derived `weekly_budget_usd` (42/47 rows populated), 450 vitest tests and production build passing.
- **Milestone 2A (`3c9eab7`):** Compatible SQLite migration 146 (`category` column, `minimum_compatible_version` kept at 145), failed-probe `last_good` preservation + usage-priced routing, and `PowerRankingStore` with compiled-in baseline + 24h/`CARGO_PKG_VERSION` disk cache.
- **Milestone 2B (`ebd8615`):** `CONFIG_VERSION = 24`, four equal-peer `Vec<SubagentChoice>` categories (`primary`, `straightforward`, `subtle`, `oracle`), single-model-per-provider validation, removal of `[subagents.yolo.*]` and `use_same_on_all_targets` with once-per-path `WARN` upgrade notice, Settings array editors, CLI/TUI/Web surfaces.
- **Milestone 2C (`a61b6cf`):** Pool-first `tasks_per_200` value draw (`alpha = 2.0`), retired legacy top-level 5h tier & dead scheduler paths, Paid/API tier scoring, `QuotaTelemetryStore` trailing-60m velocity + durable session-derived mean reservations, unified 5h/weekly runway gate (`p90` durations, local switchover at `N >= 200`), and raw-target native guardian eligibility filtering across all automatic placement paths.
- **Milestone 4 + Concurrency & Live Fixes:** Depth-2 Supervisor role (`SubagentTaskType::Supervisor`, `SubagentMcpRole::Supervisor`, `SUPERVISOR_INSTRUCTIONS`), breaking SQLite migration 147 (`sessions.supervisors`, `supervisor_preference`, `minimum_compatible_version = 147`), `PROTOCOL_VERSION` bump, `ChildCompletion::AwaitingChildren`, live-child `handback` refusal, container-wide `max_concurrent` + `max_concurrent / 2` anti-starvation cap (spawn + unpark), tree-wide occupancy/suspend/Move descendant walks, New/Move Supervisors checkbox + working `"Auto (Primary)"` across CLI/TUI/Web, atomic `QuotaTelemetryStore::draw_and_reserve` with RAII `PlacementReservation` and `session_id`-deduplicated `refresh_from_store`.
- **Live `tmux` verification (isolated `--instance`, Gemini models):** Verified v23→v24 upgrade + single WARN notice + clean v24 save, live TUI Settings 4-category view with discovered Gemini models, bare `mj new` without saved defaults, live TUI New wizard `Auto (Primary)` + `Mjolnir` + `☒ Supervisors` session creation (with `gemini-4-neon` + `effort = low` applied), live Move wizard `☒ Supervisors`, and a complete Primary → Depth-1 Supervisor (`sup-live-check`, load-balanced to `gemini-b`) → Depth-2 Worker (`leaf-live-check`, load-balanced to `gemini-a`, wrote `supervisor-verified.txt`) chain with `AwaitingChildren`, `SubagentFinished`, `handback`, and `category` accounting (`primary`, `supervisor`, `straightforward`). Live testing caught two real bugs (TUI `start_create_session` empty `profile_id` and bare `mj new` pre-check without saved preferences), both fixed and re-verified live before review.

## Context and Orientation


Mjolnir is a Rust workspace (`Cargo.toml`, currently version `2.73.0`) split into a control-plane daemon (`mj-daemon` over `mj-controller`, `mj-server`, `mj-store`, `mj-accounts`, `mj-profile-catalog`, `mj-core`, `mj-client`) and a data-plane worker (`mj-worker`, `mj-worker-bin`, `mj-worker-launch`, `mj-worker-staging`). User surfaces are the CLI (`mj-cli`), terminal UI (`mj-tui`), and browser UI (`mj-server/src/web/viewer.js`, using snapshot types in `mj-server/src/server/viewer_types.rs` and `viewer_wire.rs`). A separate private repository, `BrokkAi/bifrost-service`, hosts the Next.js site at `https://slopcop.com`, including the statically prerendered `/power-ranking` page.

Key terms used in this plan:

- **Profile:** A configured harness account under `[profiles.<id>]` in `config.toml` (`HarnessProfile` in `mj-core/src/config/harness.rs`), with a `kind: HarnessKind` (`codex`, `claude`, `kimi`, `grok`, `muse`, `opencode`, `dsh`, `antigravity`), `enabled: bool`, and optional `load_balancing_may_consume_credits: bool`.
- **Quota pool:** One independent quota allowance on a profile, identified by `(profile_id, binding_window_scope)` — for example, a Claude profile's unscoped `Week` pool (shared by Opus and Sonnet), a Claude profile's scoped `Week (Fable)` pool, or an Antigravity profile's `gemini` versus `claude,gpt` pool. Drawing over pools ensures that listing three models sharing one pool does not multiply that pool's draw probability.
- **Category (task type):** A named agent role under `[subagents]` holding a list of equal-peer `(model, effort)` pairs (`SubagentChoice`): `primary` ("Primary and supervisor"), `straightforward`, `subtle`, and `oracle`. Review slots (`[[review.reviewers]]` and `[review.brief_writer]`) live on `[review]` and call the same scheduler.
- **Supervisor:** A depth-1 child session (`SubagentKind::Worker` with `supervisor: true`) spawned by a top-level session with `task_type = "supervisor"`. It receives `SubagentMcpRole::Supervisor` (`spawn`, `list_agents`, `send_message`, `close`, `fetch`, `handback` — no `ask_oracle`, no `mj-review`, and no `supervisor` in its `spawn` schema), allowing it to spawn depth-2 non-supervisor workers (`straightforward` and `subtle`) while forbidding depth-3 delegation structurally.
- **ExecutionPolicy:** A target's approval model (`mj-core/src/config/targets.rs:778-803`). `ConfiguredApprovals` applies to raw guardian targets (`LocalBare` and `SshBare` with `permissions = "guardian"`). `Unconstrained` applies to container runtimes (Podman, Docker, Apple Container, Nitro) and `SshBare` with `permissions = "yolo"`. Live sessions record their policy in `TargetRuntimeSettings`.
- **GuardianReviewer:** `HarnessKind::guardian()` (`mj-core/src/config/harness.rs:1350-1357`) classifies Codex, Claude, Grok, Muse, and Antigravity as `GuardianReviewer::Harness`, and Kimi, OpenCode, and DSH as `GuardianReviewer::Mjolnir`.
- **Power ranking `tasks_per_200` and `weekly_budget_usd`:** Benchmark task capacity per $200/month (`tasks_per_200_sub`, fallback `tasks_per_200_api`) and weekly API-equivalent subscription budget (`B_plan_weekly_usd`) published from `brokkbench26/bt/ranked.csv` via `BrokkAi/bifrost-service`.
- **Config, store, and protocol versions:** `CONFIG_VERSION` is currently `23` (`mj-core/src/config.rs:1132`). SQLite `SCHEMA_VERSION` is currently `145` (`mj-store/src/database.rs:34`, `MINIMUM_COMPATIBLE_VERSION = 145`). Daemon wire `PROTOCOL_VERSION` is in `mj-client/src/daemon.rs:3943`. Compatible SQLite migrations keep `minimum_compatible_version` unchanged; breaking migrations raise it in the same transaction.

Current selection flow: every automatic spawn or launch (`start_session` model-first in `mj-server/src/server/api/sessions.rs:165`, HTTP/tool `spawn` in `mj-server/src/server/api/subagents.rs:108` and `mj-daemon/src/server_runtime/api.rs:1295`, `ask_oracle` in `mj-daemon/src/server_runtime/api/oracle.rs:42`, reviewers and brief writer in `mj-daemon/src/review_host/selection.rs:73,98`) enters `resolve_model_profile_selection_from` (`mj-server/src/server/api/subagents.rs:400`), which filters ineligible profiles and calls `draw_candidates` (`mj-accounts/src/profile_scheduler.rs:81`). Quota automove (`mj-daemon/src/daemon/continuation/quota.rs:35-59`) calls `ineligibility` and `draw_candidates` directly on same-harness profiles with `native_continuity`.

## Plan of Work


The work proceeds in four independently verifiable milestones. Milestone 1 lives in `BrokkAi/bifrost-service` and can run in parallel with Milestone 2. Within `mjolnir`, Milestone 2 lands categories, guardian filtering, pool-first value load balancing, the `Unknown` bug fix, Paid/API scoring, and the compatible `category` column so real category metrics begin accruing immediately. Milestone 3 runs an offline simulation against the live workload distributions to confirm `alpha` and duration thresholds, then adds trailing-60-minute velocity, durable reservations, the unified 5h/weekly runway gate, and local category quantile switchover. Milestone 4 adds the Supervisor role with the complete `ChildCompletion::AwaitingChildren` state machine, nested slot anti-starvation, breaking store migration, and tree-wide lifecycle walks.

### Milestone 1 (`BrokkAi/bifrost-service`): Publish static `/power-ranking.json`


Goal: Expose the build-time power-ranking table from `client/data/power-ranking/ranked.csv` as a public, statically prerendered JSON route at `https://slopcop.com/power-ranking.json` with structured `model`, `effort`, `harness`, `tasks_per_200_sub`, `tasks_per_200_api`, `weekly_budget_usd` (`B_plan_weekly_usd`), `plan_usd_per_month`, `plan_assumed`, `basis`, and `minutes` so Mjolnir never has to scrape HTML or guess harness suffixes from `config`.

In `BrokkAi/bifrost-service`:

1. Extend `client/lib/power-ranking/data.ts` with a `loadPowerRankingExport()` helper (reading `ranked.csv` and `meta.json` at build time alongside `loadPowerRankingData()`) that preserves `B_plan_weekly_usd` (ignored by the HTML table builder today) and splits each `config` (`<model>/<effort>`) cleanly into structured fields: `config`, `model` (normalized base model without `-dsh`/`-mj` suffixes), `effort`, `harness` (optional explicit harness tag when the row is harness-specific, such as `"dsh"` for `deepseek-flash-dsh` or inferred from `profile`/`vendor`), `tasks_per_200_sub`, `tasks_per_200_api`, `weekly_budget_usd` (`B_plan_weekly_usd`), `plan_usd_per_month`, `plan_assumed`, `basis`, and `minutes`.
2. Add `client/app/power-ranking.json/route.ts` with `export const dynamic = "force-static";`. Inside `GET()`, call `loadPowerRankingExport()`, fail the static build loudly if the CSVs or required columns are missing, and return JSON (`Content-Type: application/json; charset=utf-8`, `Cache-Control: public, max-age=300, s-maxage=3600`):

       {
         "schema_version": 1,
         "as_of": "2026-11-19",
         "source_commit": "a8ed8ccf43",
         "entries": [
           {
             "config": "opus-5.5/xhigh",
             "model": "claude-opus-5-5",
             "effort": "xhigh",
             "harness": null,
             "tasks_per_200_sub": 142.3,
             "tasks_per_200_api": 28.1,
             "weekly_budget_usd": 2320.0,
             "plan_usd_per_month": 200.0,
             "plan_assumed": false,
             "basis": "model-measured",
             "minutes": 11.4
           }
         ]
       }

3. Add a route unit test beside the existing power-ranking tests asserting HTTP 200, `schema_version === 1`, valid structured `model`/`effort` fields, and numeric `weekly_budget_usd` / `tasks_per_200_*` matching `ranked.csv`. Verify `brokk.ai/power-ranking.json` redirects via `client/next.config.ts` and `robots.ts` allows `/power-ranking.json`.

Acceptance: Client tests and `npm run build` pass in `BrokkAi/bifrost-service`, and after deployment `curl -fsS https://slopcop.com/power-ranking.json` returns HTTP 200 JSON matching `ranked.csv`.

### Milestone 2 (Mjolnir Phase A): Categories, YOLO Drop, `category` Column, Power-Ranking Cache, `Unknown` Fix, Guardian Filter, and Pool-First Value Draw


Goal: Land the generalized multi-pair category config, remove `[subagents.yolo.*]`, start recording `category` in SQLite immediately via a compatible migration, fix the failed-probe → `Unknown` bug, enforce raw-target guardian filtering, and upgrade the scheduler to pool-first `tasks_per_200` value weighting with scored Paid and API tiers.

#### 2.1 Generalized `[subagents]` config and removal of `[subagents.yolo.*]` (`CONFIG_VERSION = 24`)

In `mj-core/src/config.rs`, bump `CONFIG_VERSION` from `23` to `24` and update `SubagentConfig` to hold four equal-peer lists: `pub primary: Vec<SubagentChoice>`, `pub straightforward: Vec<SubagentChoice>`, `pub subtle: Vec<SubagentChoice>`, and `pub oracle: Vec<SubagentChoice>`. Delete `SubagentChoices`, `yolo`, and `use_same_on_all_targets` from serialized output. Implement a custom `Deserialize` helper on `SubagentConfig` (following `ReviewConfig`'s legacy upgrade pattern in `mj-core/src/config.rs:207-332`) that accepts either a singular table `[subagents.<cat>]` or an array `[[subagents.<cat>]]` for each category, silently accepts and drops legacy `enabled`, `eligible_profiles`, `use_same_on_all_targets`, and `yolo`, and records a one-time load notice (`WARN` log and CLI/startup notice) when a loaded file contained non-empty `[subagents.yolo.*]` choices that were dropped.

Validation rejects `"adaptive"` effort on any entry, rejects empty `model` strings, rejects duplicate `(model, effort)` entries within one category, and enforces that non-empty `subtle` requires non-empty `straightforward`. Replace `choice_for(task, yolo)` with `choices(&self, task_type: SubagentTaskType) -> &[SubagentChoice]`, `is_configured(&self) -> bool`, `oracle_configured(&self) -> bool`, and `primary_configured(&self) -> bool`. Update `Config::selected_models` (`mj-core/src/config.rs:1044-1075`) to union all four category lists plus `review.reviewers` plus `review.brief_writer`, removing the `use_same_on_all_targets` branch. Keep `TargetTemplate::is_yolo()`, `ExecutionPolicy::Unconstrained`, `TargetRuntimeSettings::is_yolo()`, and `SessionRecord::is_yolo_target()` intact for approval enforcement and guardian filtering.

Update Settings, CLI, TUI, Web, and docs to remove all YOLO subagent-choice threading and render all four categories using the existing `[[review.reviewers]]` array-editor pattern:

- `mj-client/src/settings/{schema,page,edit,discovery}.rs`: remove `is_yolo_subagent_path`, `shared_subagents`, and hidden YOLO rows; expose `primary` ("Primary and supervisor"), `straightforward`, `subtle`, and `oracle` as add/remove `(model, effort)` arrays, clearing effort on model change and probing efforts for every listed model.
- `mj-cli/src/{api_commands,api_client,main}.rs`: remove `[subagents.yolo.*]` remedies; support repeatable `MODEL[:EFFORT]` flags for `--subagent-primary`, `--subagent-straightforward`, `--subagent-subtle`, and `--subagent-oracle` while retaining existing single `--subagent-*-effort` flags for compatibility.
- `mj-tui/src/wizards/{subagents,dashboard/new_session,dashboard/begin}.rs`, `mj-server/src/server/viewer_types.rs`, `mj-server/src/web/viewer.js`, `mj-profile-catalog/src/profile_config.rs`, `mj-controller/src/controller.rs`, `move_session.rs`, `mj-daemon/src/server_runtime/{api,api/oracle}.rs`, `mj-worker-launch/src/worker_launch.rs`, and `configuration.md`: remove `yolo` from subagent choice lookups.
- Primary auto-launch on `mj new` / New dialog: in `start_session` (`mj-server/src/server/api/sessions.rs:158-186`), when `request.profile_id.is_none()` and `request.model.is_none()` and `config.subagents.primary` is non-empty, resolve `(profile_id, model, effort)` via load balancing across `config.subagents.primary` (passing target `ExecutionPolicy` and `Placement::TopLevel`). When `primary` is empty, preserve the existing saved-default profile fallback unchanged. Explicit `--profile` pins and explicit `--model` launches continue to work identically. In TUI/Web New dialogs, offer `"Auto (Primary)"` when `primary` is non-empty so users can launch load-balanced primary sessions or pick a specific profile.

#### 2.2 Compatible SQLite migration for `category` (`mj-store`)

Land a **compatible** SQLite migration (revision `146`, leaving `minimum_compatible_version = 145` unchanged) ahead of the later breaking supervisor migration so real category history starts accruing immediately:

- `ALTER TABLE session_turn_selections ADD COLUMN category TEXT;`
- `ALTER TABLE subagent_accounting ADD COLUMN category TEXT;`
- Canonical values: `"primary"`, `"supervisor"`, `"straightforward"`, `"subtle"`, `"oracle"`, `"reviewer"`, `"brief_writer"`.
- Populate `category` in `record_child_accounting` (`mj-store/src/database/usage.rs:6-29`) when registering a child and in `record_turn_selection` (`usage.rs:32-68`) at turn start (defaulting to `"primary"` for top-level sessions and copying from `subagent_accounting.category` for children). Older writers name explicit columns and leave `category` NULL, preserving full forward/backward compatibility.

#### 2.3 Power-ranking compiled-in baseline and 24h / release cache (`mj-accounts/src/power_ranking.rs`)

Add `mj-accounts/src/power_ranking.rs` with a compiled-in baseline JSON asset (`mj-accounts/assets/power-ranking-baseline.json`, refreshed from Milestone 1's output at build/release time so offline runs, cold starts, and isolated tests never degrade to neutral weights) plus an atomic disk cache at `data_dir().join("power-ranking-cache.json")` storing `{ mj_version, fetched_at_epoch_seconds, snapshot }`:

- Fetch URL: `https://slopcop.com/power-ranking.json`, overridable via `MJ_POWER_RANKING_URL` for isolated testing (tests never reach the public network).
- Freshness rule: fresh iff `cached.mj_version == env!("CARGO_PKG_VERSION")` and `now_seconds.saturating_sub(cached.fetched_at_epoch_seconds) < 86_400` (24 hours).
- On daemon startup: load valid disk cache if present (falling back to the compiled-in baseline) into `Arc<RwLock<PowerRankingSnapshot>>` immediately without blocking; spawn an asynchronous background refresh if missing or stale (and re-check hourly on the daemon maintenance loop) using `reqwest` (5s timeout, `User-Agent: mj/{CARGO_PKG_VERSION}`). Failed fetches log at `WARN` and keep the current snapshot.
- Lookup `lookup(&self, model_identity: &str, effort: Option<&str>, harness: HarnessKind) -> Option<PowerRankingLookup>`: matches normalized `(model, effort)` directly against Milestone 1's structured fields (with fallback canonicalization for Claude dotted/hyphenated names and Kimi prefixes, preferring a matching `harness` tag when present); if `effort` is `None` or unranked for that model, falls back to `"high"`, then `"medium"`, then geometric mean of ranked efforts for that model.

#### 2.4 Fix the failed-probe → `Unknown` bug (`mj-accounts/src/quota.rs`, `mj-daemon`, `mj-accounts/src/profile_scheduler.rs`)

Fix the live-store defect where 3–48% of Claude probes fail with non-429 network errors and turn `0%`-exhausted profiles into `Unknown`:

- Store `pub last_good: Option<ProfileQuota>` on `QuotaReportPublication` (`mj-accounts/src/quota.rs:250-299`), seeded at daemon startup from `quota_reset_cache` (`mj-store/src/database/quota_cache.rs`) and updated only when `report.error.is_none()`.
- Keep `publication.report` carrying the latest raw probe (including `error`) so display surfaces still report probe failures honestly via `effective_report`.
- Add `CandidateQuota::from_publication(publication: Option<&QuotaReportPublication>, harness: HarnessKind, now_seconds: i64)` in `mj-accounts/src/profile_scheduler.rs`:
  1. Choose `effective = publication.report` if `report.error.is_none()`, else `publication.last_good` provided at least one of its windows still has `resets_at_epoch_seconds.is_none_or(|t| t > now_seconds)` (or its `billing_class()` is `Usage`). If `last_good`'s windows have all passed their reset deadlines, treat those expired windows as reset rather than stale `0%`, or fall back to `Unknown` if `last_good` is absent.
  2. When `billing_class()` is `Unknown`, consult `CandidateQuota::billing(harness)` (`mj-accounts/src/profile_scheduler.rs:43-50`) so usage-priced harnesses (`dsh`, `claude-aws`, API-only `opencode`) route cleanly to `UsagePriced` rather than `Unknown`.

#### 2.5 Raw-target guardian filter, pool-first value draw, and Paid/API tier scoring (`mj-accounts/src/profile_scheduler.rs`, `mj-server`, `mj-daemon`)

1. **Plumb `ExecutionPolicy` into every automatic selection caller:**
   - `start_session` (`mj-server/src/server/api/sessions.rs:158`, which already resolves `target_id` before model selection)
   - `spawn_subagent` / `resolve_subagent_policy_selection` (`mj-server/src/server/api/subagents.rs` and `mj-daemon/src/server_runtime/api.rs`)
   - `ask_oracle` (`mj-daemon/src/server_runtime/api/oracle.rs`)
   - `review_host/selection.rs` (`reviewers` and `brief_writer`, reading the reviewed session's recorded `TargetRuntimeSettings.execution_policy`)
   - `quota::destinations` (`mj-daemon/src/daemon/continuation/quota.rs`, also fixing `Plan::ChildSwitch` to pass `Placement::Child` instead of `Placement::TopLevel`)
2. **Hard eligibility filters (bypassed only by explicit `--profile` pins):**
   - When `execution_policy == ExecutionPolicy::ConfiguredApprovals`, retain only candidates whose `harness.guardian() == GuardianReviewer::Harness` (Codex, Claude, Grok, Muse, Antigravity; excluding Kimi, OpenCode, and DSH).
   - Drop `profile_health::Unhealthy` for `(profile_id, offer.value)`, `QuotaRank::Exhausted`, and `QuotaRank::Paid` when `!may_consume_credits`.
3. **Pool-first Efraimidis–Spirakis draw with linear capacity and pace exponent:**
   - Expand `(enabled eligible profile) × (choice in category)` across equal peers, deduplicating by `(profile_id, offer.value, resolved_effort)`.
   - Group candidates by **quota pool** `(profile_id, binding_window_scope)` so listing multiple models on the same profile never inflates that pool's probability.
   - Enforce single-model-per-provider-per-category: define `model_catalog::model_provider(model_identity) -> Option<QuotaProvider>` mapping known families (`claude`/`opus`/`sonnet`/`haiku`/`fable`/`mythos` → `Anthropic`, `gpt`/`o1`/`o3`/`o4` → `OpenAI`, `gemini`/`argon`/`g1-` → `Google`, `grok` → `XAI`, `kimi`/`k2`/`k3` → `Moonshot`, `deepseek` → `DeepSeek`, `glm` → `Zai`, `muse` → `Meta`) and falling back to the offering profile's `quota_provider()` during Settings/discovery validation for custom models. Reject any category containing two entries for the same `QuotaProvider` in both `Config::validate` and Settings validation with a clear message naming the conflicting models and provider. At draw time, keep one pool weight per `(profile_id, binding_window_scope)` pool (and if a multi-provider pool like Antigravity's `claude,gpt` ever matches entries from two distinct providers, select the higher `tasks_per_200` entry for that pool).
   - Reserve floor: `RESERVE_FLOOR_PCT = 1.0`. Usable remaining fraction `H_w = (remaining_pct - 1.0).max(0.0) / 100.0`.
   - Pace ratio (without the old `4.0` cap!): for each applying window `w` with reset in the future and duration `D_w`, `f_w = (t_left / D_w).clamp(1800.0 / D_w, 1.0)` and `u_w = H_w / f_w` (`u_w = 1.0` means exactly on pace to hit 1% right at reset; expired reset scores `u_w = 1.0`; undated window uses `H_w / 0.5`). Let `u_long` be the minimum `u_w` across applying weekly/monthly windows and `u_5h` the minimum across applying 5h windows (`1.0` if none).
   - Effective monthly price `P_eff`: `profile.monthly_price_usd` when inferred (Claude, Codex); else `lookup.plan_usd_per_month` from `power-ranking.json`; else geometric mean of `(tasks_per_200 * P_eff / 200)` across priced peers in the draw (never a fixed `$100`), logged in selection explanations.
   - Included-subscription pool weight:

         weight = tasks_per_200_sub * (P_eff / 200.0) * u_long.powf(alpha) * u_5h.min(1.0)

     (Starting at `alpha = 2.0`, finalized by Milestone 3's offline simulation. For 5h-only subscriptions with no long window, substitute `u_5h.powf(alpha)`.)
   - **Score Paid and API tiers:**
     - `Paid` tier: weight by `tasks_per_200_sub.or(tasks_per_200_api)` scaled by remaining credit balance (or `1.0` for `Unlimited`/`Available`).
     - `UsagePriced` (API) tier: weight/order by `tasks_per_200_api` (higher benchmark tasks per $200 API spend ranks ahead, via Efraimidis–Spirakis draw or deterministic ordering).
   - **Tiers in Milestone 2 (retiring legacy top-level Tier 2 per user decision #1; velocity runway gate arrives in Milestone 3):**
     - `0`: Included subscription with `H_w > 0` (`remaining > 1%`) and not projecting 5h exhaustion
     - `1`: Included subscription with `H_w > 0` and projecting 5h exhaustion (replaced in Milestone 3 by the velocity runway gate)
     - `2`: `Unknown` (truly unknown only — never failed-probe exhausted pools or usage-priced harnesses)
     - `3`: At-reserve subscription (`0% < remaining <= 1%`)
     - `4`: `Paid` credits (scored)
     - `5`: `UsagePriced` / API (scored by `tasks_per_200_api`)
     - `6`: `Exhausted` (`remaining == 0%`, filtered out before draw)
   - Within each tier: `[quota] prefer` order first, then Efraimidis–Spirakis `ln(u) / weight`, then anchor and deterministic `profile_id` tiebreak.

Acceptance: v23 configs with `[subagents.yolo.*]` load cleanly into v24 with one upgrade notice and drop `yolo` on save; `session_turn_selections.category` and `subagent_accounting.category` populate on new turns/spawns; a failed non-429 probe on an exhausted Claude profile keeps it `Exhausted` rather than `Unknown`; raw guardian targets exclude Kimi/OpenCode/DSH automatically while container targets include them; listing three Codex models alongside one Claude model gives Codex one pool weight rather than three; and API-tier candidates select in `tasks_per_200_api` order.

### Milestone 3 (Mjolnir Phase B): Offline Simulation, Trailing-60m Velocity, Durable Reservations, and Unified Runway Gate


Goal: Validate `alpha` and category duration horizons against the live-store workload distributions, then replace the single-sample 5h projection with a single trailing-60-minute velocity and durable reservation runway gate covering both 5-hour and weekly windows.

#### 3.1 Prototyping / Simulation Sub-step (before locking constants)

Write a small, self-contained simulation script in `<scratch>/simulate_lb.py` driven by the empirical arrival rates, burst sizes (12 median / 31 p90 / 90 max per 10-min gap), heavy-tailed task cost/duration distributions, and multi-profile weekly/5h reset schedules captured in the Advisor's live-store reports (`5edc633dc75943375e020bbe3050d6fd/report.md`, `c4ecd0f90d14c80a4f0d25104e36eebe/report.md`). Sweep `alpha ∈ {1.0, 1.5, 2.0, 2.5, 3.0}` and category runway duration quantiles (`p50`, `p75`, `p90`), measuring: (1) mid-task exhaustions, (2) stranded quota at reset, and (3) unnecessary Paid/API placements while a subscription pool still had runway. Record the winning `alpha` and duration quantile in the `Decision Log` and `Surprises & Discoveries` sections of this plan before coding the constants into Rust.

#### 3.2 Trailing-60-minute velocity, durable reservations, and unified runway gate (`mj-accounts/src/quota_telemetry.rs`)

Add `mj-accounts/src/quota_telemetry.rs`, recomputed in memory on the daemon's 10-minute quota refresh tick (no 14- or 30-day joins on the poll):

1. **Trailing-60-minute velocity `v` (`%/hr`) per `(profile_id, cache_identity, window_kind, model_scope)`:**
   - Load the last 90 minutes of `quota_history` via `load_quota_history_since`, keeping only clean rows (`error.is_none()` and non-empty windows).
   - Walk consecutive clean readings `(r_prev, r_cur)` over the trailing 60 minutes (at least two clean readings). Compute usage as `100.0 * used / limit` when both `used` and `limit > 0` are present, else `100.0 - remaining_percent`.
   - **Drop corrupted intervals:** skip any step where `used_cur < used_prev` (top-ups/rebounds), where `resets_at_epoch_seconds` shifted by more than a small jitter tolerance (e.g. `> 120s`, handling Codex moving deadlines and Antigravity idle resets), or where `Δt <= 0`.
   - Compute `v = (sum Δused) / (sum Δt_hours)` (`0.0` if no valid positive intervals exist).
   - Also record `second_to_last_clean_ts` (the timestamp of the reading before last, or `last_clean_ts - 600s` when only one clean reading exists) so reservations cover exactly the window of placements not yet reflected in `v`.
   - For profiles that publish both 5h and weekly windows, compute the profile's own observed `five_hour_to_weekly_ratio` from paired positive `Δused_5h / Δused_week` intervals (handling the measured 2.5x Kimi, 3.2x Antigravity, 9x–32x Claude spread cleanly; if no paired data exists, omit the 5h per-task reservation term and rely on `v_5h * min(d_category, t_5h_left)`).

2. **Category mean burn and duration constants (with local switchover at `N >= 200`):**
   - Ship named constants anchored to the 2026-11-21 live-store measurements (`c4ecd0f90d14c80a4f0d25104e36eebe/report.md`), converting `mean_api_usd(category)` into weekly-% using `weekly_budget_usd` (`B_plan_weekly_usd`) from `power-ranking.json` scaled by `P_eff / plan_usd_per_month`:

         mean_burn_weekly_pct = 100.0 * mean_api_usd(category) / (weekly_budget_usd * (P_eff / plan_usd_per_month))

     Measured defaults:
     - `straightforward`: `mean_api_usd = 0.12`, `d_p90 = 71 min` (`1.18h`)
     - `subtle`: `mean_api_usd = 3.47`, `d_p90 = 166 min` (`2.77h`)
     - `reviewer` / `brief_writer`: `mean_api_usd = 0.22`, `d_p90 = 6 min` (`0.10h`)
     - `oracle`: `mean_api_usd = 0.50`, `d_p90 = 15 min` (`0.25h`) (unmeasured in 2026-11-21 store; conservative initial default)
     - `supervisor` / `primary`: `mean_api_usd = 5.00`, `d_p90 = 180 min` (`3.0h`) (conservative initial default)
   - Once a category accumulates at least `200` completed sessions with `category` populated in SQLite (checked lazily at startup and once per hour, bounded to the most recent 1,000 sessions per category), replace that category's default `mean_api_usd` and `d_category` with its locally observed mean and chosen duration quantile.

3. **Durable placement reservations (`reserved`):**
   - Query placements created on `profile_id` since `second_to_last_clean_ts` directly from durable `sessions` + `subagent_accounting` (plus any placement currently in-flight between draw and commit within the daemon's admission lock), summing `mean_burn_pct(placement.category, window)` — using the **mean** (not p90) because 12–90 placements sum between polls.
   - Because reservations derive from durable session records, a daemon restart immediately reconstructs identical reservations with no ephemeral counter drift.

4. **Unified runway gate (identical for BOTH 5-hour and weekly windows):**
   - For each applying window `w` whose reset is `t_left_hours > 0` ahead:

         rem_eff = remaining_pct - 1.0 - reserved(w) - mean_burn_pct(new_task, w)
         window_safe = rem_eff >= v(w) * min(d_category_hours, t_left_hours)

   - `five_hour_safe` is `window_safe` across applying 5h windows; `weekly_safe` is `window_safe` across applying weekly/monthly windows.
   - Tier ladder in `profile_scheduler.rs` (unified for top-level and child placements; legacy top-level active-5h tier retired per user decision #1):
     - `0`: Included subscription, `H_w > 0`, `five_hour_safe && weekly_safe`
     - `1`: Included subscription, `H_w > 0`, `weekly_safe && !five_hour_safe` (5h-pressured, recovers within `<= 5h`)
     - `2`: `Unknown` (truly unknown only)
     - `3`: Included subscription, `!weekly_safe` or at-reserve (`0% < remaining <= 1%`)
     - `4`: `Paid` credits (scored)
     - `5`: `UsagePriced` / API (scored by `tasks_per_200_api`)
     - `6`: `Exhausted` (`remaining == 0%`, filtered out before draw)

Acceptance: Tricky-condition tests prove that 30 burst placements on a single stale reading reserve `30 * mean_burn` and steer subsequent placements to peer pools; a usage top-up or moved deadline never produces negative or corrupted velocity; a pool burning at 12 %/h with 5% weekly remaining fails the runway gate for a `subtle` task (`d = 166 min`) while passing for a `reviewer` (`d = 6 min`) or when weekly reset is 15 minutes away; and daemon restart preserves reservations identically from SQLite.

### Milestone 4 (Mjolnir Phase C): Supervisor Role, `ChildCompletion::AwaitingChildren`, and New/Move Checkbox


Goal: Enable top-level sessions with the **Supervisors** checkbox to spawn depth-1 supervisors from `[[subagents.primary]]`, allow those supervisors to delegate to depth-2 non-supervisor workers and safely await them without parking, structurally forbid depth-3 delegation and nested slot deadlock, and keep container occupancy, suspend, and Move walks accurate across the full descendant tree.

#### 4.1 Core types, durable state, breaking store migration, and wire protocol

- Add `SubagentTaskType::Supervisor` (`id = "supervisor"`, mapped to `SubagentConfig.primary`) in `mj-core/src/subagent.rs`.
- Keep `SubagentKind` unchanged (`Worker`, `Reviewer`, `Oracle`) so retained older workers still parse `launch.json`. Add `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub supervisor: bool` to `SubagentRecord`, `SubagentMetadata`, and `WorkerLaunchConfig` (`mj-core/src/worker_launch.rs`), plus `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub supervisors_enabled: bool` on `WorkerLaunchConfig` for top-level sessions.
- Add `pub supervisors: bool` (default `false`) to `SessionRecord` (`mj-core/src/state.rs`) and `pub last_supervisor_choices: BTreeMap<String, bool>` to `State`. (Supervisors always draw from `[subagents.primary]`; no speculative per-session `supervisor_choice` override column is added.)
- Add a **breaking** SQLite migration (revision `147`, raising `minimum_compatible_version = 147` in the same transaction, mirroring migration v111 for `oracle`: older writers drop the new column and reject `supervisor: true` in `record_json` due to `deny_unknown_fields`):
  - `ALTER TABLE sessions ADD COLUMN supervisors INTEGER NOT NULL DEFAULT 0;`
  - `CREATE TABLE supervisor_preference (profile_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL);`
  - Wire load/save in `mj-store/src/database/state_io.rs`.
- Add `supervisors: Option<bool>` to `StartSessionRequest` (`mj-server/src/server/api/types.rs`) and `MoveSelection` (`mj-core/src/state/session_move.rs`), bump `PROTOCOL_VERSION` in `mj-client/src/daemon.rs`, and treat a `supervisors` change in `move_keeps_profile_and_policy` (`move_session.rs:3070-3083`) as requiring a restart.
- Add `--supervisors enabled|disabled` to `NewArgs` (`mj-cli/src/api_commands.rs`) and `MoveArgs` (`mj-cli/src/main.rs`).

#### 4.2 New/Move checkbox with target-aware availability

- Define one shared predicate `supervisors_available_for_target(config, offered_profiles, execution_policy) -> bool`: true iff `config.subagents.max_concurrent >= 2`, `config.subagents.straightforward` is non-empty, and at least one enabled profile matching an entry in `config.subagents.primary` satisfies BOTH `harness.supports_delegation_tools()` AND (`execution_policy == Unconstrained || harness.guardian() == GuardianReviewer::Harness`).
- Expose `supervisors_available` per target and `last_supervisor_choices` in `viewer_types.rs` / `viewer_wire.rs` and in TUI `SubagentDraft` (`mj-tui/src/wizards/subagents.rs`). Hide the **Supervisors** checkbox whenever Mjolnir delegation is off or `supervisors_available` is false for the selected target; remember per-profile state via `last_supervisor_choices` (default `false`). Add the matching checkbox to `mj-server/src/web/viewer.js` New and Move dialogs.

#### 4.3 Depth-2 invariant, `SubagentMcpRole::Supervisor`, and scheduling gate

- Define one shared invariant function `validate_subagent_parent(parent_subagent: Option<&SubagentRecord>, requested_kind: SubagentKind, requested_supervisor: bool) -> Result<()>` in `mj-core/src/subagent.rs` and call it from BOTH `Controller::register_subagent_inner` (`mj-controller/src/controller/subagents.rs:327-336`) and `State::validate_subagent` (`mj-core/src/state.rs:3093-3097`):
  - `Reviewer`: allowed at any valid parent (existing rule).
  - Top-level (`parent_subagent.is_none()`): `Worker` (with `supervisor = true` only when `parent.supervisors` is true and Mjolnir delegation is active) and `Oracle` allowed.
  - Depth-1 Supervisor (`parent_subagent` is `Worker` with `supervisor == true`): only non-supervisor `Worker` (`!requested_supervisor`) allowed; `requested_supervisor == true` and `Oracle` refused.
  - Non-supervisor depth-1 `Worker`, `Oracle`, or depth-2 `Worker`: non-reviewer children refused.
- Add `SubagentMcpRole::Supervisor` (`id = "supervisor"`) in `mj-core/src/subagent.rs:1022-1104` with `tool_names = &["spawn", "list_agents", "send_message", "close", "fetch", "handback"]`. Accept `--role supervisor` in `mj-worker-bin/src/main.rs` and harness MCP registration (`mj-worker/src/worker_runtime/subagents.rs`). In `mj-worker/src/subagent_mcp.rs`, allow `handback` for `Child | Supervisor`, advertise `task_type` values `["straightforward", "subtle", "supervisor"]` on `Parent` when `supervisors_enabled` is true (omitting `subtle` if unconfigured), and strictly `["straightforward", "subtle"]` (never `supervisor`) on `Supervisor`.
- Update `SubagentPolicy::for_launch` (`mj-core/src/subagent.rs:153-161`) and `resolve_subagent_mcp_role` (`mj-core/src/worker_launch.rs:477-497`) to take `subagent: Option<&SubagentRecord>`: a supervisor child keeps `SubagentPolicy::Mjolnir` (inheriting `straightforward`/`subtle` overrides) and gets `SubagentMcpRole::Supervisor` with `oracle = false` and `review_trigger = None`.
- Add `SUPERVISOR_INSTRUCTIONS` (`mj-core/src/session_instructions.rs`) and `mj-core/assets/supervisor-delegation.md` (`mj-worker-staging/src/worker_binary/install.rs`) explaining that a supervisor owns its delegated slice, may delegate `straightforward`/`subtle` workers, must wait for or close all its workers before calling `handback`, and must call `handback` once to report back to its parent.
- In `profile_scheduler.rs`, when `placement == Placement::Supervisor`, require `harness.supports_delegation_tools()` as a hard eligibility gate so a supervisor is never placed on Kimi, Grok, or Muse.

#### 4.4 `ChildCompletion::AwaitingChildren`, live-child `handback` refusal, anti-starvation cap, and tree walks

- **`ChildCompletion::AwaitingChildren` (`mj-daemon/src/daemon/delegation.rs`):** Add one durable helper `RuntimeState::supervisor_active_children(supervisor_id) -> Vec<ActiveChildSummary>` inspecting direct `SubagentKind::Worker` children in SQLite that either have `SessionState::has_live_worker()` OR have an undelivered `SubagentFinished` mailbox row pending for `supervisor_id`.
  - In `settle_child` (`delegation.rs:614-788`), right beside `review_open → AwaitingReview`: if the child is a supervisor and `supervisor_active_children` is non-empty, return `ChildCompletion::AwaitingChildren` — **skipping handback reminder, parent finish notice, and parking**, and retrying on the busy schedule (1s–30s) just like `AwaitingReview`.
  - In `reconcile_children` (`mj-daemon/src/server_runtime/api.rs:2249-2252`), treat non-empty `supervisor_active_children` as `ChildDisposition::NotNews("the supervisor awaits its sub-agents")`.
  - In `handback` execution (`api.rs:1607-1631`), if the caller is a supervisor and `supervisor_active_children` is non-empty, return a stored tool error listing the still-live child task names and session IDs ("wait for or close your running sub-agents before calling handback: ...").
  - Because `supervisor_active_children` reads durable SQLite state, a daemon restart rebuilds `AwaitingChildren` identically with zero extra persisted state, and a supervisor waiting on workers is never parked while workers are alive.
- **Anti-starvation nested `max_concurrent` cap (`mj-controller/src/controller/subagents.rs`):**
  - Walk all `SubagentKind::Worker` descendants under the container root (`State::workspace_owner`) when enforcing `config.subagents.max_concurrent` in `ensure_subagent_slot_available` (called on both `spawn` and `unpark_child`).
  - Additionally, enforce a supervisor sub-cap: spawning or unparking a supervisor requires `config.subagents.max_concurrent >= 2` and `live_supervisors_in_container < config.subagents.max_concurrent / 2` (at least 1 when `max_concurrent >= 2`), guaranteeing supervisors can never consume every container slot and deadlock their own depth-2 workers. If a completed parked supervisor is later woken by the primary via `send_message` while the cap is full, `unpark_child` returns the ordinary `slot_refusal` error (telling the primary which workers occupy slots).
- **Quota `Plan::ChildSwitch` on a supervisor:** Preserve `switch_child_quota_profile`'s existing root-local park/unpark (same session ID, borrowed container, relay journal, and intact `subagent_sessions` parent links so depth-2 workers keep running undisturbed and any `SubagentFinished` mailbox rows arriving during the ~2s switch drain immediately after unpark).
- **Tree-wide lifecycle walks (`mj-core`, `mj-daemon`, `mj-controller`):**
  - Add `State::descendant_subagent_ids(root_id)` (BFS with cycle guard) in `mj-core/src/state.rs`.
  - Update `State::active_child_session_ids`, `State::subagents_occupy_container`, `stop_subagents_for_suspend` / `record_stopped_subagents` (`mj-daemon/src/daemon/close.rs`), and Move's `live_move_children` / `move_children` (`mj-controller/src/controller/move_session.rs:3092-3146`) to walk descendants so a parked supervisor with live depth-2 workers is never invisible. Keep `list_agents`, `close`, and `reconcile_parent_reports` 1-hop by design.

Acceptance: Top-level with `--supervisors enabled` spawns a supervisor; the supervisor spawns depth-2 `straightforward` workers, idles between turns while waiting for them without being reminded or parked, has premature `handback` refused with the live worker IDs, receives `SubagentFinished` reports and hands back cleanly once they finish, refuses `supervisor` spawns from depth-1 and all worker spawns from depth-2, survives daemon restart while `AwaitingChildren`, respects both the container-wide `max_concurrent` cap and the `max_concurrent / 2` supervisor sub-cap, and tears down the full tree on primary close or suspend.

## Concrete Steps


All Mjolnir commands run in `/workspace/811f71013aa31834aae114b250c70c8b/mjolnir` on the dev profile (outside the restricted sandbox per `AGENTS.md`).

Milestone 1 (in a checkout of `BrokkAi/bifrost-service`, `client/` directory):

    npm test -- power-ranking
    npm run build

Milestone 2 (Mjolnir Phase A):

    cargo test -p brokk-mj-core config::tests
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-accounts power_ranking
    cargo test -p brokk-mj-accounts profile_scheduler
    cargo test -p brokk-mj-accounts quota::tests
    cargo test -p brokk-mj-client settings
    cargo test -p brokk-mj-server subagents
    cargo test -p brokk-mj-tui setup::tests::goldens
    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-accounts -p brokk-mj-client -p brokk-mj-server -p brokk-mj-tui -p brokk-mj-cli --all-targets -- -D warnings

Milestone 3 (Mjolnir Phase B):

    python3 /var/lib/hel/profiles/811f71013aa31834aae114b250c70c8b/antigravity-cli/brain/53afbfe3-2691-4c27-9bb1-3bc358cd9779/scratch/simulate_lb.py
    cargo test -p brokk-mj-accounts quota_telemetry
    cargo test -p brokk-mj-accounts profile_scheduler
    cargo test -p brokk-mj-daemon continuation::quota
    cargo clippy -p brokk-mj-accounts -p brokk-mj-daemon -p brokk-mj-server --all-targets -- -D warnings

Milestone 4 (Mjolnir Phase C):

    cargo test -p brokk-mj-core subagent
    cargo test -p brokk-mj-store schema
    cargo test -p brokk-mj-controller subagents
    cargo test -p brokk-mj-daemon delegation
    cargo test -p brokk-mj-daemon subagent
    cargo test -p brokk-mj-worker subagent_mcp
    cargo clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-controller -p brokk-mj-daemon -p brokk-mj-worker --all-targets -- -D warnings

Isolated live validation (never against the default instance; always stop the test daemon when done):

    mj --instance subagent-lb-test daemon status
    # exercise isolated scenarios, then:
    mj --instance subagent-lb-test daemon stop

## Validation and Acceptance


Per `AGENTS.md` testing rules, add ONLY golden, end-to-end, integration, and tricky-condition tests (do not add hand-built toy scoring examples that merely restate the formula). Run `TZ=UTC` for `mj-chat` goldens and all `golden` tests after any dashboard/settings layout change.

1. **Integration — Config & Store Migrations (`mj-core`, `mj-store`):**
   - Loading v23 `config.toml` with singular `[subagents.*]`, `use_same_on_all_targets = false`, and `[subagents.yolo.*]` upgrades to v24, converts singular choices to 1-element lists, drops `yolo` and `use_same_on_all_targets` with one upgrade notice, and omits them on save.
   - Store migration 146 (`category`) keeps `minimum_compatible_version = 145` and survives writes from older column-named inserts; migration 147 (`supervisors`) raises `minimum_compatible_version = 147` in the same transaction.
2. **Integration — Power-Ranking Cache (`mj-accounts`):**
   - Local mock HTTP server via `MJ_POWER_RANKING_URL`: verifies compiled-in baseline loads offline, cache hits within 24h on matching `CARGO_PKG_VERSION`, refreshes after 24h expiry, invalidates immediately when `mj_version` changes, and retains the last valid snapshot on HTTP 500 or network failure.
3. **Tricky Conditions — Scheduler, Velocity, Reservations, and Probe Failures (`mj-accounts`, `mj-server`, `mj-daemon`):**
   - **Failed probe after exhaustion:** Exhausted `0%` Claude profile hits a non-429 network error on its next probe; verify `CandidateQuota::from_publication` preserves `Exhausted` rather than promoting it to `Unknown` ahead of Paid/API, and clears once the window's reset timestamp passes.
   - **Usage-priced harness without report:** `dsh` / `claude-aws` / API-only `opencode` with no quota windows routes to `UsagePriced` (ordered by `tasks_per_200_api`), never `Unknown`.
   - **Pool-first draw invariance:** Listing three Codex models and one Claude model in a category gives the Codex pool one draw entry rather than 3x weight.
   - **Burst placements on a stale reading:** 30 placements dispatched between 10-minute polls accumulate durable mean-burn reservations that trip the runway gate and divert subsequent spawns to peer pools; rebuilding telemetry from SQLite after a simulated daemon restart yields identical reservations.
   - **Corrupted `quota_history` intervals:** Usage drops (mid-window top-ups), shifted Codex/Antigravity reset deadlines, and already-passed reset timestamps are ignored cleanly without negative velocity or false exhaustion.
   - **Raw-target guardian gate:** Under `ExecutionPolicy::ConfiguredApprovals`, Kimi/OpenCode/DSH candidates in a multi-pair category are excluded across `spawn`, `ask_oracle`, `review`, `brief_writer`, model-first `mj new`, and `primary` auto-launch, while remaining eligible under `ExecutionPolicy::Unconstrained` and on explicit `--profile` pins.
4. **Tricky Conditions — Supervisor Hierarchy & `ChildCompletion` (`mj-controller`, `mj-daemon`, `mj-worker`):**
   - Supervisor whose turn ends while depth-2 workers are live enters `ChildCompletion::AwaitingChildren` (no handback reminder, no parent notice, no park), refuses premature `handback` naming the live workers, preserves `AwaitingChildren` across a simulated daemon restart, and settles cleanly after its workers finish and it calls `handback`.
   - Depth-3 spawn refusal at both MCP schema/dispatch and `register_subagent_inner`; supervisor spawn refuses harnesses with `supports_delegation_tools() == false`; `supervisors_available` hides the checkbox on targets where no `primary` candidate can delegate.
   - Nested slot anti-starvation: with `max_concurrent = 4`, spawning a 3rd supervisor is refused (`max_concurrent / 2 = 2`) so depth-2 workers always have room; total depth-1 + depth-2 live workers never exceed `max_concurrent`.
   - Tree-wide lifecycle: suspending or closing a primary tears down depth-2 workers even if the depth-1 supervisor is parked; `Plan::ChildSwitch` on a supervisor leaves live depth-2 workers running and delivers their queued finish mail after unpark.
5. **Goldens (`mj-tui`, `mj-server`, `tests/e2e/web`):**
   - Updated Settings goldens (four category arrays, no YOLO toggle), New/Move wizard goldens (Supervisors checkbox hidden/shown per target capability), and Web e2e settings/wizard tests.

## Idempotence and Recovery


Config upgrades from v23 to v24 happen cleanly in memory on load and save atomically via `mj-core/src/config/loading.rs`. Old `[subagents.yolo.*]` keys are dropped cleanly with one notice rather than causing parse errors, so upgrades complete without user intervention.

SQLite migrations execute inside transactional `BEGIN IMMEDIATE` ... `COMMIT` blocks in `mj-store/src/database/schema.rs`. Migration 146 (`category`) is purely additive and compatible; migration 147 (`supervisors`) raises `minimum_compatible_version` in the same transaction so older binaries cannot corrupt supervisor records.

`power-ranking-cache.json` writes atomically via temp file + rename inside `data_dir()`. Deleting the file is always safe: Mjolnir falls back immediately to the compiled-in baseline snapshot and repopulates the cache on the next background refresh.

## Artifacts and Notes


Reconnaissance and empirical reports backing this plan:

- Codebase maps (under this session's `.mj-agents/` report directories): `profile-quota-map.md`, `quota-interrupt-signaling-map.md`, `agent-tiers-map.md`, `monthly-price-inference.md`, `power-ranking-and-burn.md`, `profile-scheduling-map.md`, `subagent-redesign-surface-map.md`, `execplan-mechanisms.md`.
- Advisor live-store empirical reports (fetched from session `91233b0efdd193066e7b4eeb46c8db10`): `5edc633dc75943375e020bbe3050d6fd/report.md` (40,975 `quota_history` rows) and `c4ecd0f90d14c80a4f0d25104e36eebe/report.md` (180,417 turns / 50,192 children).

## Interfaces and Dependencies


Key types and signatures across the milestones:

In `mj-core/src/config.rs`:

    pub const CONFIG_VERSION: u32 = 24;

    pub struct SubagentConfig {
        pub primary: Vec<SubagentChoice>,
        pub straightforward: Vec<SubagentChoice>,
        pub subtle: Vec<SubagentChoice>,
        pub oracle: Vec<SubagentChoice>,
        pub max_concurrent: usize,
        // Legacy ignored keys accepted on load, omitted on save:
        // enabled, legacy_eligible_profiles, legacy_yolo, legacy_use_same_on_all_targets
    }

In `mj-core/src/subagent.rs`:

    pub enum SubagentTaskType {
        Straightforward,
        Subtle,
        Oracle,
        Supervisor,
    }

    pub enum SubagentMcpRole {
        Parent,
        Supervisor,
        MessageOnly,
        FetchOnly,
        Child,
    }

    pub fn validate_subagent_parent(
        parent_subagent: Option<&SubagentRecord>,
        requested_kind: SubagentKind,
        requested_supervisor: bool,
    ) -> anyhow::Result<()>;

In `mj-accounts/src/power_ranking.rs`:

    pub struct PowerRankingEntry {
        pub config: String,
        pub model: String,
        pub effort: Option<String>,
        pub harness: Option<String>,
        pub tasks_per_200_sub: Option<f64>,
        pub tasks_per_200_api: Option<f64>,
        pub weekly_budget_usd: Option<f64>,
        pub plan_usd_per_month: Option<f64>,
        pub plan_assumed: bool,
        pub basis: String,
        pub minutes: Option<f64>,
    }

In `mj-accounts/src/quota_telemetry.rs`:

    pub struct WindowTelemetry {
        pub trailing_60m_pct_per_hour: f64,
        pub second_to_last_clean_ts: Option<i64>,
        pub five_hour_to_weekly_ratio: Option<f64>,
    }

    pub struct CategoryBurnDefaults {
        pub mean_api_usd: f64,
        pub runway_duration_hours: f64,
        pub local_sample_count: usize,
    }

    pub struct QuotaTelemetrySnapshot {
        pub windows: BTreeMap<WindowKey, WindowTelemetry>,
        pub recent_placements: BTreeMap<String, Vec<RecentPlacement>>,
        pub category_stats: BTreeMap<String, CategoryBurnDefaults>,
    }

In `mj-accounts/src/profile_scheduler.rs`:

    pub enum Placement {
        TopLevel,
        Supervisor,
        Child,
    }

    pub struct SchedulingContext<'a> {
        pub execution_policy: mj_core::config::ExecutionPolicy,
        pub placement: Placement,
        pub category: Option<&'a str>,
        pub ranking_anchor: Option<&'a str>,
        pub prefer: &'a [String],
        pub telemetry: &'a crate::quota_telemetry::QuotaTelemetrySnapshot,
        pub power_ranking: &'a crate::power_ranking::PowerRankingSnapshot,
        pub now_seconds: i64,
    }

    pub fn draw_scheduled_candidates<T: AsRef<ScheduledCandidate>>(
        candidates: &mut Vec<T>,
        ctx: &SchedulingContext<'_>,
    ) -> anyhow::Result<()>;

In `mj-daemon/src/daemon/delegation.rs`:

    pub enum ChildCompletion {
        Settled,
        AwaitingReview,
        AwaitingChildren,
        AwaitingHandback,
        RetryPark,
        ParkAfterParentTurn,
        Retry(String),
    }

---

Revision note (2026-11-21): Substantially revised after Advisor (`91233b0efdd193066e7b4eeb46c8db10`) review and live-store empirical analysis, plus direct verification of `ChildCompletion`, `ChildSwitch`/park caps, and failed-probe handling. Simplified Milestone 4's math (pool-first draw, linear capacity with `u^alpha` pace exponent, trailing-60m diff + durable session-derived mean reservations + unified 5h/weekly runway gate, `B_plan_weekly_usd` + measured defaults instead of online `pct_per_api_usd`/Empirical Bayes), added the failed-probe `Unknown` fix and Paid/API tier scoring, added `ChildCompletion::AwaitingChildren` and supervisor slot anti-starvation to the Supervisor milestone, and reordered milestones into Phase A (categories + value/guardian LB + `category` column), Phase B (offline simulation + velocity/reservation runway gate), and Phase C (Supervisors).
