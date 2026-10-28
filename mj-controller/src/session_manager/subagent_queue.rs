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
        let target = target.clone();
        let task = tokio::spawn(async move {
            let mut client = None;
            let mut delay = RECONNECT_INTERVAL;
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
                    }
                    client
                        .as_mut()
                        .expect("connected queue")
                        .subagent_requests()
                        .await
                }
                .await;
                let pause = match result {
                    Ok((requests, completions)) => {
                        delay = RECONNECT_INTERVAL;
                        tx.send_if_modified(|view| {
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
                        if *activity.borrow() {
                            SESSION_SYNC_INTERVAL
                        } else {
                            QUIET_SESSION_SYNC_INTERVAL
                        }
                    }
                    Err(error) => {
                        client = None;
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
        Self { view, task, active }
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
