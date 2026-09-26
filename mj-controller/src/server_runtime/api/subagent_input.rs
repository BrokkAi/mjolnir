//! Deferred child input uses the parent's existing durable request queue.

use super::*;
use mj_core::subagent::{SubagentToolAction, SubagentToolRequest};

impl ApiBackend {
    pub(super) async fn subagent_input_progress(&self, parent: &str) -> Result<InputProgress> {
        let Some(handle) = self.session_handle(parent.to_owned()).await? else {
            return Ok(InputProgress::default());
        };
        // The snapshot that scheduled this tool includes preceding inputs.
        // The actor publishes completion updates; forcing a sync here would
        // race the lease used to deliver those very completions.
        Ok(handle
            .view()
            .snapshot
            .as_ref()
            .map(InputProgress::from_snapshot)
            .unwrap_or_default())
    }

    pub(super) async fn deliver_subagent_input(
        &self,
        parent: &str,
        child: &str,
        message: &str,
        request: &SubagentToolRequest,
    ) -> Result<u64> {
        let command_id = format!("subagent-input-{}", request.request_id);
        // Completed turns survive worker history collection in the existing
        // projection. Consult it even when the child was subsequently closed.
        if let Some(ordinal) = prompt_acceptance(child, &command_id).await? {
            return Ok(ordinal);
        }
        let elapsed_ms = mj_core::clock::epoch_millis()
            .saturating_sub(request.created_at_ms)
            .max(0) as u64;
        let remaining = START_DEADLINE.saturating_sub(Duration::from_millis(elapsed_ms));
        let deadline = tokio::time::Instant::now() + remaining;
        loop {
            let notified = self.starts_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            ensure!(
                !self.exports.close_is_requested(child),
                "child session is closing; queued input was not delivered"
            );
            let record = self
                .exports
                .session_record(child)
                .context("child session no longer exists")?;
            ensure!(
                matches!(
                    record.state,
                    SessionState::Provisioning
                        | SessionState::Running
                        | SessionState::Disconnected
                        | SessionState::Checkpointing
                        | SessionState::Parked
                ),
                "child session is {:?}; queued input was not delivered{}",
                record.state,
                record
                    .last_error
                    .as_ref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            );
            ensure!(
                tokio::time::Instant::now() < deadline,
                "child was not ready for queued input within 30 minutes"
            );
            match self.start_status(child.to_owned()).await? {
                Some(StartStatus::Failed { message }) => bail!("child startup failed: {message}"),
                Some(StartStatus::Pending) => {
                    tokio::select! {
                        () = &mut notified => {},
                        () = tokio::time::sleep(START_POLL) => {},
                    }
                    continue;
                }
                _ => {}
            }
            if crate::upgrade::is_draining() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            if record.state == SessionState::Parked || self.exports.subagent_park_running(child) {
                let parent_id = parent.to_owned();
                let child_id = child.to_owned();
                blocking("admit parked child restart", move || {
                    Controller::load()?.ensure_subagent_slot_available(&parent_id, Some(&child_id))
                })
                .await?;
                self.unpark_child(child).await?;
                continue;
            }
            let Some(mut handle) = self.session_handle(child.to_owned()).await? else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            let view = handle.view();
            if let Some(ViewError::TargetMissing(detail)) = view.error {
                bail!("child lost its target: {detail}");
            }
            if !view.connected
                || !view
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.operational.native_session_is_ready())
            {
                if let Ok(Err(error)) = tokio::time::timeout(START_POLL, handle.changed()).await {
                    tracing::debug!(child, %error, "reacquiring child actor while input is queued");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                continue;
            }
            let Ok(_submission) =
                crate::upgrade::activity_unless_draining("subagent input delivery")
            else {
                continue;
            };
            // Synchronization materializes every accepted prompt before we
            // decide whether this request still needs a relay submission.
            if let Err(error) = handle.sync_now().await {
                if matches!(
                    handle.view().error,
                    Some(ViewError::ProjectionIntegrity(_) | ViewError::TargetMissing(_))
                ) {
                    return Err(error);
                }
                // Sync has no delivery side effect. A park may reserve the
                // actor after our view was read; wait for its new owner.
                tracing::debug!(child, %error, "child not available for queued input yet");
                drop(_submission);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if let Some(ordinal) = prompt_acceptance(child, &command_id).await? {
                return Ok(ordinal);
            }
            ensure!(
                !self.exports.close_is_requested(child),
                "child session is closing; queued input was not delivered"
            );
            let result = handle
                .submit(
                    command_id.clone(),
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new(message))],
                    },
                )
                .await;
            match result {
                Ok(ordinal) => return Ok(ordinal),
                Err(error)
                    if error
                        .downcast_ref::<mj_client::session::DeliveryUnconfirmed>()
                        .is_some() =>
                {
                    // Never treat a missing acknowledgement as permission to
                    // make a new prompt. Expose the uncertainty to the parent.
                    return Err(error.context(
                        "input delivery is unconfirmed; do not resend without checking the child",
                    ));
                }
                Err(error) => {
                    if self
                        .exports
                        .session_record(child)
                        .is_some_and(|r| r.state == SessionState::Parked)
                        || self.exports.subagent_park_running(child)
                    {
                        // The existing park admission explicitly refused the
                        // command. Re-enter readiness with the same identity.
                        drop(_submission);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }
}

async fn prompt_acceptance(child: &str, command: &str) -> Result<Option<u64>> {
    let child = child.to_owned();
    let command = command.to_owned();
    blocking("reconcile sub-agent input", move || {
        crate::database::load_prompt_acceptance(&child, &command)
    })
    .await
}

#[derive(Default)]
pub(super) struct InputProgress {
    pending: BTreeMap<String, Vec<String>>,
    deliveries: BTreeMap<String, Vec<serde_json::Value>>,
}

impl InputProgress {
    pub(super) fn from_snapshot(snapshot: &mj_core::state::ManagedSessionSnapshot) -> Self {
        let mut progress = Self::default();
        let mut requests = snapshot.subagent_requests.iter().collect::<Vec<_>>();
        requests.sort_by_key(|r| (r.created_at_ms, &r.request_id));
        for request in requests {
            if let SubagentToolAction::SendInput {
                child_session_id, ..
            } = &request.action
            {
                progress
                    .pending
                    .entry(child_session_id.clone())
                    .or_default()
                    .push(request.request_id.clone());
            }
        }
        for result in &snapshot.subagent_results {
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&result.message) else {
                continue;
            };
            let Some(child) = value
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            else {
                continue;
            };
            // These fields distinguish input results from spawn/close/wait.
            if value.get("created_at_ms").is_none() || value.get("status").is_none() {
                continue;
            }
            value["request_id"] = result.request_id.clone().into();
            progress.deliveries.entry(child).or_default().push(value);
        }
        for deliveries in progress.deliveries.values_mut() {
            deliveries.sort_by(|a, b| {
                (a["created_at_ms"].as_i64(), a["request_id"].as_str())
                    .cmp(&(b["created_at_ms"].as_i64(), b["request_id"].as_str()))
            });
        }
        progress
    }

    pub fn status(
        &self,
        child: &str,
        current: (String, Option<String>, bool),
    ) -> (String, Option<String>, bool) {
        if matches!(current.0.as_str(), "stopping" | "stopped") {
            return current;
        }
        if self
            .pending
            .get(child)
            .is_some_and(|requests| !requests.is_empty())
        {
            return ("running".into(), None, false);
        }
        if let Some(last) = self.deliveries.get(child).and_then(|items| items.last())
            && last["status"] == "failed"
        {
            return (
                "failed".into(),
                Some(format!(
                    "Input {}: {}",
                    last["request_id"].as_str().unwrap_or_default(),
                    last["error"].as_str().unwrap_or("delivery failed")
                )),
                true,
            );
        }
        current
    }

    pub fn annotate(&self, child: &str, entry: &mut serde_json::Value) {
        if let Some(pending) = self.pending.get(child) {
            entry["pending_inputs"] = serde_json::json!(pending);
        }
        if let Some(deliveries) = self.deliveries.get(child) {
            entry["input_deliveries"] = serde_json::json!(deliveries);
        }
    }
}
