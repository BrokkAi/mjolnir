//! One resolver for turn review and plan turn review.
use crate::controller::Controller;
use crate::quota::{ProfileQuota, window_applies};
use crate::server_runtime::profile_catalog::ProfileCatalog;
use crate::session_manager::ManagedSessionHandle;
use crate::utility_llm::UtilityLlmRuntime;
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{Config, ReviewConfig};
use mj_core::review::settings::{
    ResolvedReviewSettings, ReviewModelFamily, ReviewModelSettings, model_version_cmp,
};
use mj_core::usage::{model_catalog, pricing};
use mj_core::worker_launch::ProfileConfig;
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Debug)]
struct Candidate {
    profile: String,
    model: String,
    identity: String,
    family: Option<ReviewModelFamily>,
    usage_priced: bool,
    price: Option<u64>,
    pinned: bool,
}

impl Candidate {
    fn tier(&self) -> u8 {
        if self.family.is_some() || self.pinned {
            u8::from(self.usage_priced)
        } else {
            2
        }
    }

    fn duplicates(&self, primary: Option<&str>) -> bool {
        primary.is_some_and(|primary| match ReviewModelFamily::for_model(primary) {
            Some(family) => self.family == Some(family),
            None => self.identity.eq_ignore_ascii_case(primary),
        })
    }
}

