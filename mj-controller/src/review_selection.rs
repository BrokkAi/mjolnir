//! Resolve every reviewer through the daemon's shared profile candidate service.
use crate::controller::Controller;
use crate::server::api::{
    SubagentBackend, resolve_pinned_reviewer_model_profile_selection,
    resolve_reviewer_model_profile_selection,
};
use crate::session_manager::ManagedSessionHandle;
use anyhow::{Context, Result, ensure};
use mj_core::config::{
    LegacyReview, ReviewConfig, SessionReview, SubagentChoice, apply_legacy_override,
};
use mj_core::review::autoconfigure::autoconfigure;
use mj_core::review::settings::{ResolvedReviewSettings, ReviewModelSettings};
use mj_core::worker_launch::ProfileConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
    cancelled: Arc<AtomicBool>,
    backend: Arc<dyn SubagentBackend>,
) -> Result<Vec<ResolvedReviewSettings>> {
    let controller = tokio::task::spawn_blocking(Controller::load).await??;
    let settings = settings.unwrap_or_else(|| controller.config.review.clone());
    let session = controller
        .state
        .sessions
        .get(handle.session_id())
        .context("review session is missing")?;
    ensure!(!session.archived, "this session is archived");
    let (mut choices, autoconfigured) =
        configured_choices(settings.clone(), &controller.config, &cancelled, &backend).await?;
    if autoconfigured
        && let Some(SessionReview::Legacy {
            count,
            model,
            effort,
            ..
        }) = session.review.as_ref()
    {
        choices =
            apply_legacy_override(&choices, *count, model.as_ref(), effort.as_ref(), &settings);
    }
    let pinned_profile = if autoconfigured {
        settings
            .legacy
            .as_ref()
            .and_then(|legacy| legacy.profile.as_ref())
            .filter(|profile| controller.config.enabled_profile(profile).is_some())
    } else {
        None
    };
    let mut resolved = Vec::with_capacity(choices.len());
    for SubagentChoice { model, effort } in choices {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "review preparation cancelled"
        );
        let selected = await_or_cancel(&cancelled, async {
            if let Some(profile) = pinned_profile {
                resolve_pinned_reviewer_model_profile_selection(
                    &backend,
                    profile,
                    &model,
                    effort.as_deref(),
                )
                .await
            } else {
                resolve_reviewer_model_profile_selection(&backend, &model, effort.as_deref()).await
            }
        })
        .await?
        .map_err(|error| anyhow::anyhow!("reviewer model {model:?}: {}", error.message))?;
        resolved.push(ResolvedReviewSettings {
            profile: selected.profile_id,
            main: ReviewModelSettings {
                model: Some(selected.model),
                effort: selected.effort,
                fast_mode: selected.fast_mode,
            },
            autoconfigured,
        });
    }
    Ok(resolved)
}

async fn configured_choices(
    settings: ReviewConfig,
    config: &mj_core::config::Config,
    cancelled: &AtomicBool,
    backend: &Arc<dyn SubagentBackend>,
) -> Result<(Vec<SubagentChoice>, bool)> {
    let autoconfigured = settings.is_legacy();
    let choices = if autoconfigured {
        let pinned = settings
            .legacy
            .as_ref()
            .and_then(|legacy| legacy.profile.as_ref())
            .filter(|profile| config.enabled_profile(profile).is_some());
        let ready =
            await_or_cancel(cancelled, backend.reviewer_ready_candidates(String::new())).await??;
        let candidates = if let Some(profile) = pinned {
            if let Some(candidate) = ready
                .offered
                .into_iter()
                .find(|candidate| &candidate.profile_id == profile)
            {
                vec![(candidate.profile_id, candidate.choices)]
            } else {
                let discovery = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    backend.profile_config(profile.clone(), None, false),
                );
                let choices = await_or_cancel(cancelled, discovery)
                    .await?
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "review profile {profile:?} did not become available within 30 seconds"
                        )
                    })?
                    .map_err(|error| {
                        anyhow::anyhow!("review profile {profile:?} is unavailable: {error:#}")
                    })?;
                vec![(profile.clone(), choices)]
            }
        } else {
            let candidates = if ready.offered.is_empty() {
                await_or_cancel(cancelled, backend.reviewer_candidates()).await??
            } else {
                ready
            };
            candidates
                .offered
                .into_iter()
                .map(|candidate| (candidate.profile_id, candidate.choices))
                .collect::<Vec<_>>()
        };
        let available = candidates;
        if let Some(profile) = pinned
            && available
                .iter()
                .all(|(_, choices)| choices.model.is_none() && choices.models.is_empty())
        {
            anyhow::bail!("review profile {profile:?} has no available reviewer model");
        }
        let (choices, _) = upgrade_reviewers(
            settings.legacy.as_ref().expect("legacy review"),
            &available,
            |profile, model| {
                let backend = Arc::clone(backend);
                async move { backend.profile_config(profile, Some(model), false).await }
            },
            Some(cancelled),
        )
        .await?;
        choices
    } else {
        settings.reviewers
    };
    ensure!(
        !choices.is_empty(),
        "No reviewers are configured; choose reviewers or press Autoconfigure in Settings › Code Review."
    );
    Ok((choices, autoconfigured))
}

