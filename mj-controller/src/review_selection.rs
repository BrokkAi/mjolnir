//! One resolver for turn review and plan second opinion.
use crate::controller::Controller;
use crate::review_settings::{
    ReviewCapabilityChoices, ReviewDiscoveryOutcome, ReviewDiscoveryRequest,
};
use crate::session_manager::ManagedSessionHandle;
use crate::utility_llm::{UtilityLlmRuntime, UtilityQuotaClass, classify_quota};
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::ReviewConfig;
use mj_core::review::settings::{
    ResolvedReviewSettings, ReviewModelSettings, ReviewProvider, model_matches_family,
    model_version_cmp,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct Candidate {
    id: String,
    provider: ReviewProvider,
    group: u8,
    quota: UtilityQuotaClass,
    score: u8,
}

fn rank(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            .then_with(|| b.quota.cmp(&a.quota))
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.id.cmp(&b.id))
    });
}

pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
    specialists: bool,
    cancelled: Arc<AtomicBool>,
) -> Result<ResolvedReviewSettings> {
    let controller = Arc::new(tokio::task::spawn_blocking(Controller::load).await??);
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
            let mut providers = std::collections::BTreeMap::new();
            let mut reasons = Vec::new();
            for (id, profile) in config
                .enabled_profiles()
                .filter(|(_, p)| p.kind.supports_injected_mcp())
            {
                match ReviewProvider::for_profile(profile) {
                    Ok(provider) => {
                        providers.insert(id.to_owned(), provider);
                    }
                    Err(error) => {
                        reasons.push(format!("{id}: could not inspect provider: {error:#}"))
                    }
                }
            }
            (providers, primary_provider, primary, reasons)
        })
        .await?;
    let mut candidates = Vec::new();
    if let Some(id) = &settings.profile {
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
        });
    } else {
        ensure!(
            settings.model.is_none() && settings.effort.is_none(),
            "Auto uses fixed model and effort; select a named reviewer profile to override them"
        );
        let supported = controller
            .config
            .enabled_profiles()
            .filter(|(id, _)| providers.get(*id).and_then(|p| p.main_policy()).is_some())
            .collect::<Vec<_>>();
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
            });
        }
        rank(&mut candidates);
    }
    for candidate in candidates {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "review preparation cancelled"
        );
        let result = resolve_candidate(
            controller.clone(),
            &handle,
            &candidate,
            &settings,
            specialists,
            &cancelled,
        )
        .await;
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
        reasons.push("configure an enabled Codex, Claude, DeepSeek, or Kimi profile, or select a reviewer manually in Settings".into());
    }
    bail!("No usable Auto reviewer: {}", reasons.join("; "))
}

