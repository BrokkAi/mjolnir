//! Provider identity is resolved off the UI thread; pooling uses only snapshots.
use std::collections::BTreeMap;
use std::thread::JoinHandle;

use mj_client::quota::{AntigravityQuotaPool, ProfileQuota, QuotaWindow};
use mj_core::config::{Config, HarnessKind, HarnessProfile, QuotaProvider};

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
            self.tasks.insert(
                id.to_owned(),
                ProviderTask {
                    profile: profile.clone(),
                    generation: self.generation,
                    handle: std::thread::spawn(move || owned.quota_provider()),
                },
            );
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
        let mut failed = Vec::new();
        for id in finished {
            let task = self.tasks.remove(&id).expect("finished task");
            if self.finish(id.clone(), task) {
                failed.push(id);
            }
        }
        self.spawn_missing(config);
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
    pub(crate) fn percentage(&self) -> String {
        // With every reading past its reset, nothing is known to remain.
        let known = self.members.iter().any(|member| !member.awaiting_reading);
        format!(
            "{}{}%{}",
            if self.unweighted { "avg " } else { "" },
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
    pub banked: Option<u64>,
    pub five_hour: Option<QuotaWindow>,
    pub five_hour_awaiting_reading: bool,
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
        let provider = providers.provider(id, profile).unwrap_or_else(|| {
            if matches!(profile.kind, HarnessKind::Codex | HarnessKind::OpenCode) {
                QuotaProvider::Other(format!("{} (resolving)", profile.kind.display_name()))
            } else {
                profile.quota_provider()
            }
        });
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
                    Some(extra) if !quota.is_usage_priced() => format!("{label} · {extra}"),
                    _ => label,
                },
            ));
            continue;
        }
        let reports = if profile.kind == HarnessKind::Antigravity {
            AntigravityQuotaPool::ALL
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
                let key = PoolKey {
                    provider: group.provider.clone(),
                    scope: scope.clone(),
                    period: PoolPeriod::of(window),
                };
                let index = group
                    .pools
                    .iter()
                    .position(|pool| pool.key == key)
                    .unwrap_or_else(|| {
                        group.pools.push(QuotaPool {
                            key,
                            label: match scope.as_deref() {
                                Some("gemini") => "Gemini".into(),
                                Some("claude,gpt") => "Claude & GPT".into(),
                                _ => String::new(),
                            },
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
                    banked: report.banked_resets_for_window(window),
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
                    // Claude omits the short window until first use; other adapters
                    // do not define an absent window as idle.
                    five_hour_idle: profile.kind == HarnessKind::Claude,
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
                    let eligible =
                        pool.key.scope.is_none() || profile.kind == HarnessKind::Antigravity;
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
                        AntigravityQuotaPool::ALL
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
            let total: f64 = pool
                .members
                .iter()
                .filter(|member| !member.awaiting_reading)
                .map(|member| member.weight)
                .sum();
            pool.remaining = if total > 0.0 {
                (pool
                    .members
                    .iter()
                    .filter(|member| !member.awaiting_reading)
                    .map(|member| member.weight * f64::from(member.remaining))
                    .sum::<f64>()
                    / total)
                    .round() as u8
            } else {
                0
            };
        }
    }
    grouped.into_values().collect()
}

/// Largest-remainder apportionment, with one cell per member whenever they fit.
pub(crate) fn segment_widths(members: &[PoolMember], cells: usize) -> Vec<usize> {
    let mut widths = vec![0; members.len()];
    if cells < members.len() {
        widths.iter_mut().take(cells).for_each(|width| *width = 1);
        return widths;
    }
    let weight: f64 = members.iter().map(|member| member.weight).sum();
    let mut fractions = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let exact = cells as f64 * member.weight / weight;
        widths[index] = (exact.floor() as usize).max(1);
        fractions.push((index, exact.fract()));
    }
    while widths.iter().sum::<usize>() > cells {
        let index = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, width)| **width)
            .map(|(index, _)| index)
            .expect("nonempty widths");
        widths[index] -= 1;
    }
    fractions.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let leftover = cells - widths.iter().sum::<usize>();
    for (index, _) in fractions.into_iter().take(leftover) {
        widths[index] += 1;
    }
    widths
}
