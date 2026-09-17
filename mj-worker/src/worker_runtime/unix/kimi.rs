use super::*;

pub(crate) const KIMI_TASK_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(500);

pub(crate) const KIMI_TASK_MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(8);

pub(crate) struct KimiTaskMonitor {
    home: std::result::Result<PathBuf, String>,
    native_session_id: Option<String>,
    follower: Option<acp::KimiWireFollower>,
    consecutive_failures: u32,
    retry_at: tokio::time::Instant,
    last_warning: Option<String>,
}

impl KimiTaskMonitor {
    pub(crate) fn new(home: std::result::Result<PathBuf, String>) -> Self {
        Self {
            home,
            native_session_id: None,
            follower: None,
            consecutive_failures: 0,
            retry_at: tokio::time::Instant::now(),
            last_warning: None,
        }
    }

    fn detach(&mut self) {
        self.native_session_id = None;
        self.follower = None;
        self.consecutive_failures = 0;
        self.retry_at = tokio::time::Instant::now();
        self.last_warning = None;
    }

    pub(crate) async fn attach(
        &mut self,
        native_session_id: String,
        relay: &Arc<Mutex<DurableRelay>>,
    ) -> Result<()> {
        self.native_session_id = Some(native_session_id);
        self.follower = None;
        self.consecutive_failures = 0;
        self.retry_at = tokio::time::Instant::now();
        self.refresh(relay, true).await
    }

    pub(crate) async fn refresh(
        &mut self,
        relay: &Arc<Mutex<DurableRelay>>,
        force: bool,
    ) -> Result<()> {
        if self.native_session_id.is_none()
            || (!force && tokio::time::Instant::now() < self.retry_at)
        {
            return Ok(());
        }
        let home = match &self.home {
            Ok(home) => home.clone(),
            Err(error) => {
                return self
                    .record_failure(relay, anyhow::anyhow!("cannot locate Kimi home: {error}"));
            }
        };
        let native_session_id = self
            .native_session_id
            .clone()
            .expect("Kimi refresh is guarded by a native session id");
        let follower = self.follower.take();
        let scanned = tokio::task::spawn_blocking(move || -> Result<_> {
            let session_dir = acp::resolve_kimi_session_dir(&home, &native_session_id)?;
            let wire_path = session_dir.join("agents/main/wire.jsonl");
            let mut follower = match follower {
                Some(follower) if follower.wire_path() == wire_path => follower,
                _ => acp::KimiWireFollower::open(wire_path)?,
            };
            let snapshot = match follower.refresh()? {
                acp::KimiWireRefresh::Updated(snapshot) => snapshot,
                acp::KimiWireRefresh::RescanRequired => follower.rescan()?,
            };
            Ok((follower, snapshot))
        })
        .await;
        match scanned {
            Ok(Ok((follower, snapshot))) => {
                self.follower = Some(follower);
                self.consecutive_failures = 0;
                self.retry_at = tokio::time::Instant::now() + KIMI_TASK_POLL_INTERVAL;
                self.last_warning = None;
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .kimi_background_tasks_changed(
                        snapshot.tasks,
                        snapshot.provider_tool_ids,
                        snapshot.observed_task_ids,
                    )
            }
            Ok(Err(error)) => self.record_failure(relay, error),
            Err(error) => self.record_failure(
                relay,
                anyhow::anyhow!("Kimi background task scan stopped: {error}"),
            ),
        }
    }

    fn record_failure(
        &mut self,
        relay: &Arc<Mutex<DurableRelay>>,
        error: anyhow::Error,
    ) -> Result<()> {
        self.follower = None;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let exponent = self.consecutive_failures.saturating_sub(1).min(4);
        let delay = KIMI_TASK_POLL_INTERVAL
            .checked_mul(1_u32 << exponent)
            .unwrap_or(KIMI_TASK_MAX_RETRY_DELAY)
            .min(KIMI_TASK_MAX_RETRY_DELAY);
        self.retry_at = tokio::time::Instant::now() + delay;
        let message = format!(
            "Kimi background task tracking is unavailable; worker replacement is blocked: {error:#}"
        );
        let mut relay = relay.lock().expect("relay state lock poisoned");
        relay.kimi_background_tasks_unavailable()?;
        if self.last_warning.as_deref() != Some(&message) {
            relay.record_observation(RelayObservation::Warning {
                message: message.clone(),
            })?;
            self.last_warning = Some(message);
        }
        Ok(())
    }
}

pub(crate) async fn prepare_kimi_runtime_event(
    monitor: &mut Option<KimiTaskMonitor>,
    relay: &Arc<Mutex<DurableRelay>>,
    event: &mut RuntimeEvent,
) -> Result<()> {
    let Some(monitor) = monitor.as_mut() else {
        return Ok(());
    };
    match event {
        RuntimeEvent::SessionStarted {
            native_session_id, ..
        } => monitor.attach(native_session_id.clone(), relay).await,
        RuntimeEvent::SessionConfigured { .. } => monitor.refresh(relay, true).await,
        RuntimeEvent::PromptFinished {
            request_id,
            stop_reason,
            diagnostic,
            ..
        } => {
            let started = relay
                .lock()
                .expect("relay state lock poisoned")
                .operational_state()
                .active_prompt
                .filter(|prompt| prompt.command_id == *request_id)
                .map(|prompt| prompt.started_at_ms);
            monitor.refresh(relay, true).await?;
            if let Some(native) = started
                .and_then(|started| monitor.follower.as_ref()?.turn_diagnostic_since(started))
            {
                *stop_reason = if native.is_usage_limit() {
                    mj_core::diagnostic::QUOTA_STOP_REASON.into()
                } else {
                    "error".into()
                };
                *diagnostic = Some(native);
            }
            Ok(())
        }
        RuntimeEvent::HarnessRestarting { .. } | RuntimeEvent::Stopped => {
            monitor.detach();
            Ok(())
        }
        _ => Ok(()),
    }
}