fn rank(candidates: &mut [Candidate], primary: Option<&str>) {
    candidates.sort_by(|a, b| {
        a.tier()
            .cmp(&b.tier())
            .then_with(|| {
                (a.tier() == 2 && a.price.is_none()).cmp(&(b.tier() == 2 && b.price.is_none()))
            })
            .then_with(|| a.duplicates(primary).cmp(&b.duplicates(primary)))
            .then_with(|| a.family.cmp(&b.family))
            // Preferred families have fixed priorities, independent of cost.
            .then_with(|| {
                if a.tier() == 2 {
                    a.price
                        .unwrap_or(u64::MAX)
                        .cmp(&b.price.unwrap_or(u64::MAX))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| model_version_cmp(&b.identity, &a.identity))
            .then_with(|| a.profile.cmp(&b.profile))
            .then_with(|| a.model.cmp(&b.model))
    });
}

fn candidates_for_profile(
    profile: &str,
    offered: &ProfileConfig,
    quota: Option<&ProfileQuota>,
    settings: &ReviewConfig,
    reasons: &mut Vec<String>,
) -> Vec<Candidate> {
    let mut newest = BTreeMap::new();
    let mut others = Vec::new();
    for choice in &offered.models {
        if settings
            .model
            .as_ref()
            .is_some_and(|model| model != &choice.value)
        {
            continue;
        }
        let identity = model_catalog::model_id(&offered.models, &choice.value);
        if quota.is_some_and(|quota| {
            quota.error.is_none()
                && quota.windows.iter().any(|window| {
                    window_applies(window, &identity) && window.remaining_percent == Some(0)
                })
        }) {
            reasons.push(format!(
                "{profile}: quota is exhausted for {}",
                choice.value
            ));
            continue;
        }
        let family = ReviewModelFamily::for_model(&identity);
        let candidate = Candidate {
            profile: profile.into(),
            model: choice.value.clone(),
            price: pricing::rates(&identity).map(|rates| rates[0] + rates[1]),
            identity,
            family,
            usage_priced: quota.is_some_and(ProfileQuota::is_usage_priced),
            pinned: settings.model.is_some(),
        };
        if let Some(family) = family {
            if newest.get(&family).is_none_or(|current: &Candidate| {
                model_version_cmp(&candidate.identity, &current.identity).is_gt()
            }) {
                newest.insert(family, candidate);
            }
        } else {
            others.push(candidate);
        }
    }
    others.extend(newest.into_values());
    if others.is_empty()
        && settings.model.is_some()
        && !offered
            .models
            .iter()
            .any(|choice| Some(&choice.value) == settings.model.as_ref())
    {
        reasons.push(format!(
            "{profile}: does not advertise model {}",
            settings.model.as_deref().unwrap()
        ));
    }
    others
}

/// Read the primary model from the session owner's current publication. All
/// model discovery uses the daemon's one catalog; independent profiles join
/// their cached discovery concurrently and remain cancellable.
pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
    cancelled: Arc<AtomicBool>,
    catalog: Option<Arc<ProfileCatalog>>,
) -> Result<ResolvedReviewSettings> {
    let controller = tokio::task::spawn_blocking(Controller::load).await??;
    let settings = settings.unwrap_or_else(|| controller.config.review.clone());
    let session = controller
        .state
        .sessions
        .get(handle.session_id())
        .context("review session is missing")?;
    ensure!(!session.archived, "this session is archived");
    if let Some(profile) = &settings.profile {
        ensure!(
            controller.config.enabled_profile(profile).is_some(),
            "reviewer profile {profile:?} is missing or disabled"
        );
        return resolve_candidate(
            catalog.as_ref(),
            profile,
            ReviewModelSettings {
                model: settings.model.clone(),
                effort: settings.effort.clone(),
                fast_mode: false,
            },
            false,
            false,
            &cancelled,
        )
        .await;
    }
    let profiles = controller.config.enabled_profiles().collect::<Vec<_>>();
    let quotas = UtilityLlmRuntime::shared()
        .quotas(&controller.config, &profiles)
        .await;
    let primary = handle.view().snapshot.and_then(|snapshot| {
        snapshot
            .materialized
            .configuration
            .values
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    });
    resolve_auto(
        catalog.as_ref(),
        &controller.config,
        &quotas,
        &settings,
        primary
            .as_deref()
            .map(|model| (session.last_profile.as_str(), model)),
        &cancelled,
    )
    .await
}

async fn resolve_auto(
    catalog: Option<&Arc<ProfileCatalog>>,
    config: &Config,
    quotas: &BTreeMap<String, ProfileQuota>,
    settings: &ReviewConfig,
    primary_selection: Option<(&str, &str)>,
    cancelled: &AtomicBool,
) -> Result<ResolvedReviewSettings> {
    use mj_core::profile_capabilities::CapabilityState;
    let snapshot = catalog.map(|catalog| catalog.snapshot());
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    let mut reasons = Vec::new();
    for (id, profile) in config.enabled_profiles() {
        if let Some(model) = &settings.model {
            // A pinned model must never stage an unrelated profile, and a
            // broken unknown profile must not delay a confirmed match.
            let ready = Some(profile.capabilities_key(id))
                .and_then(|key| snapshot.as_ref()?.profiles.get(&key))
                .and_then(|entry| match &entry.choices {
                    CapabilityState::Ready(choices) => Some(choices),
                    _ => None,
                });
            match ready {
                Some(choices) if choices.models.iter().any(|choice| &choice.value == model) => {
                    known.push(id)
                }
                Some(_) => reasons.push(format!("{id}: does not advertise model {model}")),
                None => unknown.push(id),
            }
        } else {
            known.push(id);
        }
    }
    let mut primary = primary_selection.map(|(_, model)| model.to_owned());
    for profiles in [known, unknown] {
        let discoveries =
            futures::future::join_all(profiles.iter().map(|id| async move {
                (*id, offered_choices(catalog, id, None, cancelled).await)
            }))
            .await;
        let mut candidates = Vec::new();
        for (id, discovery) in discoveries {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "review preparation cancelled"
            );
            match discovery {
                Ok(offered) => {
                    if let Some((_, model)) =
                        primary_selection.filter(|(profile, _)| *profile == id)
                    {
                        primary = Some(model_catalog::model_id(&offered.models, model));
                    }
                    candidates.extend(candidates_for_profile(
                        id,
                        &offered,
                        quotas.get(id),
                        settings,
                        &mut reasons,
                    ));
                }
                Err(error) => reasons.push(format!("{id}: {error:#}")),
            }
        }
        rank(&mut candidates, primary.as_deref());
        for candidate in candidates {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "review preparation cancelled"
            );
            let price_unknown = candidate.tier() == 2 && candidate.price.is_none();
            let result = resolve_candidate(
                catalog,
                &candidate.profile,
                ReviewModelSettings {
                    model: Some(candidate.model.clone()),
                    effort: Some("high".into()),
                    fast_mode: false,
                },
                true,
                price_unknown,
                cancelled,
            )
            .await;
            match result {
                Ok(resolved) => return Ok(resolved),
                Err(error) => {
                    tracing::warn!(profile = %candidate.profile, model = %candidate.model, %error, "Auto reviewer candidate unavailable");
                    reasons.push(format!(
                        "{} / {}: {error:#}",
                        candidate.profile, candidate.model
                    ));
                }
            }
        }
    }
    if reasons.is_empty() {
        reasons.push("enable a profile that advertises a model with High effort".into());
    }
    bail!("No usable Auto reviewer: {}", reasons.join("; "))
}

