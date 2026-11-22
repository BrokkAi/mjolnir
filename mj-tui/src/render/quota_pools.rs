//! Provider identity is resolved off the UI thread; pooling uses only snapshots.
use std::collections::BTreeMap;
use std::thread::JoinHandle;

use mj_client::quota::{ProfileQuota, QuotaWindow};
use mj_core::config::{AntigravityQuotaPool, Config, HarnessKind, HarnessProfile, QuotaProvider};

use super::quota_remaining_percent;

// Real length of a recognised window label, used when the report gives no duration.
fn period_seconds(window: &QuotaWindow) -> i64 {
    window
        .duration_seconds
        .unwrap_or_else(|| match window.label.to_ascii_lowercase().as_str() {
            "5h" | "5-hour" | "5 hour" => 18000,
            "week" | "weekly" | "7d" => 604800,
            "month" | "monthly" => 2_592_000,
            _ => 0,
        })
}

/// A profile's pooled allowances: its Week window and its Month window each
/// join their own pool; without either, the longest readable window stands in.
fn readable_windows(report: &ProfileQuota) -> Vec<&QuotaWindow> {
    let readable: Vec<_> = report
        .windows
        .iter()
        .filter(|window| window.model_scope.is_none() && quota_remaining_percent(window).is_some())
        .collect();
    let long: Vec<_> = readable
        .iter()
        .copied()
        .filter(|window| matches!(PoolPeriod::of(window), PoolPeriod::Week | PoolPeriod::Month))
        .collect();
    if !long.is_empty() {
        // One window per period, in the order the report lists them.
        let mut seen = Vec::new();
        return long
            .into_iter()
            .filter(|window| {
                let period = PoolPeriod::of(window);
                !seen.contains(&period) && {
                    seen.push(period);
                    true
                }
            })
            .collect();
    }
    readable
        .into_iter()
        .max_by_key(|window| period_seconds(window))
        .into_iter()
        .collect()
}

/// Profile and provider read for it, and the refresh generation that read it.
type Providers = BTreeMap<String, (HarnessProfile, QuotaProvider, u64)>;

struct ProviderTask {
    profile: HarnessProfile,
    generation: u64,
    handle: JoinHandle<QuotaProvider>,
}

/// Codex and OpenCode providers come from files, so each profile is read on
/// its own background thread and published as soon as it finishes.
#[derive(Default)]
pub(crate) struct ProviderDiscovery {
    resolved: Providers,
    tasks: BTreeMap<String, ProviderTask>,
    generation: u64,
    refresh_requested: bool,
    /// Profiles whose discovery thread could not be started, reported on the next poll.
    spawn_failures: Vec<String>,
}

