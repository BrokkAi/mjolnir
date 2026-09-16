//! Background discovery against the same workers and adapters that run review.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::targets::CancellableProcessExecutor;
use mj_core::acp::{SessionConfigChoice, session_config_choices};
use mj_core::worker_launch::ReviewerLaunchConfig;
use tokio::sync::mpsc::UnboundedSender;

use crate::controller::Controller;
use crate::session_manager::{
    ManagedSessionHandle, ReviewerAction, ReviewerOutcome, SessionManagerControl,
};
use crate::worker_client::StartedReviewer;

const REVIEW_DISCOVERY_CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
const REVIEW_DISCOVERY_STAGING_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiscoveryRequest {
    pub profile: String,
    pub model: Option<String>,
    pub preferred_session: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewCapabilityChoices {
    pub model_choices: Vec<SessionConfigChoice>,
    pub effort_choices: Vec<SessionConfigChoice>,
    /// Whether effort choices were obtained for the selected model. An
    /// explicit model that the adapter does not advertise leaves this false,
    /// even when the initial startup advertised generic effort choices.
    pub effort_capabilities_discovered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiscoveryOutcome {
    Available {
        choices: ReviewCapabilityChoices,
        cleanup_warning: Option<String>,
    },
    Unavailable,
}

/// Discover review selectors from one already connected active worker.
///
/// The selected dashboard session is preferred when it is an eligible
/// worker. Otherwise eligible sessions are considered by session id. The
/// reviewer is staged without MCP servers and started only to read its ACP
/// configuration choices; no review prompt, repository inspection, or tool
/// verification is performed.
pub async fn discover_review_settings(
    control: SessionManagerControl,
    request: ReviewDiscoveryRequest,
    cancelled: Arc<AtomicBool>,
    progress: UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewDiscoveryOutcome, String> {
    check_cancelled(&cancelled)?;
    let controller = Arc::new(
        tokio::task::spawn_blocking(Controller::load)
            .await
            .map_err(|error| format!("load review settings task failed: {error}"))?
            .map_err(|error| format!("load review settings: {error:#}"))?,
    );
    let Some(profile) = controller.config.profiles.get(&request.profile) else {
        return Err(format!("Unknown reviewer profile {:?}", request.profile));
    };
    if !profile.enabled {
        return Err(format!(
            "Reviewer profile {:?} is disabled",
            request.profile
        ));
    }
    if !profile.kind.supports_injected_mcp() {
        return Err("Muse Code cannot be a reviewer because muse-acp does not accept the required MCP tools".into());
    }

    let Some((session_id, handle)) = select_worker(
        &control,
        &controller,
        request.preferred_session.as_deref(),
        &cancelled,
    )
    .await?
    else {
        return Ok(ReviewDiscoveryOutcome::Unavailable);
    };

    // Once a worker has been selected, every path through the attempt runs a
    // bounded cleanup. This includes cancellation and a failed Start: a
    // worker can launch its process before returning a configuration error.
    let generation = crate::review_host::next_review_generation()?;
    discover_selected_worker(
        Arc::clone(&controller),
        session_id,
        generation,
        handle,
        &request,
        &cancelled,
        &progress,
    )
    .await
}

async fn discover_selected_worker(
    controller: Arc<Controller>,
    session_id: String,
    generation: u64,
    handle: ManagedSessionHandle,
    request: &ReviewDiscoveryRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewDiscoveryOutcome, String> {
    let role = format!("settings-{generation:016x}");
    let discovery = discover_on_worker(
        controller,
        &session_id,
        generation,
        &handle,
        request,
        cancelled,
        progress,
    )
    .await;
    let cleanup = cleanup_worker(&handle, &role).await;
    if let Err(error) = &cleanup {
        tracing::warn!(
            session_id = %session_id,
            role = %role,
            error = %error,
            "review settings discovery cleanup failed"
        );
    }

    match (discovery, cleanup) {
        (Ok(choices), Ok(())) => Ok(ReviewDiscoveryOutcome::Available {
            choices,
            cleanup_warning: None,
        }),
        (Ok(choices), Err(warning)) => Ok(ReviewDiscoveryOutcome::Available {
            choices,
            cleanup_warning: Some(warning),
        }),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
    }
}

async fn select_worker(
    control: &SessionManagerControl,
    controller: &Controller,
    preferred_session: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<Option<(String, ManagedSessionHandle)>, String> {
    let mut session_ids = controller
        .state
        .sessions
        .iter()
        .filter(|(_, session)| session.target.is_some() && session.state.is_active())
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();
    session_ids.sort();
    if let Some(preferred) = preferred_session
        && let Some(index) = session_ids
            .iter()
            .position(|session_id| session_id == preferred)
    {
        let selected = session_ids.remove(index);
        session_ids.insert(0, selected);
    }

    for session_id in session_ids {
        check_cancelled(cancelled)?;
        let handle = match cancellable(cancelled, async {
            control
                .session(session_id.clone())
                .await
                .map_err(|error| format!("{error:#}"))
        })
        .await
        {
            Ok(handle) => handle,
            Err(_) => {
                check_cancelled(cancelled)?;
                continue;
            }
        };
        let view = handle.view();
        if view.connected && !handle.is_stopped() {
            return Ok(Some((session_id, handle)));
        }
    }
    check_cancelled(cancelled)?;
    Ok(None)
}

async fn discover_on_worker(
    controller: Arc<Controller>,
    session_id: &str,
    generation: u64,
    handle: &ManagedSessionHandle,
    request: &ReviewDiscoveryRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewCapabilityChoices, String> {
    let role = format!("settings-{generation:016x}");
    check_cancelled(cancelled)?;
    let session_id_for_stage = session_id.to_owned();
    let profile = request.profile.clone();
    let flag = Arc::clone(cancelled);
    let config = tokio::task::spawn_blocking(move || {
        let executor =
            CancellableProcessExecutor::new(flag).with_deadline(REVIEW_DISCOVERY_STAGING_TIMEOUT);
        controller.stage_reviewer_profile_controlled(
            &session_id_for_stage,
            &profile,
            generation,
            &[],
            &executor,
        )
    })
    .await
    .map_err(|error| format!("Reviewer staging task failed: {error}"))?
    .map_err(|error| format!("Stage reviewer: {error:#}"))?;

    let mut config = config;
    config.model = None;
    config.effort = None;
    let started = cancellable(cancelled, start(handle, &role, &config)).await?;
    let model_choices = session_config_choices(&started.config_options, "model");
    let initial_effort_choices = session_config_choices(&started.config_options, "effort");
    let mut choices = ReviewCapabilityChoices {
        model_choices,
        effort_choices: initial_effort_choices,
        effort_capabilities_discovered: true,
    };

    if let Some(model) = &request.model {
        if !choices
            .model_choices
            .iter()
            .any(|choice| choice.value == *model)
        {
            // The initial Start still gave us useful model choices. Its
            // generic effort choices cannot be claimed for an unsupported
            // explicit model.
            choices.effort_choices.clear();
            choices.effort_capabilities_discovered = false;
        } else {
            check_cancelled(cancelled)?;
            config.model = Some(model.clone());
            config.effort = None;
            let started = cancellable(cancelled, start(handle, &role, &config)).await?;
            choices.effort_choices = session_config_choices(&started.config_options, "effort");
            choices.effort_capabilities_discovered = true;
        }
    }

    check_cancelled(cancelled)?;
    progress
        .send(choices.clone())
        .map_err(|_| "Review capability progress receiver closed".to_owned())?;
    Ok(choices)
}

async fn cleanup_worker(handle: &ManagedSessionHandle, role: &str) -> Result<(), String> {
    match tokio::time::timeout(
        REVIEW_DISCOVERY_CLEANUP_TIMEOUT,
        call(handle, role, ReviewerAction::Pause),
    )
    .await
    {
        Ok(Ok(ReviewerOutcome::Paused)) => Ok(()),
        Ok(Ok(_)) => Err("Unexpected response while stopping review settings discovery".to_owned()),
        Ok(Err(error)) => Err(format!("Could not stop review settings discovery: {error}")),
        Err(_) => Err(format!(
            "Could not stop review settings discovery within {} seconds",
            REVIEW_DISCOVERY_CLEANUP_TIMEOUT.as_secs()
        )),
    }
}

async fn start(
    handle: &ManagedSessionHandle,
    role: &str,
    config: &ReviewerLaunchConfig,
) -> Result<StartedReviewer, String> {
    match call(
        handle,
        role,
        ReviewerAction::Start {
            config: Box::new(config.clone()),
        },
    )
    .await?
    {
        ReviewerOutcome::Started(started) => Ok(*started),
        _ => Err("Worker returned an unexpected reviewer startup response".to_owned()),
    }
}

async fn call(
    handle: &ManagedSessionHandle,
    role: &str,
    action: ReviewerAction,
) -> Result<ReviewerOutcome, String> {
    handle
        .reviewer_as(Some(role.to_owned()), action)
        .await
        .map_err(|error| format!("{error:#}"))
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("Review settings discovery cancelled".to_owned())
    } else {
        Ok(())
    }
}

async fn cancellable<T>(
    cancelled: &AtomicBool,
    operation: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = async {
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            while !cancelled.load(Ordering::Acquire) {
                interval.tick().await;
            }
        } => Err("Review settings discovery cancelled".to_owned()),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests;
