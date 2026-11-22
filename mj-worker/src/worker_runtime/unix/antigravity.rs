use super::*;
use crate::acp::antigravity_tasks::AntigravityStateFollower;

pub(crate) const ANTIGRAVITY_TASK_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(500);
const ANTIGRAVITY_TASK_MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(8);

pub(crate) struct AntigravityTaskMonitor {
    home: std::result::Result<PathBuf, String>,
    native_session_id: Option<String>,
    follower: Option<AntigravityStateFollower>,
    retry_at: tokio::time::Instant,
    retry_delay: std::time::Duration,
    last_warning: Option<String>,
}

impl AntigravityTaskMonitor {
    pub(crate) fn new(home: std::result::Result<PathBuf, String>) -> Self {
        Self {
            home,
            native_session_id: None,
            follower: None,
            retry_at: tokio::time::Instant::now(),
            retry_delay: ANTIGRAVITY_TASK_POLL_INTERVAL,
            last_warning: None,
        }
    }

    fn detach(&mut self) {
        self.native_session_id = None;
        self.follower = None;
        self.retry_at = tokio::time::Instant::now();
        self.retry_delay = ANTIGRAVITY_TASK_POLL_INTERVAL;
        self.last_warning = None;
    }

    async fn attach(&mut self, native_session_id: String, relay: &Arc<RelayCell>) -> Result<()> {
        if self.native_session_id.as_ref() == Some(&native_session_id) {
            return Ok(());
        }
        self.detach();
        self.native_session_id = Some(native_session_id);
        self.refresh(relay, true).await
    }

    pub(crate) async fn refresh(&mut self, relay: &Arc<RelayCell>, force: bool) -> Result<()> {
        let Some(native_session_id) = self.native_session_id.as_ref() else {
            return Ok(());
        };
        if !force && tokio::time::Instant::now() < self.retry_at {
            return Ok(());
        }
        let mut follower = match (self.follower.take(), &self.home) {
            (Some(follower), _) => follower,
            (None, Ok(home)) => {
                AntigravityStateFollower::new(home.clone(), native_session_id.clone())
            }
            (None, Err(error)) => {
                return self.record_failure(
                    relay,
                    anyhow::anyhow!("cannot locate Antigravity home: {error}"),
                );
            }
        };
        let scanned = mj_core::runtime::spawn_blocking(move || {
            let snapshot = follower.refresh();
            (follower, snapshot)
        })
        .await;
        match scanned {
            Ok((follower, snapshot)) => {
                self.follower = Some(follower);
                match snapshot {
                    Ok(snapshot) => {
                        self.retry_delay = ANTIGRAVITY_TASK_POLL_INTERVAL;
                        self.retry_at = tokio::time::Instant::now() + self.retry_delay;
                        self.last_warning = None;
                        relay
                            .lock()
                            .expect("relay state lock poisoned")
                            .antigravity_snapshot_changed(snapshot)
                    }
                    Err(error) => self.record_failure(relay, error),
                }
            }
            Err(error) => self.record_failure(
                relay,
                anyhow::anyhow!("Antigravity background task scan stopped: {error}"),
            ),
        }
    }

    fn record_failure(&mut self, relay: &Arc<RelayCell>, error: anyhow::Error) -> Result<()> {
        self.retry_at = tokio::time::Instant::now() + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(ANTIGRAVITY_TASK_MAX_RETRY_DELAY);
        let message = format!(
            "Antigravity background task tracking is unavailable; worker replacement is blocked: {error:#}"
        );
        let mut relay = relay.lock().expect("relay state lock poisoned");
        relay.antigravity_tasks_unavailable()?;
        if self.last_warning.as_deref() != Some(&message) {
            relay.record_observation(RelayObservation::Warning {
                message: message.clone(),
            })?;
            self.last_warning = Some(message);
        }
        Ok(())
    }
}

pub(crate) async fn prepare_antigravity_runtime_event(
    monitor: &mut Option<AntigravityTaskMonitor>,
    relay: &Arc<RelayCell>,
    event: &RuntimeEvent,
) -> Result<()> {
    let Some(monitor) = monitor.as_mut() else {
        return Ok(());
    };
    match event {
        RuntimeEvent::SessionStarted {
            native_session_id, ..
        }
        | RuntimeEvent::ContextCleared {
            native_session_id, ..
        } if monitor.native_session_id.as_ref() != Some(native_session_id) => {
            // Polls between session open and configuration must not publish
            // state from the previous conversation.
            monitor.detach();
            Ok(())
        }
        // The preceding SessionStarted or ContextCleared records the ID.
        // SessionConfigured is also emitted for later config changes, so
        // attach keeps the existing follower when that ID has not changed.
        RuntimeEvent::SessionConfigured { .. } => {
            let id = relay
                .lock()
                .expect("relay state lock poisoned")
                .operational_state()
                .native_session_id;
            if let Some(id) = id {
                monitor.attach(id, relay).await?;
            }
            Ok(())
        }
        RuntimeEvent::HarnessRestarting { .. } | RuntimeEvent::ContextClearing { .. } => {
            monitor.detach();
            relay
                .lock()
                .expect("relay state lock poisoned")
                .reset_antigravity_state()
        }
        RuntimeEvent::PromptFinished { .. } => monitor.refresh(relay, true).await,
        _ => Ok(()),
    }
}
