//! Provider identity is resolved off the UI thread; pooling uses only snapshots.
use std::collections::BTreeMap;
use std::thread::JoinHandle;

use mj_client::quota::{AntigravityQuotaPool, ProfileQuota, QuotaWindow};
use mj_core::config::{Config, HarnessKind, HarnessProfile, QuotaProvider};

use super::quota_remaining_percent;

type Providers = BTreeMap<String, QuotaProvider>;

#[derive(Debug, Default)]
pub(crate) struct ProviderDiscovery {
    profiles: BTreeMap<String, HarnessProfile>,
    resolved: Providers,
    task: Option<JoinHandle<Providers>>,
    refresh_requested: bool,
}

impl ProviderDiscovery {
    pub(crate) fn start(config: &Config) -> Self {
        let profiles = config.profiles.clone();
        let resolved = profiles
            .iter()
            .filter(|(_, profile)| {
                !matches!(profile.kind, HarnessKind::Codex | HarnessKind::OpenCode)
            })
            .map(|(id, profile)| (id.clone(), profile.quota_provider()))
            .collect();
        let pending: Vec<_> = profiles
            .iter()
            .filter(|(_, profile)| {
                matches!(profile.kind, HarnessKind::Codex | HarnessKind::OpenCode)
            })
            .map(|(id, profile)| (id.clone(), profile.clone()))
            .collect();
        let task = (!pending.is_empty()).then(|| {
            std::thread::spawn(move || {
                pending
                    .into_iter()
                    .map(|(id, profile)| (id, profile.quota_provider()))
                    .collect()
            })
        });
        Self {
            profiles,
            resolved,
            task,
            refresh_requested: false,
        }
    }

    pub(crate) fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    fn provider(&self, id: &str, profile: &HarnessProfile) -> Option<&QuotaProvider> {
        (self.profiles.get(id) == Some(profile))
            .then(|| self.resolved.get(id))
            .flatten()
    }

    pub(crate) fn ready(&self) -> bool {
        self.task.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub(crate) fn poll(&mut self, config: &Config) -> Option<String> {
        let mut error = None;
        if self.ready() {
            match self.task.take().expect("finished task").join() {
                Ok(providers) => self.resolved.extend(providers),
                Err(_) => {
                    for (id, profile) in &self.profiles {
                        self.resolved.entry(id.clone()).or_insert_with(|| {
                            QuotaProvider::Other(format!(
                                "{} (provider unavailable)",
                                profile.kind.display_name()
                            ))
                        });
                    }
                    error =
                        Some("Quota provider discovery failed; refresh profiles to retry.".into())
                }
            }
        }
        // Finish supervising the previous read before replacing its snapshot.
        if self.task.is_none() && (self.profiles != config.profiles || self.refresh_requested) {
            let mut next = Self::start(config);
            for (id, provider) in &self.resolved {
                if self.profiles.get(id) == config.profiles.get(id) {
                    next.resolved.insert(id.clone(), provider.clone());
                }
            }
            *self = next;
        }
        error
    }

    #[cfg(test)]
    pub(crate) fn settle(&mut self) {
        if let Some(task) = self.task.take() {
            self.resolved
                .extend(task.join().expect("quota provider discovery"));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PoolKey {
    pub provider: QuotaProvider,
    pub scope: Option<String>,
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
}

pub(crate) struct PoolMember {
    pub id: String,
    pub weight: f64,
    pub remaining: u8,
    pub reset: Option<i64>,
    pub banked: Option<u64>,
    pub five_hour: Option<QuotaWindow>,
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
        let provider = providers.provider(id, profile).cloned().unwrap_or_else(|| {
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
            group.text.push((id.into(), "unavailable".into()));
            continue;
        };
        let label = if quota.is_usage_priced() {
            Some("pay-per-use".into())
        } else if quota.is_not_reported() {
            Some("not reported".into())
        } else if quota.error.is_some() {
            Some(
                quota
                    .refused_check_without_reading(now)
                    .unwrap_or_else(|| "unavailable".into()),
            )
        } else {
            None
        };
        if let Some(label) = label {
            group.text.push((id.into(), label));
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
            let Some(window) = report.weekly_window().or_else(|| report.five_hour_window()) else {
                continue;
            };
            let Some(remaining) = quota_remaining_percent(window) else {
                continue;
            };
            added = true;
            let key = PoolKey {
                provider: group.provider.clone(),
                scope: scope.clone(),
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
                    });
                    group.pools.len() - 1
                });
            group.pools[index].members.push(PoolMember {
                id: id.into(),
                weight: quota.monthly_price_usd.map(f64::from).unwrap_or(0.0),
                remaining,
                reset: window.resets_at_epoch_seconds,
                banked: quota.banked_resets,
                five_hour: report.five_hour_window().cloned(),
                fable: quota
                    .fable_weekly_window()
                    .and_then(quota_remaining_percent),
                note: quota.rate_limit_label(now),
            });
        }
        if !added {
            group.text.push((id.into(), "unavailable".into()));
        }
    }
    for group in grouped.values_mut() {
        // Gemini first, matching the existing independent-pool presentation.
        group
            .pools
            .sort_by_key(|pool| pool.key.scope.as_deref() != Some("gemini"));
        for pool in &mut group.pools {
            pool.members.sort_by(|a, b| {
                a.reset
                    .unwrap_or(i64::MAX)
                    .cmp(&b.reset.unwrap_or(i64::MAX))
                    .then_with(|| a.id.cmp(&b.id))
            });
            if pool.members.iter().any(|member| member.weight <= 0.0) {
                for member in &mut pool.members {
                    member.weight = 1.0;
                }
            }
            let total: f64 = pool.members.iter().map(|member| member.weight).sum();
            pool.remaining = (pool
                .members
                .iter()
                .map(|member| member.weight * f64::from(member.remaining))
                .sum::<f64>()
                / total)
                .round() as u8;
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