async fn offered_choices(
    catalog: Option<&Arc<ProfileCatalog>>,
    profile: &str,
    model: Option<String>,
    cancelled: &AtomicBool,
) -> Result<ProfileConfig> {
    let lookup = async {
        match (catalog, model) {
            (Some(catalog), Some(model)) => {
                catalog.model_capabilities(profile.to_owned(), model).await
            }
            (Some(catalog), None) => {
                Ok(catalog.capabilities(&[profile.to_owned()]).await?.remove(0))
            }
            (None, model) => {
                crate::controller::profile_config::discover(profile.to_owned(), model, false).await
            }
        }
    };
    tokio::select! {
        biased;
        _ = async {
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            while !cancelled.load(Ordering::Acquire) {
                interval.tick().await;
            }
        } => bail!("review preparation cancelled"),
        result = lookup => result,
    }
}

async fn resolve_candidate(
    catalog: Option<&Arc<ProfileCatalog>>,
    profile: &str,
    main: ReviewModelSettings,
    automatic: bool,
    price_unknown: bool,
    cancelled: &AtomicBool,
) -> Result<ResolvedReviewSettings> {
    let offered = offered_choices(catalog, profile, None, cancelled).await?;
    if let Some(model) = &main.model {
        ensure!(
            offered.models.iter().any(|c| &c.value == model),
            "reviewer {profile} does not advertise model {model}"
        );
    }
    if let Some(effort) = &main.effort {
        let for_model = match &main.model {
            Some(model) if offered.model.as_ref() != Some(model) => {
                offered_choices(catalog, profile, Some(model.clone()), cancelled).await?
            }
            _ => offered,
        };
        ensure!(
            (main.model.is_none() || for_model.model == main.model)
                && for_model.efforts.iter().any(|c| &c.value == effort),
            "reviewer {profile} does not advertise effort {effort} for {}",
            main.model.as_deref().unwrap_or("its default model")
        );
    }
    Ok(ResolvedReviewSettings {
        profile: profile.into(),
        main,
        automatic,
        price_unknown,
    })
}

