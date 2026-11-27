//! Shared capacity ranking and weighted assignment for profiles.

use std::collections::BTreeSet;

use mj_core::usage::pricing::normalize_model;

use crate::database::{CellCostObservation, canonical_observed_category};
use crate::power_ranking::PowerRankingLookup;
use crate::quota_telemetry::{
    effective_weekly_budget_usd, mean_burn_for_window_pct, normalized_cell_effort,
};

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
    Orchestrator,
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
    pub placement: Placement,
    pub category: Option<&'a str>,
    pub ranking_anchor: Option<&'a str>,
    pub telemetry: &'a crate::quota_telemetry::QuotaTelemetrySnapshot,
    pub power_ranking: &'a crate::power_ranking::PowerRankingSnapshot,
    pub now_seconds: i64,
}

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct PlacementDecision {
    pub chosen_profile: String,
    pub chosen_tier: u8,
    pub reason: PlacementDecisionReason,
    pub estimated_cost_usd: Option<f64>,
    pub binding_profile: Option<String>,
    pub binding_window: Option<crate::quota_telemetry::WindowKind>,
    pub binding_remaining_pct: Option<f64>,
}

/// Generalized pool-first, velocity- and value-weighted draw across
/// `candidates`. Filters out exhausted and disallowed paid-credit
/// candidates, deduplicates to one entry per quota pool
/// `(profile_id, binding_window_scope)`, and orders the surviving pools by
/// tier, Efraimidis–Spirakis weighted sample, `ranking_anchor`,
/// and `profile_id`.
pub fn draw_scheduled_candidates<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    ctx: &SchedulingContext<'_>,
) -> anyhow::Result<()> {
    draw_scheduled_candidates_with_decision(candidates, ctx).map(|_| ())
}

pub fn draw_scheduled_candidates_with_decision<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    ctx: &SchedulingContext<'_>,
) -> anyhow::Result<Option<PlacementDecision>> {
    let mut units = Vec::with_capacity(candidates.len());
    for _ in 0..candidates.len() {
        let bits = getrandom::u64().map_err(|error| anyhow::anyhow!("draw a profile: {error}"))?;
        units.push(((bits >> 11) + 1) as f64 / (1u64 << 53) as f64);
    }
    Ok(order_scheduled_candidates_by_draw_with_decision(
        candidates, &units, ctx,
    ))
}

/// Deterministic counterpart to [`draw_scheduled_candidates`] taking one
/// uniform `(0, 1]` sample per surviving pool in deterministic pre-draw order.
pub fn order_scheduled_candidates_by_draw<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    units: &[f64],
    ctx: &SchedulingContext<'_>,
) {
    let _ = order_scheduled_candidates_by_draw_with_decision(candidates, units, ctx);
}