pub(crate) async fn upgrade_reviewers<F, Fut>(
    legacy: &LegacyReview,
    available: &[(String, ProfileConfig)],
    probe: F,
    cancelled: Option<&AtomicBool>,
) -> Result<(Vec<SubagentChoice>, bool)>
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ProfileConfig>>,
{
    if let Some((_, choices)) = available
        .iter()
        .find(|(id, _)| legacy.profile.as_ref() == Some(id))
        && let Some(model) = choices
            .model
            .clone()
            .or_else(|| choices.models.first().map(|item| item.value.clone()))
    {
        let efforts = if choices.model.as_deref() == Some(&model) {
            Some(choices.efforts.clone())
        } else {
            let discovery = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                probe(legacy.profile.clone().unwrap(), model.clone()),
            );
            let result = match cancelled {
                Some(cancelled) => await_or_cancel(cancelled, discovery).await?,
                None => discovery.await,
            };
            result
                .ok()
                .and_then(Result::ok)
                .map(|choices| choices.efforts)
        };
        let effort = legacy
            .effort
            .as_ref()
            .filter(|effort| {
                efforts
                    .as_ref()
                    .is_some_and(|items| items.iter().any(|item| &item.value == *effort))
            })
            .cloned();
        let count = legacy
            .count
            .map_or(2, |count| count.get())
            .clamp(1, mj_core::config::MAX_REVIEWERS);
        return Ok((
            vec![SubagentChoice { model, effort }; count],
            efforts.is_some(),
        ));
    }
    autoconfigure_with_efforts(available, probe, cancelled).await
}

/// Compute the ordered legacy Auto preset using the same model and effort
/// discovery for the daemon upgrade and the startup review fallback.
pub(crate) async fn autoconfigure_with_efforts<F, Fut>(
    available: &[(String, ProfileConfig)],
    probe: F,
    cancelled: Option<&AtomicBool>,
) -> Result<(Vec<SubagentChoice>, bool)>
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ProfileConfig>>,
{
    let models = available
        .iter()
        .flat_map(|(_, choices)| choices.models.iter().cloned())
        .collect::<Vec<_>>();
    let draft = autoconfigure(&models, |_| None).unwrap_or_default();
    let mut efforts = BTreeMap::new();
    let mut known = true;
    for model in draft
        .iter()
        .map(|choice| &choice.model)
        .collect::<BTreeSet<_>>()
    {
        let mut union = Vec::<mj_core::acp::SessionConfigChoice>::new();
        let mut model_known = true;
        for (profile, offered) in available
            .iter()
            .filter(|(_, offered)| offered.models.iter().any(|choice| &choice.value == model))
        {
            let discovered = if offered.model.as_deref() == Some(model) {
                Some(offered.efforts.clone())
            } else {
                let discovery = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    probe(profile.clone(), model.clone()),
                );
                let result = match cancelled {
                    Some(cancelled) => await_or_cancel(cancelled, discovery).await?,
                    None => discovery.await,
                };
                match result {
                    Ok(Ok(choices))
                        if choices.models.iter().any(|choice| &choice.value == model) =>
                    {
                        Some(choices.efforts)
                    }
                    Ok(Ok(_)) => {
                        tracing::warn!(%profile, %model, "model-specific reviewer discovery omitted the requested model");
                        None
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%profile, %error, "reviewer effort discovery failed during legacy Auto configuration");
                        None
                    }
                    Err(_) => {
                        tracing::warn!(%profile, %model, "reviewer effort discovery timed out during legacy Auto configuration");
                        None
                    }
                }
            };
            if let Some(discovered) = discovered {
                for effort in discovered {
                    if !union.iter().any(|item| item.value == effort.value) {
                        union.push(effort);
                    }
                }
            } else {
                known = false;
                model_known = false;
            }
        }
        if model_known {
            efforts.insert(model.clone(), union);
        }
    }
    Ok((
        autoconfigure(&models, |model| efforts.get(model).cloned()).unwrap_or_default(),
        known,
    ))
}

