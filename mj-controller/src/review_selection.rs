//! One resolver for turn review and plan turn review.
use crate::controller::Controller;
use crate::server_runtime::profile_catalog::ProfileCatalog;
use crate::session_manager::ManagedSessionHandle;
use crate::utility_llm::{UtilityLlmRuntime, UtilityQuotaClass, classify_quota};
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{Config, HarnessProfile, ReviewConfig};
use mj_core::profile_capabilities::ProfileCapabilitiesSnapshot;
use mj_core::review::settings::{
    ResolvedReviewSettings, ReviewModelSettings, ReviewProvider, model_matches_family,
    model_version_cmp,
};
use mj_core::worker_launch::ProfileConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Debug)]
struct Candidate {
    id: String,
    provider: ReviewProvider,
    group: u8,
    quota: UtilityQuotaClass,
    score: u8,
    /// The daemon's catalog lists the pinned model for this profile, so trying
    /// it first needs no discovery to rule it out.
    advertised: bool,
}

/// What the daemon's profile catalog says about a pinned model, per profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogVerdict {
    /// The catalog lists the model: try this profile first.
    Offers,
    /// The catalog is ready and does not list the model: skip without staging.
    Lacks,
    /// No ready catalog entry: discovery has to decide, after the known ones.
    Unknown,
}

/// Judges one profile against a pinned model using the daemon's catalog, so
/// Auto never stages or launches a profile only to learn it lacks the model
/// (2026-10-02: a DeepSeek session asking for gpt-6-luna staged eight other
/// profiles, then retried a broken Muse discovery every minute, and its
/// review never started).
fn catalog_verdict(
    offered: Option<&mj_core::profile_capabilities::ProfileCapabilitiesSnapshot>,
    key: &str,
    model: &str,
) -> CatalogVerdict {
    use mj_core::profile_capabilities::CapabilityState;
    match offered
        .and_then(|snapshot| snapshot.profiles.get(key))
        .map(|entry| &entry.choices)
    {
        Some(CapabilityState::Ready(choices)) => {
            if choices.models.iter().any(|choice| choice.value == model) {
                CatalogVerdict::Offers
            } else {
                CatalogVerdict::Lacks
            }
        }
        _ => CatalogVerdict::Unknown,
    }
}

/// Each enabled profile's review provider, and why any could not be
/// inspected. This reads harness configuration, so call it off an actor loop.
pub(crate) fn inspect_providers(
    config: &Config,
) -> (BTreeMap<String, ReviewProvider>, Vec<String>) {
    let mut providers = BTreeMap::new();
    let mut reasons = Vec::new();
    for (id, profile) in config.enabled_profiles() {
        match ReviewProvider::for_profile(profile) {
            Ok(provider) => {
                providers.insert(id.to_owned(), provider);
            }
            Err(error) => reasons.push(format!("{id}: could not inspect provider: {error:#}")),
        }
    }
    (providers, reasons)
}

/// The enabled profiles Auto may try, before quota is read.
#[derive(Debug, Default)]
pub(crate) struct AutoProfiles<'a> {
    /// In configuration order.
    pub(crate) supported: Vec<(&'a str, &'a HarnessProfile)>,
    /// Profiles the catalog confirms offer the named model.
    pub(crate) advertised: BTreeSet<String>,
    /// Enabled profiles Auto has no model policy for, while no model is named.
    pub(crate) excluded: Vec<String>,
}

