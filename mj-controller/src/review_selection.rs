//! Resolve every reviewer through the daemon's shared profile candidate service.
use crate::controller::Controller;
use crate::server::api::{SubagentBackend, resolve_reviewer_model_profile_selection};
use crate::session_manager::ManagedSessionHandle;
use anyhow::{Context, Result, ensure};
use mj_core::config::{ReviewConfig, SubagentChoice};
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
    let (choices, autoconfigured) = configured_choices(settings, &cancelled, &backend).await?;
    let mut resolved = Vec::with_capacity(choices.len());
    for SubagentChoice { model, effort } in choices {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "review preparation cancelled"
        );
        let selected = await_or_cancel(
            &cancelled,
            resolve_reviewer_model_profile_selection(&backend, &model, effort.as_deref()),
        )
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
    cancelled: &AtomicBool,
    backend: &Arc<dyn SubagentBackend>,
) -> Result<(Vec<SubagentChoice>, bool)> {
    let autoconfigured = settings.legacy_auto;
    let choices = if autoconfigured {
        let candidates = await_or_cancel(cancelled, backend.reviewer_candidates()).await??;
        let available = candidates
            .offered
            .into_iter()
            .map(|candidate| (candidate.profile_id, candidate.choices))
            .collect::<Vec<_>>();
        autoconfigure_with_efforts(
            &available,
            |profile, model| {
                let backend = Arc::clone(backend);
                async move { backend.profile_config(profile, Some(model), false).await }
            },
            Some(cancelled),
        )
        .await?
    } else {
        settings.reviewers
    };
    ensure!(
        !choices.is_empty(),
        "No reviewers are configured; choose reviewers or press Autoconfigure in Settings › Code Review."
    );
    Ok((choices, autoconfigured))
}

/// Compute the ordered legacy Auto preset using the same model and effort
/// discovery for the daemon upgrade and the startup review fallback.
pub(crate) async fn autoconfigure_with_efforts<F, Fut>(
    available: &[(String, ProfileConfig)],
    probe: F,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<SubagentChoice>>
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
    for model in draft
        .iter()
        .map(|choice| &choice.model)
        .collect::<BTreeSet<_>>()
    {
        for (profile, offered) in available
            .iter()
            .filter(|(_, offered)| offered.models.iter().any(|choice| &choice.value == model))
        {
            if offered.model.as_deref() == Some(model) {
                efforts.insert(model.clone(), offered.efforts.clone());
                break;
            }
            let discovery = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                probe(profile.clone(), model.clone()),
            );
            let result = match cancelled {
                Some(cancelled) => await_or_cancel(cancelled, discovery).await?,
                None => discovery.await,
            };
            match result {
                Ok(Ok(choices)) if choices.models.iter().any(|choice| &choice.value == model) => {
                    efforts.insert(model.clone(), choices.efforts);
                    break;
                }
                Ok(Ok(_)) => tracing::warn!(%profile, %model,
                    "model-specific reviewer discovery omitted the requested model"),
                Ok(Err(error)) => tracing::warn!(%profile, %error,
                    "reviewer effort discovery failed during legacy Auto configuration"),
                Err(_) => tracing::warn!(%profile, %model,
                    "reviewer effort discovery timed out during legacy Auto configuration"),
            }
        }
    }
    Ok(autoconfigure(&models, |model| efforts.get(model).cloned()).unwrap_or_default())
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
