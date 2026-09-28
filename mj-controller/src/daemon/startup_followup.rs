use super::*;

#[derive(Debug)]
pub(super) struct StartupRejected(pub String);

impl std::fmt::Display for StartupRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for StartupRejected {}

/// One startup step's durable command id. The worker relay accepts only ASCII
/// alphanumerics, `-` and `_` in a command id, so the step key is joined with
/// `-`, never `:`: a rejected id leaves `mj new --model` and every spawned
/// child waiting forever for a step the worker will never admit.
pub(crate) fn startup_step_id(group_id: &str, key: &str) -> String {
    format!("{group_id}-{key}")
}

impl RuntimeState {
    pub(crate) async fn queue_api_followup(
        self: &Arc<Self>,
        session_id: &str,
        group_id: String,
        followup: crate::server::api::StartFollowup,
    ) -> Result<()> {
        let mut steps = Vec::new();
        for (key, value) in [("model", followup.model), ("effort", followup.effort)] {
            if let Some(value) = value {
                steps.push((
                    startup_step_id(&group_id, key),
                    StartupStep::Configure {
                        key: key.to_owned(),
                        value,
                        optional: false,
                    },
                ));
            }
        }
        if followup.fast_mode {
            steps.push((
                startup_step_id(&group_id, "fast-mode"),
                StartupStep::Configure {
                    key: "fast-mode".to_owned(),
                    value: "on".to_owned(),
                    optional: true,
                },
            ));
        }
        if let Some(text) = followup.prompt {
            steps.push((
                startup_step_id(&group_id, "prompt"),
                StartupStep::ApiPrompt { text },
            ));
        }
        // The daemon's startup owner cancels and joins every drain during
        // handoff. Waiting for readiness is resumable and holds no admission.
        self.queue_startup_steps_with_ids(
            session_id,
            steps,
            Some(group_id),
            &CancellationToken::new(),
        )
        .await
    }

    pub(crate) async fn cancel_api_followup(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let _enqueue = self.startup_enqueue.lock().await;
        let task = {
            let mut queues = self
                .startup_prompts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            queues.get_mut(session_id).and_then(|queue| {
                queue.cancel.cancel();
                queue.task.take()
            })
        };
        if let Some(mut task) = task {
            match tokio::time::timeout(Duration::from_secs(1), &mut task).await {
                Ok(result) => result.context("startup cancellation task failed")?,
                Err(_) => {
                    task.abort();
                    let result = task.await;
                    if let Err(error) = result
                        && !error.is_cancelled()
                    {
                        return Err(error).context("startup cancellation task failed");
                    }
                }
            }
        }
        let id = session_id.to_owned();
        blocking(move || crate::database::cancel_startup_groups(&id)).await?;
        self.startup_prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(session_id);
        let id = session_id.to_owned();
        if let Some(delivery) =
            blocking(move || crate::database::next_startup_delivery(&id)).await?
        {
            self.start_persisted_startup_delivery(delivery, &CancellationToken::new());
        }
        Ok(())
    }
}

pub(crate) async fn configure_startup(
    handle: &mj_client::session::SessionHandle,
    command_id: &str,
    key: &str,
    value: &str,
    optional: bool,
) -> Result<Option<u64>> {
    let mut view_handle = handle.clone();
    let deadline = tokio::time::Instant::now() + crate::controller::NATIVE_SESSION_STARTUP_TIMEOUT;
    loop {
        let view = view_handle.view();
        if view.connected
            && view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.native_session_is_ready())
        {
            break;
        }
        tokio::time::timeout_at(deadline, view_handle.changed())
            .await
            .context("session did not become ready for startup configuration")??;
    }
    // A retained acceptance owns the old decision, even when the current
    // model advertises different choices after reconnecting.
    let receipt = handle.command_receipt(command_id.to_owned()).await?;
    if receipt.is_none() {
        let snapshot = handle
            .view()
            .snapshot
            .context("session configuration is unavailable")?;
        let offered =
            mj_core::acp::session_config_choices(&snapshot.operational.config_options, key)
                .iter()
                .any(|choice| choice.value == value);
        if !offered {
            if optional {
                return Ok(None);
            }
            return Err(
                StartupRejected(format!("this agent does not offer {value} as a {key}")).into(),
            );
        }
    }
    let ordinal = handle
        .submit_durable(
            command_id.to_owned(),
            RelayCommand::SetConfig {
                key: key.to_owned(),
                value: value.to_owned(),
            },
        )
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(receipt) = handle.command_receipt(command_id.to_owned()).await? {
            if let Some(error) = receipt.failure {
                if optional {
                    tracing::warn!(key, value, %error, "optional startup configuration was rejected");
                    return Ok(Some(ordinal));
                }
                return Err(StartupRejected(error).into());
            }
            if matches!(
                receipt.outcome,
                Some(mj_core::relay::RelayCommandOutcome::Configured)
            ) {
                handle.sync_now().await?;
                return Ok(Some(ordinal));
            }
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "startup configuration did not finish within 60 seconds"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `validate_identifier` in `mj-worker/src/relay/commands.rs`.
    fn relay_accepts(command_id: &str) -> bool {
        (8..=128).contains(&command_id.len())
            && command_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
    }

    #[test]
    fn startup_step_ids_satisfy_the_worker_relay_identifier_rule() {
        for group in [
            new_command_id("api-startup").unwrap(),
            "subagent-spawn-3d79eb2c70f5f5d16e524b460525d1a7".to_owned(),
        ] {
            for key in ["model", "effort", "fast-mode", "prompt"] {
                let id = startup_step_id(&group, key);
                assert!(relay_accepts(&id), "{id}");
                assert!(id.starts_with(&group) && id.ends_with(key), "{id}");
            }
        }
    }
}
