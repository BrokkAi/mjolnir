use super::*;
use crate::acp::antigravity_tasks::{
    AntigravityRefreshCursor, AntigravitySnapshot, AntigravityStateFollower, is_sqlite_busy,
    read_summary_row_digest,
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

struct InFlightAntigravityScan {
    task: tokio::task::JoinHandle<(
        AntigravityStateFollower,
        Result<(AntigravitySnapshot, AntigravityRefreshCursor)>,
    )>,
    force: bool,
    hook_epoch: u64,
    attempts_remaining: u8,
    waiters: Vec<oneshot::Sender<Option<u64>>>,
}

pub(crate) struct AntigravityTaskMonitor {
    home: std::result::Result<PathBuf, String>,
    native_session_id: Option<String>,
    follower: Option<AntigravityStateFollower>,
    in_flight: Option<InFlightAntigravityScan>,
    pending_idle_waiters: Vec<oneshot::Sender<Option<u64>>>,
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
            in_flight: None,
            pending_idle_waiters: Vec::new(),
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
        self.fail_idle_waiters();
        if let Some(scan) = self.in_flight.take() {
            scan.task.abort();
        }

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
        while self.has_in_flight_scan() {
            self.finish_in_flight_scan(relay).await?;
        }
        if force {
            self.start_scan(relay, true, 1, Vec::new())?;
        } else {
            self.start_periodic_poll(relay)?;
        }
        while self.has_in_flight_scan() {
            self.finish_in_flight_scan(relay).await?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn refresh_for_idle_reservation(
        &mut self,
        relay: &Arc<RelayCell>,
    ) -> Result<bool> {
        let (reply, result) = oneshot::channel();
        self.request_idle_reservation_refresh(relay, reply)?;
        while self.has_in_flight_scan() {
            self.finish_in_flight_scan(relay).await?;
        }
        Ok(matches!(result.await, Ok(Some(_))))
    }

    pub(crate) fn start_periodic_poll(&mut self, relay: &Arc<RelayCell>) -> Result<()> {
        if self.in_flight.is_some() {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if self.next_error_retry_at.is_some_and(|at| now < at) {
            return Ok(());
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
            return Ok(());
        }
        self.start_scan(relay, false, 0, Vec::new())
    }

    pub(crate) fn request_idle_reservation_refresh(
        &mut self,
        relay: &Arc<RelayCell>,
        reply: oneshot::Sender<Option<u64>>,
    ) -> Result<()> {
        if self.native_session_id.is_none() {
            let _ = reply.send(None);
            return Ok(());
        }
        let epoch = relay
            .lock()
            .expect("relay state lock poisoned")
            .antigravity_hook_epoch();
        if let Some(scan) = &mut self.in_flight {
            if scan.force && scan.hook_epoch == epoch {
                scan.waiters.push(reply);
            } else {
                self.pending_idle_waiters.push(reply);
            }
            return Ok(());
        }
        self.start_scan(relay, true, 1, vec![reply])
    }

    pub(crate) fn fail_idle_waiters(&mut self) {
        for reply in self.pending_idle_waiters.drain(..) {
            let _ = reply.send(None);
        }
        if let Some(scan) = &mut self.in_flight {
            for reply in scan.waiters.drain(..) {
                let _ = reply.send(None);
            }
        }
    }

    pub(crate) fn has_in_flight_scan(&self) -> bool {
        self.in_flight.is_some()
    }

    fn start_scan(
        &mut self,
        relay: &Arc<RelayCell>,
        force: bool,
        attempts_remaining: u8,
        waiters: Vec<oneshot::Sender<Option<u64>>>,
    ) -> Result<()> {
        debug_assert!(self.in_flight.is_none());
        let Some(id) = &self.native_session_id else {
            for reply in waiters {
                let _ = reply.send(None);
            }
            return Ok(());
        };
        let mut follower = match (self.follower.take(), &self.home) {
            (Some(follower), _) => follower,
            (None, Ok(home)) => AntigravityStateFollower::new(home.clone(), id.clone()),
            (None, Err(error)) => {
                for reply in waiters {
                    let _ = reply.send(None);
                }
                return self.record_failure(
                    relay,
                    anyhow::anyhow!("cannot locate Antigravity home: {error}"),
                );
            }
        };
        let hook_epoch = relay
            .lock()
            .expect("relay state lock poisoned")
            .antigravity_hook_epoch();
        let task = mj_core::runtime::spawn_blocking(move || {
            let result = follower.refresh_pending();
            (follower, result)
        });
        self.in_flight = Some(InFlightAntigravityScan {
            task,
            force,
            hook_epoch,
            attempts_remaining,
            waiters,
        });
        Ok(())
    }

    pub(crate) async fn finish_in_flight_scan(&mut self, relay: &Arc<RelayCell>) -> Result<()> {
        // Poll by reference: select! cancellation must not lose the follower or waiters.
        let result = (&mut self.in_flight.as_mut().expect("scan exists").task).await;
        let scan = self.in_flight.take().expect("completed scan exists");
        let mut synced_epoch = None;
        match result {
            Ok((mut follower, result)) => match result {
                Ok((snapshot, cursor)) => {
                    self.next_error_retry_at = None;
                    self.retry_delay = ANTIGRAVITY_TASK_POLL_INTERVAL;
                    self.busy_since = None;
                    self.last_warning = None;
                    let applied = relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .antigravity_snapshot_changed(snapshot, scan.hook_epoch)?;
                    if applied {
                        follower.commit_refresh(cursor);
                        self.last_success_at = Some(tokio::time::Instant::now());
                        self.last_synced_hook_epoch = Some(scan.hook_epoch);
                        synced_epoch = Some(scan.hook_epoch);
                    }
                    self.follower = Some(follower);
                    if !applied && scan.force && scan.attempts_remaining > 0 {
                        return self.start_scan(
                            relay,
                            true,
                            scan.attempts_remaining - 1,
                            scan.waiters,
                        );
                    }
                }
                Err(error) => {
                    self.follower = Some(follower);
                    self.record_failure(relay, error)?;
                }
            },
            Err(error) => self.record_failure(
                relay,
                anyhow::anyhow!("Antigravity background task scan stopped: {error}"),
            )?,
        }
        for reply in scan.waiters {
            let _ = reply.send(synced_epoch);
        }
        if !self.pending_idle_waiters.is_empty() {
            let waiters = std::mem::take(&mut self.pending_idle_waiters);
            self.start_scan(relay, true, 1, waiters)?;
        }
        Ok(())
    }

    fn record_failure(&mut self, relay: &Arc<RelayCell>, error: anyhow::Error) -> Result<()> {
        let now = tokio::time::Instant::now();
        self.next_error_retry_at = Some(now + self.retry_delay);
        self.retry_delay = (self.retry_delay * 2).min(ANTIGRAVITY_TASK_MAX_RETRY_DELAY);
        relay
            .lock()
            .expect("relay state lock poisoned")
            .antigravity_tasks_unavailable()?;
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
        if self.last_warning.as_deref() != Some(&message) {
            relay.record_observation(RelayObservation::Warning {
                message: message.clone(),
            })?;
            self.last_warning = Some(message);
        }
        Ok(())
    }
}

impl Drop for AntigravityTaskMonitor {
    fn drop(&mut self) {
        self.detach();
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
