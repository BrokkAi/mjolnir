use super::*;
use crate::acp::antigravity_tasks::{
    AntigravityStateFollower, is_sqlite_busy, read_summary_row_digest,
};

pub(super) async fn handle_antigravity_stop(
    relay: &Arc<RelayCell>,
    envelope: RelayRequestEnvelope,
    credentials: &SharedCredentials,
) -> RelayResponseEnvelope {
    let credentials = credentials.read().await;
    let home = credentials
        .as_ref()
        .ok()
        .filter(|endpoint| endpoint.harness == HarnessKind::Antigravity)
        .map(|endpoint| endpoint.home.clone());
    let RelayRequest::AntigravityStop {
        conversation_id,
        fully_idle,
        ..
    } = &envelope.request
    else {
        unreachable!("handle_antigravity_stop only serves Antigravity Stop hooks");
    };

    let baseline = if *fully_idle != Some(true)
        && let (Some(home), Some(id)) = (home, conversation_id.as_deref())
        && relay
            .lock()
            .expect("relay state lock poisoned")
            .matches_antigravity_conversation(Some(id))
    {
        let id = id.to_owned();
        // Antigravity waits for the hook before publishing its deferred Idle
        // transition. Read the committed row now, even if the monitor missed it.
        match mj_core::runtime::spawn_blocking(move || read_summary_row_digest(&home, &id))
            .await
            .context("sample Antigravity Stop baseline")
            .and_then(|result| result)
        {
            Ok(digest) => Some(digest),
            Err(error) => {
                tracing::warn!(%error, "could not read Antigravity Stop baseline");
                None
            }
        }
    } else {
        None
    };

    let response = relay
        .lock()
        .expect("relay state lock poisoned")
        .handle_with_antigravity_stop_baseline(envelope, baseline);
    // Keep reload waiting until this hook has applied the sampled baseline.
    drop(credentials);
    response
}

pub(crate) const ANTIGRAVITY_TASK_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(500);
const ANTIGRAVITY_TASK_MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(8);

const ANTIGRAVITY_TASK_IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const ANTIGRAVITY_BUSY_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

pub(crate) struct AntigravityTaskMonitor {
    home: std::result::Result<PathBuf, String>,
    native_session_id: Option<String>,
    follower: Option<AntigravityStateFollower>,
    next_error_retry_at: Option<tokio::time::Instant>,
    busy_since: Option<tokio::time::Instant>,
    last_success_at: Option<tokio::time::Instant>,
    last_synced_hook_epoch: Option<u64>,
    retry_delay: std::time::Duration,
    last_warning: Option<String>,
    #[cfg(test)]
    pub(crate) busy_warn_after: std::time::Duration,
}

