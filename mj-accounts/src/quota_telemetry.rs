//! Trailing-60-minute quota burn velocity, durable placement reservations, and
//! category burn statistics for the unified runway gate.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use mj_client::quota::{ProfileQuota, QuotaWindow, WEEKLY_PERIOD_SECONDS};
use mj_core::usage::pricing::normalize_model;

use crate::database::{CellCostKey, CellCostObservation, canonical_observed_category};
use crate::profile_scheduler::{
    benchmark_task_usd, calibrated_prior_scale, cell_prior_usd, shrink_cell_cost_usd,
};

/// Tolerance for reset-deadline jitter before an interval is treated as a
/// shifted deadline (Codex / Antigravity) and dropped from velocity.
const RESET_SHIFT_TOLERANCE_SECONDS: i64 = 120;

/// Trailing window over which clean `quota_history` intervals are integrated.
const VELOCITY_WINDOW_SECONDS: i64 = 60 * 60;

/// Lookback window loaded from `quota_history` so the first clean reading
/// before the 60-minute boundary is included.
pub const HISTORY_LOOKBACK_SECONDS: u64 = 90 * 60;

/// Ratio history spans seven days. Re-logins that change the cache identity
/// isolate immediately. Plan-tier changes with the same identity blend until
/// the older readings age out.
pub const RATIO_LOOKBACK_SECONDS: u64 = 7 * 24 * 60 * 60;
const RATIO_REHYDRATE_INTERVAL_SECONDS: i64 = 60 * 60;

/// Default reservation lookback when only a single clean reading exists.
const SINGLE_READING_RESERVATION_LOOKBACK_SECONDS: i64 = 10 * 60;

/// Minimum completed sessions with `category` populated before replacing the
/// measured default burn/duration constants with local SQLite observations.
pub const LOCAL_CATEGORY_SWITCHOVER_MIN_SAMPLES: usize = 200;

/// Maximum recent completed sessions inspected per category during the hourly
/// background refresh.
pub const LOCAL_CATEGORY_MAX_SAMPLES: usize = 1000;

/// Maximum priced terminal sessions retained per category, model and effort.
pub const LOCAL_CELL_MAX_SAMPLES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowKind {
    Short5h,
    Long,
}

