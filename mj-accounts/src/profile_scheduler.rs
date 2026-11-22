//! Shared capacity ranking and weighted assignment for profiles.

/// A candidate's last quota reading, scored for the requested model when
/// candidates are ranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateQuota {
    /// No report, a failed one, or one with no usable capacity information.
    Unknown,
    /// Billed per use, with no allowance to run out of.
    UsagePriced,
    Subscription {
        /// Monthly list price as a capacity proxy, when the plan is recognized.
        monthly_price_usd: Option<u32>,
        windows: Vec<crate::quota::QuotaWindow>,
        paid_credits: Option<mj_client::quota::PaidCredits>,
        /// When the windows were read, which is when their usage was measured.
        refreshed_at_epoch_seconds: u64,
    },
}

impl CandidateQuota {
    pub fn from_report(report: Option<&crate::quota::ProfileQuota>) -> Self {
        use mj_client::quota::QuotaBilling;
        let Some(report) = report else {
            return Self::Unknown;
        };
        match report.billing_class() {
            QuotaBilling::Usage => Self::UsagePriced,
            QuotaBilling::Subscription => Self::Subscription {
                monthly_price_usd: report.monthly_price_usd,
                windows: report.windows.clone(),
                paid_credits: report.paid_credits.clone(),
                refreshed_at_epoch_seconds: report.refreshed_at_epoch_seconds,
            },
            QuotaBilling::Unknown => Self::Unknown,
        }
    }

    /// The reading selection should trust.
    ///
    /// A probe with no error wins. A failed probe falls back to `last_good`
    /// when that reading is usage-priced, still has subscription windows, or
    /// records paid credits.
    /// `quota_score` treats a window whose reset is at or before `now_seconds`
    /// as already reset, so an exhausted window stops blocking once its reset
    /// passes. When the reading's billing class is still unknown, a harness
    /// that is billed per use (`dsh`, API-only OpenCode) is usage-priced
    /// rather than unknown.
    pub fn from_publication(
        publication: Option<&crate::quota::QuotaReportPublication>,
        harness: mj_core::config::HarnessKind,
        now_seconds: i64,
    ) -> Self {
        use mj_client::quota::QuotaBilling;
        let report =
            publication.and_then(|publication| scheduling_report(publication, now_seconds));
        let quota = Self::from_report(report);
        if matches!(quota, Self::Unknown) && quota.billing(harness) == QuotaBilling::Usage {
            Self::UsagePriced
        } else {
            quota
        }
    }
}

fn scheduling_report(
    publication: &crate::quota::QuotaReportPublication,
    now_seconds: i64,
) -> Option<&crate::quota::ProfileQuota> {
    if publication
        .report
        .as_ref()
        .is_some_and(|report| report.error.is_none())
    {
        return publication.report.as_ref();
    }
    publication
        .last_good
        .as_ref()
        .filter(|last_good| last_good_schedulable(last_good, now_seconds))
}

/// A failed probe may reuse `last_good` when it is usage-priced, it still
/// carries subscription windows, or it records paid credits. An expired
/// window is still evidence: scoring treats `resets_at_epoch_seconds <=
/// now_seconds` as a reset. A windowless non-usage reading with no paid
/// credits is not.
fn last_good_schedulable(report: &crate::quota::ProfileQuota, now_seconds: i64) -> bool {
    use mj_client::quota::QuotaBilling;
    if report.billing_class() == QuotaBilling::Usage || report.paid_credits.is_some() {
        return true;
    }
    let refreshed_at = i64::try_from(report.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
    let age = now_seconds.saturating_sub(refreshed_at);
    report.windows.iter().any(|window| {
        if window.resets_at_epoch_seconds.is_some() {
            window.remaining_percent.is_some()
                || (window.used.is_some() && window.limit.is_some())
                || window
                    .resets_at_epoch_seconds
                    .is_some_and(|reset| reset <= now_seconds)
        } else {
            let has_reading = window.remaining_percent.is_some()
                || (window.used.is_some() && window.limit.is_some());
            let max_age = window.duration_seconds.unwrap_or_else(|| {
                if window.is_five_hour() {
                    5 * 3600
                } else {
                    7 * 86_400
                }
            });
            has_reading && age < max_age
        }
    })
}

impl CandidateQuota {
    /// The candidate's billing: its report decides, then what the harness
    /// implies (see [`mj_client::quota::harness_billing`]).
    pub fn billing(&self, harness: mj_core::config::HarnessKind) -> mj_client::quota::QuotaBilling {
        use mj_client::quota::QuotaBilling;
        match self {
            Self::UsagePriced => QuotaBilling::Usage,
            Self::Subscription { .. } => QuotaBilling::Subscription,
            Self::Unknown => mj_client::quota::harness_billing(harness),
        }
    }
}

/// A candidate profile with its discovered choices and last quota reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentCandidate {
    pub profile_id: String,
    pub harness: mj_core::config::HarnessKind,
    pub choices: mj_core::worker_launch::ProfileConfig,
    pub quota: CandidateQuota,
    /// The profile's "Load balancing may consume credits" option. See
    /// [`ineligibility`].
    pub may_consume_credits: bool,
    /// The models this profile should not run now, by model id, from
    /// provider failures its sessions just had (#1361).
    pub unhealthy: std::collections::BTreeMap<String, crate::profile_health::Unhealthy>,
    /// Quota cache identity for the profile's current login, matching
    /// `quota_history.identity` so velocity telemetry never mixes superseded logins.
    pub cache_identity: Option<String>,
}

impl AsRef<SubagentCandidate> for SubagentCandidate {
    fn as_ref(&self) -> &SubagentCandidate {
        self
    }
}

/// Which spawn the ranking serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    TopLevel,
    Supervisor,
    Child,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum QuotaRank {
    Available(f64),
    Unknown,
    Paid(f64),
    UsagePriced,
    Exhausted,
}