pub(crate) async fn await_or_cancel<T>(
    cancelled: &AtomicBool,
    work: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        value = work => Ok(value),
        _ = async {
            while !cancelled.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } => anyhow::bail!("review preparation cancelled"),
    }
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
    use mj_core::acp::SessionConfigChoice;

    #[tokio::test]
    async fn legacy_named_profile_uses_its_default_model_and_validates_effort() {
        let legacy = LegacyReview {
            count: std::num::NonZeroUsize::new(3),
            profile: Some("claude".into()),
            effort: Some("high".into()),
        };
        let available = vec![(
            "claude".into(),
            ProfileConfig {
                model: Some("haiku".into()),
                models: vec![SessionConfigChoice {
                    value: "haiku".into(),
                    name: "Haiku".into(),
                    description: None,
                }],
                efforts: vec![SessionConfigChoice {
                    value: "high".into(),
                    name: "High".into(),
                    description: None,
                }],
                observed_at: 1,
            },
        )];
        let (choices, known) =
            upgrade_reviewers(&legacy, &available, |_, _| async { unreachable!() }, None)
                .await
                .unwrap();
        assert!(known);
        assert_eq!(choices.len(), 3);
        assert!(
            choices
                .iter()
                .all(|choice| choice.model == "haiku" && choice.effort.as_deref() == Some("high"))
        );
    }

    #[tokio::test]
    async fn legacy_named_profile_with_unknown_efforts_cannot_persist() {
        let legacy = LegacyReview {
            count: None,
            profile: Some("claude".into()),
            effort: Some("high".into()),
        };
        let available = vec![(
            "claude".into(),
            ProfileConfig {
                model: None,
                models: vec![SessionConfigChoice {
                    value: "haiku".into(),
                    name: "Haiku".into(),
                    description: None,
                }],
                efforts: Vec::new(),
                observed_at: 1,
            },
        )];
        let (choices, known) = upgrade_reviewers(
            &legacy,
            &available,
            |_, _| async { anyhow::bail!("probe unavailable") },
            None,
        )
        .await
        .unwrap();
        assert!(!known);
        assert_eq!(choices.len(), 2);
        assert!(
            choices
                .iter()
                .all(|choice| choice.model == "haiku" && choice.effort.is_none())
        );
    }
    #[tokio::test]
    async fn legacy_auto_keeps_effort_offered_by_any_matching_profile() {
        let model = SessionConfigChoice {
            value: "claude-haiku-4.5".into(),
            name: "Haiku".into(),
            description: None,
        };
        let profile = |efforts: Vec<SessionConfigChoice>| ProfileConfig {
            model: Some(model.value.clone()),
            models: vec![model.clone()],
            efforts,
            observed_at: 1,
        };
        let available = vec![
            ("no-effort".into(), profile(Vec::new())),
            (
                "high-effort".into(),
                profile(vec![SessionConfigChoice {
                    value: "xhigh".into(),
                    name: "Extra high".into(),
                    description: None,
                }]),
            ),
        ];
        let (choices, known) =
            autoconfigure_with_efforts(&available, |_, _| async { unreachable!() }, None)
                .await
                .unwrap();
        assert!(known);
        assert!(
            choices
                .iter()
                .all(|choice| choice.effort.as_deref() == Some("xhigh"))
        );
    }

    #[tokio::test]
    async fn cancelling_pending_reviewer_discovery_releases_preparation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiting = {
            let cancelled = Arc::clone(&cancelled);
            tokio::spawn(
                async move { await_or_cancel(&cancelled, std::future::pending::<()>()).await },
            )
        };
        tokio::task::yield_now().await;
        cancelled.store(true, Ordering::Release);
        let error = tokio::time::timeout(std::time::Duration::from_millis(250), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("review preparation cancelled"));
    }
}