impl std::fmt::Debug for ProviderDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDiscovery")
            .field("resolved", &self.resolved.keys().collect::<Vec<_>>())
            .field("in_flight", &self.tasks.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn reads_files(profile: &HarnessProfile) -> bool {
    matches!(profile.kind, HarnessKind::Codex | HarnessKind::OpenCode)
}

fn provider_unavailable(profile: &HarnessProfile) -> QuotaProvider {
    QuotaProvider::Other(format!(
        "{} (provider unavailable)",
        profile.kind.display_name()
    ))
}

impl ProviderDiscovery {
    pub(crate) fn start(config: &Config) -> Self {
        let mut discovery = Self::default();
        discovery.spawn_missing(config);
        discovery
    }

    pub(crate) fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    /// Start one task per enabled file-backed profile that has no reading
    /// for its current configuration and none in flight.
    fn spawn_missing(&mut self, config: &Config) {
        for (id, profile) in config.enabled_profiles() {
            if !reads_files(profile)
                || self.tasks.contains_key(id)
                || self.resolved.get(id).is_some_and(|(seen, _, generation)| {
                    seen == profile && *generation == self.generation
                })
            {
                continue;
            }
            let owned = profile.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("quota-provider-{id}"))
                .spawn(move || owned.quota_provider());
            match spawned {
                Ok(handle) => {
                    self.tasks.insert(
                        id.to_owned(),
                        ProviderTask {
                            profile: profile.clone(),
                            generation: self.generation,
                            handle,
                        },
                    );
                }
                // The OS refused a thread: record this profile as unresolved and
                // report it; a profile refresh retries.
                Err(_) => {
                    self.resolved.insert(
                        id.to_owned(),
                        (
                            profile.clone(),
                            provider_unavailable(profile),
                            self.generation,
                        ),
                    );
                    self.spawn_failures.push(id.to_owned());
                }
            }
        }
    }

    fn provider(&self, id: &str, profile: &HarnessProfile) -> Option<QuotaProvider> {
        if !reads_files(profile) {
            return Some(profile.quota_provider());
        }
        self.resolved
            .get(id)
            .filter(|(seen, _, _)| seen == profile)
            .map(|(_, provider, _)| provider.clone())
    }

    pub(crate) fn ready(&self) -> bool {
        self.tasks.values().any(|task| task.handle.is_finished())
    }

    /// Join a finished task. A refresh or configuration change since it
    /// started makes its reading stale, and it is dropped to be read again.
    fn finish(&mut self, id: String, task: ProviderTask) -> bool {
        let stale = task.generation != self.generation;
        let (provider, failed) = match task.handle.join() {
            Ok(provider) => (provider, false),
            Err(_) => (provider_unavailable(&task.profile), true),
        };
        if !stale {
            self.resolved
                .insert(id, (task.profile, provider, task.generation));
        }
        failed
    }

    pub(crate) fn poll(&mut self, config: &Config) -> Option<String> {
        if std::mem::take(&mut self.refresh_requested) {
            // Keep showing the previous reading until the new one arrives.
            self.generation += 1;
        }
        let finished: Vec<_> = self
            .tasks
            .iter()
            .filter(|(_, task)| task.handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        let mut failed = std::mem::take(&mut self.spawn_failures);
        for id in finished {
            let task = self.tasks.remove(&id).expect("finished task");
            if self.finish(id.clone(), task) {
                failed.push(id);
            }
        }
        self.spawn_missing(config);
        failed.append(&mut self.spawn_failures);
        (!failed.is_empty()).then(|| {
            format!(
                "Quota provider discovery failed for {}; refresh profiles to retry.",
                failed.join(", ")
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn settle(&mut self) {
        for (id, task) in std::mem::take(&mut self.tasks) {
            assert!(!self.finish(id, task), "quota provider discovery");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PoolKey {
    pub provider: QuotaProvider,
    pub scope: Option<String>,
    pub period: PoolPeriod,
}

/// Period identity keeps unrelated rolling and long-running allowances apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PoolPeriod {
    Week,
    Month,
    FiveHour,
    Other { label: String, seconds: Option<i64> },
}

impl PoolPeriod {
    fn of(window: &QuotaWindow) -> Self {
        let label = window.label.to_ascii_lowercase();
        let seconds = window.duration_seconds;
        if matches!(label.as_str(), "week" | "weekly" | "7d") && seconds.is_none_or(|s| s == 604800)
        {
            Self::Week
        } else if matches!(label.as_str(), "month" | "monthly")
            && seconds.is_none_or(|s| s > 604800)
        {
            Self::Month
        } else if matches!(label.as_str(), "5h" | "5-hour" | "5 hour")
            && seconds.is_none_or(|s| s == 18000)
        {
            Self::FiveHour
        } else {
            Self::Other {
                label: window.label.clone(),
                seconds,
            }
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Week => "week",
            Self::Month => "Month",
            Self::FiveHour => "5H",
            Self::Other { label, .. } => label,
        }
    }

    /// One character naming the period on timelines and in the legend.
    pub(crate) fn marker(&self) -> char {
        match self {
            Self::Week => 'W',
            Self::Month => 'M',
            Self::FiveHour => '5',
            Self::Other { label, .. } => label
                .chars()
                .find(|ch| ch.is_alphabetic())
                .map_or('R', |ch| ch.to_ascii_uppercase()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuotaRow {
    Pool(PoolKey),
    Profile(String),
    Lane(PoolKey, String),
}

impl QuotaRow {
    pub(crate) fn pool(&self) -> Option<&PoolKey> {
        match self {
            Self::Pool(key) | Self::Lane(key, _) => Some(key),
            Self::Profile(_) => None,
        }
    }
}

pub(crate) fn selectable_rows(
    providers: &[ProviderQuota],
    expanded: &std::collections::BTreeSet<PoolKey>,
) -> Vec<QuotaRow> {
    providers
        .iter()
        .flat_map(|provider| {
            provider
                .pools
                .iter()
                .flat_map(|pool| {
                    std::iter::once(QuotaRow::Pool(pool.key.clone())).chain(
                        pool.members
                            .iter()
                            .filter(|_| expanded.contains(&pool.key))
                            .map(|member| QuotaRow::Lane(pool.key.clone(), member.id.clone())),
                    )
                })
                .chain(
                    provider
                        .text
                        .iter()
                        .map(|(id, _)| QuotaRow::Profile(id.clone())),
                )
        })
        .collect()
}

pub(crate) struct ProviderQuota {
    pub provider: QuotaProvider,
    pub pools: Vec<QuotaPool>,
    pub text: Vec<(String, String)>,
}

pub(crate) struct QuotaPool {
    pub key: PoolKey,
    pub label: String,
    pub members: Vec<PoolMember>,
    pub remaining: u8,
    pub unweighted: bool,
    pub missing: usize,
}

impl QuotaPool {
    pub(super) fn reported_members(&self) -> impl Iterator<Item = &PoolMember> {
        self.members
            .iter()
            .filter(|member| !member.awaiting_reading)
    }

    pub(super) fn reported_weight(&self) -> f64 {
        self.reported_members().map(|member| member.weight).sum()
    }

    // Capacity is measured in weighted percentage points.
    pub(super) fn available_capacity(&self) -> f64 {
        self.reported_members()
            .map(|member| member.weight * f64::from(member.remaining))
            .sum()
    }

    pub(crate) fn percentage(&self) -> String {
        // With every reading past its reset, nothing is known to remain.
        let known = self.reported_members().next().is_some();
        format!(
            "{}{}%{}",
            // `~` marks an unweighted average: plan sizes are unknown.
            if self.unweighted { "~" } else { "" },
            if known {
                self.remaining.to_string()
            } else {
                "?".into()
            },
            if self.missing > 0 { "*" } else { "" }
        )
    }
}

pub(crate) struct PoolMember {
    pub id: String,
    pub weight: f64,
    pub limit: Option<i64>,
    pub remaining: u8,
    pub reset: Option<i64>,
    /// The reset time has passed and no reading taken since has arrived.
    pub awaiting_reading: bool,
    /// Latest reset among the profile's other exhausted unscoped long windows
    /// (`i64::MAX` when one reports no reset): the lane stays blocked until then.
    pub other_long_blocked_until: Option<i64>,
    pub banked: Option<u64>,
    pub five_hour: Option<QuotaWindow>,
    pub five_hour_awaiting_reading: bool,
    /// The provider's reset text, for reports without a reset timestamp.
    pub reset_text: Option<String>,
    pub five_hour_idle: bool,
    pub fable: Option<u8>,
    pub note: Option<String>,
}

pub(crate) fn provider_pools(
    config: &Config,
    quotas: &BTreeMap<String, ProfileQuota>,
    providers: &ProviderDiscovery,
    now: u64,
) -> Vec<ProviderQuota> {
    let mut grouped = BTreeMap::<QuotaProvider, ProviderQuota>::new();
    for (id, profile) in config.enabled_profiles() {
        if matches!(profile.kind, HarnessKind::Codex | HarnessKind::OpenCode)
            && providers.provider(id, profile).is_none()
        {
            let heading =
                QuotaProvider::Other(format!("{} (resolving)", profile.kind.display_name()));
            grouped
                .entry(heading.clone())
                .or_insert_with(|| ProviderQuota {
                    provider: heading,
                    pools: Vec::new(),
                    text: Vec::new(),
                })
                .text
                .push((
                    id.into(),
                    format!("resolving provider{}", mj_chat::theme::glyphs().ellipsis),
                ));
            continue;
        }
        let provider = profile_provider(id, profile, providers);
        let group = grouped
            .entry(provider.clone())
            .or_insert_with(|| ProviderQuota {
                provider,
                pools: Vec::new(),
                text: Vec::new(),
            });
        let Some(quota) = quotas.get(id) else {
            group.text.push((
                id.into(),
                format!("refreshing{}", mj_chat::theme::glyphs().ellipsis),
            ));
            continue;
        };
        let label = if quota.is_usage_priced() {
            Some("pay-per-use".into())
        } else if quota.is_not_reported() || quota.extra.as_deref() == Some("n/a") {
            Some("not reported".into())
        } else if quota.is_idle_without_reading() {
            Some("idle; usage shows after the next request".into())
        } else if quota.error.is_some() {
            Some(
                quota
                    .refused_check_without_reading(now)
                    .or_else(|| quota.error_label())
                    .expect("errored report has a label"),
            )
        } else {
            None
        };
        if let Some(label) = label {
            group.text.push((
                id.into(),
                match &quota.extra {
                    Some(extra) if !quota.is_usage_priced() && !quota.is_idle_without_reading() => {
                        format!("{label} · {extra}")
                    }
                    _ => label,
                },
            ));
            continue;
        }
        let reports = if profile.kind == HarnessKind::Antigravity {
            profile
                .antigravity_pools()
                .into_iter()
                .map(|pool| {
                    (
                        Some(pool.model_scope().to_owned()),
                        quota.for_antigravity_pool(pool),
                    )
                })
                .collect()
        } else {
            vec![(None, quota.clone())]
        };
        let mut added = false;
        for (scope, report) in reports {
            for window in readable_windows(&report) {
                let Some(remaining) = quota_remaining_percent(window) else {
                    continue;
                };
                added = true;
                let key_period = PoolPeriod::of(window);
                let key = PoolKey {
                    provider: group.provider.clone(),
                    scope: scope.clone(),
                    period: key_period.clone(),
                };
                let index = group
                    .pools
                    .iter()
                    .position(|pool| pool.key == key)
                    .unwrap_or_else(|| {
                        group.pools.push(QuotaPool {
                            key,
                            label: scope
                                .as_deref()
                                .and_then(AntigravityQuotaPool::of_scope)
                                .map(|pool| pool.name().to_owned())
                                .unwrap_or_default(),
                            members: Vec::new(),
                            remaining: 0,
                            unweighted: false,
                            missing: 0,
                        });
                        group.pools.len() - 1
                    });
                group.pools[index].members.push(PoolMember {
                    id: id.into(),
                    weight: quota.monthly_price_usd.map(f64::from).unwrap_or(0.0),
                    limit: window.limit.filter(|limit| *limit > 0),
                    remaining,
                    reset: window.resets_at_epoch_seconds,
                    awaiting_reading: window.resets_at_epoch_seconds.is_some_and(|reset| {
                        reset >= 0
                            && reset as u64 <= now
                            && quota.refreshed_at_epoch_seconds < reset as u64
                    }),
                    other_long_blocked_until: report
                        .windows
                        .iter()
                        .filter(|other| {
                            other.model_scope.is_none()
                                && matches!(
                                    PoolPeriod::of(other),
                                    PoolPeriod::Week | PoolPeriod::Month
                                )
                                && PoolPeriod::of(other) != key_period
                                && quota_remaining_percent(other) == Some(0)
                        })
                        .map(|other| other.resets_at_epoch_seconds.unwrap_or(i64::MAX))
                        .max(),
                    banked: report.banked_resets_for_window(window),
                    reset_text: window.resets.clone(),
                    five_hour: report
                        .five_hour_window()
                        .filter(|five| *five != window)
                        .cloned(),
                    five_hour_awaiting_reading: report.five_hour_window().is_some_and(|five| {
                        five.resets_at_epoch_seconds.is_some_and(|reset| {
                            reset >= 0
                                && reset as u64 <= now
                                && quota.refreshed_at_epoch_seconds < reset as u64
                        })
                    }),
                    // Claude omits the short window until first use, and Muse
                    // while none is open; other adapters do not define an
                    // absent window as idle.
                    five_hour_idle: matches!(profile.kind, HarnessKind::Claude | HarnessKind::Muse),
                    fable: quota
                        .fable_weekly_window()
                        .and_then(quota_remaining_percent),
                    note: {
                        let mut notes: Vec<String> = report
                            .windows
                            .iter()
                            .filter(|window| {
                                window.model_scope.is_none()
                                    && quota_remaining_percent(window).is_none()
                            })
                            .map(|window| format!("{} unknown", window.label))
                            .collect();
                        let extra = match (quota.rate_limit_label(now), &quota.extra) {
                            (Some(rate), Some(extra)) => Some(format!("{rate} · {extra}")),
                            (rate, extra) => rate.or_else(|| extra.clone()),
                        };
                        notes.extend(extra);
                        (!notes.is_empty()).then(|| notes.join(" · "))
                    },
                });
            }
        }
        if !added {
            group.text.push((
                id.into(),
                match &quota.extra {
                    Some(extra) => format!("unavailable · {extra}"),
                    None => "unavailable".into(),
                },
            ));
        }
    }
    for group in grouped.values_mut() {
        // Gemini first, matching the existing independent-pool presentation.
        // A Month pool follows its Week pool whatever order the report lists them.
        group.pools.sort_by_key(|pool| {
            (
                pool.key.scope.as_deref() != Some("gemini"),
                pool.key.period == PoolPeriod::Month,
            )
        });
        let multiple_periods = group.pools.iter().any(|pool| {
            group.pools.iter().any(|other| {
                other.key.scope == pool.key.scope && other.key.period != pool.key.period
            })
        });
        for pool in &mut group.pools {
            if multiple_periods || pool.key.period != PoolPeriod::Week {
                pool.label = format!("{} {}", pool.label, pool.key.period.label())
                    .trim()
                    .to_owned();
            }
            pool.members.sort_by(|a, b| {
                a.reset
                    .unwrap_or(i64::MAX)
                    .cmp(&b.reset.unwrap_or(i64::MAX))
                    .then_with(|| a.id.cmp(&b.id))
            });
            pool.missing = config
                .enabled_profiles()
                .filter(|(id, profile)| {
                    if !providers
                        .provider(id, profile)
                        .is_some_and(|provider| provider == group.provider)
                        || pool.members.iter().any(|member| member.id == *id)
                    {
                        return false;
                    }
                    // An Antigravity profile belongs only to the pools it uses.
                    let eligible = pool.key.scope.is_none()
                        || profile.antigravity_pools().into_iter().any(|candidate| {
                            Some(candidate.model_scope()) == pool.key.scope.as_deref()
                        });
                    let Some(report) = quotas.get(*id) else {
                        return eligible;
                    };
                    if report.is_usage_priced()
                        || report.is_not_reported()
                        || report.extra.as_deref() == Some("n/a")
                    {
                        return false;
                    }
                    // A failed probe cannot tell us which allowances the profile holds.
                    if report.error.is_some() || report.windows.is_empty() {
                        return eligible;
                    }
                    let scoped = if profile.kind == HarnessKind::Antigravity {
                        profile
                            .antigravity_pools()
                            .into_iter()
                            .find(|candidate| {
                                Some(candidate.model_scope()) == pool.key.scope.as_deref()
                            })
                            .map(|candidate| report.for_antigravity_pool(candidate))
                    } else if pool.key.scope.is_none() {
                        Some(report.clone())
                    } else {
                        None
                    };
                    scoped.is_some_and(|report| {
                        report.windows.iter().any(|window| {
                            PoolPeriod::of(window) == pool.key.period
                                && (quota_remaining_percent(window).is_none()
                                    || window.resets_at_epoch_seconds.is_some_and(|reset| {
                                        reset >= 0
                                            && reset as u64 <= now
                                            && report.refreshed_at_epoch_seconds < reset as u64
                                    }))
                        })
                    })
                })
                .count()
                // A pooled reading older than its own reset is missing too.
                + pool
                    .members
                    .iter()
                    .filter(|member| member.awaiting_reading)
                    .count();
            // The provider, scope and period identify the allowance's unit.
            // Prefer its absolute capacity over a subscription's dollar price.
            if pool.members.iter().all(|member| member.limit.is_some()) {
                for member in &mut pool.members {
                    member.weight = member.limit.unwrap() as f64;
                }
            } else if pool.members.iter().any(|member| member.weight <= 0.0) {
                pool.unweighted = true;
                for member in &mut pool.members {
                    member.weight = 1.0;
                }
            }
            let total = pool.reported_weight();
            pool.remaining = if total > 0.0 {
                (pool.available_capacity() / total).round() as u8
            } else {
                0
            };
        }
    }
    grouped.into_values().collect()
}

pub(crate) fn profile_provider(
    id: &str,
    profile: &HarnessProfile,
    providers: &ProviderDiscovery,
) -> QuotaProvider {
    providers.provider(id, profile).unwrap_or_else(|| {
        if matches!(profile.kind, HarnessKind::Codex | HarnessKind::OpenCode) {
            QuotaProvider::Other(format!("{} (resolving)", profile.kind.display_name()))
        } else {
            profile.quota_provider()
        }
    })
}

/// Largest-remainder apportionment for the available quota and refill chunks.
/// Give every positive segment a cell when they all fit, taking cells only
/// from other refills. On narrower bars, keep positive availability visible
/// and let the caller list omitted refills.
pub(super) fn refill_widths(
    available: f64,
    refills: impl Iterator<Item = f64>,
    total: f64,
    cells: usize,
) -> Vec<usize> {
    let capacities: Vec<_> = std::iter::once(available).chain(refills).collect();
    let mut widths = Vec::new();
    let mut fractions = Vec::new();
    for (index, &capacity) in capacities.iter().enumerate() {
        let exact = cells as f64 * capacity / total;
        widths.push(exact.floor() as usize);
        fractions.push((index, exact.fract()));
    }
    fractions.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let leftover = cells - widths.iter().sum::<usize>();
    for (index, _) in fractions.into_iter().take(leftover) {
        widths[index] += 1;
    }
    let all_fit = capacities
        .iter()
        .filter(|&&capacity| capacity > 0.0)
        .count()
        <= cells;
    // Minimum cells come only from refill chunks, never from the available
    // fill, so the fill stays proportional to the percentage printed in it.
    // A refill that cannot get a cell is listed under the bar instead.
    for (index, capacity) in capacities.into_iter().enumerate() {
        if capacity > 0.0 && widths[index] == 0 && (all_fit || index == 0) {
            // A refill only takes a cell from a refill that keeps one.
            let spare = if index == 0 { 0 } else { 1 };
            if let Some(donor) = widths
                .iter_mut()
                .skip(1)
                .filter(|width| **width > spare)
                .max_by_key(|width| **width)
            {
                *donor -= 1;
                widths[index] = 1;
            }
        }
    }
    widths
}