fn quota_rank(candidate: &SubagentCandidate, model: &str, now_seconds: i64) -> QuotaRank {
    match &candidate.quota {
        CandidateQuota::Unknown => QuotaRank::Unknown,
        CandidateQuota::UsagePriced => QuotaRank::UsagePriced,
        CandidateQuota::Subscription {
            windows,
            paid_credits,
            monthly_price_usd,
            ..
        } => match crate::quota::quota_score(windows, model, now_seconds) {
            Some(score) if score > 0.0 => QuotaRank::Available(
                score
                    * monthly_price_usd
                        .filter(|price| *price > 0)
                        .map_or(1.0, |price| f64::from(price) / 100.0),
            ),
            score => paid_weight(paid_credits.as_ref())
                .map(QuotaRank::Paid)
                .unwrap_or(
                    if score.is_some() || (windows.is_empty() && paid_credits.is_some()) {
                        QuotaRank::Exhausted
                    } else {
                        QuotaRank::Unknown
                    },
                ),
        },
    }
}

fn paid_weight(credits: Option<&mj_client::quota::PaidCredits>) -> Option<f64> {
    use mj_client::quota::PaidCredits;
    match credits? {
        PaidCredits::Limited(balance) => balance
            .as_f64()
            .filter(|balance| balance.is_finite() && *balance > 0.0),
        // No finite balance to compare: give known usable accounts equal weight.
        PaidCredits::Unlimited | PaidCredits::Available => Some(1.0),
    }
}

/// Why load balancing must not offer a candidate for `model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    /// The subscription is out of quota for the model and has no paid credits.
    OutOfQuota,
    /// The subscription is out of quota; only paid credits remain, and the
    /// profile's option forbids load balancing from spending them.
    CreditsNotAllowed,
}

impl Ineligible {
    /// "profile codex2 is out of subscription quota for model ..." with what
    /// the user can do about it.
    pub fn describe(self, profile_id: &str, model: &str) -> String {
        match self {
            Self::OutOfQuota => {
                format!("profile {profile_id} is out of subscription quota for model {model:?}")
            }
            Self::CreditsNotAllowed => format!(
                "profile {profile_id} is out of subscription quota for model {model:?}; \
                 enabling \"Load balancing may consume credits\" on the profile would let it \
                 use its paid credits"
            ),
        }
    }
}

/// The one decision whether load balancing may offer a candidate. A
/// subscription out of quota for the model is excluded; one with only paid
/// credits left is excluded unless its profile allows spending them.
/// Usage-priced profiles bill per use by nature, and unknown quota is not
/// evidence of exhaustion, so both stay eligible. A session pinned to a named
/// profile is not load balancing and does not ask.
pub fn ineligibility(
    candidate: &SubagentCandidate,
    model: &str,
    now_seconds: i64,
) -> Option<Ineligible> {
    match quota_rank(candidate, model, now_seconds) {
        QuotaRank::Exhausted => Some(Ineligible::OutOfQuota),
        QuotaRank::Paid(_)
            if matches!(candidate.quota, CandidateQuota::Subscription { .. })
                && !candidate.may_consume_credits =>
        {
            Some(Ineligible::CreditsNotAllowed)
        }
        _ => None,
    }
}

/// Candidate profiles split into those whose choices are known and those
/// whose discovery failed, with the reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentCandidates {
    pub offered: Vec<SubagentCandidate>,
    pub unavailable: Vec<(String, String)>,
}

/// Reserve floor in percentage points: usable remaining fraction `H_w` is
/// `(remaining_pct - RESERVE_FLOOR_PCT).max(0.0) / 100.0`.
pub const RESERVE_FLOOR_PCT: f64 = 1.0;

/// Pace exponent locked from the 2026-11-21 20-seed offline simulation.
pub const PACE_EXPONENT_ALPHA: f64 = 2.0;

/// Minimum fraction `f_w` for a window with reset ahead (30 minutes / D_w).
const MIN_PACE_REMAINING_SECONDS: f64 = 1800.0;

/// One `(profile, model, effort)` candidate for generalized value- and
/// velocity-based load balancing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledCandidate {
    pub profile_id: String,
    pub harness: mj_core::config::HarnessKind,
    pub model: String,
    pub effort: Option<String>,
    pub quota: CandidateQuota,
    pub may_consume_credits: bool,
    pub cache_identity: Option<String>,
}

impl AsRef<ScheduledCandidate> for ScheduledCandidate {
    fn as_ref(&self) -> &ScheduledCandidate {
        self
    }
}

/// Shared context for a single automatic scheduling draw.
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

/// Hard raw-target guardian filter: under `ConfiguredApprovals`, only harnesses
/// with a native guardian (`GuardianReviewer::Harness`: Codex, Claude, Grok,
/// Muse, Antigravity) are eligible for automatic scheduling. Under
/// `Unconstrained`, all harnesses stay eligible. Explicit `--profile` pins
/// never enter this filter.
pub fn guardian_eligible(
    harness: mj_core::config::HarnessKind,
    execution_policy: mj_core::config::ExecutionPolicy,
) -> bool {
    match execution_policy {
        mj_core::config::ExecutionPolicy::Unconstrained => true,
        mj_core::config::ExecutionPolicy::ConfiguredApprovals => {
            harness.guardian() == mj_core::config::GuardianReviewer::Harness
        }
    }
}

/// Generalized pool-first, velocity- and value-weighted draw across
/// `candidates`. Filters out guardian-ineligible, exhausted, and disallowed
/// paid-credit candidates, deduplicates to one entry per quota pool
/// `(profile_id, binding_window_scope)`, and orders the surviving pools by
/// tier, `[quota] prefer`, Efraimidis–Spirakis weighted sample, `ranking_anchor`,
/// and `profile_id`.
pub fn draw_scheduled_candidates<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    ctx: &SchedulingContext<'_>,
) -> anyhow::Result<()> {
    let mut units = Vec::with_capacity(candidates.len());
    for _ in 0..candidates.len() {
        let bits = getrandom::u64().map_err(|error| anyhow::anyhow!("draw a profile: {error}"))?;
        units.push(((bits >> 11) + 1) as f64 / (1u64 << 53) as f64);
    }
    order_scheduled_candidates_by_draw(candidates, &units, ctx);
    Ok(())
}

