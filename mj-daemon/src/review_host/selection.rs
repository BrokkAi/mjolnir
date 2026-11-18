//! Resolve every reviewer through the daemon's shared profile candidate service.
use crate::server::api::{
    SubagentBackend, resolve_pinned_reviewer_model_profile_selection,
    resolve_reviewer_model_profile_selection,
};
use anyhow::{Context, Result, ensure};
use futures::future::join_all;
use mj_controller::controller::Controller;
use mj_controller::session_manager::ManagedSessionHandle;
use mj_core::config::{ReviewConfig, SessionReview, SubagentChoice, apply_legacy_override};
use mj_core::review::autoconfigure::{autoconfigure, merged_model_offers};
use mj_core::review::settings::{ResolvedReviewSettings, ReviewModelSettings};
use std::sync::Arc;

use mj_controller::review_selection::*;

pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
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
    let choice = controller
        .state
        .effective_review(handle.session_id())
        .map_err(|session| {
            anyhow::anyhow!(
                "the review settings of session {session} cannot be resolved: its parent is missing or the parent chain repeats"
            )
        })?;
    let (mut choices, autoconfigured) =
        configured_choices(settings.clone(), &controller.config, &backend).await?;
    if autoconfigured
        && let Some(SessionReview::Legacy {
            count,
            model,
            effort,
            ..
        }) = choice
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
    let selections = join_all(choices.into_iter().map(|SubagentChoice { model, effort }| {
        let backend = Arc::clone(&backend);
        let pinned_profile = pinned_profile.cloned();
        async move {
            let selected = if let Some(profile) = pinned_profile.as_ref() {
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
            .map_err(|error| anyhow::anyhow!("reviewer model {model:?}: {}", error.message))?;
            Ok(ResolvedReviewSettings {
                profile: selected.profile_id,
                main: ReviewModelSettings {
                    model: Some(selected.model),
                    effort: selected.effort,
                    fast_mode: false,
                },
                autoconfigured,
            })
        }
    }))
    .await;
    selections.into_iter().collect()
}

/// Resolves one model choice, such as the brief writer's, to a profile the
/// way every reviewer is resolved.
pub(crate) async fn resolve_choice(
    backend: &Arc<dyn SubagentBackend>,
    choice: SubagentChoice,
) -> Result<ResolvedReviewSettings> {
    let SubagentChoice { model, effort } = choice;
    let selected = resolve_reviewer_model_profile_selection(backend, &model, effort.as_deref())
        .await
        .map_err(|error| anyhow::anyhow!("model {model:?}: {}", error.message))?;
    Ok(ResolvedReviewSettings {
        profile: selected.profile_id,
        main: ReviewModelSettings {
            model: Some(selected.model),
            effort: selected.effort,
            fast_mode: false,
        },
        autoconfigured: false,
    })
}

async fn configured_choices(
    settings: ReviewConfig,
    config: &mj_core::config::Config,
    backend: &Arc<dyn SubagentBackend>,
) -> Result<(Vec<SubagentChoice>, bool)> {
    let autoconfigured = settings.is_legacy();
    let choices = if autoconfigured {
        let pinned = settings
            .legacy
            .as_ref()
            .and_then(|legacy| legacy.profile.as_ref())
            .filter(|profile| config.enabled_profile(profile).is_some());
        let ready = backend.reviewer_ready_candidates(String::new()).await?;
        let mut usage_priced = usage_priced_profiles(&ready);
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
                let choices = discovery
                    .await
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
            let ready_models = ready
                .offered
                .iter()
                .flat_map(|candidate| candidate.choices.models.iter().cloned())
                .collect::<Vec<_>>();
            let candidates =
                if autoconfigure(&merged_model_offers(&ready_models), |_| None).is_none() {
                    backend.reviewer_autoconfigure_candidates().await?
                } else {
                    ready
                };
            usage_priced.extend(usage_priced_profiles(&candidates));
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
        let (choices, _, _) = upgrade_reviewers(
            settings.legacy.as_ref().expect("legacy review"),
            &available,
            &usage_priced,
            |profile, model| {
                let backend = Arc::clone(backend);
                async move { backend.profile_config(profile, Some(model), false).await }
            },
        )
        .await;
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