async fn discover(
    controller: Arc<Controller>,
    handle: &ManagedSessionHandle,
    profile: &str,
    model: Option<String>,
    cancelled: &Arc<AtomicBool>,
) -> Result<ReviewCapabilityChoices> {
    let (progress, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let request = ReviewDiscoveryRequest {
        profile: profile.into(),
        model,
        preferred_session: Some(handle.session_id().into()),
    };
    let result = crate::review_settings::discover_selected_worker(
        controller,
        handle.session_id().into(),
        crate::review_host::next_review_generation().map_err(anyhow::Error::msg)?,
        handle.clone(),
        &request,
        cancelled,
        &progress,
    )
    .await
    .map_err(anyhow::Error::msg)?;
    match result {
        ReviewDiscoveryOutcome::Available {
            choices,
            cleanup_warning,
        } => {
            if let Some(warning) = cleanup_warning {
                bail!("{warning}");
            }
            Ok(choices)
        }
        ReviewDiscoveryOutcome::Unavailable => bail!("review worker is unavailable"),
    }
}

fn family_model(choices: &ReviewCapabilityChoices, family: &str) -> Result<String> {
    choices
        .model_choices
        .iter()
        .filter(|c| model_matches_family(&c.value, family))
        .max_by(|a, b| model_version_cmp(&a.value, &b.value))
        .map(|c| c.value.clone())
        .with_context(|| format!("no advertised {family} model"))
}

async fn validate_model(
    controller: Arc<Controller>,
    handle: &ManagedSessionHandle,
    profile: &str,
    settings: &ReviewModelSettings,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let choices = discover(
        controller,
        handle,
        profile,
        settings.model.clone(),
        cancelled,
    )
    .await?;
    if let Some(model) = &settings.model {
        ensure!(
            choices.model_choices.iter().any(|c| &c.value == model),
            "reviewer {profile} does not advertise model {model}"
        );
    }
    if let Some(effort) = &settings.effort {
        ensure!(
            choices.effort_capabilities_discovered
                && choices.effort_choices.iter().any(|c| &c.value == effort),
            "reviewer {profile} does not advertise effort {effort} for {}",
            settings.model.as_deref().unwrap_or("its default model")
        );
    }
    Ok(())
}

fn select_models(
    provider: ReviewProvider,
    settings: &ReviewConfig,
    catalog: &ReviewCapabilityChoices,
    specialists: bool,
) -> Result<(ReviewModelSettings, ReviewModelSettings)> {
    let automatic = settings.profile.is_none();
    let main = if automatic {
        let (family, effort) = provider.main_policy().context("provider is manual-only")?;
        ReviewModelSettings {
            model: Some(family_model(catalog, family)?),
            effort: Some(effort.into()),
            fast_mode: false,
        }
    } else {
        ReviewModelSettings {
            model: settings.model.clone(),
            effort: settings.effort.clone(),
            fast_mode: false,
        }
    };
    let specialist = if specialists {
        if let Some((family, effort)) = provider.specialist_policy() {
            ReviewModelSettings {
                model: Some(family_model(catalog, family)?),
                effort: Some(effort.into()),
                fast_mode: provider == ReviewProvider::Codex,
            }
        } else {
            main.clone()
        }
    } else {
        main.clone()
    };
    Ok((main, specialist))
}

async fn resolve_candidate(
    controller: Arc<Controller>,
    handle: &ManagedSessionHandle,
    candidate: &Candidate,
    settings: &ReviewConfig,
    specialists: bool,
    cancelled: &Arc<AtomicBool>,
) -> Result<ResolvedReviewSettings> {
    let automatic = settings.profile.is_none();
    let catalog = discover(controller.clone(), handle, &candidate.id, None, cancelled).await?;
    let (main, specialist) = select_models(candidate.provider, settings, &catalog, specialists)?;
    if main.model.is_some() || main.effort.is_some() {
        validate_model(controller.clone(), handle, &candidate.id, &main, cancelled).await?;
    }
    if specialists && specialist != main {
        validate_model(
            controller.clone(),
            handle,
            &candidate.id,
            &specialist,
            cancelled,
        )
        .await?;
    }
    Ok(ResolvedReviewSettings {
        profile: candidate.id.clone(),
        generation: crate::review_host::next_review_generation().map_err(anyhow::Error::msg)?,
        main,
        specialist,
        automatic,
        same_provider: false,
    })
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
        }
    }
    #[test]
    fn auto_prefers_other_providers_then_quota_class_then_provider() {
        use ReviewProvider::*;
        use UtilityQuotaClass::*;
        let mut candidates = vec![
            candidate("primary", Codex, 2, Healthy, 100),
            candidate("other-codex", Codex, 1, Healthy, 100),
            candidate("claude-reserve", Claude, 0, Reserve, 10),
            candidate("deepseek", DeepSeek, 0, Healthy, 100),
            candidate("claude", Claude, 0, Healthy, 20),
            candidate("kimi-unknown", Kimi, 0, Unknown, 0),
        ];
        rank(&mut candidates);
        assert_eq!(
            candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            [
                "claude",
                "deepseek",
                "claude-reserve",
                "kimi-unknown",
                "other-codex",
                "primary"
            ]
        );
    }
    #[test]
    fn newest_family_model_uses_advertised_ids() {
        let choices = ReviewCapabilityChoices {
            model_choices: ["gpt-5.9-luna", "gpt-5.10-luna", "gpt-6-astra"]
                .map(|value| mj_core::acp::SessionConfigChoice {
                    value: value.into(),
                    name: value.into(),
                    description: None,
                })
                .into(),
            ..Default::default()
        };
        assert_eq!(family_model(&choices, "luna").unwrap(), "gpt-5.10-luna");
        assert!(family_model(&choices, "sonnet").is_err());
    }
    fn catalog(ids: &[&str]) -> ReviewCapabilityChoices {
        ReviewCapabilityChoices {
            model_choices: ids
                .iter()
                .map(|id| mj_core::acp::SessionConfigChoice {
                    value: (*id).into(),
                    name: (*id).into(),
                    description: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn manual_main_model_does_not_override_specialist_policy() {
        let config = ReviewConfig {
            profile: Some("work".into()),
            model: Some("gpt-6-astra".into()),
            effort: Some("high".into()),
            ..Default::default()
        };
        let (main, specialist) = select_models(
            ReviewProvider::Codex,
            &config,
            &catalog(&["gpt-5.9-luna", "gpt-5.10-luna"]),
            true,
        )
        .unwrap();
        assert_eq!(main.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(main.effort.as_deref(), Some("high"));
        assert_eq!(specialist.model.as_deref(), Some("gpt-5.10-luna"));
        assert_eq!(specialist.effort.as_deref(), Some("xhigh"));
        assert!(specialist.fast_mode);
        let (main, specialist) =
            select_models(ReviewProvider::Other, &config, &catalog(&[]), true).unwrap();
        assert_eq!(main, specialist);
    }

    #[test]
    fn auto_uses_newest_k_series_and_deepseek_efforts() {
        let (main, specialist) = select_models(
            ReviewProvider::Kimi,
            &ReviewConfig::default(),
            &catalog(&["kimi-code/k3", "kimi-code/k4", "kimi-latest"]),
            true,
        )
        .unwrap();
        assert_eq!(main.model.as_deref(), Some("kimi-code/k4"));
        assert_eq!(main.effort.as_deref(), Some("max"));
        assert_eq!(main, specialist);
        let (main, specialist) = select_models(
            ReviewProvider::DeepSeek,
            &ReviewConfig::default(),
            &catalog(&["deepseek-v4-flash", "deepseek-v5-flash", "deepseek-v6-pro"]),
            true,
        )
        .unwrap();
        assert_eq!(main.model.as_deref(), Some("deepseek-v5-flash"));
        assert_eq!(main.effort.as_deref(), Some("max"));
        assert_eq!(specialist.effort.as_deref(), Some("high"));
        assert!(!specialist.fast_mode);
        assert!(
            select_models(
                ReviewProvider::Other,
                &ReviewConfig::default(),
                &catalog(&[]),
                false
            )
            .is_err()
        );
    }
}