/// Judges each enabled profile by [`ReviewProvider::auto_eligible`], the
/// rule the launch's review baseline also applies, and then by the catalog
/// when a model is named. A named model can come from any enabled profile
/// that offers it, not only from providers Auto has a policy for. A profile
/// missing from `providers` could not be inspected, and its reason is
/// already recorded.
pub(crate) fn auto_profiles<'a>(
    config: &'a Config,
    providers: &BTreeMap<String, ReviewProvider>,
    settings: &ReviewConfig,
    offered: Option<&ProfileCapabilitiesSnapshot>,
    reasons: &mut Vec<String>,
) -> AutoProfiles<'a> {
    let mut auto = AutoProfiles::default();
    for (id, profile) in config.enabled_profiles() {
        let Some(provider) = providers.get(id) else {
            continue;
        };
        if !provider.auto_eligible(settings) {
            auto.excluded.push(id.to_owned());
            continue;
        }
        if let Some(model) = settings.model.as_deref() {
            match catalog_verdict(offered, &profile.capabilities_key(id), model) {
                CatalogVerdict::Offers => {
                    auto.advertised.insert(id.to_owned());
                }
                CatalogVerdict::Lacks => {
                    reasons.push(format!("{id}: does not advertise model {model}"));
                    continue;
                }
                CatalogVerdict::Unknown => {}
            }
        }
        auto.supported.push((id, profile));
    }
    auto
}

