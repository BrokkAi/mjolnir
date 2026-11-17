//! Delegation replication owns its transport, so reports cannot take control down.
use super::*;
use mj_core::subagent::{SubagentCompletion, SubagentToolRequest};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    requests: mj_core::snapshot_map::SharedSnapshot<Vec<SubagentToolRequest>>,
    completions: mj_core::snapshot_map::SharedSnapshot<Vec<SubagentCompletion>>,
    error: Option<String>,
}

pub(super) struct SubagentQueueObserver {
    view: watch::Receiver<Observation>,
    task: tokio::task::JoinHandle<()>,
    active: watch::Sender<bool>,
    /// Counts the relay changes this connection's waits observed and the
    /// queue changes it published, for the session actor to sync on.
    relay_changes: watch::Receiver<u64>,
    /// Whether this connection is waiting for changes on a worker that
    /// supports it, so the actor need not poll.
    waiting: watch::Receiver<bool>,
    /// The worker ordinal and state digest the actor last synced, where a new
    /// connection's first wait starts.
    synced: watch::Sender<Option<(u64, String)>>,
}

impl Drop for SubagentQueueObserver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SubagentQueueObserver {
    pub async fn stop(mut self) {
        self.task.abort();
        // Borrow the handle because Drop also owns cancellation.
        match (&mut self.task).await {
            Err(error) if !error.is_cancelled() => {
                tracing::error!(%error, "delegation observer failed during retirement")
            }
            _ => {}
        }
    }

    pub fn spawn(target: &RelaySessionTarget) -> Self {
        let (tx, view) = watch::channel(Observation {
            requests: Default::default(),
            completions: Default::default(),
            error: Some("delegation queue is connecting".into()),
        });
        let (active, activity) = watch::channel(true);
        let (relay_changed, relay_changes) = watch::channel(0_u64);
        let (waiting_tx, waiting) = watch::channel(false);
        let (synced, synced_rx) = watch::channel(None::<(u64, String)>);
        let target = target.clone();
        let task = tokio::spawn(async move {
            let mut client: Option<crate::worker_client::RelayClient> = None;
            let mut delay = RECONNECT_INTERVAL;
            // What the last change wait reported (on a new connection, what the
            // actor last synced), and whether the queue must be read before the
            // next wait (always first on a new connection).
            let mut observed: Option<(u64, String)> = None;
            let mut queue_due = true;
            loop {
                let result = async {
                    if client.is_none() {
                        client = Some(
                            crate::worker_client::RelayClient::connect(
                                &target.spec,
                                &target.session_id,
                            )
                            .await?,
                        );
                        observed = synced_rx.borrow().clone();
                        queue_due = true;
                    }
                    let client = client.as_mut().expect("connected queue");
                    let waits =
                        client.protocol_version() >= mj_core::relay::RELAY_CHANGE_WAIT_PROTOCOL;
                    if queue_due || !waits {
                        let (requests, completions) = client.subagent_requests().await?;
                        // Wake the actor now. The following change wait can
                        // hold for its full bound while this request needs a reply.
                        let published = tx.send_if_modified(|view| {
                            let next = Observation {
                                requests,
                                completions,
                                error: None,
                            };
                            if *view == next {
                                false
                            } else {
                                *view = next;
                                true
                            }
                        });
                        // The actor reads the queue on its next sync, which
                        // a quiet session no longer runs on a short timer.
                        if published {
                            relay_changed.send_modify(|count| *count = count.wrapping_add(1));
                        }
                    }
                    if !waits {
                        return Ok(None);
                    }
                    queue_due = false;
                    waiting_tx.send_if_modified(|waiting| !std::mem::replace(waiting, true));
                    let (after_ordinal, state_digest) = observed
                        .clone()
                        .map_or((0, None), |(ordinal, digest)| (ordinal, Some(digest)));
                    client
                        .wait_for_change(after_ordinal, state_digest)
                        .await
                        .map(Some)
                }
                .await;
                let pause = match result {
                    Ok(change) => {
                        delay = RECONNECT_INTERVAL;
                        match change {
                            // A worker that waits for changes answers as they
                            // happen; following a busy one no more often than
                            // the polling cadence keeps the daemon's work per
                            // session where it was.
                            Some(change) => {
                                let now = (change.latest_ordinal, change.state_digest);
                                let relay = observed.as_ref() != Some(&now);
                                if relay {
                                    observed = Some(now);
                                    relay_changed
                                        .send_modify(|count| *count = count.wrapping_add(1));
                                }
                                queue_due = change.subagent_queue_changed;
                                if relay || queue_due {
                                    SESSION_SYNC_INTERVAL
                                } else {
                                    Duration::ZERO
                                }
                            }
                            None if *activity.borrow() => SESSION_SYNC_INTERVAL,
                            None => QUIET_SESSION_SYNC_INTERVAL,
                        }
                    }
                    Err(error) => {
                        client = None;
                        waiting_tx.send_if_modified(|waiting| std::mem::replace(waiting, false));
                        // The actor polls again until a wait works, so it
                        // must not miss whatever the lost wait would have seen.
                        relay_changed.send_modify(|count| *count = count.wrapping_add(1));
                        // Reachability belongs to the control actor. A lost wait
                        // is also normal when its lifecycle owner stops a worker.
                        let transport_dead =
                            crate::worker_client::RelayTransportDead::marks(&error);
                        let error = format!("delegation queue synchronization failed: {error:#}");
                        tx.send_if_modified(|view| {
                            if view.error.as_deref() == Some(&error) { return false }
                            if !transport_dead && view.error.as_deref().is_none_or(|previous| previous == "delegation queue is connecting") {
                                tracing::warn!(session_id = %target.session_id, %error, "delegation is unavailable; worker controls remain attached");
                            } else {
                                tracing::debug!(session_id = %target.session_id, %error, "delegation synchronization retry");
                            }
                            view.error = Some(error);
                            true
                        });
                        let pause = delay;
                        delay = delay.saturating_mul(2).min(RECONNECT_BACKOFF_CEILING);
                        pause
                    }
                };
                tokio::time::sleep(pause).await;
            }
        });
        Self {
            view,
            task,
            active,
            relay_changes,
            waiting,
            synced,
        }
    }