/// Deterministic counterpart to [`draw_scheduled_candidates`] taking one
/// uniform `(0, 1]` sample per surviving pool in deterministic pre-draw order.
pub fn order_scheduled_candidates_by_draw<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    units: &[f64],
    ctx: &SchedulingContext<'_>,
) {
    let scored = prepare_scored_pools(std::mem::take(candidates), ctx);
    let mut keyed: Vec<(f64, ScoredPool<T>)> = scored
        .into_iter()
        .enumerate()
        .map(|(idx, pool)| {
            let unit = units
                .get(idx)
                .copied()
                .unwrap_or(0.5)
                .clamp(f64::MIN_POSITIVE, 1.0);
            let key = if matches!(pool.tier, 0 | 1 | 3 | 4 | 5) && pool.weight > 0.0 {
                unit.ln() / pool.weight
            } else {
                f64::NEG_INFINITY
            };
            (key, pool)
        })
        .collect();

    keyed.sort_by(|left, right| {
        left.1
            .tier
            .cmp(&right.1.tier)
            .then_with(|| left.1.prefer_pos.cmp(&right.1.prefer_pos))
            .then_with(|| right.0.total_cmp(&left.0))
            .then_with(|| right.1.is_anchor.cmp(&left.1.is_anchor))
            .then_with(|| {
                left.1
                    .item
                    .as_ref()
                    .profile_id
                    .cmp(&right.1.item.as_ref().profile_id)
            })
    });
    candidates.extend(keyed.into_iter().map(|(_, pool)| pool.item));
}

/// Deterministic ranking (no random draw): orders surviving pools by tier,
/// `[quota] prefer`, descending weight, `ranking_anchor`, and `profile_id`.
pub fn rank_scheduled_candidates<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    ctx: &SchedulingContext<'_>,
) {
    let mut scored = prepare_scored_pools(std::mem::take(candidates), ctx);
    scored.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| left.prefer_pos.cmp(&right.prefer_pos))
            .then_with(|| right.weight.total_cmp(&left.weight))
            .then_with(|| right.is_anchor.cmp(&left.is_anchor))
            .then_with(|| {
                left.item
                    .as_ref()
                    .profile_id
                    .cmp(&right.item.as_ref().profile_id)
            })
    });
    candidates.extend(scored.into_iter().map(|pool| pool.item));
}

struct ScoredPool<T> {
    item: T,
    tier: u8,
    weight: f64,
    prefer_pos: usize,
    is_anchor: bool,
}

#[derive(Debug, Clone)]
struct WindowEval {
    kind: crate::quota_telemetry::WindowKind,
    remaining_pct: f64,
    h_w: f64,
    u_w: f64,
    t_left_hours: f64,
    model_scope: Option<String>,
}

fn window_remaining_pct(window: &crate::quota::QuotaWindow) -> Option<f64> {
    if let (Some(used), Some(limit)) = (window.used, window.limit)
        && limit > 0
    {
        let rem = limit.saturating_sub(used);
        return Some((100.0 * rem as f64 / limit as f64).clamp(0.0, 100.0));
    }
    window.remaining_percent.map(f64::from)
}

fn evaluate_applying_windows(
    windows: &[crate::quota::QuotaWindow],
    model: &str,
    now_seconds: i64,
) -> Vec<WindowEval> {
    let mut evals = Vec::new();
    for window in windows {
        if !crate::quota::window_applies(window, model) {
            continue;
        }
        let kind = crate::quota_telemetry::WindowKind::from_window(window);
        if let Some(reset) = window.resets_at_epoch_seconds
            && reset <= now_seconds
        {
            evals.push(WindowEval {
                kind,
                remaining_pct: 100.0,
                h_w: (100.0 - RESERVE_FLOOR_PCT) / 100.0,
                u_w: 1.0,
                t_left_hours: 0.0,
                model_scope: window.model_scope.clone(),
            });
            continue;
        }
        let Some(remaining_pct) = window_remaining_pct(window) else {
            continue;
        };
        let h_w = (remaining_pct - RESERVE_FLOOR_PCT).max(0.0) / 100.0;
        let (u_w, t_left_hours) = match window.resets_at_epoch_seconds {
            Some(reset) => {
                let t_left = (reset - now_seconds) as f64;
                let d_w = window
                    .duration_seconds
                    .filter(|d| *d > 0)
                    .unwrap_or(match kind {
                        crate::quota_telemetry::WindowKind::Short5h => 5 * 3600,
                        crate::quota_telemetry::WindowKind::Long => 7 * 86_400,
                    }) as f64;
                let f_w = (t_left / d_w).clamp(MIN_PACE_REMAINING_SECONDS / d_w, 1.0);
                (h_w / f_w, t_left / 3600.0)
            }
            None => (h_w / 0.5, 0.0),
        };
        evals.push(WindowEval {
            kind,
            remaining_pct,
            h_w,
            u_w,
            t_left_hours,
            model_scope: window.model_scope.clone(),
        });
    }
    evals
}

/// Identify the quota pool `(profile_id, binding_window_scope)` for a candidate.
/// If a scoped applying window exists, bind to its scope; otherwise `None`
/// (the profile's unscoped shared pool).
fn binding_window_scope(candidate: &ScheduledCandidate) -> Option<String> {
    let CandidateQuota::Subscription { windows, .. } = &candidate.quota else {
        return None;
    };
    windows
        .iter()
        .find(|w| w.model_scope.is_some() && crate::quota::window_applies(w, &candidate.model))
        .and_then(|w| w.model_scope.clone())
}

