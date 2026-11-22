//! Trailing-60-minute quota burn velocity, durable placement reservations, and
//! category burn statistics for the unified runway gate.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use mj_client::quota::{ProfileQuota, QuotaWindow};

/// Tolerance for reset-deadline jitter before an interval is treated as a
/// shifted deadline (Codex / Antigravity) and dropped from velocity.
const RESET_SHIFT_TOLERANCE_SECONDS: i64 = 120;

/// Trailing window over which clean `quota_history` intervals are integrated.
const VELOCITY_WINDOW_SECONDS: i64 = 60 * 60;

/// Lookback window loaded from `quota_history` so the first clean reading
/// before the 60-minute boundary is included.
pub const HISTORY_LOOKBACK_SECONDS: u64 = 90 * 60;

/// Default reservation lookback when only a single clean reading exists.
const SINGLE_READING_RESERVATION_LOOKBACK_SECONDS: i64 = 10 * 60;

/// Minimum completed sessions with `category` populated before replacing the
/// measured default burn/duration constants with local SQLite observations.
pub const LOCAL_CATEGORY_SWITCHOVER_MIN_SAMPLES: usize = 200;

/// Maximum recent completed sessions inspected per category during the hourly
/// background refresh.
pub const LOCAL_CATEGORY_MAX_SAMPLES: usize = 1000;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentPlacement {
    pub category: String,
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
/// `supervisor`, `primary`). Replaced per category once `N >= 200` local
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
        "supervisor" | "primary" => CategoryBurnStats {
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

/// Convert `mean_api_usd` into weekly percentage points using
/// `weekly_budget_usd` from `PowerRankingSnapshot` scaled by
/// `p_eff / plan_usd_per_month`.
pub fn mean_burn_weekly_pct(
    mean_api_usd: f64,
    weekly_budget_usd: Option<f64>,
    p_eff: f64,
    plan_usd_per_month: Option<f64>,
) -> f64 {
    let Some(weekly_budget) = weekly_budget_usd.filter(|v| v.is_finite() && *v > 0.0) else {
        return 0.0;
    };
    let scale = match plan_usd_per_month.filter(|v| v.is_finite() && *v > 0.0) {
        Some(plan_price) if p_eff.is_finite() && p_eff > 0.0 => p_eff / plan_price,
        _ => 1.0,
    };
    let effective_budget = weekly_budget * scale;
    if !effective_budget.is_finite() || effective_budget <= 0.0 {
        return 0.0;
    }
    (100.0 * mean_api_usd / effective_budget).max(0.0)
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaTelemetrySnapshot {
    pub windows: BTreeMap<WindowKey, WindowTelemetry>,
    pub recent_placements: BTreeMap<String, Vec<RecentPlacement>>,
    pub category_stats: BTreeMap<String, CategoryBurnStats>,
}

impl QuotaTelemetrySnapshot {
    pub fn category_stats(&self, category: &str) -> CategoryBurnStats {
        self.category_stats
            .get(category)
            .cloned()
            .unwrap_or_else(|| default_category_burn_stats(category))
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
    ) -> f64 {
        let Some(placements) = self.recent_placements.get(profile_id) else {
            return 0.0;
        };
        placements
            .iter()
            .filter(|placement| {
                second_to_last_clean_ts
                    .is_none_or(|cutoff| placement.created_at_epoch_seconds > cutoff)
            })
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
    let mut clean: Vec<&ProfileQuota> = history
        .iter()
        .filter(|row| row.error.is_none() && !row.windows.is_empty())
        .collect();
    clean.sort_by_key(|row| row.refreshed_at_epoch_seconds);

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

    // Compute paired 5h-to-weekly ratio per model_scope (and unscoped fallback)
    // across consecutive clean rows that carry both a valid 5h and Long delta.
    let mut ratio_num_by_scope: BTreeMap<Option<String>, (f64, f64)> = BTreeMap::new();
    for pair in clean.windows(2) {
        let (prev, cur) = (pair[0], pair[1]);
        let prev_ts = i64::try_from(prev.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
        let cur_ts = i64::try_from(cur.refreshed_at_epoch_seconds).unwrap_or(i64::MAX);
        if cur_ts <= prev_ts {
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
            if delta > 0.0 {
                match kind {
                    WindowKind::Short5h => {
                        short_deltas.insert(cur_w.model_scope.clone(), delta);
                    }
                    WindowKind::Long => {
                        long_deltas.insert(cur_w.model_scope.clone(), delta);
                    }
                }
            }
        }
        for (scope, d_5h) in &short_deltas {
            if let Some(d_long) = long_deltas.get(scope).or_else(|| long_deltas.get(&None))
                && *d_long > 0.0
            {
                let entry = ratio_num_by_scope
                    .entry(scope.clone())
                    .or_insert((0.0, 0.0));
                entry.0 += *d_5h;
                entry.1 += *d_long;
            }
        }
    }

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

        let five_hour_to_weekly_ratio = ratio_num_by_scope
            .get(&model_scope)
            .or_else(|| ratio_num_by_scope.get(&None))
            .and_then(|(num, den)| (*den > 0.0 && *num > 0.0).then_some(*num / *den));

        out.insert(
            WindowKey {
                profile_id: profile_id.to_owned(),
                cache_identity: cache_identity.to_owned(),
                window_kind,
                model_scope,
            },
            WindowTelemetry {
                trailing_60m_pct_per_hour,
                second_to_last_clean_ts,
                five_hour_to_weekly_ratio,
            },
        );
    }
    out
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

#[derive(Default)]
struct QuotaTelemetryStoreInner {
    windows: BTreeMap<WindowKey, WindowTelemetry>,
    durable_placements: BTreeMap<String, Vec<RecentPlacement>>,
    category_stats: BTreeMap<String, CategoryBurnStats>,
    next_reservation_id: u64,
    in_flight: BTreeMap<u64, (String, RecentPlacement)>,
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
        for (profile_id, placement) in self.in_flight.values() {
            recent_placements
                .entry(profile_id.clone())
                .or_default()
                .push(placement.clone());
        }
        QuotaTelemetrySnapshot {
            windows: self.windows.clone(),
            recent_placements,
            category_stats: self.category_stats.clone(),
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
        self.committed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut guard = self
            .store
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = guard.in_flight.remove(&self.reservation_id) {
            let key = (
                session_id.unwrap_or_else(|| format!("__unkeyed-{}", self.reservation_id)),
                entry.0.clone(),
            );
            guard.committed_pending.insert(key, entry);
        }
    }
}

impl Drop for PlacementReservation {
    fn drop(&mut self) {
        if !self.committed.load(std::sync::atomic::Ordering::Relaxed) {
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

    /// Atomically snapshot telemetry, draw/order `candidates`, and reserve
    /// `candidates[0]` in-flight under a single short synchronous lock.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_and_reserve<T: AsRef<crate::profile_scheduler::ScheduledCandidate>>(
        &self,
        candidates: &mut Vec<T>,
        execution_policy: mj_core::config::ExecutionPolicy,
        placement: crate::profile_scheduler::Placement,
        category: Option<&str>,
        ranking_anchor: Option<&str>,
        prefer: &[String],
        power_ranking: &crate::power_ranking::PowerRankingSnapshot,
        now_seconds: i64,
    ) -> anyhow::Result<Option<PlacementReservation>> {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshot = guard.build_snapshot();
        let ctx = crate::profile_scheduler::SchedulingContext {
            execution_policy,
            placement,
            category,
            ranking_anchor,
            prefer,
            telemetry: &snapshot,
            power_ranking,
            now_seconds,
        };
        crate::profile_scheduler::draw_scheduled_candidates(candidates, &ctx)?;
        let Some(first) = candidates.first() else {
            return Ok(None);
        };
        let reservation_id = guard.next_reservation_id;
        guard.next_reservation_id = guard.next_reservation_id.saturating_add(1);
        guard.in_flight.insert(
            reservation_id,
            (
                first.as_ref().profile_id.clone(),
                RecentPlacement {
                    category: category.unwrap_or("primary").to_owned(),
                    created_at_epoch_seconds: now_seconds,
                },
            ),
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
                    created_at_epoch_seconds,
                },
            ),
        );
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
    pub fn refresh_from_store(
        &self,
        profiles: &[(String, String)],
        now_seconds: i64,
    ) -> anyhow::Result<()> {
        let since = u64::try_from(now_seconds)
            .unwrap_or(0)
            .saturating_sub(HISTORY_LOOKBACK_SECONDS);
        let mut all_windows = BTreeMap::new();
        for (profile_id, cache_identity) in profiles {
            let history =
                crate::database::load_quota_history_since(cache_identity, profile_id, since)?;
            let computed =
                compute_window_telemetry(profile_id, cache_identity, &history, now_seconds);
            all_windows.extend(computed);
        }
        let placements_since = now_seconds.saturating_sub(HISTORY_LOOKBACK_SECONDS as i64);
        let rows = crate::database::load_recent_placements_since(placements_since)?;
        self.apply_store_refresh(all_windows, rows, placements_since);
        Ok(())
    }

    pub fn apply_store_refresh(
        &self,
        all_windows: BTreeMap<WindowKey, WindowTelemetry>,
        rows: Vec<crate::database::RecentPlacementRow>,
        placements_since: i64,
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
        self.replace_category_stats(stats);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
    fn draw_and_reserve_serializes_concurrent_draws_releases_on_drop_and_dedups_refresh() {
        use crate::power_ranking::PowerRankingSnapshot;
        use crate::profile_scheduler::{CandidateQuota, Placement, ScheduledCandidate};
        use crate::quota::QuotaWindow;
        use mj_core::config::{ExecutionPolicy, HarnessKind};

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
                    ExecutionPolicy::Unconstrained,
                    Placement::Child,
                    Some("subtle"),
                    None,
                    &[],
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
                        ExecutionPolicy::Unconstrained,
                        Placement::Child,
                        Some("subtle"),
                        None,
                        &[],
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
                ExecutionPolicy::Unconstrained,
                Placement::Child,
                Some("primary"),
                None,
                &[],
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
                created_at_epoch_seconds: now,
            }],
            now - 7200,
        );
        assert_eq!(moved.snapshot().recent_placements["profile-b"].len(), 1);
    }
}
