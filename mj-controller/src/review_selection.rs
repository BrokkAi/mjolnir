//! Resolve every reviewer through the daemon's shared profile candidate service.
use crate::controller::Controller;
use crate::server::api::{SubagentBackend, resolve_reviewer_model_profile_selection};
use crate::server_runtime::profile_catalog::ProfileCatalog;
use crate::session_manager::ManagedSessionHandle;
use anyhow::{Context, Result, ensure};
use mj_core::config::{ReviewConfig, SubagentChoice};
use mj_core::review::autoconfigure::autoconfigure;
use mj_core::review::settings::{ResolvedReviewSettings, ReviewModelSettings};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) async fn resolve(
    handle: ManagedSessionHandle,
    settings: Option<ReviewConfig>,
    cancelled: Arc<AtomicBool>,
    _catalog: Option<Arc<ProfileCatalog>>,
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
    let autoconfigured = settings.reviewers.is_empty();
    let choices = if autoconfigured {
        let candidates = backend.reviewer_candidates().await?;
        let models = candidates
            .offered
            .iter()
            .flat_map(|candidate| candidate.choices.models.iter().cloned())
            .collect::<Vec<_>>();
        autoconfigure(&models).context(
            "No reviewers can be autoconfigured; choose reviewers in Settings > Code Review",
        )?
    } else {
        settings.reviewers
    };
    ensure!(
        !choices.is_empty(),
        "choose reviewers in Settings > Code Review"
    );
    let mut resolved = Vec::with_capacity(choices.len());
    for SubagentChoice { model, effort } in choices {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "review preparation cancelled"
        );
        let selected =
            resolve_reviewer_model_profile_selection(&backend, &model, effort.as_deref())
                .await
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

/// Whether a refusal only says another operation held the session: a lease
/// refused the reviewer action, a lease taken meanwhile cancelled it, or the
/// worker turned reviewer work away during a checkpoint or replacement (the
/// recovery copy a finished turn starts, or an upgrade to the current build).
pub(crate) fn preempted_by_lifecycle(reason: &str) -> bool {
    reason.contains("session is reserved for a lifecycle operation")
        || reason.contains("cancelled for session lifecycle change")
        || reason.contains("worker is reserved for checkpoint or replacement")
}
