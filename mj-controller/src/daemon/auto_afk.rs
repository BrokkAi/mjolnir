use super::*;
use tokio::time::Instant;

/// The config coordinator owns the clock as well as every presence transition.
/// A timer wake is only a hint: expiry must acquire this owner and recheck.
pub(super) struct Inactivity {
    minutes: u32,
    last_activity: Option<Instant>,
}

impl Inactivity {
    pub(super) fn new(minutes: u32) -> Self {
        let mut clock = Self {
            minutes,
            last_activity: None,
        };
        clock.reset();
        clock
    }

    pub(super) fn reset(&mut self) {
        self.last_activity = (self.minutes > 0).then(Instant::now);
    }

    fn remaining(&self) -> Option<Duration> {
        self.last_activity.map(|last| {
            Duration::from_secs(u64::from(self.minutes) * 60).saturating_sub(last.elapsed())
        })
    }
}

impl RuntimeState {
    pub(super) fn configure_inactivity(&self, clock: &mut Inactivity, minutes: u32) {
        if clock.minutes != minutes {
            clock.minutes = minutes;
            clock.reset();
            self.inactivity_changed.notify_one();
        }
    }

    pub async fn record_operator_activity(&self) {
        self.config_mutation.lock().await.reset();
        self.inactivity_changed.notify_one();
    }

    async fn expire_inactivity(&self, cancellation: &CancellationToken) -> Result<()> {
        let mut clock = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            clock = self.config_mutation.lock() => clock,
        };
        if clock.remaining() != Some(Duration::ZERO) {
            return Ok(());
        }
        // No retry spin when already away or when handoff has begun.
        clock.last_activity = None;
        if self.owner().controller().state.operator_presence.away {
            return Ok(());
        }
        let Ok(_work) = crate::upgrade::activity_unless_draining("automatic operator away") else {
            return Ok(());
        };
        let ids = match self.persist_operator_away(true).await {
            Ok(ids) => ids,
            Err(error) => {
                // Report storage failures and retry after a full interval.
                clock.reset();
                return Err(error);
            }
        };
        drop(clock);
        self.deliver_operator_away(ids).await;
        Ok(())
    }
}

