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
        let steps = api_followup_steps(&group_id, followup);
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

/// One peer message held until startup can take it. The command id is the
/// turn id a later delivery uses, so a retry of the same request is the same
/// step. `created_at_ms` is the first request's clock; a retry must not
/// rewrite it.
#[derive(Clone, Debug)]
pub(crate) struct QueuedSessionMessage {
    pub request_id: String,
    pub sender_id: String,
    pub sender_title: String,
    pub text: String,
    pub created_at_ms: u64,
    pub waited_on_group: Option<String>,
}

pub(crate) fn session_message_startup_step(
    message: &QueuedSessionMessage,
) -> (String, StartupStep) {
    (
        mj_core::continuation::GeneratedPrompt::SessionMessageTurn.command_id(&message.request_id),
        StartupStep::SessionMessage {
            sender_id: message.sender_id.clone(),
            sender_title: message.sender_title.clone(),
            text: message.text.clone(),
            created_at_ms: message.created_at_ms,
            waited_on_group: message.waited_on_group.clone(),
        },
    )
}

impl RuntimeState {
    /// Accept one peer message into the session's startup queue. The row stays
    /// ungrouped so it cannot become the status `mj wait` reads.
    pub(crate) async fn queue_startup_session_message(
        self: &Arc<Self>,
        session_id: &str,
        message: QueuedSessionMessage,
    ) -> Result<()> {
        let (command_id, step) = session_message_startup_step(&message);
        self.queue_startup_steps_with_ids(
            session_id,
            vec![(command_id, step)],
            None,
            &CancellationToken::new(),
        )
        .await
    }
}

pub(crate) fn api_followup_steps(
    group_id: &str,
    followup: crate::server::api::StartFollowup,
) -> Vec<(String, StartupStep)> {
    let mut steps = Vec::new();
    for (key, value) in [("model", followup.model), ("effort", followup.effort)] {
        if let Some(value) = value {
            steps.push((
                startup_step_id(group_id, key),
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
            startup_step_id(group_id, "fast-mode"),
            StartupStep::Configure {
                key: "fast-mode".to_owned(),
                value: "on".to_owned(),
                optional: true,
            },
        ));
    }
    if let Some(text) = followup.prompt {
        steps.push((
            startup_step_id(group_id, "prompt"),
            StartupStep::ApiPrompt { text },
        ));
    }
    steps
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
    let snapshot = handle
        .view()
        .snapshot
        .context("session configuration is unavailable")?;
    let offered = mj_core::acp::session_config_choices(&snapshot.operational.config_options, key)
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
    // The step's stable command id makes a retry after a daemon restart a
    // duplicate the worker answers with the original acceptance.
    match handle
        .set_config_with_id(command_id.to_owned(), key.to_owned(), value.to_owned())
        .await
    {
        Ok(ordinal) => Ok(Some(ordinal)),
        Err(error) if optional => {
            tracing::warn!(key, value, %error, "optional startup configuration was rejected");
            Ok(None)
        }
        Err(error) => Err(StartupRejected(format!("{error:#}")).into()),
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

    // Hard-won: 89874f12: the worker rejected colon-delimited startup IDs and retried forever.
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
