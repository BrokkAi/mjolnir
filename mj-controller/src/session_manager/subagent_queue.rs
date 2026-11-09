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
                    let queue = if queue_due || !waits {
                        Some(client.subagent_requests().await?)
                    } else {
                        None
                    };
                    if !waits {
                        return Ok::<_, anyhow::Error>((queue, None));
                    }
                    queue_due = false;
                    waiting_tx.send_if_modified(|waiting| !std::mem::replace(waiting, true));
                    let (after_ordinal, state_digest) = observed
                        .clone()
                        .map_or((0, None), |(ordinal, digest)| (ordinal, Some(digest)));
                    let change = client.wait_for_change(after_ordinal, state_digest).await?;
                    Ok((queue, Some(change)))
                }
                .await;
                let pause = match result {
                    Ok((queue, change)) => {
                        delay = RECONNECT_INTERVAL;
                        if let Some((requests, completions)) = queue {
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
                        let error = format!("delegation queue synchronization failed: {error:#}");
                        tx.send_if_modified(|view| {
                            if view.error.as_deref() == Some(&error) { return false }
                            tracing::warn!(session_id = %target.session_id, %error, "delegation is unavailable; worker controls remain attached");
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