    /// Record what the actor's sync saw, for a new connection's first wait.
    pub fn note_synced(&self, latest_ordinal: u64, state_digest: &str) {
        self.synced.send_if_modified(|synced| {
            let next = Some((latest_ordinal, state_digest.to_owned()));
            if *synced == next {
                false
            } else {
                *synced = next;
                true
            }
        });
    }

    /// Whether the actor can rely on this connection's waits instead of
    /// polling its worker.
    pub fn waits_for_changes(&self) -> bool {
        *self.waiting.borrow() && !self.task.is_finished()
    }

    /// Resolves when a wait observed a relay change, the queue changed, or the
    /// waits failed.
    pub async fn relay_changed(&mut self) {
        if self.relay_changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    pub fn observe(
        &self,
        active: bool,
    ) -> (
        mj_core::snapshot_map::SharedSnapshot<Vec<SubagentToolRequest>>,
        mj_core::snapshot_map::SharedSnapshot<Vec<SubagentCompletion>>,
        Option<String>,
    ) {
        self.active.send_if_modified(|previous| {
            if *previous == active {
                false
            } else {
                *previous = active;
                true
            }
        });
        let observation = self.view.borrow();
        let error = if self.task.is_finished() {
            Some("delegation queue supervisor stopped unexpectedly".into())
        } else {
            observation.error.clone()
        };
        (
            observation.requests.clone(),
            observation.completions.clone(),
            error,
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session_manager::tests::{RELAY_REQUEST_LOG, leased_relay_target};
    use futures::FutureExt as _;
    use mj_core::relay::RELAY_CHANGE_WAIT_BOUND;
    use mj_core::subagent::SubagentToolAction;

    // Hard-won: #1580: a queue read waited for the next change wait before it was published
    #[tokio::test]
    async fn publishes_a_queue_read_before_the_next_change_wait() {
        let root = tempfile::tempdir().unwrap();
        let request = SubagentToolRequest {
            originating_command_id: None,
            request_id: "pending".into(),
            created_at_ms: 1,
            action: SubagentToolAction::ListAgents,
        };
        std::fs::write(
            root.path().join("subagent-request.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        std::fs::write(root.path().join("block-change-wait"), b"").unwrap();
        let mut observer = SubagentQueueObserver::spawn(&leased_relay_target(root.path()));

        let log = root.path().join(RELAY_REQUEST_LOG);
        tokio::time::timeout(RELAY_CHANGE_WAIT_BOUND / 2, async {
            while !std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .any(|method| method == "wait_for_change")
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the relay reached its change wait");
        // The subprocess has the wait and cannot answer it. The preceding
        // queue exchange must already have woken the actor.
        assert!(observer.relay_changed().now_or_never().is_some());
        let (requests, _, error) = observer.observe(true);
        assert_eq!(requests.as_slice(), std::slice::from_ref(&request));
        assert!(error.is_none());

        std::fs::write(root.path().join("worker-unreachable"), b"").unwrap();
        let requests_after_failure = tokio::time::timeout(
            RELAY_CHANGE_WAIT_BOUND / 2,
            observer.view.wait_for(|view| view.error.is_some()),
        )
        .await
        .expect("the change wait failure was reported")
        .unwrap()
        .requests
        .clone();
        assert_eq!(requests_after_failure.as_slice(), &[request]);
        observer.stop().await;
    }
}