/// Whether a refusal only says another operation held the session: a lease
/// refused the reviewer action, a lease taken meanwhile cancelled it, or the
/// worker turned reviewer work away during a checkpoint or replacement (the
/// recovery copy a finished turn starts, or an upgrade to the current build).
pub(crate) fn preempted_by_lifecycle(reason: &str) -> bool {
    reason.contains("session is reserved for a lifecycle operation")
        || reason.contains("cancelled for session lifecycle change")
        || reason.contains("worker is reserved for checkpoint or replacement")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_runtime::profile_catalog::test_config;
    use mj_client::quota::{API_LABEL, QuotaWindow};
    use mj_core::acp::SessionConfigChoice;
    use mj_core::config::HarnessKind;

    fn choices(ids: &[&str]) -> Vec<SessionConfigChoice> {
        ids.iter()
            .map(|id| SessionConfigChoice {
                value: (*id).into(),
                name: (*id).into(),
                description: None,
            })
            .collect()
    }

    fn offered(ids: &[&str]) -> ProfileConfig {
        ProfileConfig {
            model: ids.first().map(|id| (*id).into()),
            models: choices(ids),
            efforts: choices(&["low", "high"]),
            observed_at: 0,
        }
    }

    fn quota(id: &str, remaining: Option<u8>, api: bool) -> ProfileQuota {
        ProfileQuota {
            paid_credits: None,
            banked_resets: None,
            profile_id: id.into(),
            harness: HarnessKind::Codex,
            windows: remaining
                .map(|remaining| QuotaWindow {
                    label: "weekly".into(),
                    remaining_percent: Some(remaining),
                    duration_seconds: None,
                    model_scope: None,
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: None,
                })
                .into_iter()
                .collect(),
            extra: api.then(|| API_LABEL.into()),
            error: None,
            refreshed_at_epoch_seconds: 0,
            rate_limited_until_epoch_seconds: None,
        }
    }

    fn catalog(
        entries: BTreeMap<String, ProfileConfig>,
        no_high: Vec<String>,
    ) -> (Config, Arc<ProfileCatalog>) {
        let profiles = entries
            .keys()
            .map(|id| (id.as_str(), HarnessKind::Dsh))
            .collect::<Vec<_>>();
        let config = test_config(&profiles);
        let entries = Arc::new(entries);
        let initial = entries.clone();
        let initial_no_high = no_high.clone();
        let catalog = ProfileCatalog::with_probes(
            Arc::new(move |id| {
                let mut result = initial[&id].clone();
                if result
                    .model
                    .as_ref()
                    .is_some_and(|model| initial_no_high.contains(model))
                {
                    result.efforts = choices(&["medium", "max"]);
                }
                Box::pin(async move { Ok(result) })
            }),
            Arc::new(move |id, model| {
                let mut result = entries[&id].clone();
                result.model = Some(model.clone());
                if no_high.contains(&model) {
                    result.efforts = choices(&["medium", "max"]);
                }
                Box::pin(async move { Ok(result) })
            }),
        );
        catalog.sync(&config);
        (config, catalog)
    }

    #[tokio::test]
    async fn acp_catalog_selection_orders_billing_then_model_diversity_then_newest_family() {
        let options: Vec<agent_client_protocol::v1::schema::SessionConfigOption> =
            serde_json::from_str(include_str!(
                "../tests/fixtures/review-acp-config-options.json"
            ))
            .unwrap();
        let terra = ProfileConfig {
            model: None,
            models: mj_core::acp::session_config_choices(&options, "model"),
            efforts: mj_core::acp::session_config_choices(&options, "effort"),
            observed_at: 0,
        };
        let mut haiku = offered(&["haiku"]);
        haiku.models[0].name = "Haiku 5.5".into();
        let (config, catalog) = catalog(
            BTreeMap::from([
                ("terra".into(), terra),
                (
                    "flash".into(),
                    offered(&["deepseek-v4-pro", "deepseek-flash", "deepseek-v4-flash"]),
                ),
                ("haiku".into(), haiku),
                ("grok".into(), offered(&["grok-4.6", "grok-4.7"])),
            ]),
            vec![],
        );
        // Quota headroom no longer changes family priority.
        let sub = BTreeMap::from([
            ("terra".into(), quota("terra", Some(1), false)),
            ("flash".into(), quota("flash", Some(100), false)),
        ]);
        for (primary, expected_profile, expected_model) in [
            (None, "terra", "gpt-6.10-terra"),
            (
                Some(("terra", "gpt-6.9-terra")),
                "flash",
                "deepseek-v4-flash",
            ),
            (Some(("flash", "deepseek-flash")), "terra", "gpt-6.10-terra"),
        ] {
            let selected = resolve_auto(
                Some(&catalog),
                &config,
                &sub,
                &ReviewConfig::default(),
                primary,
                &AtomicBool::new(false),
            )
            .await
            .unwrap();
            assert_eq!(
                (selected.profile.as_str(), selected.main.model.as_deref()),
                (expected_profile, Some(expected_model))
            );
            assert_eq!(selected.main.effort.as_deref(), Some("high"));
        }
        let all_api = config
            .profiles
            .keys()
            .map(|id| (id.clone(), quota(id, None, true)))
            .collect::<BTreeMap<_, _>>();
        let mut billing = all_api.clone();
        billing.insert("grok".into(), quota("grok", Some(1), false));
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &billing,
            &ReviewConfig::default(),
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(
            selected.profile, "grok",
            "subscription Grok precedes API Terra"
        );
        billing.insert("terra".into(), quota("terra", Some(1), false));
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &billing,
            &ReviewConfig::default(),
            Some(("grok", "grok-4.7")),
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(selected.profile, "terra");
        billing.insert("grok".into(), quota("grok", None, true));
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &billing,
            &ReviewConfig::default(),
            Some(("terra", "gpt-6.10-terra")),
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(
            selected.profile, "terra",
            "subscription wins even when it duplicates the primary family"
        );
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &all_api,
            &ReviewConfig::default(),
            Some(("terra", "gpt-6.10-terra")),
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(selected.profile, "flash");
    }

    #[tokio::test]
    async fn catalog_efforts_and_model_scoped_quota_decide_availability_without_weakening_high() {
        let (config, catalog) = catalog(
            BTreeMap::from([
                ("terra".into(), offered(&["gpt-6.1-terra"])),
                (
                    "multi".into(),
                    offered(&[
                        "deepseek-v4-pro",
                        "deepseek-v4-flash",
                        "claude-haiku-5-5",
                        "grok-4.7",
                    ]),
                ),
            ]),
            vec!["gpt-6.1-terra".into()],
        );
        let mut report = quota("multi", Some(0), false);
        report.windows[0].model_scope = Some("flash".into());
        let quotas = BTreeMap::from([("multi".into(), report.clone())]);
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &quotas,
            &ReviewConfig::default(),
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(selected.main.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(selected.main.effort.as_deref(), Some("high"));
        // Unknown quota stays eligible, even after a failed reading.
        report.error = Some("provider unavailable".into());
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::from([("multi".into(), report)]),
            &ReviewConfig::default(),
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(selected.main.model.as_deref(), Some("deepseek-v4-flash"));
        let settings = ReviewConfig {
            model: Some("deepseek-v4-flash".into()),
            effort: Some("low".into()),
            ..Default::default()
        };
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::new(),
            &settings,
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(
            selected.main.effort.as_deref(),
            Some("high"),
            "a session effort cannot weaken Auto"
        );
    }

    #[tokio::test]
    async fn fallback_uses_shared_prices_and_keeps_unknown_distinct_from_free() {
        let (config, catalog) = catalog(
            BTreeMap::from([(
                "other".into(),
                offered(&[
                    "unknown-expensive-model",
                    "kimi-k3",
                    "glm-5.3-flash",
                    "gpt-6-luna",
                ]),
            )]),
            vec![],
        );
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::new(),
            &ReviewConfig::default(),
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(selected.main.model.as_deref(), Some("gpt-6-luna"));
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::new(),
            &ReviewConfig::default(),
            Some(("other", "gpt-6-luna")),
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(selected.main.model.as_deref(), Some("glm-5.3-flash"));
        let settings = ReviewConfig {
            model: Some("unknown-expensive-model".into()),
            ..Default::default()
        };
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::new(),
            &settings,
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(
            selected.main.model.as_deref(),
            Some("unknown-expensive-model")
        );
        let (config, catalog) = self::catalog(
            BTreeMap::from([("unpriced".into(), offered(&["unknown-expensive-model"]))]),
            vec![],
        );
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::new(),
            &ReviewConfig::default(),
            None,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert!(selected.description().contains("price unavailable"));
        let (config, catalog) = self::catalog(
            BTreeMap::from([("last-priced".into(), offered(&["kimi-k3", "unknown-model"]))]),
            vec![],
        );
        let selected = resolve_auto(
            Some(&catalog),
            &config,
            &BTreeMap::new(),
            &ReviewConfig::default(),
            Some(("last-priced", "kimi-k3")),
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(
            selected.main.model.as_deref(),
            Some("kimi-k3"),
            "avoiding the primary cannot promote an unknown price ahead of a known one"
        );
    }

    // Hard-won: 3eefab64: a broken unknown profile delayed a pinned model already offered by a ready profile
    #[tokio::test]
    async fn a_confirmed_pinned_model_does_not_wait_for_unrelated_discovery() {
        let config = test_config(&[("known", HarnessKind::Dsh), ("unknown", HarnessKind::Dsh)]);
        let catalog = ProfileCatalog::with_probes(
            Arc::new(|id| {
                Box::pin(async move {
                    if id == "unknown" {
                        std::future::pending::<()>().await;
                    }
                    Ok(offered(&["gpt-6.1-terra"]))
                })
            }),
            Arc::new(|_, model| Box::pin(async move { Ok(offered(&[&model])) })),
        );
        catalog.sync(&config);
        offered_choices(Some(&catalog), "known", None, &AtomicBool::new(false))
            .await
            .unwrap();
        let settings = ReviewConfig {
            model: Some("gpt-6.1-terra".into()),
            ..Default::default()
        };
        let selected = tokio::time::timeout(
            Duration::from_secs(2),
            resolve_auto(
                Some(&catalog),
                &config,
                &BTreeMap::new(),
                &settings,
                None,
                &AtomicBool::new(false),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(selected.profile, "known");
    }
}

#[cfg(test)]
mod preemption_tests {
    use super::preempted_by_lifecycle;

    /// The worker's refusal while a finished turn's recovery copy or a build
    /// upgrade holds it is about the session, not the reviewer candidate, so
    /// an Auto choice waits and asks again instead of skipping every
    /// candidate (Series 34 #3444, 2026-10-03: four Codex profiles refused in
    /// five seconds, then "No usable Auto reviewer").
    // Hard-won: bc774743: checkpoint preparation made Auto skip every profile and report no usable reviewer
    #[test]
    fn a_worker_reserved_for_checkpoint_is_a_lifecycle_preemption() {
        let refusal = "relay 2.28.0 could not perform reviewer_start: relay rejected request \
                       (InvalidState): worker is reserved for checkpoint or replacement; \
                       reviewer work was not admitted";
        assert!(preempted_by_lifecycle(refusal));
        assert!(preempted_by_lifecycle(
            "session is reserved for a lifecycle operation"
        ));
        assert!(!preempted_by_lifecycle(
            "does not advertise model gpt-6-luna"
        ));
    }
}