impl WindowKind {
    pub fn from_window(window: &QuotaWindow) -> Self {
        if window.is_five_hour() {
            Self::Short5h
        } else {
            Self::Long
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowKey {
    pub profile_id: String,
    pub cache_identity: String,
    pub window_kind: WindowKind,
    pub model_scope: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowTelemetry {
    pub trailing_60m_pct_per_hour: f64,
    pub second_to_last_clean_ts: Option<i64>,
    pub five_hour_to_weekly_ratio: Option<f64>,
    pub has_trailing_interval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentPlacement {
    pub category: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub created_at_epoch_seconds: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CategoryBurnStats {
    pub mean_api_usd: f64,
    pub runway_duration_hours: f64,
    pub local_sample_count: usize,
}

/// Measured category burn defaults from the 2026-11-21 live-store empirical
/// analysis (180,417 turns / 50,192 children; `c4ecd0f90d14c80a4f0d25104e36eebe/report.md`),
/// with conservative initial defaults for unmeasured categories (`oracle`,
/// `orchestrator`, `primary`). Replaced per category once `N >= 200` local
/// completed sessions accumulate.
pub fn default_category_burn_stats(category: &str) -> CategoryBurnStats {
    match category {
        "straightforward" => CategoryBurnStats {
            mean_api_usd: 0.12,
            runway_duration_hours: 71.0 / 60.0,
            local_sample_count: 0,
        },
        "subtle" => CategoryBurnStats {
            mean_api_usd: 3.47,
            runway_duration_hours: 166.0 / 60.0,
            local_sample_count: 0,
        },
        "reviewer" | "brief_writer" => CategoryBurnStats {
            mean_api_usd: 0.22,
            runway_duration_hours: 6.0 / 60.0,
            local_sample_count: 0,
        },
        "oracle" => CategoryBurnStats {
            mean_api_usd: 0.50,
            runway_duration_hours: 15.0 / 60.0,
            local_sample_count: 0,
        },
        "supervisor" | "orchestrator" | "primary" => CategoryBurnStats {
            mean_api_usd: 5.00,
            runway_duration_hours: 180.0 / 60.0,
            local_sample_count: 0,
        },
        _ => CategoryBurnStats {
            mean_api_usd: 0.50,
            runway_duration_hours: 1.0,
            local_sample_count: 0,
        },
    }
}

/// Scale the benchmark weekly budget to the profile's monthly plan price.
pub fn effective_weekly_budget_usd(
    weekly_budget_usd: Option<f64>,
    p_eff: f64,
    plan_usd_per_month: Option<f64>,
) -> Option<f64> {
    let weekly_budget = weekly_budget_usd.filter(|v| v.is_finite() && *v > 0.0)?;
    let plan_price = plan_usd_per_month
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(200.0);
    let scale = if p_eff.is_finite() && p_eff > 0.0 {
        p_eff / plan_price
    } else {
        1.0
    };
    let effective_budget = weekly_budget * scale;
    (effective_budget.is_finite() && effective_budget > 0.0).then_some(effective_budget)
}

/// Convert a task's USD cost into weekly quota percentage points.
pub fn mean_burn_weekly_pct(
    mean_api_usd: f64,
    weekly_budget_usd: Option<f64>,
    p_eff: f64,
    plan_usd_per_month: Option<f64>,
) -> f64 {
    effective_weekly_budget_usd(weekly_budget_usd, p_eff, plan_usd_per_month)
        .map_or(0.0, |budget| (100.0 * mean_api_usd / budget).max(0.0))
}

/// Convert a weekly percentage burn into a 5-hour percentage burn when the
/// profile has an observed `five_hour_to_weekly_ratio`. When no paired ratio
/// is observed, returns `0.0` so the 5h runway gate relies on
/// `v_5h * min(d_category, t_5h_left)`.
pub fn mean_burn_for_window_pct(
    mean_api_usd: f64,
    window_kind: WindowKind,
    weekly_budget_usd: Option<f64>,
    p_eff: f64,
    plan_usd_per_month: Option<f64>,
    five_hour_to_weekly_ratio: Option<f64>,
) -> f64 {
    let weekly_pct =
        mean_burn_weekly_pct(mean_api_usd, weekly_budget_usd, p_eff, plan_usd_per_month);
    match window_kind {
        WindowKind::Long => weekly_pct,
        WindowKind::Short5h => {
            match five_hour_to_weekly_ratio.filter(|r| r.is_finite() && *r > 0.0) {
                Some(ratio) => weekly_pct * ratio,
                None => 0.0,
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaidSpillCounters {
    pub total_draws: u64,
    pub empty_draws: u64,
    pub empty_draws_by_category: BTreeMap<String, u64>,
    pub unsafe_sub_fallback: u64,
    pub unsafe_sub_fallback_by_category: BTreeMap<String, u64>,
    pub paid_spill_5h_unsafe: u64,
    pub paid_spill_weekly_unsafe: u64,
    pub paid_spill_exhausted_or_absent: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaTelemetrySnapshot {
    pub projections: Vec<mj_core::usage::ProfileQuotaProjection>,
    pub paid_spill_counters: PaidSpillCounters,
    pub windows: BTreeMap<WindowKey, WindowTelemetry>,
    pub recent_placements: BTreeMap<String, Vec<RecentPlacement>>,
    pub category_stats: BTreeMap<String, CategoryBurnStats>,
    pub cell_costs: BTreeMap<CellCostKey, CellCostObservation>,
}

impl QuotaTelemetrySnapshot {
    pub fn category_stats(&self, category: &str) -> CategoryBurnStats {
        self.category_stats
            .get(category)
            .cloned()
            .unwrap_or_else(|| default_category_burn_stats(category))
    }

    pub fn cell_cost_observation(
        &self,
        category: &str,
        model: &str,
        effort: Option<&str>,
    ) -> Option<CellCostObservation> {
        let mut key = CellCostKey {
            category: canonical_observed_category(category)
                .unwrap_or(category)
                .to_owned(),
            model: normalize_model(model.trim()),
            effort: None,
        };
        let mut total = CellCostObservation {
            total_api_usd: 0.0,
            sample_count: 0,
        };
        let mut add = |obs: &CellCostObservation| {
            total.total_api_usd += obs.total_api_usd;
            total.sample_count += obs.sample_count;
        };
        if let Some(effort) = normalized_cell_effort(effort) {
            if let Some(obs) = self.cell_costs.get(&key) {
                add(obs);
            }
            key.effort = Some(effort);
            if let Some(obs) = self.cell_costs.get(&key) {
                add(obs);
            }
        } else {
            for (_, obs) in self
                .cell_costs
                .range(key.clone()..)
                .take_while(|(cell, _)| cell.category == key.category && cell.model == key.model)
            {
                add(obs);
            }
        }
        (total.sample_count > 0).then_some(total)
    }

    pub fn window_telemetry(
        &self,
        profile_id: &str,
        cache_identity: Option<&str>,
        window_kind: WindowKind,
        model_scope: Option<&str>,
    ) -> Option<&WindowTelemetry> {
        if let Some(identity) = cache_identity {
            let key = WindowKey {
                profile_id: profile_id.to_owned(),
                cache_identity: identity.to_owned(),
                window_kind,
                model_scope: model_scope.map(str::to_owned),
            };
            // Exact match on `(profile_id, cache_identity)`: never mix history
            // from a superseded login identity.
            return self.windows.get(&key);
        }
        // Fallback match by `(profile_id, window_kind, model_scope)` only when
        // callers do not carry `cache_identity`.
        self.windows.iter().find_map(|(k, v)| {
            (k.profile_id == profile_id
                && k.window_kind == window_kind
                && k.model_scope.as_deref() == model_scope)
                .then_some(v)
        })
    }

    pub fn qualifying_placements(
        &self,
        profile_id: &str,
        second_to_last_clean_ts: Option<i64>,
        min_inclusive_ts: Option<i64>,
        now_seconds: i64,
    ) -> impl Iterator<Item = &RecentPlacement> + use<'_> {
        self.recent_placements
            .get(profile_id)
            .into_iter()
            .flatten()
            .filter(move |placement| {
                second_to_last_clean_ts.map_or_else(
                    || {
                        placement.created_at_epoch_seconds
                            >= now_seconds.saturating_sub(HISTORY_LOOKBACK_SECONDS as i64)
                    },
                    |cutoff| placement.created_at_epoch_seconds > cutoff,
                ) && min_inclusive_ts
                    .is_none_or(|floor| placement.created_at_epoch_seconds >= floor)
            })
    }

    /// Sum `mean_burn_pct(placement.category, window_kind)` over placements on
    /// `profile_id` created strictly after `second_to_last_clean_ts` (or all
    /// recent placements if `second_to_last_clean_ts` is `None`).
    #[allow(clippy::too_many_arguments)]
    pub fn reserved_pct(
        &self,
        profile_id: &str,
        window_kind: WindowKind,
        second_to_last_clean_ts: Option<i64>,
        weekly_budget_usd: Option<f64>,
        p_eff: f64,
        plan_usd_per_month: Option<f64>,
        five_hour_to_weekly_ratio: Option<f64>,
        now_seconds: i64,
    ) -> f64 {
        self.qualifying_placements(profile_id, second_to_last_clean_ts, None, now_seconds)
            .map(|placement| {
                let stats = self.category_stats(&placement.category);
                mean_burn_for_window_pct(
                    stats.mean_api_usd,
                    window_kind,
                    weekly_budget_usd,
                    p_eff,
                    plan_usd_per_month,
                    five_hour_to_weekly_ratio,
                )
            })
            .sum()
    }
}

pub(crate) fn normalized_cell_effort(effort: Option<&str>) -> Option<String> {
    effort
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Extract usage percentage in `[0.0, 100.0]` from a `QuotaWindow`:
/// `100.0 * used / limit` when both `used` and `limit > 0`, else
/// `100.0 - remaining_percent`.
fn window_used_pct(window: &QuotaWindow) -> Option<f64> {
    if let (Some(used), Some(limit)) = (window.used, window.limit)
        && limit > 0
    {
        return Some((100.0 * used as f64 / limit as f64).clamp(0.0, 100.0));
    }
    window
        .remaining_percent
        .map(|rem| (100.0 - f64::from(rem)).clamp(0.0, 100.0))
}

#[derive(Debug, Clone)]
struct SamplePoint {
    ts: i64,
    used_pct: f64,
    resets_at: Option<i64>,
}

/// Compute `WindowTelemetry` entries for one `(profile_id, cache_identity)`
/// from raw `quota_history` rows (in any order).
pub fn compute_window_telemetry(
    profile_id: &str,
    cache_identity: &str,
    history: &[ProfileQuota],
    now_seconds: i64,
) -> BTreeMap<WindowKey, WindowTelemetry> {
    let clean = clean_history(history);

    let cutoff = now_seconds.saturating_sub(VELOCITY_WINDOW_SECONDS);

    // Group clean samples by (window_kind, model_scope).
    let mut series: BTreeMap<(WindowKind, Option<String>), Vec<SamplePoint>> = BTreeMap::new();
    for row in &clean {
        let ts = i64::try_from(row.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
        for window in &row.windows {
            let Some(used_pct) = window_used_pct(window) else {
                continue;
            };
            let kind = WindowKind::from_window(window);
            let scope = window.model_scope.clone();
            series.entry((kind, scope)).or_default().push(SamplePoint {
                ts,
                used_pct,
                resets_at: window.resets_at_epoch_seconds,
            });
        }
    }

    let mut sums_by_scope = BTreeMap::new();
    accumulate_ratio_deltas(&clean, None, &mut sums_by_scope);

    let mut out = BTreeMap::new();
    for ((window_kind, model_scope), points) in series {
        let second_to_last_clean_ts = match points.len() {
            0 => None,
            1 => Some(
                points[0]
                    .ts
                    .saturating_sub(SINGLE_READING_RESERVATION_LOOKBACK_SECONDS),
            ),
            n => Some(points[n - 2].ts),
        };

        let mut sum_delta_used = 0.0;
        let mut sum_delta_hours = 0.0;
        for pair in points.windows(2) {
            let (prev, cur) = (&pair[0], &pair[1]);
            // Only integrate intervals whose end lies within the trailing 60 minutes.
            if cur.ts < cutoff {
                continue;
            }
            if is_corrupted_interval(
                prev.ts,
                cur.ts,
                prev.used_pct,
                cur.used_pct,
                prev.resets_at,
                cur.resets_at,
            ) {
                continue;
            }
            sum_delta_used += cur.used_pct - prev.used_pct;
            sum_delta_hours += (cur.ts - prev.ts) as f64 / 3600.0;
        }
        let trailing_60m_pct_per_hour = if sum_delta_hours > 0.0 {
            (sum_delta_used / sum_delta_hours).max(0.0)
        } else {
            0.0
        };

        let five_hour_to_weekly_ratio = ratio_for_scope(&sums_by_scope, &model_scope);

        out.insert(
            WindowKey {
                profile_id: profile_id.to_owned(),
                cache_identity: cache_identity.to_owned(),
                window_kind,
                model_scope,
            },
            WindowTelemetry {
                trailing_60m_pct_per_hour,
                has_trailing_interval: sum_delta_hours > 0.0,
                second_to_last_clean_ts,
                five_hour_to_weekly_ratio,
            },
        );
    }
    out
}

fn clean_history(history: &[ProfileQuota]) -> Vec<&ProfileQuota> {
    let mut clean: Vec<&ProfileQuota> = history
        .iter()
        .filter(|row| row.error.is_none() && !row.windows.is_empty())
        .collect();
    clean.sort_by_key(|row| row.refreshed_at_epoch_seconds);
    clean
}

fn accumulate_ratio_deltas(
    clean: &[&ProfileQuota],
    min_cur_ts_exclusive: Option<i64>,
    sums_by_scope: &mut BTreeMap<Option<String>, (f64, f64)>,
) -> Option<i64> {
    for pair in clean.windows(2) {
        let (prev, cur) = (pair[0], pair[1]);
        let prev_ts = i64::try_from(prev.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
        let cur_ts = i64::try_from(cur.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
        if cur_ts <= prev_ts || min_cur_ts_exclusive.is_some_and(|min| cur_ts <= min) {
            continue;
        }
        // Match windows by (kind, model_scope) between prev and cur.
        let mut short_deltas: BTreeMap<Option<String>, f64> = BTreeMap::new();
        let mut long_deltas: BTreeMap<Option<String>, f64> = BTreeMap::new();
        for cur_w in &cur.windows {
            let Some(cur_used) = window_used_pct(cur_w) else {
                continue;
            };
            let kind = WindowKind::from_window(cur_w);
            let Some(prev_w) = prev.windows.iter().find(|pw| {
                WindowKind::from_window(pw) == kind && pw.model_scope == cur_w.model_scope
            }) else {
                continue;
            };
            let Some(prev_used) = window_used_pct(prev_w) else {
                continue;
            };
            if is_corrupted_interval(
                prev_ts,
                cur_ts,
                prev_used,
                cur_used,
                prev_w.resets_at_epoch_seconds,
                cur_w.resets_at_epoch_seconds,
            ) {
                continue;
            }
            let delta = cur_used - prev_used;
            let deltas = match kind {
                WindowKind::Short5h => &mut short_deltas,
                WindowKind::Long => &mut long_deltas,
            };
            deltas.insert(cur_w.model_scope.clone(), delta);
        }
        for (scope, d_5h) in &short_deltas {
            if let Some(d_long) = long_deltas.get(scope).or_else(|| long_deltas.get(&None))
                && (*d_5h > 0.0 || *d_long > 0.0)
            {
                let entry = sums_by_scope.entry(scope.clone()).or_default();
                entry.0 += *d_5h;
                entry.1 += *d_long;
            }
        }
    }
    clean
        .last()
        .map(|row| i64::try_from(row.refreshed_at_epoch_seconds).unwrap_or(i64::MAX))
}

fn ratio_for_scope(
    sums_by_scope: &BTreeMap<Option<String>, (f64, f64)>,
    scope: &Option<String>,
) -> Option<f64> {
    sums_by_scope
        .get(scope)
        .or_else(|| sums_by_scope.get(&None))
        .and_then(|(num, den)| (*den > 0.0 && *num > 0.0).then_some(*num / *den))
}

fn is_corrupted_interval(
    prev_ts: i64,
    cur_ts: i64,
    prev_used: f64,
    cur_used: f64,
    prev_reset: Option<i64>,
    cur_reset: Option<i64>,
) -> bool {
    if cur_ts <= prev_ts {
        return true;
    }
    if cur_used < prev_used {
        return true;
    }
    if let (Some(pr), Some(cr)) = (prev_reset, cur_reset)
        && (cr - pr).abs() > RESET_SHIFT_TOLERANCE_SECONDS
    {
        return true;
    }
    false
}

/// One enabled account whose weekly consumption a report asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaUsageProfile {
    pub profile_id: String,
    /// The identity the daemon's quota poller currently probes this profile
    /// under; `None` before its first publication, when nothing is attributable.
    pub cache_identity: Option<String>,
    pub monthly_price_usd: Option<u32>,
}

/// The usage-report entry for one enabled profile, from its poller publication
/// (`None` before its first). Profiles known to be usage-priced or without any
/// quota to report have no allowance to consume, so they are left out and never
/// count as quota the user holds.
pub fn quota_usage_profile(
    profile_id: &str,
    publication: Option<&crate::quota::QuotaReportPublication>,
) -> Option<QuotaUsageProfile> {
    let report = publication.and_then(|publication| {
        publication
            .last_good
            .as_ref()
            .or(publication.report.as_ref())
    });
    if report.is_some_and(|report| report.is_usage_priced() || report.is_not_reported()) {
        return None;
    }
    Some(QuotaUsageProfile {
        profile_id: profile_id.to_owned(),
        cache_identity: publication.map(|publication| publication.cache_identity.clone()),
        monthly_price_usd: report.and_then(|report| report.monthly_price_usd),
    })
}

/// Each profile's unscoped weekly quota consumed in the inclusive intervals,
/// read from `quota_history`. Blocking: one bracketing read per profile. A
/// profile with no identity, no measurable weekly reading or an unreadable
/// history yields `None` (a failed read is logged). An empty selection is zero.
pub fn weekly_quota_usage(
    profiles: Vec<QuotaUsageProfile>,
    intervals: &[(i64, i64)],
) -> Vec<mj_core::usage::ProfileQuotaUsage> {
    weekly_quota_usage_with_loader(profiles, intervals, |identity, profile, start, end| {
        crate::database::load_quota_history_bracketing(
            identity,
            profile,
            start,
            end,
            WEEKLY_PERIOD_SECONDS as u64,
            has_weekly_reading,
        )
    })
}

fn weekly_quota_usage_with_loader(
    profiles: Vec<QuotaUsageProfile>,
    intervals: &[(i64, i64)],
    load: impl Fn(&str, &str, u64, u64) -> anyhow::Result<Vec<ProfileQuota>>,
) -> Vec<mj_core::usage::ProfileQuotaUsage> {
    let spans: Vec<_> = intervals
        .iter()
        .map(|&(start, end)| weekly_usage_span(start, end))
        .collect();
    profiles
        .into_iter()
        .map(|profile| {
            let consumption = if spans.is_empty() {
                Some((
                    WeeklyConsumption {
                        basis_points: 0,
                        partial: false,
                    },
                    0,
                ))
            } else {
                profile.cache_identity.as_deref().and_then(|identity| {
                    let since = spans.first()?.0;
                    let until = spans.last()?.1;
                    let bounds = u64::try_from(since).ok().zip(u64::try_from(until).ok())?;
                    match load(identity, &profile.profile_id, bounds.0, bounds.1) {
                        Ok(history) => weekly_consumption_with_coverage(&history, &spans),
                        Err(error) => {
                            tracing::warn!(
                                profile_id = %profile.profile_id,
                                error = %format!("{error:#}"),
                                "could not read the stored quota probes for the usage report"
                            );
                            None
                        }
                    }
                })
            };
            mj_core::usage::ProfileQuotaUsage {
                profile: profile.profile_id,
                consumed_basis_points: consumption.map(|(consumption, _)| consumption.basis_points),
                partial: consumption.is_some_and(|(consumption, _)| consumption.partial),
                covered_seconds: consumption.and_then(|(_, covered)| u64::try_from(covered).ok()),
                monthly_price_usd: profile.monthly_price_usd,
            }
        })
        .collect()
}

/// A report span in whole seconds, `since` inclusive and `until` exclusive,
/// from the report's millisecond bounds, whose `until_ms` is inclusive: a
/// completed hour ends at `HH:59:59.999`, and its last second must count.
fn weekly_usage_span(since_ms: i64, until_ms: i64) -> (i64, i64) {
    (
        since_ms.div_euclid(1000),
        until_ms.div_euclid(1000).saturating_add(1),
    )
}

/// The unscoped weekly reading of a successful probe: the one interpretation
/// both the history reader's edge search and the measurement use. A reading
/// whose own reset time has passed describes no live weekly window (the
/// provider has not yet reported the next one), so it is not a reading.
fn weekly_reading(probe: &ProfileQuota) -> Option<SamplePoint> {
    if probe.error.is_some() {
        return None;
    }
    let window = probe
        .windows
        .iter()
        .find(|window| window.is_weekly_period())?;
    sample_for_window(probe, window)
}

fn sample_for_window(probe: &ProfileQuota, window: &QuotaWindow) -> Option<SamplePoint> {
    if probe.error.is_some() {
        return None;
    }
    let ts = i64::try_from(probe.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
    if window
        .resets_at_epoch_seconds
        .is_some_and(|reset| reset <= ts)
    {
        return None;
    }
    Some(SamplePoint {
        ts,
        used_pct: window_used_pct(window)?,
        resets_at: window.resets_at_epoch_seconds,
    })
}

/// Whether the probe carries a weekly reading the measurement can use.
pub fn has_weekly_reading(probe: &ProfileQuota) -> bool {
    weekly_reading(probe).is_some()
}

/// Weekly quota consumed over a span and whether the probes covered all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeeklyConsumption {
    /// 10,000 is all of one weekly allowance.
    pub basis_points: u64,
    /// Some of the span could not be measured: it lies before the first
    /// reading or after the last, or it is an interval that could not be read.
    pub partial: bool,
}

/// Weekly quota consumed over `since..until` (epoch seconds, `until`
/// exclusive), estimated from the probes around and inside the span.
/// - Consecutive probes that both read the unscoped weekly window give the
///   used-percent growth between them; an interval crossing a span edge counts
///   in proportion to the part inside, as if use were even, which can over- or
///   under-count that edge.
/// - A probe whose own reset time has passed is no reading (`weekly_reading`),
///   so a counter left standing after its deadline is never an endpoint.
/// - When the weekly deadline fell inside the interval (the old deadline is at
///   or before the later probe) and the next deadline is later, only the later
///   reading counts, spread from the old deadline to that probe. What was used
///   before the reset is lost, so that part of the span is not covered.
/// - Intervals that cannot be read as use count for nothing and are not
///   covered: a deadline that moved without elapsing (provider jitter), a fall
///   in use with no reset, and probes more than one weekly period apart, which
///   hide whole allowance cycles.
/// - Nothing is extrapolated before the first reading or past the last, so a
///   span ending now is partial.
///
/// `partial` is true unless every second of the span lies in a measured
/// interval. `None` when no interval overlapping the span could be measured,
/// which is not the same as measuring zero use.
pub fn weekly_consumption(
    history: &[ProfileQuota],
    since: i64,
    until: i64,
) -> Option<WeeklyConsumption> {
    weekly_consumption_in_spans(history, &[(since, until)])
}

fn weekly_consumption_in_spans(
    history: &[ProfileQuota],
    spans: &[(i64, i64)],
) -> Option<WeeklyConsumption> {
    weekly_consumption_with_coverage(history, spans).map(|(c, _)| c)
}

fn weekly_consumption_with_coverage(
    history: &[ProfileQuota],
    spans: &[(i64, i64)],
) -> Option<(WeeklyConsumption, i64)> {
    consumption_with_coverage(history, spans, weekly_reading)
}

fn consumption_with_coverage(
    history: &[ProfileQuota],
    spans: &[(i64, i64)],
    reading: impl Fn(&ProfileQuota) -> Option<SamplePoint>,
) -> Option<(WeeklyConsumption, i64)> {
    let duration: i64 = spans
        .iter()
        .map(|&(since, until)| until.saturating_sub(since).max(0))
        .sum();
    if duration == 0 {
        return None;
    }
    let mut readings: Vec<SamplePoint> = history.iter().filter_map(reading).collect();
    readings.sort_by_key(|reading| reading.ts);
    // Seconds of `from..to` inside the span.
    let inside = |from: i64, to: i64| {
        spans
            .iter()
            .map(|&(since, until)| (to.min(until) - from.max(since)).max(0))
            .sum::<i64>()
    };
    let mut covered = 0i64;
    let mut consumed_pct = 0.0;
    for pair in readings.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if cur.ts <= prev.ts || cur.ts - prev.ts > WEEKLY_PERIOD_SECONDS {
            continue;
        }
        let (from, used) = match (prev.resets_at, cur.resets_at) {
            (Some(old), Some(new)) if (new - old).abs() > RESET_SHIFT_TOLERANCE_SECONDS => {
                // A reset happened between the probes only if the old deadline
                // (after the earlier probe, which `weekly_reading` ensures)
                // fell by the later one; otherwise the use cannot be split.
                if old > cur.ts {
                    continue;
                }
                (old, cur.used_pct)
            }
            _ if cur.used_pct < prev.used_pct => continue,
            _ => (prev.ts, cur.used_pct - prev.used_pct),
        };
        if cur.ts <= from {
            continue;
        }
        let overlap = inside(from, cur.ts);
        covered += overlap;
        consumed_pct += used * overlap as f64 / (cur.ts - from) as f64;
    }
    (covered > 0).then(|| {
        (
            WeeklyConsumption {
                basis_points: (consumed_pct * 100.0).round() as u64,
                partial: covered < duration,
            },
            covered,
        )
    })
}

#[derive(Debug, Clone, Default)]
struct RatioLedger {
    sums_by_scope: BTreeMap<Option<String>, (f64, f64)>,
    last_clean_ts: Option<i64>,
    hydrated_at: i64,
}

impl RatioLedger {
    fn accumulate(&mut self, history: &[ProfileQuota]) {
        let observed_max_ts = accumulate_ratio_deltas(
            &clean_history(history),
            self.last_clean_ts,
            &mut self.sums_by_scope,
        );
        self.last_clean_ts = self.last_clean_ts.max(observed_max_ts);
    }

    fn ratio_for_scope(&self, scope: &Option<String>) -> Option<f64> {
        ratio_for_scope(&self.sums_by_scope, scope)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StampedQuotaProjection {
    cache_identity: String,
    refreshed_at_epoch_seconds: u64,
    projection: mj_core::usage::ProfileQuotaProjection,
}

struct PendingReservation {
    profile_id: String,
    placement: RecentPlacement,
    decision: Option<crate::profile_scheduler::PlacementDecision>,
}

#[derive(Default)]
struct QuotaTelemetryStoreInner {
    projections: Vec<StampedQuotaProjection>,
    paid_spill_counters: PaidSpillCounters,
    windows: BTreeMap<WindowKey, WindowTelemetry>,
    ratio_ledgers: BTreeMap<(String, String), RatioLedger>,
    durable_placements: BTreeMap<String, Vec<RecentPlacement>>,
    category_stats: BTreeMap<String, CategoryBurnStats>,
    cell_costs: BTreeMap<CellCostKey, CellCostObservation>,
    next_reservation_id: u64,
    in_flight: BTreeMap<u64, PendingReservation>,
    committed_pending: BTreeMap<(String, String), (String, RecentPlacement)>,
}

impl QuotaTelemetryStoreInner {
    fn build_snapshot(&self) -> QuotaTelemetrySnapshot {
        let mut recent_placements = self.durable_placements.clone();
        for (profile_id, placement) in self.committed_pending.values() {
            recent_placements
                .entry(profile_id.clone())
                .or_default()
                .push(placement.clone());
        }
        for pending in self.in_flight.values() {
            recent_placements
                .entry(pending.profile_id.clone())
                .or_default()
                .push(pending.placement.clone());
        }
        QuotaTelemetrySnapshot {
            projections: self
                .projections
                .iter()
                .map(|entry| entry.projection.clone())
                .collect(),
            paid_spill_counters: self.paid_spill_counters.clone(),
            windows: self.windows.clone(),
            recent_placements,
            category_stats: self.category_stats.clone(),
            cell_costs: self.cell_costs.clone(),
        }
    }
}

/// RAII handle for a tentative in-flight placement reservation created
/// atomically during `QuotaTelemetryStore::draw_and_reserve`.
///
/// Dropping without calling [`Self::commit`] releases the reservation
/// immediately so failed spawns/switches never leak reserved burn.
pub struct PlacementReservation {
    store: QuotaTelemetryStore,
    reservation_id: u64,
    committed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for PlacementReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacementReservation")
            .field("reservation_id", &self.reservation_id)
            .field(
                "committed",
                &self.committed.load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish()
    }
}

impl PlacementReservation {
    /// Promote this tentative reservation to a committed placement. Passing
    /// `Some(session_id)` enables exact deduplication on `refresh_from_store`.
    pub fn commit(&self, session_id: Option<String>) {
        if self
            .committed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let mut guard = self
            .store
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(pending) = guard.in_flight.remove(&self.reservation_id) {
            guard.paid_spill_counters.total_draws += 1;
            if let Some(decision) = pending.decision {
                use crate::profile_scheduler::PlacementDecisionReason;
                match decision.reason {
                    PlacementDecisionReason::PaidSpill {
                        sub_5h_unsafe,
                        sub_weekly_unsafe,
                        sub_exhausted_or_absent,
                    } => {
                        guard.paid_spill_counters.paid_spill_5h_unsafe +=
                            u64::from(sub_5h_unsafe > 0);
                        guard.paid_spill_counters.paid_spill_weekly_unsafe +=
                            u64::from(sub_weekly_unsafe > 0);
                        guard.paid_spill_counters.paid_spill_exhausted_or_absent += u64::from(
                            sub_exhausted_or_absent > 0
                                || (sub_5h_unsafe == 0 && sub_weekly_unsafe == 0),
                        );
                    }
                    PlacementDecisionReason::UnsafeSubscriptionFallback => {
                        guard.paid_spill_counters.unsafe_sub_fallback += 1;
                        *guard
                            .paid_spill_counters
                            .unsafe_sub_fallback_by_category
                            .entry(pending.placement.category.clone())
                            .or_default() += 1;
                    }
                    PlacementDecisionReason::SafeSubscription
                    | PlacementDecisionReason::UnknownQuota => {}
                }
            }
            let key = (
                session_id.unwrap_or_else(|| format!("__unkeyed-{}", self.reservation_id)),
                pending.profile_id.clone(),
            );
            guard
                .committed_pending
                .insert(key, (pending.profile_id, pending.placement));
        }
    }
}

impl Drop for PlacementReservation {
    fn drop(&mut self) {
        if !self.committed.load(std::sync::atomic::Ordering::Acquire) {
            let mut guard = self
                .store
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.in_flight.remove(&self.reservation_id);
        }
    }
}

/// Shared, cheaply cloneable handle to the daemon's current
/// `QuotaTelemetrySnapshot`.
#[derive(Clone, Default)]
pub struct QuotaTelemetryStore {
    inner: Arc<RwLock<QuotaTelemetryStoreInner>>,
}

impl QuotaTelemetryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> QuotaTelemetrySnapshot {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .build_snapshot()
    }

    /// Return only projections whose `(cache_identity, refreshed_at_epoch_seconds)`
    /// matches the caller's currently published balance reading for that profile.
    pub fn projections_matching(
        &self,
        stamps: &BTreeMap<String, (String, u64)>,
    ) -> Vec<mj_core::usage::ProfileQuotaProjection> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .projections
            .iter()
            .filter(|entry| {
                stamps.get(&entry.projection.profile_id).is_some_and(
                    |(cache_identity, refreshed_at)| {
                        *cache_identity == entry.cache_identity
                            && *refreshed_at == entry.refreshed_at_epoch_seconds
                    },
                )
            })
            .map(|entry| entry.projection.clone())
            .collect()
    }

    /// Atomically snapshot telemetry, draw/order `candidates`, and reserve
    /// `candidates[0]` in-flight under a single short synchronous lock.
    pub fn draw_and_reserve<T: AsRef<crate::profile_scheduler::ScheduledCandidate>>(
        &self,
        candidates: &mut Vec<T>,
        placement: crate::profile_scheduler::Placement,
        category: Option<&str>,
        ranking_anchor: Option<&str>,
        power_ranking: &crate::power_ranking::PowerRankingSnapshot,
        now_seconds: i64,
    ) -> anyhow::Result<Option<PlacementReservation>> {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshot = guard.build_snapshot();
        let ctx = crate::profile_scheduler::SchedulingContext {
            placement,
            category,
            ranking_anchor,
            telemetry: &snapshot,
            power_ranking,
            now_seconds,
        };
        let decision =
            crate::profile_scheduler::draw_scheduled_candidates_with_decision(candidates, &ctx)?;
        let cat_key = category.unwrap_or("primary");
        if let Some(decision) = &decision {
            use crate::profile_scheduler::PlacementDecisionReason;
            match &decision.reason {
                PlacementDecisionReason::PaidSpill { .. }
                | PlacementDecisionReason::UnsafeSubscriptionFallback => {
                    tracing::info!(
                        category = cat_key,
                        chosen_profile = %decision.chosen_profile,
                        chosen_tier = decision.chosen_tier,
                        reason = ?decision.reason,
                        estimated_cost_usd = ?decision.estimated_cost_usd,
                        binding_profile = ?decision.binding_profile,
                        binding_window = ?decision.binding_window,
                        binding_remaining_pct = ?decision.binding_remaining_pct,
                        "quota placement decision"
                    );
                }
                _ => {
                    tracing::debug!(
                        category = cat_key,
                        chosen_profile = %decision.chosen_profile,
                        chosen_tier = decision.chosen_tier,
                        reason = ?decision.reason,
                        estimated_cost_usd = ?decision.estimated_cost_usd,
                        binding_profile = ?decision.binding_profile,
                        binding_window = ?decision.binding_window,
                        binding_remaining_pct = ?decision.binding_remaining_pct,
                        "quota placement decision"
                    );
                }
            }
        } else {
            guard.paid_spill_counters.total_draws += 1;
            guard.paid_spill_counters.empty_draws += 1;
            *guard
                .paid_spill_counters
                .empty_draws_by_category
                .entry(cat_key.to_owned())
                .or_default() += 1;
            tracing::info!(
                category = cat_key,
                "quota placement decision: no eligible candidate"
            );
        }
        let Some(first) = candidates.first() else {
            return Ok(None);
        };
        let reservation_id = guard.next_reservation_id;
        guard.next_reservation_id = guard.next_reservation_id.saturating_add(1);
        guard.in_flight.insert(
            reservation_id,
            PendingReservation {
                profile_id: first.as_ref().profile_id.clone(),
                placement: RecentPlacement {
                    category: cat_key.to_owned(),
                    model: Some(first.as_ref().model.clone()),
                    effort: first.as_ref().effort.clone(),
                    created_at_epoch_seconds: now_seconds,
                },
                decision,
            },
        );
        Ok(Some(PlacementReservation {
            store: self.clone(),
            reservation_id,
            committed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub fn replace_windows(&self, windows: BTreeMap<WindowKey, WindowTelemetry>) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.windows = windows;
    }

    pub fn replace_recent_placements(&self, placements: BTreeMap<String, Vec<RecentPlacement>>) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.durable_placements = placements;
    }

    pub fn record_placement(
        &self,
        profile_id: &str,
        category: &str,
        created_at_epoch_seconds: i64,
    ) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = guard.next_reservation_id;
        guard.next_reservation_id = guard.next_reservation_id.saturating_add(1);
        guard.committed_pending.insert(
            (format!("__recorded-{id}"), format!("__recorded-{id}")),
            (
                profile_id.to_owned(),
                RecentPlacement {
                    category: category.to_owned(),
                    model: None,
                    effort: None,
                    created_at_epoch_seconds,
                },
            ),
        );
    }

    pub fn record_committed_placement(
        &self,
        session_id: &str,
        profile_id: &str,
        category: &str,
        created_at_epoch_seconds: i64,
    ) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.committed_pending.insert(
            (session_id.to_owned(), profile_id.to_owned()),
            (
                profile_id.to_owned(),
                RecentPlacement {
                    category: category.to_owned(),
                    model: None,
                    effort: None,
                    created_at_epoch_seconds,
                },
            ),
        );
    }

    pub fn replace_cell_costs(&self, cell_costs: BTreeMap<CellCostKey, CellCostObservation>) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.cell_costs = cell_costs;
    }

    pub fn replace_category_stats(&self, stats: BTreeMap<String, CategoryBurnStats>) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.category_stats = stats;
    }

    /// Recompute `windows` and `durable_placements` from SQLite for the given
    /// `(profile_id, cache_identity)` pairs. Safe to call on the 10-minute
    /// quota poller background task (never on the spawn hot path).
    ///
    /// Deduplicates `committed_pending` by `(session_id, profile_id)` against the loaded
    /// SQLite rows so a placement committed concurrently with a refresh is
    /// never double-counted.
    pub fn refresh_projections(
        &self,
        reports: &[(String, String, ProfileQuota)],
        now: i64,
    ) -> anyhow::Result<()> {
        self.refresh_projections_with_loader(
            reports,
            now,
            crate::database::load_quota_history_since,
        )
    }

    pub fn refresh_projections_with_loader(
        &self,
        reports: &[(String, String, ProfileQuota)],
        now: i64,
        mut load_history: impl FnMut(&str, &str, u64) -> anyhow::Result<Vec<ProfileQuota>>,
    ) -> anyhow::Result<()> {
        let snapshot = self.snapshot();
        let mut projections = Vec::new();
        for (profile_id, identity, report) in reports {
            let history = load_history(
                identity,
                profile_id,
                u64::try_from(now)
                    .unwrap_or(0)
                    .saturating_sub(RATIO_LOOKBACK_SECONDS),
            )?;
            for window in &report.windows {
                let Some(remaining_f64) =
                    window_used_pct(window).map(|used| (100.0 - used).clamp(0.0, 100.0))
                else {
                    continue;
                };
                let remaining_percent = remaining_f64.round() as u8;
                let kind = WindowKind::from_window(window);
                let observed_at = i64::try_from(report.refreshed_at_epoch_seconds)
                    .unwrap_or(now)
                    .min(now);
                let max_reading_age = match kind {
                    WindowKind::Short5h => HISTORY_LOOKBACK_SECONDS as i64,
                    WindowKind::Long => 6 * 3600,
                };
                let stale_reading = now.saturating_sub(observed_at) > max_reading_age;
                let observed = if stale_reading {
                    None
                } else {
                    match kind {
                        WindowKind::Short5h => snapshot
                            .window_telemetry(
                                profile_id,
                                Some(identity),
                                kind,
                                window.model_scope.as_deref(),
                            )
                            .filter(|w| w.has_trailing_interval)
                            .map(|w| w.trailing_60m_pct_per_hour),
                        WindowKind::Long => {
                            let rate = |seconds, min_coverage| {
                                consumption_with_coverage(
                                    &history,
                                    &[(now.saturating_sub(seconds), now)],
                                    |report| {
                                        let matching = report.windows.iter().find(|w| {
                                            WindowKind::from_window(w) == WindowKind::Long
                                                && w.model_scope == window.model_scope
                                                && w.label == window.label
                                        })?;
                                        sample_for_window(report, matching)
                                    },
                                )
                                .filter(|(_, covered)| *covered >= min_coverage)
                                .map(|(value, covered)| {
                                    value.basis_points as f64 / 100.0 / (covered as f64 / 3600.0)
                                })
                            };
                            match (rate(24 * 3600, 6 * 3600), rate(7 * 86400, 12 * 3600)) {
                                (Some(a), Some(b)) => Some(a.max(b)),
                                (a, b) => a.or(b),
                            }
                        }
                    }
                };
                // Anchor sustainable pace and projected exhaustion to the
                // balance observation timestamp so a reused `last_good` reading
                // never drifts its predicted exhaustion forward on later ticks.
                let sustainable = (!stale_reading)
                    .then_some(window.resets_at_epoch_seconds)
                    .flatten()
                    .filter(|reset| *reset > now && *reset > observed_at)
                    .map(|reset| remaining_f64 / ((reset - observed_at) as f64 / 3600.0));
                let raw_pace = observed
                    .zip(sustainable)
                    .filter(|(_, s)| *s > 0.0)
                    .map(|(o, s)| o / s);
                let raw_exhaustion = observed
                    .filter(|o| *o > 0.0 && raw_pace.is_some_and(|p| p > 1.0))
                    .map(|o| {
                        observed_at.saturating_add((remaining_f64 / o * 3600.0).round() as i64)
                    });
                let (pace, exhaustion) = if raw_exhaustion.is_some_and(|t| t <= now) {
                    (None, None)
                } else {
                    (raw_pace, raw_exhaustion)
                };
                projections.push(StampedQuotaProjection {
                    cache_identity: identity.clone(),
                    refreshed_at_epoch_seconds: report.refreshed_at_epoch_seconds,
                    projection: mj_core::usage::ProfileQuotaProjection {
                        profile_id: profile_id.clone(),
                        window_label: report.window_label(window),
                        is_short_window: kind == WindowKind::Short5h,
                        is_weekly_window: Some(
                            window.is_weekly_period() && window.model_scope.is_none(),
                        ),
                        remaining_percent,
                        resets_at_epoch_seconds: window.resets_at_epoch_seconds,
                        observed_pct_per_hour: observed,
                        sustainable_pct_per_hour: sustainable,
                        pace_ratio: pace,
                        projected_exhaustion_epoch_seconds: exhaustion,
                    },
                });
            }
        }
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .projections = projections;
        Ok(())
    }

    pub fn refresh_from_store(
        &self,
        profiles: &[(String, String)],
        now_seconds: i64,
    ) -> anyhow::Result<()> {
        self.refresh_from_store_with_loaders(
            profiles,
            now_seconds,
            crate::database::load_quota_history_since,
            crate::database::load_recent_placements_since,
        )
    }

    fn refresh_from_store_with_loaders(
        &self,
        profiles: &[(String, String)],
        now_seconds: i64,
        mut load_history: impl FnMut(&str, &str, u64) -> anyhow::Result<Vec<ProfileQuota>>,
        load_placements: impl FnOnce(i64) -> anyhow::Result<Vec<crate::database::RecentPlacementRow>>,
    ) -> anyhow::Result<()> {
        let refresh_plan: BTreeMap<_, _> = {
            let guard = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            profiles
                .iter()
                .map(|key| {
                    let incremental = guard.ratio_ledgers.get(key).filter(|ledger| {
                        now_seconds >= ledger.hydrated_at
                            && now_seconds.saturating_sub(ledger.hydrated_at)
                                < RATIO_REHYDRATE_INTERVAL_SECONDS
                    });
                    (key.clone(), incremental.cloned())
                })
                .collect()
        };

        let now = u64::try_from(now_seconds).unwrap_or(0);
        let since_90m = now.saturating_sub(HISTORY_LOOKBACK_SECONDS);
        let since_7d = now.saturating_sub(RATIO_LOOKBACK_SECONDS);
        let mut all_windows = BTreeMap::new();
        let mut ratio_ledgers = BTreeMap::new();
        for ((profile_id, cache_identity), incremental) in refresh_plan {
            let (ledger, history_90m) = if let Some(mut ledger) = incremental {
                let history = load_history(&cache_identity, &profile_id, since_90m)?;
                ledger.accumulate(&history);
                (ledger, history)
            } else {
                let mut history = load_history(&cache_identity, &profile_id, since_7d)?;
                let mut ledger = RatioLedger {
                    hydrated_at: now_seconds,
                    ..RatioLedger::default()
                };
                ledger.accumulate(&history);
                history.retain(|row| row.refreshed_at_epoch_seconds >= since_90m);
                (ledger, history)
            };
            let mut computed =
                compute_window_telemetry(&profile_id, &cache_identity, &history_90m, now_seconds);
            for (key, telemetry) in &mut computed {
                telemetry.five_hour_to_weekly_ratio = ledger.ratio_for_scope(&key.model_scope);
            }
            for scope in ledger.sums_by_scope.keys() {
                if let Some(ratio) = ledger.ratio_for_scope(scope) {
                    computed
                        .entry(WindowKey {
                            profile_id: profile_id.clone(),
                            cache_identity: cache_identity.clone(),
                            window_kind: WindowKind::Short5h,
                            model_scope: scope.clone(),
                        })
                        .or_insert(WindowTelemetry {
                            five_hour_to_weekly_ratio: Some(ratio),
                            ..WindowTelemetry::default()
                        });
                }
            }
            all_windows.extend(computed);
            ratio_ledgers.insert((profile_id, cache_identity), ledger);
        }
        let placements_since = now_seconds.saturating_sub(HISTORY_LOOKBACK_SECONDS as i64);
        let rows = load_placements(placements_since)?;
        self.publish_store_refresh(all_windows, rows, placements_since, Some(ratio_ledgers));
        Ok(())
    }

    pub fn apply_store_refresh(
        &self,
        all_windows: BTreeMap<WindowKey, WindowTelemetry>,
        rows: Vec<crate::database::RecentPlacementRow>,
        placements_since: i64,
    ) {
        self.publish_store_refresh(all_windows, rows, placements_since, None);
    }

    fn publish_store_refresh(
        &self,
        all_windows: BTreeMap<WindowKey, WindowTelemetry>,
        rows: Vec<crate::database::RecentPlacementRow>,
        placements_since: i64,
        ratio_ledgers: Option<BTreeMap<(String, String), RatioLedger>>,
    ) {
        let mut seen: BTreeMap<(String, String), i64> = BTreeMap::new();
        let mut durable_placements: BTreeMap<String, Vec<RecentPlacement>> = BTreeMap::new();
        for row in rows {
            seen.entry((row.session_id, row.profile_id.clone()))
                .and_modify(|timestamp| *timestamp = (*timestamp).max(row.created_at_epoch_seconds))
                .or_insert(row.created_at_epoch_seconds);
            durable_placements
                .entry(row.profile_id)
                .or_default()
                .push(RecentPlacement {
                    category: row.category,
                    model: row.model,
                    effort: row.effort,
                    created_at_epoch_seconds: row.created_at_epoch_seconds,
                });
        }
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .committed_pending
            .retain(|(session_id, profile_id), (_, placement)| {
                placement.created_at_epoch_seconds >= placements_since
                    && seen
                        .get(&(session_id.clone(), profile_id.clone()))
                        .is_none_or(|row_ts| {
                            *row_ts < placement.created_at_epoch_seconds.saturating_sub(60)
                        })
            });
        guard.windows = all_windows;
        guard.durable_placements = durable_placements;
        if let Some(ratio_ledgers) = ratio_ledgers {
            guard.ratio_ledgers = ratio_ledgers;
        }
    }

    /// Lazily refresh local category burn observations from SQLite (startup +
    /// hourly background task; never on the 10-minute poll or spawn hot path).
    pub fn refresh_category_stats_from_store(&self) -> anyhow::Result<()> {
        let observed = crate::database::load_category_burn_observations(
            LOCAL_CATEGORY_SWITCHOVER_MIN_SAMPLES,
            LOCAL_CATEGORY_MAX_SAMPLES,
        )?;
        let stats = observed
            .into_iter()
            .map(|(category, obs)| {
                (
                    category,
                    CategoryBurnStats {
                        mean_api_usd: obs.mean_api_usd,
                        runway_duration_hours: obs.p90_duration_hours,
                        local_sample_count: obs.sample_count,
                    },
                )
            })
            .collect();
        let cell_costs = crate::database::load_cell_cost_observations(LOCAL_CELL_MAX_SAMPLES)?;
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.category_stats = stats;
        guard.cell_costs = cell_costs;
        Ok(())
    }

    pub fn log_cell_cost_estimates(
        &self,
        power_ranking: &crate::power_ranking::PowerRankingSnapshot,
    ) {
        let (cell_costs, category_stats) = {
            let guard = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (guard.cell_costs.clone(), guard.category_stats.clone())
        };
        let mut categories = BTreeMap::<_, Vec<_>>::new();
        for (key, obs) in &cell_costs {
            if obs.sample_count == 0 {
                continue;
            }
            // Stored cells have no harness. Find a benchmark through the normal
            // lookup, including its fallback to untagged rows.
            let bench = mj_core::config::HarnessKind::ALL
                .into_iter()
                .find_map(|harness| {
                    power_ranking
                        .lookup(&key.model, key.effort.as_deref(), harness)
                        .as_ref()
                        .and_then(benchmark_task_usd)
                });
            categories
                .entry(key.category.as_str())
                .or_default()
                .push((key, obs, bench));
        }
        for (category, cells) in categories {
            let default_usd = category_stats.get(category).map_or_else(
                || default_category_burn_stats(category).mean_api_usd,
                |stats| stats.mean_api_usd,
            );
            let prior_scale = calibrated_prior_scale(
                default_usd,
                cells
                    .iter()
                    .filter_map(|(_, obs, bench)| bench.map(|b| (b, Some(*obs)))),
            );
            for (key, obs, bench) in cells {
                let prior_usd = cell_prior_usd(bench, prior_scale, default_usd);
                tracing::debug!(
                    category,
                    model = %key.model,
                    effort = ?key.effort,
                    n = obs.sample_count,
                    observed_mean_usd = obs.total_api_usd / obs.sample_count as f64,
                    prior_usd,
                    estimate_usd = shrink_cell_cost_usd(Some(obs), prior_usd),
                    "local empirical cell cost"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cell_cost_observation_matches_unknown_effort_and_aggregates_none_effort() {
        let mut snapshot = QuotaTelemetrySnapshot::default();
        for (effort, total_api_usd, sample_count) in [
            (None, 2.0, 1),
            (Some("high"), 6.0, 3),
            (Some("low"), 10.0, 5),
        ] {
            snapshot.cell_costs.insert(
                CellCostKey {
                    category: "orchestrator".into(),
                    model: "grok-4.7".into(),
                    effort: effort.map(str::to_owned),
                },
                CellCostObservation {
                    total_api_usd,
                    sample_count,
                },
            );
        }
        assert_eq!(
            snapshot.cell_cost_observation(" Supervisor ", " x-ai/grok-4.7 ", Some(" HIGH ")),
            Some(CellCostObservation {
                total_api_usd: 8.0,
                sample_count: 4
            }),
        );
        assert_eq!(
            snapshot.cell_cost_observation("orchestrator", "grok-4.7", None),
            Some(CellCostObservation {
                total_api_usd: 18.0,
                sample_count: 9
            }),
        );
    }

    use super::*;
    use mj_core::config::HarnessKind;

    fn sample_report(
        profile_id: &str,
        ts: u64,
        remaining_5h: Option<(u8, Option<i64>)>,
        remaining_week: Option<(u8, Option<i64>)>,
    ) -> ProfileQuota {
        let mut windows = Vec::new();
        if let Some((rem, reset)) = remaining_5h {
            windows.push(QuotaWindow {
                label: "5-hour".into(),
                remaining_percent: Some(rem),
                duration_seconds: Some(5 * 3600),
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: reset,
            });
        }
        if let Some((rem, reset)) = remaining_week {
            windows.push(QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(rem),
                duration_seconds: Some(7 * 86_400),
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: reset,
            });
        }
        ProfileQuota {
            profile_id: profile_id.into(),
            harness: HarnessKind::Claude,
            monthly_price_usd: Some(200),
            paid_credits: None,
            banked_resets: None,
            windows,
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: ts,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        }
    }

    #[test]
    fn weekly_projection_requires_measured_coverage_and_uses_seven_day_rate() {
        const NOW: i64 = 1_000_000;
        let row = |time: i64, remaining: u8| {
            sample_report("p", time as u64, None, Some((remaining, Some(NOW + 86400))))
        };
        let current = row(NOW, 20);
        let store = QuotaTelemetryStore::new();
        let reports = vec![("p".to_string(), "id".to_string(), current)];
        // Two readings over one hour cannot establish either weekly rate.
        let sparse = vec![row(NOW - 3600, 30), row(NOW, 20)];
        store
            .refresh_projections_with_loader(&reports, NOW, |_, _, _| Ok(sparse.clone()))
            .unwrap();
        assert_eq!(store.snapshot().projections[0].observed_pct_per_hour, None);
        // A high final-hour rate must not be extrapolated across the week.
        let covered = vec![row(NOW - 24 * 3600, 44), row(NOW - 3600, 30), row(NOW, 20)];
        store
            .refresh_projections_with_loader(&reports, NOW, |_, _, _| Ok(covered.clone()))
            .unwrap();
        let projection = &store.snapshot().projections[0];
        assert!(!projection.is_short_window);
        assert_eq!(projection.is_weekly_window, Some(true));
        assert!((projection.observed_pct_per_hour.unwrap() - 1.0).abs() < 1e-9);
        assert!((projection.pace_ratio.unwrap() - 1.2).abs() < 1e-9);

        // A scoped window must use its own rate, not the shared week's rate.
        let scoped_row = |at, shared, scoped| {
            let mut report = row(at, shared);
            let mut window = report.windows[0].clone();
            window.label = "Week (scoped)".into();
            window.model_scope = Some("opus".into());
            window.remaining_percent = Some(scoped);
            report.windows.push(window);
            report
        };
        let history = vec![scoped_row(NOW - 24 * 3600, 44, 80), scoped_row(NOW, 20, 68)];
        let reports = vec![("p".to_owned(), "id".to_owned(), history[1].clone())];
        store
            .refresh_projections_with_loader(&reports, NOW, |_, _, _| Ok(history.clone()))
            .unwrap();
        let both = sample_report(
            "p",
            NOW as u64,
            Some((70, Some(NOW + 3600))),
            Some((50, Some(NOW + 86400))),
        );
        store
            .refresh_projections_with_loader(&[("p".into(), "id".into(), both)], NOW, |_, _, _| {
                Ok(Vec::new())
            })
            .unwrap();
        let kinds: Vec<_> = store
            .snapshot()
            .projections
            .iter()
            .map(|p| p.is_short_window)
            .collect();
        assert_eq!(kinds, vec![true, false]);
        assert_eq!(
            store
                .snapshot()
                .projections
                .iter()
                .map(|p| p.is_weekly_window)
                .collect::<Vec<_>>(),
            vec![Some(false), Some(true)]
        );
        let mut daily = sample_report("p", NOW as u64, None, Some((50, None)));
        daily.windows[0].duration_seconds = Some(86_400);
        daily.windows[0].label = "Daily".into();
        store
            .refresh_projections_with_loader(&[("p".into(), "id".into(), daily)], NOW, |_, _, _| {
                Ok(Vec::new())
            })
            .unwrap();
        assert_eq!(
            store.snapshot().projections[0].is_weekly_window,
            Some(false)
        );
        store
            .refresh_projections_with_loader(&reports, NOW, |_, _, _| Ok(history.clone()))
            .unwrap();
        let projections = store.snapshot().projections;
        assert_eq!(projections[1].is_weekly_window, Some(false));
        assert!((projections[0].observed_pct_per_hour.unwrap() - 1.0).abs() < 1e-9);
        assert!((projections[1].observed_pct_per_hour.unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn stale_last_good_anchors_exhaustion_and_stamp_matching_drops_mismatched_readings() {
        const OBSERVED_AT: i64 = 1_000_000;
        let reset = OBSERVED_AT + 10 * 3600;
        let row = |time: i64, remaining: u8| {
            sample_report("p", time as u64, None, Some((remaining, Some(reset))))
        };
        // 10% left at OBSERVED_AT with 5%/hr over the prior 10h -> exhausts at OBSERVED_AT + 2h.
        let history = vec![row(OBSERVED_AT - 10 * 3600, 60), row(OBSERVED_AT, 10)];
        let stale_report = vec![("p".to_string(), "acct-a".to_string(), history[1].clone())];
        let store = QuotaTelemetryStore::new();

        // 30m later, exhaustion stays anchored to OBSERVED_AT + 2h (never pushed to now + 2h).
        let thirty_min_later = OBSERVED_AT + 1800;
        store
            .refresh_projections_with_loader(&stale_report, thirty_min_later, |_, _, _| {
                Ok(history.clone())
            })
            .unwrap();
        let proj = &store.snapshot().projections[0];
        assert_eq!(
            proj.projected_exhaustion_epoch_seconds,
            Some(OBSERVED_AT + 2 * 3600)
        );
        // Only the exact (cache_identity, refreshed_at) stamp matches.
        assert_eq!(
            store
                .projections_matching(&BTreeMap::from([(
                    "p".into(),
                    ("acct-a".into(), OBSERVED_AT as u64)
                )]))
                .len(),
            1
        );
        assert!(
            store
                .projections_matching(&BTreeMap::from([(
                    "p".into(),
                    ("acct-a".into(), (OBSERVED_AT + 600) as u64)
                )]))
                .is_empty()
        );
        assert!(
            store
                .projections_matching(&BTreeMap::from([(
                    "p".into(),
                    ("acct-b".into(), OBSERVED_AT as u64)
                )]))
                .is_empty()
        );

        // 3h later (past OBSERVED_AT + 2h), the stale forecast is suppressed rather than drifted.
        let three_hours_later = OBSERVED_AT + 3 * 3600;
        store
            .refresh_projections_with_loader(&stale_report, three_hours_later, |_, _, _| {
                Ok(history.clone())
            })
            .unwrap();
        let stale_proj = &store.snapshot().projections[0];
        assert_eq!(stale_proj.projected_exhaustion_epoch_seconds, None);
        assert_eq!(stale_proj.pace_ratio, None);

        // Count-based headroom below 0.5% (rounds to 0% for display) must keep
        // fractional headroom in forecast math instead of dropping the warning.
        let count_row = |time: i64, used: i64| {
            let mut r = sample_report("p", time as u64, None, Some((0, Some(reset))));
            r.windows[0].remaining_percent = None;
            r.windows[0].used = Some(used);
            r.windows[0].limit = Some(1000);
            r
        };
        let count_history = vec![
            count_row(OBSERVED_AT - 10 * 3600, 900),
            count_row(OBSERVED_AT, 998),
        ];
        store
            .refresh_projections_with_loader(
                &[("p".into(), "acct-a".into(), count_history[1].clone())],
                OBSERVED_AT,
                |_, _, _| Ok(count_history.clone()),
            )
            .unwrap();
        let frac = &store.snapshot().projections[0];
        assert_eq!(frac.remaining_percent, 0);
        assert!(frac.sustainable_pct_per_hour.unwrap() > 0.0);
        assert!(frac.pace_ratio.unwrap() > 1.0);
        assert!(frac.projected_exhaustion_epoch_seconds.is_some());

        // Antigravity forecasts include the pool name from `ProfileQuota::window_label`.
        let mut agy = sample_report("agy", OBSERVED_AT as u64, None, Some((20, Some(reset))));
        agy.harness = mj_core::config::HarnessKind::Antigravity;
        agy.windows[0].model_scope = Some("gemini".into());
        store
            .refresh_projections_with_loader(
                &[("agy".into(), "acct-agy".into(), agy.clone())],
                OBSERVED_AT,
                |_, _, _| Ok(vec![agy.clone()]),
            )
            .unwrap();
        assert_eq!(store.snapshot().projections[0].window_label, "Week (Ge)");
    }

    #[test]
    fn dropped_paid_reservation_does_not_increment_paid_spill_counters() {
        use crate::profile_scheduler::{CandidateQuota, Placement, ScheduledCandidate};
        let store = QuotaTelemetryStore::new();
        let ranking = crate::power_ranking::PowerRankingSnapshot::baseline();
        let paid = ScheduledCandidate {
            profile_id: "dsf".into(),
            harness: mj_core::config::HarnessKind::Dsh,
            model: "deepseek-v4".into(),
            effort: None,
            quota: CandidateQuota::UsagePriced,
            may_consume_credits: false,
            cache_identity: None,
        };

        let mut candidates = vec![paid.clone()];
        let dropped = store
            .draw_and_reserve(
                &mut candidates,
                Placement::Child,
                Some("subtle"),
                None,
                ranking,
                1_000,
            )
            .unwrap()
            .unwrap();
        drop(dropped);
        let after_drop = store.snapshot().paid_spill_counters;
        assert_eq!(after_drop.total_draws, 0);
        assert_eq!(after_drop.paid_spill_exhausted_or_absent, 0);

        let mut candidates = vec![paid];
        let accepted = store
            .draw_and_reserve(
                &mut candidates,
                Placement::Child,
                Some("subtle"),
                None,
                ranking,
                1_001,
            )
            .unwrap()
            .unwrap();
        accepted.commit(Some("sess-1".into()));
        // Repeated commits stay idempotent.
        accepted.commit(Some("sess-1".into()));
        let after_commit = store.snapshot().paid_spill_counters;
        assert_eq!(after_commit.total_draws, 1);
        assert_eq!(after_commit.paid_spill_exhausted_or_absent, 1);
    }

    #[test]
    fn weekly_consumption_clips_spans_and_never_turns_unmeasurable_into_zero() {
        const T: i64 = 1_000_000;
        let week = |at: i64, used: u8, reset: Option<i64>| {
            sample_report("p1", (T + at) as u64, None, Some((100 - used, reset)))
        };
        let steady = Some(T + 500_000);
        let history = vec![
            week(0, 10, steady),
            week(3600, 20, steady),
            week(7200, 20, steady),
            week(10_800, 30, steady),
        ];
        // (basis points, partial)
        let measure = |history: &[ProfileQuota], since: i64, until: i64| {
            weekly_consumption(history, T + since, T + until)
                .map(|consumption| (consumption.basis_points, consumption.partial))
        };
        // Half of the first 10-point step and half of the last: 10%, with every
        // second of the span inside a measured interval.
        assert_eq!(measure(&history, 1800, 9000), Some((1000, false)));
        // A selected hour inside an idle interval measured no growth, not an
        // unknown.
        assert_eq!(measure(&history, 3600, 7200), Some((0, false)));
        let gaps = vec![
            week(0, 10, steady),
            week(3600, 20, steady),
            week(7200, 30, steady),
            week(10_800, 40, steady),
        ];
        let spans = [(T, T + 3600), (T + 7200, T + 10_800)];
        assert_eq!(
            weekly_consumption_in_spans(&gaps, &spans),
            Some(WeeklyConsumption {
                basis_points: 2000,
                partial: false
            })
        );
        assert_eq!(measure(&gaps, 0, 10_800), Some((3000, false)));
        // A probe interval crossing the artificial boundary between adjacent
        // selected hours covers both; it must not create a partial marker.
        let crossing_boundary = vec![week(0, 10, steady), week(7200, 30, steady)];
        assert_eq!(
            weekly_consumption_in_spans(&crossing_boundary, &[(T, T + 3600), (T + 3600, T + 7200)]),
            Some(WeeklyConsumption {
                basis_points: 2000,
                partial: false
            })
        );
        assert_eq!(
            weekly_consumption_in_spans(
                &crossing_boundary,
                &[(T, T + 3600), (T + 7200, T + 10_800)]
            ),
            Some(WeeklyConsumption {
                basis_points: 1000,
                partial: true
            })
        );
        let empty = weekly_quota_usage(
            vec![QuotaUsageProfile {
                profile_id: "p1".into(),
                cache_identity: None,
                monthly_price_usd: None,
            }],
            &[],
        );
        assert_eq!(empty[0].consumed_basis_points, Some(0));
        assert_eq!(empty[0].covered_seconds, Some(0));
        let profile = || {
            vec![QuotaUsageProfile {
                profile_id: "p1".into(),
                cache_identity: Some("id".into()),
                monthly_price_usd: None,
            }]
        };
        let get = |spans: &[(i64, i64)]| {
            weekly_quota_usage_with_loader(profile(), spans, |_, _, _, _| Ok(history.clone()))
        };
        assert_eq!(
            get(&[((T + 1800) * 1000, (T + 9000) * 1000 - 1)])[0].covered_seconds,
            Some(7200)
        );
        assert_eq!(
            get(&[((T - 1800) * 1000, (T + 3600) * 1000 - 1)])[0].covered_seconds,
            Some(3600)
        );
        assert_eq!(
            get(&[((T - 10000) * 1000, (T - 5000) * 1000 - 1)])[0].covered_seconds,
            None
        );

        assert_eq!(
            weekly_consumption_with_coverage(&history, &[(T + 1800, T + 9000)])
                .unwrap()
                .1,
            7200
        );
        assert_eq!(
            weekly_consumption_with_coverage(&history, &[(T - 1800, T + 3600)])
                .unwrap()
                .1,
            3600
        );
        assert!(weekly_consumption_with_coverage(&history, &[(T - 10000, T - 5000)]).is_none());
        assert!(!empty[0].partial);
        // Spans no probe pair covers are unknown, whichever side they fall on.
        assert_eq!(measure(&history, -10_000, -5_000), None);
        assert_eq!(measure(&history, 20_000, 30_000), None);
        // A span running past either end of the readings measures what they
        // cover and says it is partial: here the first and the last interval.
        assert_eq!(measure(&history, -1800, 3600), Some((1000, true)));
        assert_eq!(measure(&history, 9000, 20_000), Some((500, true)));
        // A failed probe inside an interval carries no reading, so the
        // interval spans it.
        let mut failed = sample_report("p1", (T + 1800) as u64, None, None);
        failed.error = Some("provider down".into());
        failed.windows.clear();
        let bridged = vec![week(0, 10, steady), failed, week(3600, 20, steady)];
        assert_eq!(measure(&bridged, 0, 3600), Some((1000, false)));

        // The weekly window reset at T+3000: the 5-point reading at T+4000 was
        // used after it, and the 90 before it is not attributable to any span.
        let reset = Some(T + 3000);
        let after_reset = Some(T + 3000 + 604_800);
        let crossing = vec![week(0, 90, reset), week(4000, 5, after_reset)];
        assert_eq!(measure(&crossing, 0, 4000), Some((500, true)));
        assert_eq!(measure(&crossing, 3500, 4000), Some((250, false)));
        assert_eq!(measure(&crossing, 0, 3000), None);
        // A counter left standing after its own deadline is no reading: it
        // cannot end an interval, so only the use before it is measured and
        // the elapsed part of the span is partial; with nothing before it,
        // nothing is measurable.
        let stale = vec![week(4000, 40, reset), week(8000, 5, after_reset)];
        assert_eq!(measure(&stale, 4000, 8000), None);
        let standing = vec![
            week(0, 10, reset),
            week(2000, 15, reset),
            week(4000, 20, reset),
        ];
        assert_eq!(measure(&standing, 0, 4000), Some((500, true)));
        assert_eq!(measure(&standing, 3000, 4000), None);
        // Probes more than one weekly period apart hide whole cycles, so the
        // interval between them is not interpolated.
        let week_s = WEEKLY_PERIOD_SECONDS;
        let far = Some(T + 3 * week_s);
        let apart = vec![week(0, 10, far), week(week_s + 1, 40, far)];
        assert_eq!(measure(&apart, 0, week_s + 1), None);
        let exactly = vec![week(0, 10, far), week(week_s, 40, far)];
        assert_eq!(measure(&exactly, 0, week_s), Some((3000, false)));
        // The report's span ends at an inclusive millisecond: a completed
        // hour's last second counts, so a probe pair across the next hour's
        // start still measures it.
        let hour_ms = 3_600_000;
        let hour_start_ms = (T * 1000).div_euclid(hour_ms) * hour_ms + hour_ms;
        let (since, until) = weekly_usage_span(hour_start_ms, hour_start_ms + hour_ms - 1);
        assert_eq!(
            (since, until),
            (hour_start_ms / 1000, hour_start_ms / 1000 + 3600)
        );
        let last_second = vec![
            sample_report("p1", (until - 1) as u64, None, Some((90, steady))),
            sample_report("p1", until as u64, None, Some((80, steady))),
        ];
        assert_eq!(
            weekly_consumption(&last_second, since, until)
                .map(|consumption| (consumption.basis_points, consumption.partial)),
            Some((1000, true))
        );

        // A deadline that moved before it elapsed is provider jitter, and a
        // fall in use with the same deadline is a top-up: neither is use.
        let jitter = vec![week(0, 10, steady), week(3600, 20, Some(T + 500_900))];
        assert_eq!(measure(&jitter, 0, 3600), None);
        let top_up = vec![week(0, 20, steady), week(3600, 10, steady)];
        assert_eq!(measure(&top_up, 0, 3600), None);
        // An unreadable interval beside a readable one leaves the span
        // partial and keeps the readable part's use.
        let mixed = vec![
            week(0, 10, steady),
            week(3600, 20, Some(T + 500_900)),
            week(7200, 25, Some(T + 500_900)),
        ];
        assert_eq!(measure(&mixed, 0, 7200), Some((500, true)));

        // Only the unscoped weekly allowance counts: a 5-hour window, a
        // model-scoped weekly window or a month window is another allowance,
        // and a probe carrying only those is no reading at the span's edge.
        let five_hour = vec![
            sample_report("p1", T as u64, Some((90, steady)), None),
            sample_report("p1", (T + 3600) as u64, Some((80, steady)), None),
        ];
        assert_eq!(measure(&five_hour, 0, 3600), None);
        assert!(!five_hour.iter().any(has_weekly_reading));
        let mut scoped = week(0, 10, steady);
        scoped.windows[0].model_scope = Some("fable".into());
        assert!(!has_weekly_reading(&scoped));
        assert_eq!(measure(&[scoped, week(3600, 20, steady)], 0, 3600), None);
        let mut monthly = vec![week(0, 10, steady), week(3600, 20, steady)];
        for probe in &mut monthly {
            probe.windows[0].label = "Month".into();
            probe.windows[0].duration_seconds = None;
        }
        assert_eq!(measure(&monthly, 0, 3600), None);
        assert!(has_weekly_reading(&week(0, 10, steady)));
    }

    #[test]
    fn corrupted_intervals_do_not_produce_negative_or_inflated_velocity() {
        let now = 1_000_000i64;
        let reset = now + 4 * 3600;
        let history = vec![
            // Valid step: 10% -> 16% (+6% in 30 min = 12 %/hr)
            sample_report("p1", (now - 3600) as u64, None, Some((90, Some(reset)))),
            sample_report("p1", (now - 1800) as u64, None, Some((84, Some(reset)))),
            // Corrupted top-up: used drops 16% -> 4% (remaining jumps 84% -> 96%)
            sample_report("p1", (now - 1200) as u64, None, Some((96, Some(reset)))),
            // Corrupted shifted deadline (> 120s shift): reset jumps by +600s
            sample_report(
                "p1",
                (now - 600) as u64,
                None,
                Some((80, Some(reset + 600))),
            ),
            // Valid step after shift: 20% -> 22% (+2% in 10 min = 12 %/hr)
            sample_report("p1", now as u64, None, Some((78, Some(reset + 600)))),
        ];
        let telemetry = compute_window_telemetry("p1", "id1", &history, now);
        let key = WindowKey {
            profile_id: "p1".into(),
            cache_identity: "id1".into(),
            window_kind: WindowKind::Long,
            model_scope: None,
        };
        let wt = telemetry.get(&key).expect("long window telemetry");
        // Sum of valid deltas: 6% + 2% = 8% over 0.5h + (1/6)h = 2/3h => 12.0 %/hr
        assert!(
            (wt.trailing_60m_pct_per_hour - 12.0).abs() < 1e-6,
            "expected 12.0 %/hr, got {}",
            wt.trailing_60m_pct_per_hour
        );
        assert_eq!(wt.second_to_last_clean_ts, Some(now - 600));
    }

    #[test]
    fn five_hour_to_weekly_ratio_includes_zero_weekly_delta_pairs_and_excludes_resets() {
        let start = 1_000_000;
        let history = [
            (100, 100),
            (95, 100),
            (90, 100),
            (86, 99),
            (100, 97),
            (94, 100),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (short, week))| {
            sample_report(
                "p1",
                start + i as u64 * 600,
                Some((short, None)),
                Some((week, None)),
            )
        })
        .collect::<Vec<_>>();
        let telemetry = compute_window_telemetry("p1", "id1", &history, start as i64 + 3000);
        let key = WindowKey {
            profile_id: "p1".into(),
            cache_identity: "id1".into(),
            window_kind: WindowKind::Short5h,
            model_scope: None,
        };
        let ratio = telemetry[&key].five_hour_to_weekly_ratio.unwrap();
        assert!((ratio - 14.0).abs() < 1e-6, "got {ratio}");
    }

    #[test]
    fn ratio_ledger_persists_across_idle_window_avoids_double_counting_and_rehydrates_hourly() {
        let store = QuotaTelemetryStore::new();
        let t0 = 1_000_000u64;
        let seven_days_ago = t0 - RATIO_LOOKBACK_SECONDS;
        let two_days_ago = t0 - 2 * 86_400;
        let key = WindowKey {
            profile_id: "p1".into(),
            cache_identity: "id1".into(),
            window_kind: WindowKind::Short5h,
            model_scope: None,
        };
        let profile = (key.profile_id.clone(), key.cache_identity.clone());
        let report =
            |ts, short, week| sample_report("p1", ts, Some((short, None)), Some((week, None)));
        let history = [
            report(seven_days_ago, 100, 100),
            report(seven_days_ago + 600, 85, 99),
            report(two_days_ago, 100, 100),
            report(two_days_ago + 600, 95, 99),
            report(t0 + 100, 100, 100),
            report(t0 + 300, 95, 100),
            report(t0 + 900, 90, 99),
            report(t0 + 1500, 90, 98),
        ];

        let refresh = |elapsed, lookback, profiles: &[(String, String)]| {
            let now = t0 + elapsed;
            let mut loads = 0;
            store
                .refresh_from_store_with_loaders(
                    profiles,
                    now as i64,
                    |identity, profile_id, since| {
                        loads += 1;
                        assert_eq!(
                            (profile_id, identity),
                            (profiles[0].0.as_str(), profiles[0].1.as_str())
                        );
                        assert_eq!(since, now - lookback);
                        Ok(history
                            .iter()
                            .filter(|row| {
                                let ts = row.refreshed_at_epoch_seconds;
                                identity == "id1" && (since..=now).contains(&ts)
                            })
                            .cloned()
                            .collect())
                    },
                    |_| Ok(Vec::new()),
                )
                .unwrap();
            assert_eq!(loads, 1, "duplicate profiles must share a single refresh");
            store.snapshot()
        };

        let profiles = std::slice::from_ref(&profile);
        let snapshot = refresh(0, RATIO_LOOKBACK_SECONDS, profiles);
        let short = &snapshot.windows[&key];
        assert_eq!(short.five_hour_to_weekly_ratio, Some(10.0));
        assert_eq!(short.trailing_60m_pct_per_hour, 0.0);
        assert_eq!(short.second_to_last_clean_ts, None);

        let snapshot = refresh(
            600,
            HISTORY_LOOKBACK_SECONDS,
            &[profile.clone(), profile.clone()],
        );
        assert_eq!(snapshot.windows[&key].five_hour_to_weekly_ratio, Some(12.5));

        let snapshot = refresh(1200, HISTORY_LOOKBACK_SECONDS, profiles);
        assert_eq!(snapshot.windows[&key].five_hour_to_weekly_ratio, Some(10.0));

        // The oldest pair has aged out. The latest step advanced only the week.
        let snapshot = refresh(3600, RATIO_LOOKBACK_SECONDS, profiles);
        assert_eq!(snapshot.windows[&key].five_hour_to_weekly_ratio, Some(5.0));

        let new_profile = ("p1".into(), "id2".into());
        let snapshot = refresh(
            3610,
            RATIO_LOOKBACK_SECONDS,
            std::slice::from_ref(&new_profile),
        );
        assert!(snapshot.windows.is_empty());
        let guard = store.inner.read().unwrap();
        assert_eq!(
            guard.ratio_ledgers.keys().collect::<Vec<_>>(),
            vec![&new_profile]
        );
    }

    #[test]
    fn draw_and_reserve_serializes_concurrent_draws_releases_on_drop_and_dedups_refresh() {
        use crate::power_ranking::PowerRankingSnapshot;
        use crate::profile_scheduler::{CandidateQuota, Placement, ScheduledCandidate};
        use crate::quota::QuotaWindow;
        use mj_core::config::HarnessKind;

        let store = QuotaTelemetryStore::new();
        let now = 1_700_000_000;
        let ranking = PowerRankingSnapshot::baseline();

        let make_cand = move |id: &str, pct: u8| ScheduledCandidate {
            profile_id: id.into(),
            harness: HarnessKind::Codex,
            model: "gpt-5.4".into(),
            effort: Some("high".into()),
            may_consume_credits: false,
            quota: CandidateQuota::Subscription {
                monthly_price_usd: Some(200),
                windows: vec![QuotaWindow {
                    label: "weekly".into(),
                    remaining_percent: Some(pct),
                    resets_at_epoch_seconds: Some(now + 7 * 86400),
                    duration_seconds: Some(7 * 86400),
                    model_scope: None,
                    used: None,
                    limit: None,
                    resets: None,
                }],
                paid_credits: None,
                refreshed_at_epoch_seconds: (now - 60) as u64,
            },
            cache_identity: Some(id.into()),
        };

        // 1. Uncommitted reservation is released immediately on Drop.
        {
            let mut cands = vec![make_cand("p1", 80)];
            let reservation = store
                .draw_and_reserve(
                    &mut cands,
                    Placement::Child,
                    Some("subtle"),
                    None,
                    ranking,
                    now,
                )
                .unwrap()
                .expect("reservation");
            assert_eq!(store.snapshot().recent_placements["p1"].len(), 1);
            drop(reservation);
        }
        assert!(
            store
                .snapshot()
                .recent_placements
                .get("p1")
                .is_none_or(Vec::is_empty),
            "dropped reservation must be released immediately"
        );

        // 2. Concurrent draws serialize atomically and spill to peer once p1 is saturated.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let mut cands = vec![make_cand("p1", 3), make_cand("p2", 80)];
                barrier.wait();
                let res = store
                    .draw_and_reserve(
                        &mut cands,
                        Placement::Child,
                        Some("subtle"),
                        None,
                        PowerRankingSnapshot::baseline(),
                        now,
                    )
                    .unwrap()
                    .expect("reservation");
                (cands[0].profile_id.clone(), res)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let chosen: Vec<_> = results.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(chosen.len(), 2);
        // Commit one with a real session_id and verify refresh deduplicates it.
        let (first_profile, first_res) = results.into_iter().next().unwrap();
        first_res.commit(Some("sess-dedup-1".into()));
        assert_eq!(store.snapshot().recent_placements[&first_profile].len(), 1);

        // 3. apply_store_refresh with the same session_id never double-counts.
        store.apply_store_refresh(
            BTreeMap::new(),
            vec![crate::database::RecentPlacementRow {
                session_id: "sess-dedup-1".into(),
                profile_id: first_profile.clone(),
                category: "subtle".into(),
                model: None,
                effort: None,
                created_at_epoch_seconds: now,
            }],
            now - 3600,
        );
        assert_eq!(
            store.snapshot().recent_placements[&first_profile].len(),
            1,
            "committed_pending entry must be deduplicated by session_id on refresh"
        );

        // A Move's destination reservation survives rows that still describe
        // its source, and rows with an older session creation timestamp.
        let moved = QuotaTelemetryStore::new();
        let mut candidates = vec![make_cand("profile-b", 80)];
        moved
            .draw_and_reserve(
                &mut candidates,
                Placement::Child,
                Some("primary"),
                None,
                ranking,
                now,
            )
            .unwrap()
            .unwrap()
            .commit(Some("moved-session".into()));
        for (profile_id, created_at_epoch_seconds) in
            [("profile-a", now - 30), ("profile-b", now - 3600)]
        {
            moved.apply_store_refresh(
                BTreeMap::new(),
                vec![crate::database::RecentPlacementRow {
                    session_id: "moved-session".into(),
                    profile_id: profile_id.into(),
                    category: "primary".into(),
                    model: None,
                    effort: None,
                    created_at_epoch_seconds,
                }],
                now - 7200,
            );
            assert_eq!(
                moved.snapshot().recent_placements["profile-b"].len(),
                if profile_id == "profile-b" { 2 } else { 1 },
                "destination reservation must survive source or older destination history"
            );
        }
        moved.apply_store_refresh(
            BTreeMap::new(),
            vec![crate::database::RecentPlacementRow {
                session_id: "moved-session".into(),
                profile_id: "profile-b".into(),
                category: "primary".into(),
                model: None,
                effort: None,
                created_at_epoch_seconds: now,
            }],
            now - 7200,
        );
        assert_eq!(moved.snapshot().recent_placements["profile-b"].len(), 1);
    }
}