pub(super) fn spawn(
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        loop {
            // Register before reading the clock, so activity cannot lose a wake.
            let changed = state.inactivity_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let remaining = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                clock = state.config_mutation.lock() => clock.remaining(),
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                _ = changed => {},
                _ = async {
                    match remaining {
                        // Bound Instant arithmetic on platforms with a shorter clock horizon.
                        Some(remaining) => tokio::time::sleep(remaining.min(Duration::from_secs(86400))).await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Cancellation while waiting for the owner must not delay handoff.
                    // Once admitted, the bounded durable transition completes normally.
                    if let Err(error) = state.expire_inactivity(&cancellation).await {
                        tracing::warn!(error = %format!("{error:#}"), "automatic AFK transition failed");
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_afk_activity_over_transport_invalidates_queued_expiry_without_publication() {
        let state = super::super::tests::test_runtime_state();
        {
            let mut clock = state.config_mutation.lock().await;
            state.configure_inactivity(&mut clock, 1);
            clock.last_activity = Some(Instant::now() - Duration::from_secs(60));
        }
        let revision = state.revisions.current();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let metadata = super::super::tests::test_metadata(listener.local_addr().unwrap());
        let cancellation = CancellationToken::new();
        let service = tokio::spawn({
            let state = state.clone();
            let cancellation = cancellation.clone();
            let metadata = metadata.clone();
            async move {
                let mut clients = tokio::task::JoinSet::new();
                for _ in 0..2 {
                    let (socket, _) = listener.accept().await.unwrap();
                    clients.spawn(serve_client(
                        socket,
                        metadata.clone(),
                        state.clone(),
                        cancellation.clone(),
                    ));
                }
                while let Some(result) = clients.join_next().await {
                    result.unwrap().unwrap();
                }
            }
        });
        for request_id in 1..=2 {
            let mut socket = TcpStream::connect(metadata.address).await.unwrap();
            write_frame(
                &mut socket,
                &RequestEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    token: metadata.token.clone(),
                    action: DaemonAction::RecordOperatorActivity,
                },
            )
            .await
            .unwrap();
            let response: ResponseEnvelope = read_frame(&mut socket).await.unwrap();
            assert!(matches!(response.result.unwrap(), DaemonReply::Done));
            // The expired timer captured before this accepted request cannot commit.
            state.expire_inactivity(&cancellation).await.unwrap();
            assert!(!state.owner().controller().state.operator_presence.away);
            assert_eq!(state.revisions.current(), revision);
        }
        service.await.unwrap();
        let mut clock = state.config_mutation.lock().await;
        clock.last_activity = Some(Instant::now() - Duration::from_secs(60));
        let activity = tokio::spawn({
            let state = state.clone();
            async move { state.record_operator_activity().await }
        });
        tokio::task::yield_now().await;
        let stale_expiry = tokio::spawn({
            let state = state.clone();
            let cancellation = cancellation.clone();
            async move { state.expire_inactivity(&cancellation).await }
        });
        tokio::task::yield_now().await;
        drop(clock);
        activity.await.unwrap();
        stale_expiry.await.unwrap().unwrap();
        assert_eq!(state.revisions.current(), revision);
        assert!(!state.owner().controller().state.operator_presence.away);
    }

    #[tokio::test(start_paused = true)]
    async fn auto_afk_timer_cancels_while_config_owner_is_busy() {
        let state = super::super::tests::test_runtime_state();
        let cancellation = CancellationToken::new();
        {
            let mut clock = state.config_mutation.lock().await;
            state.configure_inactivity(&mut clock, u32::MAX);
        }
        let timer = spawn(state.clone(), cancellation.clone());
        tokio::task::yield_now().await;
        assert!(!timer.is_finished(), "the maximum setting can be scheduled");
        let mut clock = state.config_mutation.lock().await;
        state.configure_inactivity(&mut clock, 1);
        tokio::task::yield_now().await;
        cancellation.cancel();
        timer.await.unwrap().unwrap();
        drop(clock);
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn auto_afk_expiry_return_reconfigure_and_restart_preserve_durable_presence() {
        const CHILD: &str = "MJ_AUTO_AFK_STORE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "auto_afk_expiry_return_reconfigure_and_restart_preserve_durable_presence",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut state = super::super::tests::test_runtime_state();
        Arc::get_mut(&mut state).unwrap().committed =
            Some(crate::database::subscribe_committed_state().unwrap());
        let cancellation = CancellationToken::new();
        {
            let mut clock = state.config_mutation.lock().await;
            state.configure_inactivity(&mut clock, 1);
        }
        tokio::time::advance(Duration::from_secs(59)).await;
        state.record_operator_activity().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        state.expire_inactivity(&cancellation).await.unwrap();
        assert!(
            !crate::database::subscribe_committed_state()
                .unwrap()
                .borrow()
                .as_ref()
                .unwrap()
                .state
                .operator_presence
                .away
        );
        let timer_cancel = CancellationToken::new();
        let mut revisions = state.revisions();
        let timer = spawn(state.clone(), timer_cancel.clone());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        revisions.changed().await.unwrap();
        assert!(state.owner().controller().state.operator_presence.away);
        timer_cancel.cancel();
        timer.await.unwrap().unwrap();
        state.record_operator_activity().await;
        assert!(
            state.owner().controller().state.operator_presence.away,
            "activity never dismisses AFK"
        );
        let connection = rusqlite::Connection::open(crate::database::database_path()).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_afk_return BEFORE UPDATE ON operator_away BEGIN SELECT RAISE(ABORT, 'test commit failure'); END;").unwrap();
        let before_failure = state.config_mutation.lock().await.last_activity;
        assert!(state.set_operator_away(false).await.is_err());
        assert_eq!(
            state.config_mutation.lock().await.last_activity,
            before_failure
        );
        connection
            .execute_batch("DROP TRIGGER reject_afk_return; BEGIN IMMEDIATE;")
            .unwrap();
        let returning = tokio::spawn({
            let state = state.clone();
            async move { state.set_operator_away(false).await }
        });
        // The return owns the coordinator while SQLite is held by another writer.
        loop {
            if state.config_mutation.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(20)).await;
        connection.execute_batch("ROLLBACK;").unwrap();
        returning.await.unwrap().unwrap();
        assert_eq!(
            state.config_mutation.lock().await.last_activity,
            Some(Instant::now())
        );
        tokio::time::advance(Duration::from_secs(59)).await;
        state.expire_inactivity(&cancellation).await.unwrap();
        assert!(!state.owner().controller().state.operator_presence.away);
        {
            let mut clock = state.config_mutation.lock().await;
            state.configure_inactivity(&mut clock, 2);
        }
        tokio::time::advance(Duration::from_secs(119)).await;
        state.expire_inactivity(&cancellation).await.unwrap();
        assert!(!state.owner().controller().state.operator_presence.away);
        {
            let mut clock = state.config_mutation.lock().await;
            state.configure_inactivity(&mut clock, 0);
        }
        tokio::time::advance(Duration::from_secs(600)).await;
        state.expire_inactivity(&cancellation).await.unwrap();
        assert!(!state.owner().controller().state.operator_presence.away);
        state.set_operator_away(true).await.unwrap();
        let mut restarted = super::super::tests::test_runtime_state();
        Arc::get_mut(&mut restarted).unwrap().committed =
            Some(crate::database::subscribe_committed_state().unwrap());
        assert!(restarted.owner().controller().state.operator_presence.away);
        let mut clock = restarted.config_mutation.lock().await;
        restarted.configure_inactivity(&mut clock, 1);
        assert_eq!(clock.last_activity, Some(Instant::now()));
    }
}
