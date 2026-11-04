//! Review selectors from the daemon's published profile capability catalog.
use mj_core::acp::SessionConfigChoice;
use mj_core::config::Config;
use mj_core::profile_capabilities::{CapabilityState, ProfileCapabilitiesSnapshot};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, watch};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiscoveryRequest {
    pub profile: String,
    pub model: Option<String>,
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

/// Wait for the selected profile's catalog entry, including model-specific efforts.
/// A model's efforts are discovered once it is selected, so a chosen model is
/// first handed to `warm` as the reviewer selection of the saved
/// configuration. No worker or reviewer is acquired by Settings.
pub async fn discover_review_settings<W, F>(
    mut catalog: watch::Receiver<ProfileCapabilitiesSnapshot>,
    request: ReviewDiscoveryRequest,
    cancelled: Arc<AtomicBool>,
    progress: UnboundedSender<ReviewCapabilityChoices>,
    warm: W,
) -> Result<ReviewDiscoveryOutcome, String>
where
    W: FnOnce(Config) -> F,
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let config = tokio::task::spawn_blocking(Config::load)
        .await
        .map_err(|error| format!("load review settings task failed: {error}"))?
        .map_err(|error| format!("load review settings: {error:#}"))?;
    if request.model.is_some() {
        let mut selection = config.clone();
        selection.review.profile = Some(request.profile.clone());
        selection.review.model = request.model.clone();
        warm(selection)
            .await
            .map_err(|error| format!("request review effort choices: {error:#}"))?;
    }
    discover_from_catalog(&config, &mut catalog, &request, &cancelled, &progress).await
}

async fn discover_from_catalog(
    config: &Config,
    catalog: &mut watch::Receiver<ProfileCapabilitiesSnapshot>,
    request: &ReviewDiscoveryRequest,
    cancelled: &AtomicBool,
    progress: &UnboundedSender<ReviewCapabilityChoices>,
) -> Result<ReviewDiscoveryOutcome, String> {
    let profile = config
        .enabled_profile(&request.profile)
        .ok_or_else(|| format!("Unknown or disabled reviewer profile {:?}", request.profile))?;
    let identity = profile.capabilities_key(&request.profile);
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let mut published = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("Review settings discovery cancelled".to_owned());
        }
        let snapshot = catalog.borrow_and_update().clone();
        if let Some(entry) = snapshot.profiles.get(&identity) {
            match &entry.choices {
                CapabilityState::Failed(error) => {
                    return Err(format!("Review choices unavailable: {error}"));
                }
                CapabilityState::Pending => {}
                CapabilityState::Ready(initial) => {
                    let mut choices = ReviewCapabilityChoices {
                        model_choices: initial.models.clone(),
                        effort_choices: initial.efforts.clone(),
                        effort_capabilities_discovered: true,
                    };
                    let mut pending = false;
                    if let Some(model) = &request.model {
                        choices.effort_choices.clear();
                        choices.effort_capabilities_discovered = false;
                        if initial.models.iter().any(|choice| choice.value == *model) {
                            match entry.efforts.get(model) {
                                Some(CapabilityState::Ready(efforts)) => {
                                    choices.effort_choices = efforts.clone();
                                    choices.effort_capabilities_discovered = true;
                                }
                                Some(CapabilityState::Failed(error)) => {
                                    return Err(format!(
                                        "Review effort choices unavailable: {error}"
                                    ));
                                }
                                _ => pending = true,
                            }
                        }
                    }
                    if published.as_ref() != Some(&choices) {
                        progress
                            .send(choices.clone())
                            .map_err(|_| "Review capability progress receiver closed".to_owned())?;
                        published = Some(choices.clone());
                    }
                    if !pending {
                        return Ok(ReviewDiscoveryOutcome::Available {
                            choices,
                            cleanup_warning: None,
                        });
                    }
                }
            }
        }
        tokio::select! {
            result = catalog.changed() => result.map_err(|_| "Profile capability publisher stopped".to_owned())?,
            _ = interval.tick() => {},
        }
    }
}

#[cfg(test)]
mod tests;