fn prepare_scored_pools<T: AsRef<ScheduledCandidate>>(
    raw_candidates: Vec<T>,
    ctx: &SchedulingContext<'_>,
) -> Vec<ScoredPool<T>> {
    // 1. Hard guardian filter + pool-first deduplication.
    let mut pools: std::collections::BTreeMap<(String, Option<String>), (f64, T)> =
        std::collections::BTreeMap::new();

    for item in raw_candidates {
        let cand = item.as_ref();
        if !guardian_eligible(cand.harness, ctx.execution_policy) {
            continue;
        }
        if ctx.placement == Placement::Supervisor && !cand.harness.supports_delegation_tools() {
            continue;
        }
        let lookup = ctx
            .power_ranking
            .lookup(&cand.model, cand.effort.as_deref(), cand.harness);
        let entry_tasks_per_200 = match &cand.quota {
            CandidateQuota::UsagePriced => lookup
                .as_ref()
                .and_then(|l| l.tasks_per_200_api.or(l.tasks_per_200_sub))
                .unwrap_or(1.0),
            _ => lookup
                .as_ref()
                .and_then(|l| l.tasks_per_200_sub.or(l.tasks_per_200_api))
                .unwrap_or(1.0),
        };
        let pool_key = (cand.profile_id.clone(), binding_window_scope(cand));
        match pools.entry(pool_key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((entry_tasks_per_200, item));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let (existing_tp200, existing_item) = slot.get();
                let replace = entry_tasks_per_200 > *existing_tp200
                    || (entry_tasks_per_200 == *existing_tp200
                        && (cand.model.as_str(), cand.effort.as_deref())
                            < (
                                existing_item.as_ref().model.as_str(),
                                existing_item.as_ref().effort.as_deref(),
                            ));
                if replace {
                    slot.insert((entry_tasks_per_200, item));
                }
            }
        }
    }

    let pool_items: Vec<T> = pools.into_values().map(|(_, item)| item).collect();

    // 2. Compute geometric mean of `tasks_per_200_sub * (P_eff / 200.0)` across
    //    priced subscription peers in this draw, for unpriced peers.
    let mut log_sum = 0.0;
    let mut priced_count = 0usize;
    for item in &pool_items {
        let cand = item.as_ref();
        if let CandidateQuota::Subscription {
            monthly_price_usd, ..
        } = &cand.quota
        {
            let lookup =
                ctx.power_ranking
                    .lookup(&cand.model, cand.effort.as_deref(), cand.harness);
            let p_eff = monthly_price_usd
                .filter(|p| *p > 0)
                .map(f64::from)
                .or_else(|| {
                    lookup
                        .as_ref()
                        .and_then(|l| l.plan_usd_per_month)
                        .filter(|p| p.is_finite() && *p > 0.0)
                });
            let tp200 = lookup
                .as_ref()
                .and_then(|l| l.tasks_per_200_sub.or(l.tasks_per_200_api))
                .filter(|v| v.is_finite() && *v > 0.0);
            if let (Some(p_eff), Some(tp200)) = (p_eff, tp200) {
                let cap = tp200 * (p_eff / 200.0);
                if cap > 0.0 {
                    log_sum += cap.ln();
                    priced_count += 1;
                }
            }
        }
    }
    let peer_geom_mean_capacity = if priced_count > 0 {
        (log_sum / priced_count as f64).exp()
    } else {
        1.0
    };

    let cat_stats = ctx
        .telemetry
        .category_stats(ctx.category.unwrap_or("primary"));

    // 3. Score each pool, filtering out Exhausted (tier 6) and disallowed Paid.
    let mut scored = Vec::with_capacity(pool_items.len());
    for item in pool_items {
        let cand = item.as_ref();
        let lookup = ctx
            .power_ranking
            .lookup(&cand.model, cand.effort.as_deref(), cand.harness);
        let prefer_pos = ctx
            .prefer
            .iter()
            .position(|id| id == &cand.profile_id)
            .unwrap_or(usize::MAX);
        let is_anchor = ctx.ranking_anchor == Some(cand.profile_id.as_str());

        let (tier, weight) = match &cand.quota {
            CandidateQuota::Unknown => (2u8, 1.0),
            CandidateQuota::UsagePriced => {
                let tp200_api = lookup
                    .as_ref()
                    .and_then(|l| l.tasks_per_200_api.or(l.tasks_per_200_sub))
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(1.0);
                (5u8, tp200_api)
            }
            CandidateQuota::Subscription {
                monthly_price_usd,
                windows,
                paid_credits,
                ..
            } => {
                let evals = evaluate_applying_windows(windows, &cand.model, ctx.now_seconds);
                if evals.is_empty() {
                    if let Some(credit_w) = paid_weight(paid_credits.as_ref()) {
                        if !cand.may_consume_credits {
                            continue;
                        }
                        let tp200 = lookup
                            .as_ref()
                            .and_then(|l| l.tasks_per_200_sub.or(l.tasks_per_200_api))
                            .filter(|v| v.is_finite() && *v > 0.0)
                            .unwrap_or(1.0);
                        (4u8, tp200 * credit_w)
                    } else if windows.is_empty() && paid_credits.is_some() {
                        // Exhausted paid-only subscription with 0 credits.
                        continue;
                    } else {
                        (2u8, 1.0)
                    }
                } else if evals.iter().any(|e| e.remaining_pct <= 0.0) {
                    // Exhausted subscription: fall back to Paid credits if allowed, else drop (tier 6).
                    if let Some(credit_w) = paid_weight(paid_credits.as_ref()) {
                        if !cand.may_consume_credits {
                            continue;
                        }
                        let tp200 = lookup
                            .as_ref()
                            .and_then(|l| l.tasks_per_200_sub.or(l.tasks_per_200_api))
                            .filter(|v| v.is_finite() && *v > 0.0)
                            .unwrap_or(1.0);
                        (4u8, tp200 * credit_w)
                    } else {
                        continue;
                    }
                } else {
                    let plan_price = lookup
                        .as_ref()
                        .and_then(|l| l.plan_usd_per_month)
                        .filter(|p| p.is_finite() && *p > 0.0);
                    let weekly_budget = lookup
                        .as_ref()
                        .and_then(|l| l.weekly_budget_usd)
                        .filter(|v| v.is_finite() && *v > 0.0);
                    let inferred_price = monthly_price_usd.filter(|p| *p > 0).map(f64::from);
                    let p_eff_opt = inferred_price.or(plan_price);
                    let tp200_opt = lookup
                        .as_ref()
                        .and_then(|l| l.tasks_per_200_sub.or(l.tasks_per_200_api))
                        .filter(|v| v.is_finite() && *v > 0.0);

                    let (base_capacity, p_eff_for_budget) = match (p_eff_opt, tp200_opt) {
                        (Some(p_eff), Some(tp200)) => {
                            if inferred_price.is_none() {
                                tracing::trace!(
                                    profile_id = %cand.profile_id,
                                    model = %cand.model,
                                    plan_usd_per_month = p_eff,
                                    "using power-ranking plan_usd_per_month as P_eff"
                                );
                            }
                            (tp200 * (p_eff / 200.0), p_eff)
                        }
                        (Some(p_eff), None) => (peer_geom_mean_capacity * (p_eff / 200.0), p_eff),
                        (None, _) => {
                            tracing::debug!(
                                profile_id = %cand.profile_id,
                                model = %cand.model,
                                assumed_capacity = peer_geom_mean_capacity,
                                "using geometric mean peer capacity for unpriced subscription pool"
                            );
                            (peer_geom_mean_capacity, plan_price.unwrap_or(200.0))
                        }
                    };

                    let mut u_long: Option<f64> = None;
                    let mut u_5h: Option<f64> = None;
                    let mut all_above_reserve = true;
                    let mut five_hour_safe = true;
                    let mut weekly_safe = true;

                    for ev in &evals {
                        if ev.h_w <= 0.0 {
                            all_above_reserve = false;
                        }
                        match ev.kind {
                            crate::quota_telemetry::WindowKind::Long => {
                                u_long = Some(u_long.map_or(ev.u_w, |cur| cur.min(ev.u_w)));
                            }
                            crate::quota_telemetry::WindowKind::Short5h => {
                                u_5h = Some(u_5h.map_or(ev.u_w, |cur| cur.min(ev.u_w)));
                            }
                        }
                        if ev.t_left_hours > 0.0 {
                            let wt = ctx.telemetry.window_telemetry(
                                &cand.profile_id,
                                cand.cache_identity.as_deref(),
                                ev.kind,
                                ev.model_scope.as_deref(),
                            );
                            let v_60m = wt.map_or(0.0, |t| t.trailing_60m_pct_per_hour);
                            let second_to_last = wt.and_then(|t| t.second_to_last_clean_ts);
                            let ratio = wt.and_then(|t| t.five_hour_to_weekly_ratio);
                            let reserved = ctx.telemetry.reserved_pct(
                                &cand.profile_id,
                                ev.kind,
                                second_to_last,
                                weekly_budget,
                                p_eff_for_budget,
                                plan_price,
                                ratio,
                            );
                            let new_burn = crate::quota_telemetry::mean_burn_for_window_pct(
                                cat_stats.mean_api_usd,
                                ev.kind,
                                weekly_budget,
                                p_eff_for_budget,
                                plan_price,
                                ratio,
                            );
                            let rem_eff =
                                ev.remaining_pct - RESERVE_FLOOR_PCT - reserved - new_burn;
                            let safe = rem_eff
                                >= v_60m * cat_stats.runway_duration_hours.min(ev.t_left_hours);
                            match ev.kind {
                                crate::quota_telemetry::WindowKind::Short5h => {
                                    if !safe {
                                        five_hour_safe = false;
                                    }
                                }
                                crate::quota_telemetry::WindowKind::Long => {
                                    if !safe {
                                        weekly_safe = false;
                                    }
                                }
                            }
                        }
                    }

                    let weight = match (u_long, u_5h) {
                        (Some(ul), Some(u5)) => {
                            base_capacity * ul.powf(PACE_EXPONENT_ALPHA) * u5.min(1.0)
                        }
                        (Some(ul), None) => base_capacity * ul.powf(PACE_EXPONENT_ALPHA),
                        (None, Some(u5)) => base_capacity * u5.powf(PACE_EXPONENT_ALPHA),
                        (None, None) => base_capacity,
                    }
                    .max(f64::MIN_POSITIVE);

                    let tier = if !all_above_reserve || !weekly_safe {
                        3u8
                    } else if five_hour_safe {
                        0u8
                    } else {
                        1u8
                    };
                    (tier, weight)
                }
            }
        };

        scored.push(ScoredPool {
            item,
            tier,
            weight,
            prefer_pos,
            is_anchor,
        });
    }

    // Deterministic pre-draw order so `units[i]` binds to a stable pool order.
    scored.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| left.prefer_pos.cmp(&right.prefer_pos))
            .then_with(|| right.weight.total_cmp(&left.weight))
            .then_with(|| right.is_anchor.cmp(&left.is_anchor))
            .then_with(|| {
                left.item
                    .as_ref()
                    .profile_id
                    .cmp(&right.item.as_ref().profile_id)
            })
    });
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::quota::PaidCredits;

    fn candidate(quota: CandidateQuota, may_consume_credits: bool) -> SubagentCandidate {
        SubagentCandidate {
            profile_id: "p".into(),
            harness: mj_core::config::HarnessKind::Codex,
            choices: mj_core::worker_launch::ProfileConfig {
                model: None,
                models: Vec::new(),
                efforts: Vec::new(),
                observed_at: 1,
            },
            quota,
            may_consume_credits,
            unhealthy: Default::default(),
            cache_identity: None,
        }
    }

    fn subscription(remaining: u8, credits: Option<PaidCredits>) -> CandidateQuota {
        CandidateQuota::Subscription {
            monthly_price_usd: None,
            windows: vec![crate::quota::QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(remaining),
                duration_seconds: None,
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            }],
            paid_credits: credits,
            refreshed_at_epoch_seconds: 0,
        }
    }

    #[test]
    fn load_balancing_excludes_subscriptions_without_quota_unless_credits_are_allowed() {
        let ask = |quota: CandidateQuota, option| ineligibility(&candidate(quota, option), "m", 0);
        let credits = Some(PaidCredits::limited(5.0).expect("finite credits"));
        for option in [false, true] {
            assert_eq!(
                ask(subscription(0, None), option),
                Some(Ineligible::OutOfQuota)
            );
            assert_eq!(ask(subscription(50, None), option), None);
            assert_eq!(ask(CandidateQuota::UsagePriced, option), None);
            assert_eq!(ask(CandidateQuota::Unknown, option), None);
        }
        assert_eq!(
            ask(subscription(0, credits.clone()), false),
            Some(Ineligible::CreditsNotAllowed)
        );
        assert_eq!(ask(subscription(0, credits), true), None);
    }

    fn claude_window(
        remaining: u8,
        resets_at: Option<i64>,
        error: Option<&str>,
    ) -> crate::quota::ProfileQuota {
        crate::quota::ProfileQuota {
            profile_id: "claude".into(),
            harness: mj_core::config::HarnessKind::Claude,
            monthly_price_usd: Some(200),
            paid_credits: None,
            banked_resets: None,
            windows: vec![crate::quota::QuotaWindow {
                label: "5-hour".into(),
                remaining_percent: Some(remaining),
                duration_seconds: Some(5 * 60 * 60),
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: resets_at,
            }],
            extra: None,
            error: error.map(str::to_owned),
            refreshed_at_epoch_seconds: 1,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        }
    }

    fn publication(
        report: Option<crate::quota::ProfileQuota>,
        last_good: Option<crate::quota::ProfileQuota>,
    ) -> crate::quota::QuotaReportPublication {
        crate::quota::QuotaReportPublication {
            login_error: None,
            cache_identity: "claude-account".into(),
            report,
            last_good,
        }
    }

    /// A non-429 failure after a 0% Claude reading must stay exhausted.
    /// Treating the failure as unknown would schedule it ahead of paid and API.
    #[test]
    fn failed_probe_after_exhausted_reading_stays_exhausted() {
        let now = 1_000_000;
        let last_good = claude_window(0, Some(now + 3_600), None);
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            error: Some("connection reset by peer".into()),
            refreshed_at_epoch_seconds: 2,
            ..last_good.clone()
        };
        let quota = CandidateQuota::from_publication(
            Some(&publication(Some(failed), Some(last_good))),
            mj_core::config::HarnessKind::Claude,
            now,
        );
        assert!(matches!(quota, CandidateQuota::Subscription { .. }));
        let mut candidate = candidate(quota, false);
        candidate.harness = mj_core::config::HarnessKind::Claude;
        assert_eq!(
            ineligibility(&candidate, "claude-opus-5-5", now),
            Some(Ineligible::OutOfQuota)
        );
    }

    /// Once the stale 0% window's reset has passed, it no longer blocks.
    #[test]
    fn expired_last_good_window_stops_blocking() {
        let reset_at = 1_000_000;
        let last_good = claude_window(0, Some(reset_at), None);
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            error: Some("timed out".into()),
            ..last_good.clone()
        };
        let quota = CandidateQuota::from_publication(
            Some(&publication(Some(failed), Some(last_good))),
            mj_core::config::HarnessKind::Claude,
            reset_at,
        );
        assert!(
            matches!(quota, CandidateQuota::Subscription { .. }),
            "an expired window is still the last good reading, scored as reset"
        );
        let mut candidate = candidate(quota, false);
        candidate.harness = mj_core::config::HarnessKind::Claude;
        assert_eq!(ineligibility(&candidate, "claude-opus-5-5", reset_at), None);
    }

    /// Usage-priced harnesses stay on the API tier when the probe never landed.
    #[test]
    fn usage_priced_harness_without_a_report_is_not_unknown() {
        assert_eq!(
            CandidateQuota::from_publication(None, mj_core::config::HarnessKind::Dsh, 0),
            CandidateQuota::UsagePriced
        );
        let failed = claude_window(0, None, Some("dns failure"));
        let failed = crate::quota::ProfileQuota {
            harness: mj_core::config::HarnessKind::Dsh,
            profile_id: "deepseek".into(),
            windows: Vec::new(),
            ..failed
        };
        assert_eq!(
            CandidateQuota::from_publication(
                Some(&publication(Some(failed), None)),
                mj_core::config::HarnessKind::Dsh,
                0,
            ),
            CandidateQuota::UsagePriced
        );
        let claude_failed = claude_window(0, None, Some("dns failure"));
        let claude_failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            ..claude_failed
        };
        assert_eq!(
            CandidateQuota::from_publication(
                Some(&publication(Some(claude_failed), None)),
                mj_core::config::HarnessKind::Claude,
                0,
            ),
            CandidateQuota::Unknown
        );
    }

    /// Paid credits with no windows are still a last-good subscription reading.
    #[test]
    fn paid_credits_without_windows_stay_schedulable() {
        let last_good = crate::quota::ProfileQuota {
            windows: Vec::new(),
            paid_credits: Some(PaidCredits::limited(5.0).expect("finite credits")),
            ..claude_window(0, None, None)
        };
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            paid_credits: None,
            error: Some("connection reset by peer".into()),
            ..last_good.clone()
        };
        let quota = CandidateQuota::from_publication(
            Some(&publication(Some(failed), Some(last_good))),
            mj_core::config::HarnessKind::Claude,
            0,
        );
        assert!(matches!(
            quota,
            CandidateQuota::Subscription {
                paid_credits: Some(_),
                windows,
                ..
            } if windows.is_empty()
        ));
    }

    #[test]
    fn undated_exhausted_last_good_expires_after_window_duration() {
        let mut last_good = claude_window(0, None, None);
        last_good.windows[0].duration_seconds = Some(5 * 3600);
        last_good.refreshed_at_epoch_seconds = 1_000;
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            error: Some("connection reset by peer".into()),
            ..last_good.clone()
        };
        let pub_record = publication(Some(failed), Some(last_good));
        // Still within the 5-hour window duration: keep the exhausted reading.
        let before_expiry = CandidateQuota::from_publication(
            Some(&pub_record),
            mj_core::config::HarnessKind::Claude,
            1_000 + 5 * 3600 - 1,
        );
        assert!(matches!(before_expiry, CandidateQuota::Subscription { .. }));
        // Once the window duration elapses, undated last_good expires to Unknown.
        let after_expiry = CandidateQuota::from_publication(
            Some(&pub_record),
            mj_core::config::HarnessKind::Claude,
            1_000 + 5 * 3600,
        );
        assert_eq!(after_expiry, CandidateQuota::Unknown);
    }

    #[allow(clippy::too_many_arguments)]
    fn scheduled_sub(
        profile_id: &str,
        harness: mj_core::config::HarnessKind,
        model: &str,
        effort: Option<&str>,
        monthly_price_usd: Option<u32>,
        windows: Vec<crate::quota::QuotaWindow>,
        paid_credits: Option<PaidCredits>,
        may_consume_credits: bool,
    ) -> ScheduledCandidate {
        ScheduledCandidate {
            profile_id: profile_id.into(),
            harness,
            model: model.into(),
            effort: effort.map(str::to_owned),
            quota: CandidateQuota::Subscription {
                monthly_price_usd,
                windows,
                paid_credits,
                refreshed_at_epoch_seconds: 1_000_000,
            },
            may_consume_credits,
            cache_identity: Some(format!("id-{profile_id}")),
        }
    }

    fn week_window(remaining: u8, resets_at: i64) -> crate::quota::QuotaWindow {
        crate::quota::QuotaWindow {
            label: "Week".into(),
            remaining_percent: Some(remaining),
            duration_seconds: Some(7 * 86_400),
            model_scope: None,
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: Some(resets_at),
        }
    }

    #[test]
    fn pool_first_draw_invariance_deduplicates_models_sharing_one_pool() {
        let now = 1_000_000;
        let reset = now + 3 * 86_400;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        let ctx = SchedulingContext {
            execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
            placement: Placement::Child,
            category: Some("straightforward"),
            ranking_anchor: None,
            prefer: &[],
            telemetry: &telemetry,
            power_ranking: ranking,
            now_seconds: now,
        };
        let mut candidates = vec![
            scheduled_sub(
                "codex1",
                mj_core::config::HarnessKind::Codex,
                "gpt-6.1-sol",
                Some("high"),
                Some(200),
                vec![week_window(80, reset)],
                None,
                false,
            ),
            scheduled_sub(
                "codex1",
                mj_core::config::HarnessKind::Codex,
                "gpt-6.1-luna",
                Some("high"),
                Some(200),
                vec![week_window(80, reset)],
                None,
                false,
            ),
            scheduled_sub(
                "codex1",
                mj_core::config::HarnessKind::Codex,
                "gpt-5.4",
                Some("high"),
                Some(200),
                vec![week_window(80, reset)],
                None,
                false,
            ),
            scheduled_sub(
                "claude1",
                mj_core::config::HarnessKind::Claude,
                "claude-opus-5-5",
                Some("xhigh"),
                Some(200),
                vec![week_window(80, reset)],
                None,
                false,
            ),
        ];
        rank_scheduled_candidates(&mut candidates, &ctx);
        // Three Codex models share one unscoped pool on `codex1`, so only 2 pools survive.
        assert_eq!(candidates.len(), 2);
        let ids: Vec<&str> = candidates.iter().map(|c| c.profile_id.as_str()).collect();
        assert!(ids.contains(&"codex1"));
        assert!(ids.contains(&"claude1"));
    }

    #[test]
    fn burst_placements_on_stale_reading_trip_runway_gate_and_divert_to_peer() {
        let now = 1_000_000;
        let reset = now + 3 * 86_400;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let mut telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        // `codex-hot` has a stale reading (second_to_last_clean_ts = now - 600)
        // and 30 burst `subtle` placements since then.
        telemetry.windows.insert(
            crate::quota_telemetry::WindowKey {
                profile_id: "codex-hot".into(),
                cache_identity: "id-codex-hot".into(),
                window_kind: crate::quota_telemetry::WindowKind::Long,
                model_scope: None,
            },
            crate::quota_telemetry::WindowTelemetry {
                trailing_60m_pct_per_hour: 1.0,
                second_to_last_clean_ts: Some(now - 600),
                five_hour_to_weekly_ratio: None,
            },
        );
        let burst: Vec<_> = (0..30)
            .map(|i| crate::quota_telemetry::RecentPlacement {
                category: "subtle".into(),
                created_at_epoch_seconds: now - 500 + i,
            })
            .collect();
        telemetry
            .recent_placements
            .insert("codex-hot".into(), burst);

        let ctx = SchedulingContext {
            execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
            placement: Placement::Child,
            category: Some("subtle"),
            ranking_anchor: None,
            prefer: &[],
            telemetry: &telemetry,
            power_ranking: ranking,
            now_seconds: now,
        };
        let mut candidates = vec![
            scheduled_sub(
                "codex-hot",
                mj_core::config::HarnessKind::Codex,
                "gpt-6.1-sol",
                Some("high"),
                Some(200),
                // 10% remaining is enough without reservations, but 30 * subtle (~11.7%) trips the gate.
                vec![week_window(10, reset)],
                None,
                false,
            ),
            scheduled_sub(
                "codex-cool",
                mj_core::config::HarnessKind::Codex,
                "gpt-6.1-sol",
                Some("high"),
                Some(200),
                vec![week_window(10, reset)],
                None,
                false,
            ),
        ];
        rank_scheduled_candidates(&mut candidates, &ctx);
        assert_eq!(candidates[0].profile_id, "codex-cool");
        assert_eq!(candidates[1].profile_id, "codex-hot");
    }

    #[test]
    fn raw_target_guardian_filter_excludes_mjolnir_guardian_harnesses() {
        let now = 1_000_000;
        let reset = now + 3 * 86_400;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        let make_candidates = || {
            vec![
                scheduled_sub(
                    "kimi1",
                    mj_core::config::HarnessKind::Kimi,
                    "kimi-k2.5",
                    None,
                    Some(200),
                    vec![week_window(90, reset)],
                    None,
                    false,
                ),
                scheduled_sub(
                    "opencode1",
                    mj_core::config::HarnessKind::OpenCode,
                    "gpt-6.1-sol",
                    None,
                    Some(200),
                    vec![week_window(90, reset)],
                    None,
                    false,
                ),
                ScheduledCandidate {
                    profile_id: "dsh1".into(),
                    harness: mj_core::config::HarnessKind::Dsh,
                    model: "deepseek-chat".into(),
                    effort: None,
                    quota: CandidateQuota::UsagePriced,
                    may_consume_credits: false,
                    cache_identity: None,
                },
                scheduled_sub(
                    "claude1",
                    mj_core::config::HarnessKind::Claude,
                    "claude-opus-5-5",
                    Some("xhigh"),
                    Some(200),
                    vec![week_window(90, reset)],
                    None,
                    false,
                ),
            ]
        };

        let mut guarded = make_candidates();
        let guarded_ctx = SchedulingContext {
            execution_policy: mj_core::config::ExecutionPolicy::ConfiguredApprovals,
            placement: Placement::TopLevel,
            category: Some("primary"),
            ranking_anchor: None,
            prefer: &[],
            telemetry: &telemetry,
            power_ranking: ranking,
            now_seconds: now,
        };
        rank_scheduled_candidates(&mut guarded, &guarded_ctx);
        assert_eq!(
            guarded
                .iter()
                .map(|c| c.profile_id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude1"]
        );

        let mut unconstrained = make_candidates();
        let unconstrained_ctx = SchedulingContext {
            execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
            ..guarded_ctx
        };
        rank_scheduled_candidates(&mut unconstrained, &unconstrained_ctx);
        assert_eq!(unconstrained.len(), 4);
    }

    #[test]
    fn paid_and_api_tiers_order_by_tasks_per_200_and_credit_balance() {
        let now = 1_000_000;
        let ranking = crate::power_ranking::PowerRankingSnapshot {
            schema_version: 1,
            as_of: None,
            source_commit: None,
            entries: vec![
                crate::power_ranking::PowerRankingEntry {
                    config: "cheap/high".into(),
                    model: "cheap-model".into(),
                    effort: Some("high".into()),
                    harness: None,
                    tasks_per_200_sub: Some(100.0),
                    tasks_per_200_api: Some(80.0),
                    weekly_budget_usd: None,
                    plan_usd_per_month: Some(200.0),
                    plan_assumed: false,
                    basis: "test".into(),
                    minutes: None,
                },
                crate::power_ranking::PowerRankingEntry {
                    config: "expensive/high".into(),
                    model: "expensive-model".into(),
                    effort: Some("high".into()),
                    harness: None,
                    tasks_per_200_sub: Some(20.0),
                    tasks_per_200_api: Some(10.0),
                    weekly_budget_usd: None,
                    plan_usd_per_month: Some(200.0),
                    plan_assumed: false,
                    basis: "test".into(),
                    minutes: None,
                },
            ],
        };
        let telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        let ctx = SchedulingContext {
            execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
            placement: Placement::Child,
            category: Some("straightforward"),
            ranking_anchor: None,
            prefer: &[],
            telemetry: &telemetry,
            power_ranking: &ranking,
            now_seconds: now,
        };
        let mut candidates = vec![
            ScheduledCandidate {
                profile_id: "api-expensive".into(),
                harness: mj_core::config::HarnessKind::Dsh,
                model: "expensive-model".into(),
                effort: Some("high".into()),
                quota: CandidateQuota::UsagePriced,
                may_consume_credits: false,
                cache_identity: None,
            },
            ScheduledCandidate {
                profile_id: "api-cheap".into(),
                harness: mj_core::config::HarnessKind::Dsh,
                model: "cheap-model".into(),
                effort: Some("high".into()),
                quota: CandidateQuota::UsagePriced,
                may_consume_credits: false,
                cache_identity: None,
            },
            scheduled_sub(
                "paid-small",
                mj_core::config::HarnessKind::Claude,
                "expensive-model",
                Some("high"),
                Some(200),
                vec![week_window(0, now + 3600)],
                Some(PaidCredits::limited(5.0).unwrap()),
                true,
            ),
            scheduled_sub(
                "paid-large",
                mj_core::config::HarnessKind::Claude,
                "cheap-model",
                Some("high"),
                Some(200),
                vec![week_window(0, now + 3600)],
                Some(PaidCredits::limited(50.0).unwrap()),
                true,
            ),
        ];
        rank_scheduled_candidates(&mut candidates, &ctx);
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.profile_id.as_str())
                .collect::<Vec<_>>(),
            vec!["paid-large", "paid-small", "api-cheap", "api-expensive"]
        );
    }

    #[test]
    fn supervisor_placement_excludes_harnesses_without_delegation_tools() {
        let now = 1_700_000_000;
        let telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let ctx = SchedulingContext {
            execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
            placement: Placement::Supervisor,
            category: Some("supervisor"),
            ranking_anchor: None,
            prefer: &[],
            telemetry: &telemetry,
            power_ranking: ranking,
            now_seconds: now,
        };
        let mut candidates = vec![
            scheduled_sub(
                "kimi-1",
                mj_core::config::HarnessKind::Kimi,
                "k2",
                None,
                None,
                vec![week_window(90, now + 3600)],
                None,
                false,
            ),
            scheduled_sub(
                "grok-1",
                mj_core::config::HarnessKind::Grok,
                "grok-4",
                None,
                None,
                vec![week_window(90, now + 3600)],
                None,
                false,
            ),
            scheduled_sub(
                "muse-1",
                mj_core::config::HarnessKind::Muse,
                "muse",
                None,
                None,
                vec![week_window(90, now + 3600)],
                None,
                false,
            ),
            scheduled_sub(
                "codex-1",
                mj_core::config::HarnessKind::Codex,
                "gpt-5.4",
                Some("high"),
                Some(200),
                vec![week_window(80, now + 3600)],
                None,
                false,
            ),
        ];
        rank_scheduled_candidates(&mut candidates, &ctx);
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.profile_id.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-1"]
        );
    }
}