pub fn order_scheduled_candidates_by_draw_with_decision<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    units: &[f64],
    ctx: &SchedulingContext<'_>,
) -> Option<PlacementDecision> {
    let subscription_profiles: BTreeSet<String> = candidates
        .iter()
        .filter_map(|item| {
            let c = item.as_ref();
            matches!(c.quota, CandidateQuota::Subscription { .. }).then(|| c.profile_id.clone())
        })
        .collect();
    let scored = prepare_scored_pools(std::mem::take(candidates), ctx);
    let sub_5h: BTreeSet<_> = scored
        .iter()
        .filter(|p| p.tier == 3)
        .map(|p| p.item.as_ref().profile_id.clone())
        .collect();
    let sub_weekly: BTreeSet<_> = scored
        .iter()
        .filter(|p| p.tier == 4)
        .map(|p| p.item.as_ref().profile_id.clone())
        .collect();
    let available_subs: BTreeSet<_> = scored
        .iter()
        .filter(|p| {
            p.tier != 2 && matches!(p.item.as_ref().quota, CandidateQuota::Subscription { .. })
        })
        .map(|p| p.item.as_ref().profile_id.clone())
        .collect();
    let exhausted = subscription_profiles.difference(&available_subs).count();
    let spill_binding = scored
        .iter()
        .filter(|p| matches!(p.tier, 3 | 4))
        .filter_map(|p| {
            p.binding_failure
                .map(|failure| (p.item.as_ref().profile_id.clone(), failure))
        })
        .min_by(|a, b| {
            a.1.margin_pct
                .total_cmp(&b.1.margin_pct)
                .then_with(|| a.1.remaining_pct.total_cmp(&b.1.remaining_pct))
                .then_with(|| a.0.cmp(&b.0))
        });
    let mut keyed: Vec<(f64, ScoredPool<T>)> = scored
        .into_iter()
        .enumerate()
        .map(|(idx, pool)| {
            let unit = units
                .get(idx)
                .copied()
                .unwrap_or(0.5)
                .clamp(f64::MIN_POSITIVE, 1.0);
            let key = if matches!(pool.tier, 0..=4) && pool.weight > 0.0 {
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
            .then_with(|| {
                left.1
                    .paid_cost_usd
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right.1.paid_cost_usd.unwrap_or(f64::INFINITY))
            })
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
    let decision = keyed.first().map(|(_, p)| {
        let reason = match p.tier {
            0 => PlacementDecisionReason::SafeSubscription,
            1 => PlacementDecisionReason::UnknownQuota,
            2 => PlacementDecisionReason::PaidSpill {
                sub_5h_unsafe: sub_5h.len(),
                sub_weekly_unsafe: sub_weekly.len(),
                sub_exhausted_or_absent: exhausted,
            },
            _ => PlacementDecisionReason::UnsafeSubscriptionFallback,
        };
        let binding = match p.tier {
            2 => spill_binding.as_ref().map(|(id, f)| (id.clone(), *f)),
            3 | 4 => p
                .binding_failure
                .map(|f| (p.item.as_ref().profile_id.clone(), f)),
            _ => None,
        };
        PlacementDecision {
            chosen_profile: p.item.as_ref().profile_id.clone(),
            chosen_tier: p.tier,
            reason,
            estimated_cost_usd: p.estimated_cost_usd,
            binding_profile: binding.as_ref().map(|(id, _)| id.clone()),
            binding_window: binding.as_ref().map(|(_, f)| f.kind),
            binding_remaining_pct: binding.as_ref().map(|(_, f)| f.remaining_pct),
        }
    });
    candidates.extend(keyed.into_iter().map(|(_, pool)| pool.item));
    decision
}

/// Deterministic ranking (no random draw): orders surviving pools by tier,
/// descending weight, `ranking_anchor`, and `profile_id`.
pub fn rank_scheduled_candidates<T: AsRef<ScheduledCandidate>>(
    candidates: &mut Vec<T>,
    ctx: &SchedulingContext<'_>,
) {
    let mut scored = prepare_scored_pools(std::mem::take(candidates), ctx);
    scored.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| {
                left.paid_cost_usd
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right.paid_cost_usd.unwrap_or(f64::INFINITY))
            })
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

#[derive(Debug, Clone, Copy)]
struct BindingWindowFailure {
    kind: crate::quota_telemetry::WindowKind,
    remaining_pct: f64,
    margin_pct: f64,
}

fn record_binding_failure(slot: &mut Option<BindingWindowFailure>, cand: BindingWindowFailure) {
    let replace = match *slot {
        None => true,
        Some(existing) => {
            cand.margin_pct
                .total_cmp(&existing.margin_pct)
                .then_with(|| cand.remaining_pct.total_cmp(&existing.remaining_pct))
                == std::cmp::Ordering::Less
        }
    };
    if replace {
        *slot = Some(cand);
    }
}

struct ScoredPool<T> {
    item: T,
    tier: u8,
    weight: f64,
    is_anchor: bool,
    paid_cost_usd: Option<f64>,
    estimated_cost_usd: Option<f64>,
    binding_failure: Option<BindingWindowFailure>,
}

#[derive(Debug, Clone)]
struct WindowEval {
    kind: crate::quota_telemetry::WindowKind,
    remaining_pct: f64,
    h_w: f64,
    u_w: f64,
    t_left_hours: f64,
    model_scope: Option<String>,
    reset_elapsed: bool,
    cycle_start: Option<i64>,
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
            let duration = window
                .duration_seconds
                .filter(|d| *d > 0)
                .unwrap_or(match kind {
                    crate::quota_telemetry::WindowKind::Short5h => 5 * 3600,
                    crate::quota_telemetry::WindowKind::Long => 7 * 86_400,
                });
            let cycle_start = reset.saturating_add(
                (now_seconds.saturating_sub(reset) / duration).saturating_mul(duration),
            );
            let next_reset = cycle_start.saturating_add(duration);
            evals.push(WindowEval {
                kind,
                remaining_pct: 100.0,
                h_w: (100.0 - RESERVE_FLOOR_PCT) / 100.0,
                u_w: 1.0,
                t_left_hours: next_reset.saturating_sub(now_seconds) as f64 / 3600.0,
                reset_elapsed: true,
                cycle_start: Some(cycle_start),
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
            reset_elapsed: false,
            cycle_start: None,
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

// Relative variance ratio at live within-cell dispersion σ ≈ 0.65–0.95.
pub(crate) const COST_PRIOR_WEIGHT_K: f64 = 3.0;
pub(crate) const WEEKS_PER_MONTH: f64 = 4.345;

pub(crate) fn benchmark_task_usd(lookup: &PowerRankingLookup) -> Option<f64> {
    let positive = |v: &f64| v.is_finite() && *v > 0.0;
    if let (Some(weekly_budget), Some(tp200)) = (
        lookup.weekly_budget_usd.filter(positive),
        lookup.tasks_per_200_sub.filter(positive),
    ) {
        let plan_scale = lookup.plan_usd_per_month.filter(positive).unwrap_or(200.0) / 200.0;
        Some((weekly_budget * WEEKS_PER_MONTH) / (tp200 * plan_scale))
    } else {
        lookup
            .tasks_per_200_api
            .filter(positive)
            .map(|tp200| 200.0 / tp200)
    }
    .filter(positive)
}

pub(crate) fn shrink_cell_cost_usd(obs: Option<&CellCostObservation>, prior_usd: f64) -> f64 {
    if let Some(obs) = obs.filter(|obs| positive_cost_observation(obs)) {
        (COST_PRIOR_WEIGHT_K * prior_usd + obs.total_api_usd)
            / (COST_PRIOR_WEIGHT_K + obs.sample_count as f64)
    } else {
        prior_usd
    }
}

fn positive_cost_observation(obs: &CellCostObservation) -> bool {
    obs.sample_count > 0 && obs.total_api_usd.is_finite() && obs.total_api_usd > 0.0
}

pub(crate) fn calibrated_prior_scale<'a>(
    default_usd: f64,
    cells: impl Iterator<Item = (f64, Option<&'a CellCostObservation>)>,
) -> f64 {
    let mut log_bench_sum = 0.0;
    let mut bench_count = 0;
    let mut observed_bench_tasks = 0.0;
    let mut observed_samples = 0.0;
    for (bench, obs) in cells {
        log_bench_sum += bench.ln();
        bench_count += 1;
        if let Some(obs) = obs.filter(|obs| positive_cost_observation(obs)) {
            observed_bench_tasks += obs.total_api_usd / bench;
            observed_samples += obs.sample_count as f64;
        }
    }
    if observed_samples > 0.0 && observed_bench_tasks > 0.0 {
        observed_bench_tasks / observed_samples
    } else if bench_count > 0 {
        default_usd / (log_bench_sum / f64::from(bench_count)).exp()
    } else {
        1.0
    }
}

pub(crate) fn cell_prior_usd(bench: Option<f64>, scale: f64, default_usd: f64) -> f64 {
    bench.map_or(default_usd, |bench| (scale * bench).max(f64::MIN_POSITIVE))
}

struct SubscriptionPricing {
    plan_price: Option<f64>,
    weekly_budget: Option<f64>,
    p_eff: Option<f64>,
    tp200: Option<f64>,
}

impl SubscriptionPricing {
    fn new(monthly_price_usd: Option<u32>, lookup: Option<&PowerRankingLookup>) -> Self {
        let positive = |v: &f64| v.is_finite() && *v > 0.0;
        let plan_price = lookup.and_then(|l| l.plan_usd_per_month).filter(positive);
        Self {
            plan_price,
            weekly_budget: lookup.and_then(|l| l.weekly_budget_usd).filter(positive),
            p_eff: monthly_price_usd
                .filter(|p| *p > 0)
                .map(f64::from)
                .or(plan_price),
            tp200: lookup
                .and_then(|l| l.tasks_per_200_sub.or(l.tasks_per_200_api))
                .filter(positive),
        }
    }

    fn budget_price(&self) -> f64 {
        self.p_eff.unwrap_or(200.0)
    }

    fn priced_capacity(&self, estimate_usd: f64, prior_usd: f64, prior_scale: f64) -> Option<f64> {
        if let Some(budget) =
            effective_weekly_budget_usd(self.weekly_budget, self.budget_price(), self.plan_price)
            && estimate_usd > 0.0
        {
            Some(budget / estimate_usd)
        } else {
            self.p_eff.zip(self.tp200).map(|(price, tp200)| {
                let cold_weekly = (tp200 * (price / 200.0)) / (prior_scale * WEEKS_PER_MONTH);
                if estimate_usd > 0.0 && prior_usd > 0.0 {
                    cold_weekly * (prior_usd / estimate_usd)
                } else {
                    cold_weekly
                }
            })
        }
    }
}

fn prepare_scored_pools<T: AsRef<ScheduledCandidate>>(
    raw_candidates: Vec<T>,
    ctx: &SchedulingContext<'_>,
) -> Vec<ScoredPool<T>> {
    // 1. Pool-first deduplication.
    let mut pools: std::collections::BTreeMap<(String, Option<String>), (f64, T)> =
        std::collections::BTreeMap::new();

    for item in raw_candidates {
        let cand = item.as_ref();
        if ctx.placement == Placement::Orchestrator && !cand.harness.supports_delegation_tools() {
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

    let category = ctx.category.unwrap_or("primary");
    let cat_stats = ctx.telemetry.category_stats(category);
    let canonical_category = canonical_observed_category(category).unwrap_or(category);
    let pool_items: Vec<_> = pools
        .into_values()
        .map(|(_, item)| {
            let cand = item.as_ref();
            let lookup =
                ctx.power_ranking
                    .lookup(&cand.model, cand.effort.as_deref(), cand.harness);
            let bench = lookup.as_ref().and_then(benchmark_task_usd);
            let obs =
                ctx.telemetry
                    .cell_cost_observation(category, &cand.model, cand.effort.as_deref());
            (item, lookup, bench, obs)
        })
        .collect();
    let mut seen_cells = BTreeSet::new();
    let prior_scale = calibrated_prior_scale(
        cat_stats.mean_api_usd,
        pool_items.iter().filter_map(|(item, _, bench, obs)| {
            let bench = (*bench)?;
            let cand = item.as_ref();
            seen_cells
                .insert((
                    normalize_model(cand.model.trim()),
                    normalized_cell_effort(cand.effort.as_deref()),
                ))
                .then_some((bench, obs.as_ref()))
        }),
    );

    // 2. Price subscriptions in tasks per week and use the same units for peers.
    let pool_items: Vec<_> = pool_items
        .into_iter()
        .map(|(item, lookup, bench, obs)| {
            let prior_usd = cell_prior_usd(bench, prior_scale, cat_stats.mean_api_usd);
            let estimate_usd = shrink_cell_cost_usd(obs.as_ref(), prior_usd);
            let subscription = match &item.as_ref().quota {
                CandidateQuota::Subscription {
                    monthly_price_usd, ..
                } => {
                    let pricing = SubscriptionPricing::new(*monthly_price_usd, lookup.as_ref());
                    let capacity = pricing.priced_capacity(estimate_usd, prior_usd, prior_scale);
                    Some((pricing, capacity))
                }
                _ => None,
            };
            let paid_estimate = if bench.is_some()
                || obs.as_ref().is_some_and(positive_cost_observation)
            {
                estimate_usd
            } else {
                lookup
                    .as_ref()
                    .and_then(|l| {
                        l.tasks_per_200_api
                            .filter(|x| x.is_finite() && *x > 0.0)
                            .or_else(|| l.tasks_per_200_sub.filter(|x| x.is_finite() && *x > 0.0))
                    })
                    .map_or(f64::INFINITY, |tasks| 200.0 / tasks)
            };
            (item, lookup, estimate_usd, subscription, paid_estimate)
        })
        .collect();
    let mut log_sum = 0.0;
    let mut priced_count = 0;
    for (_, _, _, subscription, _) in &pool_items {
        if let Some((_, Some(capacity))) = subscription {
            log_sum += capacity.ln();
            priced_count += 1;
        }
    }
    let peer_geom_mean_capacity = if priced_count > 0 {
        (log_sum / f64::from(priced_count)).exp()
    } else {
        1.0
    };

    // 3. Score each pool, filtering out Exhausted (tier 6) and disallowed Paid.
    let mut scored = Vec::with_capacity(pool_items.len());
    for (item, lookup, cand_est_usd, subscription, paid_estimate) in pool_items {
        let cand = item.as_ref();
        let is_anchor = ctx.ranking_anchor == Some(cand.profile_id.as_str());

        let (tier, weight, binding_failure) = match &cand.quota {
            CandidateQuota::Unknown => (1u8, 1.0, None),
            CandidateQuota::UsagePriced => {
                let tp200_api = lookup
                    .as_ref()
                    .and_then(|l| l.tasks_per_200_api.or(l.tasks_per_200_sub))
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(1.0);
                (2u8, tp200_api, None)
            }
            CandidateQuota::Subscription {
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
                        (2u8, tp200 * credit_w, None)
                    } else if windows.is_empty() && paid_credits.is_some() {
                        // Exhausted paid-only subscription with 0 credits.
                        continue;
                    } else {
                        (1u8, 1.0, None)
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
                        (2u8, tp200 * credit_w, None)
                    } else {
                        continue;
                    }
                } else {
                    let (pricing, capacity) = subscription.expect("subscription pricing");
                    let base_capacity = capacity.unwrap_or_else(|| {
                        pricing.p_eff.map_or(peer_geom_mean_capacity, |p_eff| {
                            peer_geom_mean_capacity * (p_eff / 200.0)
                        })
                    });
                    let placement_cost = |placement: &crate::quota_telemetry::RecentPlacement| {
                        let p_cat = placement.category.as_str();
                        let same_category = canonical_observed_category(p_cat).unwrap_or(p_cat)
                            == canonical_category;
                        let Some(model) = placement.model.as_deref() else {
                            return if same_category {
                                cand_est_usd
                            } else {
                                ctx.telemetry.category_stats(p_cat).mean_api_usd
                            };
                        };
                        let effort = placement.effort.as_deref();
                        let obs = ctx.telemetry.cell_cost_observation(p_cat, model, effort);
                        let prior = if same_category {
                            let bench = ctx
                                .power_ranking
                                .lookup(model, effort, cand.harness)
                                .as_ref()
                                .and_then(benchmark_task_usd);
                            cell_prior_usd(bench, prior_scale, cat_stats.mean_api_usd)
                        } else {
                            ctx.telemetry.category_stats(p_cat).mean_api_usd
                        };
                        shrink_cell_cost_usd(obs.as_ref(), prior)
                    };

                    let mut u_long: Option<f64> = None;
                    let mut u_5h: Option<f64> = None;
                    let mut all_above_reserve = true;
                    let mut five_hour_safe = true;
                    let mut weekly_safe = true;
                    let mut failing_tier3: Option<BindingWindowFailure> = None;
                    let mut failing_tier4: Option<BindingWindowFailure> = None;

                    for ev in &evals {
                        match ev.kind {
                            crate::quota_telemetry::WindowKind::Long => {
                                u_long = Some(u_long.map_or(ev.u_w, |cur| cur.min(ev.u_w)));
                            }
                            crate::quota_telemetry::WindowKind::Short5h => {
                                u_5h = Some(u_5h.map_or(ev.u_w, |cur| cur.min(ev.u_w)));
                            }
                        }
                        let mut runway_margin: Option<f64> = None;
                        if ev.t_left_hours > 0.0 {
                            let wt = ctx.telemetry.window_telemetry(
                                &cand.profile_id,
                                cand.cache_identity.as_deref(),
                                ev.kind,
                                ev.model_scope.as_deref(),
                            );
                            let v_60m = if ev.reset_elapsed {
                                0.0
                            } else {
                                wt.map_or(0.0, |t| t.trailing_60m_pct_per_hour)
                            };
                            let second_to_last = wt.and_then(|t| t.second_to_last_clean_ts);
                            let ratio = wt.and_then(|t| t.five_hour_to_weekly_ratio);
                            let burn = |usd| {
                                mean_burn_for_window_pct(
                                    usd,
                                    ev.kind,
                                    pricing.weekly_budget,
                                    pricing.budget_price(),
                                    pricing.plan_price,
                                    ratio,
                                )
                            };
                            let reserved: f64 = ctx
                                .telemetry
                                .qualifying_placements(
                                    &cand.profile_id,
                                    second_to_last,
                                    ev.cycle_start,
                                    ctx.now_seconds,
                                )
                                .map(|placement| burn(placement_cost(placement)))
                                .sum();
                            let new_burn = burn(cand_est_usd);
                            let rem_eff =
                                ev.remaining_pct - RESERVE_FLOOR_PCT - reserved - new_burn;
                            let required =
                                v_60m * cat_stats.runway_duration_hours.min(ev.t_left_hours);
                            let margin = rem_eff - required;
                            runway_margin = Some(margin);
                            if margin < 0.0 {
                                let failure = BindingWindowFailure {
                                    kind: ev.kind,
                                    remaining_pct: ev.remaining_pct,
                                    margin_pct: margin,
                                };
                                match ev.kind {
                                    crate::quota_telemetry::WindowKind::Short5h => {
                                        five_hour_safe = false;
                                        record_binding_failure(&mut failing_tier3, failure);
                                    }
                                    crate::quota_telemetry::WindowKind::Long => {
                                        weekly_safe = false;
                                        record_binding_failure(&mut failing_tier4, failure);
                                    }
                                }
                            }
                        }
                        if ev.h_w <= 0.0 {
                            all_above_reserve = false;
                            let reserve_margin = ev.remaining_pct - RESERVE_FLOOR_PCT;
                            record_binding_failure(
                                &mut failing_tier4,
                                BindingWindowFailure {
                                    kind: ev.kind,
                                    remaining_pct: ev.remaining_pct,
                                    margin_pct: runway_margin
                                        .map_or(reserve_margin, |m| m.min(reserve_margin)),
                                },
                            );
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

                    // Tier 2 (Unified Paid) intentionally precedes Tier 3/4
                    // single-task runway-unsafe and 1%-floor subscriptions so
                    // bursts spill before stranding turns (distinct from any
                    // deferred multi-day cross-role reserve).
                    let (tier, binding_failure) = if !all_above_reserve || !weekly_safe {
                        (4u8, failing_tier4)
                    } else if five_hour_safe {
                        (0u8, None)
                    } else {
                        (3u8, failing_tier3)
                    };
                    (tier, weight, binding_failure)
                }
            }
        };

        let paid_cost_usd = (tier == 2).then_some(paid_estimate);
        let estimated_cost_usd = if tier == 2 {
            paid_estimate.is_finite().then_some(paid_estimate)
        } else {
            cand_est_usd.is_finite().then_some(cand_est_usd)
        };
        scored.push(ScoredPool {
            item,
            paid_cost_usd,
            estimated_cost_usd,
            tier,
            weight,
            is_anchor,
            binding_failure,
        });
    }

    // Deterministic pre-draw order so `units[i]` binds to a stable pool order.
    scored.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| {
                left.paid_cost_usd
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right.paid_cost_usd.unwrap_or(f64::INFINITY))
            })
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
    fn cold_start_preserves_relative_weights_and_centers_reservations_on_category_default() {
        use crate::quota_telemetry::{
            QuotaTelemetrySnapshot, RecentPlacement, WindowKey, WindowKind, WindowTelemetry,
        };
        use mj_core::config::HarnessKind;
        let now = 1_000_000;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let mut telemetry = QuotaTelemetrySnapshot::default();
        let candidates: Vec<_> = [
            ("cheap", "gpt-6.2-sol", "medium", 100),
            ("pricey", "gpt-6-astra", "medium", 200),
        ]
        .into_iter()
        .map(|(id, model, effort, price)| {
            scheduled_sub(
                id,
                HarnessKind::Codex,
                model,
                Some(effort),
                Some(price),
                vec![week_window(100, now)],
                None,
                false,
            )
        })
        .collect();
        let template = SchedulingContext {
            placement: Placement::Child,
            category: Some("subtle"),
            ranking_anchor: None,
            telemetry: &QuotaTelemetrySnapshot::default(),
            power_ranking: ranking,
            now_seconds: now,
        };
        let ctx = SchedulingContext {
            telemetry: &telemetry,
            ..template
        };
        let default = telemetry.category_stats("subtle").mean_api_usd;
        let lookup = |c: &ScheduledCandidate| {
            ranking
                .lookup(&c.model, c.effort.as_deref(), c.harness)
                .unwrap()
        };
        let sol = lookup(&candidates[0]);
        let astra = lookup(&candidates[1]);
        let sol_budget = sol.weekly_budget_usd.unwrap() / 2.0;
        let astra_budget = astra.weekly_budget_usd.unwrap();
        let master_ratio =
            sol.tasks_per_200_sub.unwrap() / (2.0 * astra.tasks_per_200_sub.unwrap());
        let scores = prepare_scored_pools(candidates.clone(), &ctx);
        let weight = |id: &str| {
            scores
                .iter()
                .find(|score| score.item.profile_id == id)
                .unwrap()
                .weight
        };
        assert!((weight("cheap") / weight("pricey") - master_ratio).abs() < 1e-6);
        let cheap_usd = sol_budget / weight("cheap");
        let pricey_usd = astra_budget / weight("pricey");
        assert!(cheap_usd < default && default < pricey_usd);
        assert!(((cheap_usd * pricey_usd).sqrt() - default).abs() < 1e-6);
        let single = prepare_scored_pools(vec![candidates[0].clone()], &ctx);
        assert!((sol_budget / single[0].weight - default).abs() < 1e-6);

        // With one prior placement and enough 5h headroom for two category
        // defaults, only the cheaper cell fits when both models are offered.
        let mut candidates = candidates;
        for (cand, budget) in candidates.iter_mut().zip([sol_budget, astra_budget]) {
            let CandidateQuota::Subscription { windows, .. } = &mut cand.quota else {
                unreachable!()
            };
            *windows = claude_window(11, Some(now + 3600), None).windows;
            telemetry.windows.insert(
                WindowKey {
                    profile_id: cand.profile_id.clone(),
                    cache_identity: cand.cache_identity.clone().unwrap(),
                    window_kind: WindowKind::Short5h,
                    model_scope: None,
                },
                WindowTelemetry {
                    five_hour_to_weekly_ratio: Some(10.05 * budget / (200.0 * default)),
                    ..WindowTelemetry::default()
                },
            );
            telemetry.recent_placements.insert(
                cand.profile_id.clone(),
                vec![RecentPlacement {
                    category: "subtle".into(),
                    model: Some(cand.model.clone()),
                    effort: cand.effort.clone(),
                    created_at_epoch_seconds: now,
                }],
            );
        }
        let ctx = SchedulingContext {
            telemetry: &telemetry,
            ..template
        };
        let scores = prepare_scored_pools(candidates.clone(), &ctx);
        assert_eq!(
            (scores[0].item.profile_id.as_str(), scores[0].tier),
            ("cheap", 0)
        );
        assert_eq!(
            (scores[1].item.profile_id.as_str(), scores[1].tier),
            ("pricey", 3)
        );
        assert_eq!(
            prepare_scored_pools(vec![candidates.remove(0)], &ctx)[0].tier,
            3
        );
    }

    #[test]
    fn calibration_weights_cheap_and_expensive_samples_equally() {
        let cheap = CellCostObservation {
            total_api_usd: 45.6,
            sample_count: 100,
        };
        let expensive = CellCostObservation {
            total_api_usd: 97.0,
            sample_count: 2,
        };
        let scale = calibrated_prior_scale(
            1.0,
            [(0.456, Some(&cheap)), (24.25, Some(&expensive))].into_iter(),
        );
        assert!((scale - 104.0 / 102.0).abs() < 1e-10);
    }

    #[test]
    fn learned_cell_cost_unifies_capacity_and_reservations_and_isolates_cross_category_scale() {
        use crate::database::CellCostKey;
        use crate::quota_telemetry::{
            QuotaTelemetrySnapshot, RecentPlacement, WindowKey, WindowKind, WindowTelemetry,
        };
        use mj_core::config::HarnessKind;
        let now = 1_000_000;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let mut telemetry = QuotaTelemetrySnapshot::default();
        let candidates: Vec<_> = [
            ("sol", HarnessKind::Codex, "gpt-6.2-sol", "medium"),
            ("kimi", HarnessKind::Kimi, "kimi-code/k3", "high"),
        ]
        .into_iter()
        .map(|(id, harness, model, effort)| {
            scheduled_sub(
                id,
                harness,
                model,
                Some(effort),
                Some(200),
                vec![week_window(100, now)],
                None,
                false,
            )
        })
        .collect();
        for (category, cand, mean, count) in [
            ("subtle", &candidates[0], 1.41, 100),
            ("subtle", &candidates[1], 16.0, 100),
            ("straightforward", &candidates[0], 0.20, 3),
        ] {
            telemetry.cell_costs.insert(
                CellCostKey {
                    category: category.into(),
                    model: cand.model.clone(),
                    effort: cand.effort.clone(),
                },
                CellCostObservation {
                    total_api_usd: mean * count as f64,
                    sample_count: count,
                },
            );
        }
        let lookup = |cand: &ScheduledCandidate| {
            ranking
                .lookup(&cand.model, cand.effort.as_deref(), cand.harness)
                .unwrap()
        };
        let benchmarks: Vec<_> = candidates
            .iter()
            .map(|cand| benchmark_task_usd(&lookup(cand)).unwrap())
            .collect();
        let scale = (1.41 / benchmarks[0] + 16.0 / benchmarks[1]) / 2.0;
        let expected = [
            (3.0 * scale * benchmarks[0] + 141.0) / 103.0,
            (3.0 * scale * benchmarks[1] + 1600.0) / 103.0,
        ];
        let score = |candidates, telemetry: &QuotaTelemetrySnapshot, power_ranking| {
            prepare_scored_pools(
                candidates,
                &SchedulingContext {
                    placement: Placement::Child,
                    category: Some("subtle"),
                    ranking_anchor: None,
                    telemetry,
                    power_ranking,
                    now_seconds: now,
                },
            )
        };
        let scores = score(candidates.clone(), &telemetry, ranking);
        for (cand, estimate) in candidates.iter().zip(expected) {
            let weight = scores
                .iter()
                .find(|s| s.item.profile_id == cand.profile_id)
                .unwrap()
                .weight;
            assert!((weight - lookup(cand).weekly_budget_usd.unwrap() / estimate).abs() < 1e-6);
        }

        // Exercise subscriptions with a ranked task count and no weekly USD
        // budget. Both observed cells move away from their calibrated priors.
        let mut ranking_without_budgets = ranking.clone();
        for entry in &mut ranking_without_budgets.entries {
            entry.weekly_budget_usd = None;
        }
        let fallback_scores = score(candidates.clone(), &telemetry, &ranking_without_budgets);
        let fallback_lookups: Vec<_> = candidates
            .iter()
            .map(|cand| {
                ranking_without_budgets
                    .lookup(&cand.model, cand.effort.as_deref(), cand.harness)
                    .unwrap()
            })
            .collect();
        let api_benchmarks: Vec<_> = fallback_lookups
            .iter()
            .map(|lookup| 200.0 / lookup.tasks_per_200_api.unwrap())
            .collect();
        let fallback_scale = (1.41 / api_benchmarks[0] + 16.0 / api_benchmarks[1]) / 2.0;
        for (i, observed_mean) in [1.41, 16.0].into_iter().enumerate() {
            let prior = fallback_scale * api_benchmarks[i];
            let estimate = (3.0 * prior + 100.0 * observed_mean) / 103.0;
            let cold_weekly =
                fallback_lookups[i].tasks_per_200_sub.unwrap() / (fallback_scale * WEEKS_PER_MONTH);
            let weight = fallback_scores
                .iter()
                .find(|s| s.item.profile_id == candidates[i].profile_id)
                .unwrap()
                .weight;
            assert!((weight / cold_weekly - prior / estimate).abs() < 1e-6);
            assert_eq!(weight > cold_weekly, i == 0);
        }

        let mut candidates = candidates;
        for cand in &mut candidates {
            let CandidateQuota::Subscription { windows, .. } = &mut cand.quota else {
                unreachable!()
            };
            *windows = claude_window(11, Some(now + 3600), None).windows;
        }
        let window_key = |cand: &ScheduledCandidate| WindowKey {
            profile_id: cand.profile_id.clone(),
            cache_identity: cand.cache_identity.clone().unwrap(),
            window_kind: WindowKind::Short5h,
            model_scope: None,
        };
        let kimi_budget = lookup(&candidates[1]).weekly_budget_usd.unwrap();
        let kimi_window = window_key(&candidates[1]);
        for (headroom_usd, placement, expected_tier) in [
            (0.99 * expected[1], false, 3),
            (1.01 * expected[1], false, 0),
            (1.99 * expected[1], true, 3),
            (2.01 * expected[1], true, 0),
        ] {
            telemetry.windows.insert(
                kimi_window.clone(),
                WindowTelemetry {
                    five_hour_to_weekly_ratio: Some(0.10 * kimi_budget / headroom_usd),
                    ..WindowTelemetry::default()
                },
            );
            telemetry.recent_placements.insert(
                "kimi".into(),
                if placement {
                    vec![RecentPlacement {
                        category: "subtle".into(),
                        model: Some(candidates[1].model.clone()),
                        effort: candidates[1].effort.clone(),
                        created_at_epoch_seconds: now,
                    }]
                } else {
                    vec![]
                },
            );
            assert_eq!(
                score(candidates.clone(), &telemetry, ranking)
                    .iter()
                    .find(|s| s.item.profile_id == "kimi")
                    .unwrap()
                    .tier,
                expected_tier
            );
        }

        telemetry.recent_placements.insert(
            "sol".into(),
            vec![RecentPlacement {
                category: "straightforward".into(),
                model: Some(candidates[0].model.clone()),
                effort: candidates[0].effort.clone(),
                created_at_epoch_seconds: now,
            }],
        );
        let cross_category_usd = (0.12 + 0.20) / 2.0;
        let sol_budget = lookup(&candidates[0]).weekly_budget_usd.unwrap();
        for (headroom_usd, expected_tier) in [
            (expected[0] + cross_category_usd - 0.01, 3),
            (expected[0] + cross_category_usd + 0.01, 0),
        ] {
            telemetry.windows.insert(
                window_key(&candidates[0]),
                WindowTelemetry {
                    five_hour_to_weekly_ratio: Some(0.10 * sol_budget / headroom_usd),
                    ..WindowTelemetry::default()
                },
            );
            assert_eq!(
                score(candidates.clone(), &telemetry, ranking)
                    .iter()
                    .find(|s| s.item.profile_id == "sol")
                    .unwrap()
                    .tier,
                expected_tier
            );
        }
    }

    #[test]
    fn pool_first_draw_invariance_deduplicates_models_sharing_one_pool() {
        let now = 1_000_000;
        let reset = now + 3 * 86_400;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        let ctx = SchedulingContext {
            placement: Placement::Child,
            category: Some("straightforward"),
            ranking_anchor: None,
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
                has_trailing_interval: true,
                second_to_last_clean_ts: Some(now - 600),
                five_hour_to_weekly_ratio: None,
            },
        );
        let burst: Vec<_> = (0..30)
            .map(|i| crate::quota_telemetry::RecentPlacement {
                category: "subtle".into(),
                model: None,
                effort: None,
                created_at_epoch_seconds: now - 500 + i,
            })
            .collect();
        telemetry
            .recent_placements
            .insert("codex-hot".into(), burst);

        let ctx = SchedulingContext {
            placement: Placement::Child,
            category: Some("subtle"),
            ranking_anchor: None,
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
    fn multi_cycle_reset_excludes_prior_placements_and_counts_reset_second_burst() {
        use crate::quota_telemetry::{QuotaTelemetrySnapshot, RecentPlacement};
        let now = 1_000_000;
        let start = now - 300;
        let mut window = week_window(0, start - 2 * 5 * 3600);
        window.label = "5h".into();
        window.duration_seconds = Some(5 * 3600);
        let eval = evaluate_applying_windows(&[window.clone()], "gpt-6.1-sol", now);
        assert_eq!(eval[0].cycle_start, Some(start));
        assert!((eval[0].t_left_hours - (5.0 - 300.0 / 3600.0)).abs() < 1e-9);
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let mut telemetry = QuotaTelemetrySnapshot::default();
        telemetry.windows.insert(
            crate::quota_telemetry::WindowKey {
                profile_id: "codex".into(),
                cache_identity: "id-codex".into(),
                window_kind: crate::quota_telemetry::WindowKind::Short5h,
                model_scope: None,
            },
            crate::quota_telemetry::WindowTelemetry {
                five_hour_to_weekly_ratio: Some(1.0),
                second_to_last_clean_ts: Some(start - 100),
                ..Default::default()
            },
        );
        let placement = |ts| RecentPlacement {
            category: "subtle".into(),
            model: None,
            effort: None,
            created_at_epoch_seconds: ts,
        };
        telemetry
            .recent_placements
            .insert("codex".into(), vec![placement(start - 1); 500]);
        let cand = scheduled_sub(
            "codex",
            mj_core::config::HarnessKind::Codex,
            "gpt-6.1-sol",
            Some("high"),
            Some(200),
            vec![window],
            None,
            false,
        );
        let tier = |telemetry: &QuotaTelemetrySnapshot| {
            prepare_scored_pools(
                vec![cand.clone()],
                &SchedulingContext {
                    placement: Placement::Child,
                    category: Some("subtle"),
                    ranking_anchor: None,
                    telemetry,
                    power_ranking: ranking,
                    now_seconds: now,
                },
            )[0]
            .tier
        };
        assert_eq!(tier(&telemetry), 0);
        telemetry
            .recent_placements
            .get_mut("codex")
            .unwrap()
            .extend(vec![placement(start); 500]);
        assert_eq!(tier(&telemetry), 3);
        let mut paid = cand.clone();
        paid.profile_id = "paid".into();
        paid.quota = CandidateQuota::UsagePriced;
        let ctx = SchedulingContext {
            placement: Placement::Child,
            category: Some("subtle"),
            ranking_anchor: None,
            telemetry: &telemetry,
            power_ranking: ranking,
            now_seconds: now,
        };
        let mut candidates = vec![cand, paid];
        let decision =
            order_scheduled_candidates_by_draw_with_decision(&mut candidates, &[0.1, 1.0], &ctx)
                .unwrap();
        assert_eq!(candidates[0].profile_id, "paid");
        assert_eq!(
            decision.binding_window,
            Some(crate::quota_telemetry::WindowKind::Short5h)
        );
    }

    #[test]
    fn paid_spill_reports_failing_5h_window_instead_of_lower_safe_weekly_window() {
        let now = 1_000_000;
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let mut telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        telemetry.windows.insert(
            crate::quota_telemetry::WindowKey {
                profile_id: "sub".into(),
                cache_identity: "id-sub".into(),
                window_kind: crate::quota_telemetry::WindowKind::Short5h,
                model_scope: None,
            },
            crate::quota_telemetry::WindowTelemetry {
                trailing_60m_pct_per_hour: 120.0,
                ..Default::default()
            },
        );
        let five_hour = crate::quota::QuotaWindow {
            label: "5h".into(),
            remaining_percent: Some(60),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: Some(now + 4 * 3600),
            duration_seconds: Some(5 * 3600),
            model_scope: None,
        };
        let weekly = week_window(20, now + 5 * 86_400);
        let sub = scheduled_sub(
            "sub",
            mj_core::config::HarnessKind::Codex,
            "gpt-6.1-sol",
            Some("high"),
            Some(200),
            vec![five_hour, weekly],
            None,
            false,
        );
        let mut paid = sub.clone();
        paid.profile_id = "dsf".into();
        paid.quota = CandidateQuota::UsagePriced;
        let ctx = SchedulingContext {
            placement: Placement::Child,
            category: Some("subtle"),
            ranking_anchor: None,
            telemetry: &telemetry,
            power_ranking: ranking,
            now_seconds: now,
        };
        let mut candidates = vec![sub, paid];
        let decision =
            order_scheduled_candidates_by_draw_with_decision(&mut candidates, &[0.5, 0.5], &ctx)
                .unwrap();
        assert_eq!(decision.chosen_profile, "dsf");
        assert_eq!(decision.binding_profile.as_deref(), Some("sub"));
        assert_eq!(
            decision.binding_window,
            Some(crate::quota_telemetry::WindowKind::Short5h)
        );
        assert_eq!(decision.binding_remaining_pct, Some(60.0));
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
                    tasks_per_200_api: Some(200.0 / 0.12),
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
                    tasks_per_200_api: Some(200.0 / 15.0),
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
            placement: Placement::Child,
            category: Some("straightforward"),
            ranking_anchor: None,
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
        for units in [
            [f64::MIN_POSITIVE, 1.0, 1.0, 1.0],
            [1.0, 1.0, f64::MIN_POSITIVE, 1.0],
            [1.0; 4],
        ] {
            let mut drawn = candidates.clone();
            order_scheduled_candidates_by_draw(&mut drawn, &units, &ctx);
            let ids = drawn
                .iter()
                .map(|c| c.profile_id.as_str())
                .collect::<Vec<_>>();
            assert!(
                ids.iter().position(|id| *id == "api-cheap")
                    < ids.iter().position(|id| *id == "paid-small")
            );
        }
        rank_scheduled_candidates(&mut candidates, &ctx);
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.profile_id.as_str())
                .collect::<Vec<_>>(),
            vec!["paid-large", "api-cheap", "paid-small", "api-expensive"]
        );
        let mut unknown = candidates[0].clone();
        unknown.profile_id = "unknown".into();
        unknown.quota = CandidateQuota::Unknown;
        candidates.push(unknown);
        order_scheduled_candidates_by_draw(&mut candidates, &[1.0; 5], &ctx);
        assert_eq!(candidates[0].profile_id, "unknown");
    }

    #[test]
    fn orchestrator_placement_excludes_harnesses_without_delegation_tools() {
        let now = 1_700_000_000;
        let telemetry = crate::quota_telemetry::QuotaTelemetrySnapshot::default();
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let ctx = SchedulingContext {
            placement: Placement::Orchestrator,
            category: Some("orchestrator"),
            ranking_anchor: None,
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