/// Usage-priced (API) profiles bill each request, so they come after every
/// subscription profile. Then profiles the catalog confirms come first;
/// unknown ones are tried only if they all fail.
fn rank(candidates: &mut [Candidate]) {
    let usage_priced = |candidate: &Candidate| candidate.quota == UtilityQuotaClass::UsagePriced;
    candidates.sort_by(|a, b| {
        usage_priced(a)
            .cmp(&usage_priced(b))
            .then_with(|| b.advertised.cmp(&a.advertised))
            .then_with(|| a.group.cmp(&b.group))
            .then_with(|| b.quota.cmp(&a.quota))
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// `catalog` is the daemon's profile catalog. A chat view outside the daemon
/// has none and reads the same cache through [`offered_choices`].
pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
    cancelled: Arc<AtomicBool>,
    catalog: Option<Arc<ProfileCatalog>>,
) -> Result<ResolvedReviewSettings> {
    let offered = catalog.as_ref().map(|catalog| catalog.snapshot());
    let controller = tokio::task::spawn_blocking(Controller::load).await??;
    let settings = settings.unwrap_or_else(|| controller.config.review.clone());
    let session = controller
        .state
        .sessions
        .get(handle.session_id())
        .context("review session is missing")?;
    ensure!(!session.archived, "this session is archived");
    let primary = session.last_profile.clone();
    let config = controller.config.clone();
    // Provider inspection reads harness configuration, so never do it on an actor loop.
    let (providers, primary_provider, primary, mut reasons) =
        tokio::task::spawn_blocking(move || {
            let primary_provider = config
                .profiles
                .get(&primary)
                .and_then(|profile| ReviewProvider::for_profile(profile).ok())
                .unwrap_or(ReviewProvider::Other);
            let (providers, reasons) = inspect_providers(&config);
            (providers, primary_provider, primary, reasons)
        })
        .await?;
    let mut candidates = Vec::new();
    let turned_away = if let Some(id) = &settings.profile {
        let provider = providers.get(id).with_context(|| {
            format!(
                "reviewer profile {id:?} is missing, disabled, or unavailable: {}",
                reasons.join("; ")
            )
        })?;
        candidates.push(Candidate {
            id: id.clone(),
            provider: *provider,
            group: 0,
            quota: UtilityQuotaClass::Unknown,
            score: 0,
            advertised: true,
        });
        Vec::new()
    } else {
        let AutoProfiles {
            supported,
            advertised,
            excluded,
        } = auto_profiles(
            &controller.config,
            &providers,
            &settings,
            offered.as_deref(),
            &mut reasons,
        );
        let quotas = UtilityLlmRuntime::shared()
            .quotas(&controller.config, &supported)
            .await;
        for (id, _) in supported {
            let provider = providers[id];
            let Some((quota, score)) = quotas
                .get(id)
                .map(classify_quota)
                .unwrap_or(Some((UtilityQuotaClass::Unknown, 0)))
            else {
                reasons.push(format!("{id}: quota is exhausted"));
                continue;
            };
            candidates.push(Candidate {
                id: id.to_owned(),
                provider,
                group: if id == primary {
                    2
                } else {
                    u8::from(provider == primary_provider)
                },
                quota,
                score,
                advertised: advertised.contains(id),
            });
        }
        rank(&mut candidates);
        excluded
    };
    for candidate in candidates {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "review preparation cancelled"
        );
        let result = resolve_candidate(catalog.as_ref(), &candidate, &settings, &cancelled).await;
        match result {
            Ok(mut resolved) => {
                resolved.same_provider = candidate.provider == primary_provider;
                return Ok(resolved);
            }
            Err(error) if settings.profile.is_some() => return Err(error),
            Err(error) => {
                tracing::warn!(profile = %candidate.id, %error, "Auto reviewer candidate unavailable");
                reasons.push(format!("{}: {error:#}", candidate.id));
            }
        }
    }
    if reasons.is_empty() {
        // Nothing qualified and nothing failed: every enabled profile, if
        // any, lacks an Auto policy and the session named no model.
        reasons.push(match turned_away.first() {
            None => "Auto chooses only Codex, Claude, Kimi, or DeepSeek-through-Codex profiles; enable one, or select another profile, such as DSH, as the reviewer in Settings".into(),
            Some(example) => format!(
                "Auto chooses only Codex, Claude, Kimi, or DeepSeek-through-Codex profiles, and no enabled profile is one of those (enabled: {}); enable one, or select another profile, such as {example}, as the reviewer in Settings",
                turned_away.join(", ")
            ),
        });
    }
    bail!("No usable Auto reviewer: {}", reasons.join("; "))
}

/// The models and efforts `profile` offers, with the efforts of `model` (or
/// of its default model), from the daemon's profile catalog: the same cached
/// answer `mj models` and the profile configuration endpoint give, so Auto
/// never judges a profile by different facts (#1320). Without the daemon's
/// catalog, the cached discovery that catalog is built on answers.
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

fn family_model(offered: &ProfileConfig, family: &str) -> Result<String> {
    offered
        .models
        .iter()
        .filter(|c| model_matches_family(&c.value, family))
        .max_by(|a, b| model_version_cmp(&a.value, &b.value))
        .map(|c| c.value.clone())
        .with_context(|| format!("no advertised {family} model"))
}

fn select_models(
    provider: ReviewProvider,
    settings: &ReviewConfig,
    offered: &ProfileConfig,
) -> Result<ReviewModelSettings> {
    let automatic = settings.profile.is_none();
    let main = if automatic {
        // Auto takes its model and effort from the provider's policy, except
        // where the session or `[review]` names one.
        let policy = provider.main_policy();
        let model = match &settings.model {
            Some(model) => {
                ensure!(
                    offered.models.iter().any(|c| &c.value == model),
                    "does not advertise model {model}"
                );
                model.clone()
            }
            None => family_model(offered, policy.context("provider is manual-only")?.0)?,
        };
        ReviewModelSettings {
            model: Some(model),
            effort: settings
                .effort
                .clone()
                .or_else(|| policy.map(|(_, effort)| effort.into())),
            fast_mode: false,
        }
    } else {
        ReviewModelSettings {
            model: settings.model.clone(),
            effort: settings.effort.clone(),
            fast_mode: false,
        }
    };
    Ok(main)
}

async fn resolve_candidate(
    catalog: Option<&Arc<ProfileCatalog>>,
    candidate: &Candidate,
    settings: &ReviewConfig,
    cancelled: &AtomicBool,
) -> Result<ResolvedReviewSettings> {
    let automatic = settings.profile.is_none();
    let profile = candidate.id.as_str();
    let offered = offered_choices(catalog, profile, None, cancelled).await?;
    let main = select_models(candidate.provider, settings, &offered)?;
    if let Some(model) = &main.model {
        ensure!(
            offered.models.iter().any(|c| &c.value == model),
            "reviewer {profile} does not advertise model {model}"
        );
    }
    if let Some(effort) = &main.effort {
        // Efforts belong to one model. The profile's own entry lists those of
        // its default model; any other model has an entry of its own.
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
        profile: candidate.id.clone(),
        main,
        automatic,
        same_provider: false,
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
    fn candidate(
        id: &str,
        provider: ReviewProvider,
        group: u8,
        quota: UtilityQuotaClass,
        score: u8,
    ) -> Candidate {
        Candidate {
            id: id.into(),
            provider,
            group,
            quota,
            score,
            advertised: false,
        }
    }
    fn snapshot(
        entries: &[(
            &str,
            mj_core::profile_capabilities::CapabilityState<Vec<&str>>,
        )],
    ) -> mj_core::profile_capabilities::ProfileCapabilitiesSnapshot {
        use mj_core::profile_capabilities::{CapabilityState, ProfileCapabilities};
        mj_core::profile_capabilities::ProfileCapabilitiesSnapshot {
            profiles: entries
                .iter()
                .map(|(key, state)| {
                    let choices = match state {
                        CapabilityState::Pending => CapabilityState::Pending,
                        CapabilityState::Failed(error) => CapabilityState::Failed(error.clone()),
                        CapabilityState::Ready(models) => CapabilityState::Ready(ProfileConfig {
                            model: None,
                            models: choices(models),
                            efforts: Vec::new(),
                            observed_at: 0,
                        }),
                    };
                    (
                        (*key).to_owned(),
                        ProfileCapabilities {
                            choices,
                            efforts: Default::default(),
                        },
                    )
                })
                .collect(),
        }
    }

    // Hard-won: 3eefab64: Auto staged unrelated profiles and credentials before rejecting models they could not run
    #[test]
    fn a_pinned_model_rules_profiles_in_or_out_from_the_catalog_alone() {
        use mj_core::profile_capabilities::CapabilityState;
        let offered = snapshot(&[
            (
                "codex3",
                CapabilityState::Ready(vec!["gpt-6-luna", "gpt-6.1-sol"]),
            ),
            ("kimi", CapabilityState::Ready(vec!["k3", "k3-256k"])),
            (
                "muse",
                CapabilityState::Failed("discovery harness stopped".into()),
            ),
            ("claude", CapabilityState::Pending),
        ]);
        let verdict = |key| catalog_verdict(Some(&offered), key, "gpt-6-luna");
        assert_eq!(verdict("codex3"), CatalogVerdict::Offers);
        // A ready catalog without the model is skipped before anything is staged.
        assert_eq!(verdict("kimi"), CatalogVerdict::Lacks);
        // Failed, pending or absent entries leave the decision to discovery.
        assert_eq!(verdict("muse"), CatalogVerdict::Unknown);
        assert_eq!(verdict("claude"), CatalogVerdict::Unknown);
        assert_eq!(verdict("glm"), CatalogVerdict::Unknown);
        assert_eq!(
            catalog_verdict(None, "codex3", "gpt-6-luna"),
            CatalogVerdict::Unknown
        );
    }

    // Hard-won: 3eefab64: Auto tried a failing unknown profile before the known profile that offered the pinned model
    #[test]
    fn confirmed_profiles_are_tried_before_unknown_ones_and_api_profiles_last() {
        let mut candidates = vec![
            candidate(
                "muse",
                ReviewProvider::Other,
                0,
                UtilityQuotaClass::Unknown,
                0,
            ),
            Candidate {
                advertised: true,
                ..candidate(
                    "codex3",
                    ReviewProvider::Codex,
                    1,
                    UtilityQuotaClass::Unknown,
                    0,
                )
            },
            candidate(
                "glm",
                ReviewProvider::Other,
                0,
                UtilityQuotaClass::Unknown,
                0,
            ),
            Candidate {
                advertised: true,
                ..candidate(
                    "api",
                    ReviewProvider::Other,
                    0,
                    UtilityQuotaClass::UsagePriced,
                    100,
                )
            },
        ];
        rank(&mut candidates);
        assert_eq!(
            candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["codex3", "glm", "muse", "api"]
        );
    }

    #[test]
    fn review_selection_uses_one_model_and_effort_from_an_acp_config_fixture() {
        // ACP v1 documents session options as id/name/type/currentValue/options.
        let options: Vec<agent_client_protocol::schema::v1::SessionConfigOption> =
            serde_json::from_str(include_str!(
                "../tests/fixtures/review-acp-config-options.json"
            ))
            .expect("ACP v1 session config options");
        let offered = ProfileConfig {
            model: None,
            models: mj_core::acp::session_config_choices(&options, "model"),
            efforts: mj_core::acp::session_config_choices(&options, "effort"),
            observed_at: 0,
        };

        let reviewer = select_models(ReviewProvider::Codex, &ReviewConfig::default(), &offered)
            .expect("select the advertised Codex review model");

        assert_eq!(reviewer.model.as_deref(), Some("gpt-6.10-astra"));
        assert_eq!(reviewer.effort.as_deref(), Some("medium"));
        assert!(!reviewer.fast_mode);
    }

    /// A DeepSeek-backed Codex profile as the catalog holds it: the default
    /// entry describes `deepseek-v4-pro`, and each model has its own efforts.
    fn deepseek_catalog() -> Arc<ProfileCatalog> {
        use crate::server_runtime::profile_catalog::test_config;
        let offered = |model: &str| ProfileConfig {
            model: Some(model.to_owned()),
            models: choices(&["deepseek-flash", "deepseek-v4-pro"]),
            efforts: choices(if model == "deepseek-flash" {
                &["low", "high", "max"]
            } else {
                &["high"]
            }),
            observed_at: 0,
        };
        let catalog = ProfileCatalog::with_probes(
            Arc::new(move |_| Box::pin(async move { Ok(offered("deepseek-v4-pro")) })),
            Arc::new(move |_, model| Box::pin(async move { Ok(offered(&model)) })),
        );
        catalog.sync(&test_config(&[(
            "deepseek",
            mj_core::config::HarnessKind::Codex,
        )]));
        catalog
    }

    fn deepseek() -> Candidate {
        Candidate {
            advertised: true,
            ..candidate(
                "deepseek",
                ReviewProvider::DeepSeek,
                1,
                UtilityQuotaClass::Unknown,
                0,
            )
        }
    }

    // Hard-won: #1320: Auto read a reviewer staged in the session's worker instead of the profile catalog, so a DeepSeek profile "did not advertise" the model every other surface listed
    #[tokio::test]
    async fn auto_takes_a_pinned_model_and_its_effort_from_the_profile_catalog() {
        let catalog = deepseek_catalog();
        let settings = ReviewConfig {
            model: Some("deepseek-flash".into()),
            effort: Some("max".into()),
            ..ReviewConfig::default()
        };

        let resolved = resolve_candidate(
            Some(&catalog),
            &deepseek(),
            &settings,
            &AtomicBool::new(false),
        )
        .await
        .expect("the catalog offers deepseek-flash at max");

        assert_eq!(resolved.profile, "deepseek");
        assert_eq!(resolved.main.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(resolved.main.effort.as_deref(), Some("max"));
        assert!(resolved.automatic);
    }

    #[tokio::test]
    async fn an_effort_is_judged_by_the_pinned_models_own_entry_not_the_default_models() {
        let catalog = deepseek_catalog();
        // `max` exists for deepseek-flash only; the default entry, for
        // deepseek-v4-pro, lists `high` alone.
        let refused = resolve_candidate(
            Some(&catalog),
            &deepseek(),
            &ReviewConfig {
                model: Some("deepseek-v4-pro".into()),
                effort: Some("max".into()),
                ..ReviewConfig::default()
            },
            &AtomicBool::new(false),
        )
        .await
        .expect_err("deepseek-v4-pro offers no max effort");
        assert_eq!(
            format!("{refused:#}"),
            "reviewer deepseek does not advertise effort max for deepseek-v4-pro"
        );
    }

    fn choices(ids: &[&str]) -> Vec<mj_core::acp::SessionConfigChoice> {
        ids.iter()
            .map(|id| mj_core::acp::SessionConfigChoice {
                value: (*id).into(),
                name: (*id).into(),
                description: None,
            })
            .collect()
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
