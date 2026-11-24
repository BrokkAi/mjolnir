//! Resolve every reviewer through the daemon's shared profile candidate service.
use crate::server::api::{
    SubagentBackend, resolve_pinned_reviewer_model_profile_selection,
    resolve_reviewer_model_profile_selection,
};
use anyhow::{Context, Result, ensure};
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
    let controller = mj_core::runtime::spawn_blocking(Controller::load).await??;
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
    let execution_policy = if session.is_yolo_target(&controller.config) {
        mj_core::config::ExecutionPolicy::Unconstrained
    } else {
        mj_core::config::ExecutionPolicy::ConfiguredApprovals
    };
    let mut selections = Vec::with_capacity(choices.len());
    for SubagentChoice { model, effort } in choices {
        let selected = if let Some(profile) = pinned_profile {
            resolve_pinned_reviewer_model_profile_selection(
                &backend,
                profile,
                &model,
                effort.as_deref(),
            )
            .await
        } else {
            resolve_reviewer_model_profile_selection(
                &backend,
                &model,
                effort.as_deref(),
                execution_policy,
                "reviewer",
            )
            .await
        }
        .map_err(|error| anyhow::anyhow!("reviewer model {model:?}: {}", error.message))?;
        selected.reservation.commit(None);
        selections.push(ResolvedReviewSettings {
            profile: selected.profile_id,
            main: ReviewModelSettings {
                model: Some(selected.model),
                effort: selected.effort,
                fast_mode: false,
            },
            autoconfigured,
        });
    }
    Ok(selections)
}

/// Resolves one model choice, such as the brief writer's, to a profile the
/// way every reviewer is resolved.
pub(crate) async fn resolve_choice(
    backend: &Arc<dyn SubagentBackend>,
    choice: SubagentChoice,
    execution_policy: mj_core::config::ExecutionPolicy,
) -> Result<ResolvedReviewSettings> {
    let SubagentChoice { model, effort } = choice;
    let selected = resolve_reviewer_model_profile_selection(
        backend,
        &model,
        effort.as_deref(),
        execution_policy,
        "brief_writer",
    )
    .await
    .map_err(|error| anyhow::anyhow!("model {model:?}: {}", error.message))?;
    selected.reservation.commit(None);
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

pub(crate) async fn configured_choices(
    settings: ReviewConfig,
    config: &mj_core::config::Config,
    backend: &Arc<dyn SubagentBackend>,
) -> Result<(Vec<SubagentChoice>, bool)> {
    let autoconfigured = settings.is_legacy();
    // Profiles automatic selection could not check, for the refusal below.
    let mut unavailable_profiles: Vec<(String, String)> = Vec::new();
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
            unavailable_profiles = candidates.unavailable;
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
    if choices.is_empty() {
        let mut message = "No reviewers are configured; choose reviewers or press Autoconfigure in Settings › Code Review.".to_owned();
        if !unavailable_profiles.is_empty() {
            let skipped = unavailable_profiles
                .iter()
                .map(|(id, reason)| format!("{id} ({reason})"))
                .collect::<Vec<_>>()
                .join("; ");
            message.push_str(&format!(" Could not check: {skipped}."));
        }
        anyhow::bail!(message);
    }
    Ok((choices, autoconfigured))
}