impl AntigravityTaskMonitor {
    pub(crate) fn new(home: std::result::Result<PathBuf, String>) -> Self {
        Self {
            home,
            native_session_id: None,
            follower: None,
            next_error_retry_at: None,
            busy_since: None,
            last_success_at: None,
            last_synced_hook_epoch: None,
            retry_delay: ANTIGRAVITY_TASK_POLL_INTERVAL,
            last_warning: None,
            #[cfg(test)]
            busy_warn_after: ANTIGRAVITY_BUSY_WARN_AFTER,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_busy_timeout(&mut self, timeout: std::time::Duration) {
        self.follower.as_mut().unwrap().set_busy_timeout(timeout);
    }

    fn detach(&mut self) {
        self.native_session_id = None;
        self.follower = None;
        self.next_error_retry_at = None;
        self.busy_since = None;
        self.last_success_at = None;
        self.last_synced_hook_epoch = None;
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
        self.refresh_outcome(relay, force).await.map(|_| ())
    }

    pub(crate) async fn refresh_for_idle_reservation(
        &mut self,
        relay: &Arc<RelayCell>,
    ) -> Result<bool> {
        self.refresh_outcome(relay, true).await
    }

    async fn refresh_outcome(&mut self, relay: &Arc<RelayCell>, force: bool) -> Result<bool> {
        let Some(native_session_id) = self.native_session_id.as_ref() else {
            return Ok(false);
        };
        let now = tokio::time::Instant::now();
        if !force {
            if self.next_error_retry_at.is_some_and(|at| now < at) {
                return Ok(false);
            }
            if self.next_error_retry_at.is_none()
                && !relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .antigravity_needs_fast_poll(self.last_synced_hook_epoch)
                && self.last_success_at.is_some_and(|at| {
                    now.saturating_duration_since(at) < ANTIGRAVITY_TASK_IDLE_POLL_INTERVAL
                })
            {
                return Ok(false);
            }
        }
        let follower = match (self.follower.take(), &self.home) {
            (Some(follower), _) => follower,
            (None, Ok(home)) => {
                AntigravityStateFollower::new(home.clone(), native_session_id.clone())
            }
            (None, Err(error)) => {
                return self
                    .record_failure(
                        relay,
                        anyhow::anyhow!("cannot locate Antigravity home: {error}"),
                    )
                    .map(|()| false);
            }
        };
        let mut follower = Some(follower);
        for attempt in 0..if force { 2 } else { 1 } {
            let scan_hook_epoch = relay
                .lock()
                .expect("relay state lock poisoned")
                .antigravity_hook_epoch();
            let mut scan_follower = follower.take().expect("follower returned after scan");
            let scanned = mj_core::runtime::spawn_blocking(move || {
                let result = scan_follower.refresh_pending_with_force(force);
                (scan_follower, result)
            })
            .await;
            match scanned {
                Ok((mut scan_follower, result)) => match result {
                    Ok((snapshot, cursor)) => {
                        self.retry_delay = ANTIGRAVITY_TASK_POLL_INTERVAL;
                        self.next_error_retry_at = None;
                        self.busy_since = None;
                        self.last_warning = None;
                        let applied = relay
                            .lock()
                            .expect("relay state lock poisoned")
                            .antigravity_snapshot_changed(snapshot, scan_hook_epoch)?;
                        if applied {
                            scan_follower.commit_refresh(cursor);
                            self.follower = Some(scan_follower);
                            self.last_success_at = Some(tokio::time::Instant::now());
                            self.last_synced_hook_epoch = Some(scan_hook_epoch);
                            return Ok(true);
                        }
                        if attempt == 1 || !force {
                            self.next_error_retry_at = None;
                            self.follower = Some(scan_follower);
                            return Ok(false);
                        }
                        follower = Some(scan_follower);
                    }
                    Err(error) => {
                        self.follower = Some(scan_follower);
                        return self.record_failure(relay, error).map(|()| false);
                    }
                },
                Err(error) => {
                    return self
                        .record_failure(
                            relay,
                            anyhow::anyhow!("Antigravity background task scan stopped: {error}"),
                        )
                        .map(|()| false);
                }
            }
        }
        Ok(false)
    }

    fn record_failure(&mut self, relay: &Arc<RelayCell>, error: anyhow::Error) -> Result<()> {
        let now = tokio::time::Instant::now();
        self.next_error_retry_at = Some(now + self.retry_delay);
        self.retry_delay = (self.retry_delay * 2).min(ANTIGRAVITY_TASK_MAX_RETRY_DELAY);
        if is_sqlite_busy(&error) {
            let since = *self.busy_since.get_or_insert(now);
            #[cfg(test)]
            let busy_warn_after = self.busy_warn_after;
            #[cfg(not(test))]
            let busy_warn_after = ANTIGRAVITY_BUSY_WARN_AFTER;
            if now.saturating_duration_since(since) < busy_warn_after {
                tracing::debug!(%error, "Antigravity task database temporarily locked");
                return Ok(());
            }
        } else {
            self.busy_since = None;
        }
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
