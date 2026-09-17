use super::*;

pub(super) const CLAUDE_BACKGROUND_TASKS_CHANGED_SUBTYPE: &str = "background_tasks_changed";

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ClaudeBackgroundTaskPayload {
    task_id: String,
    description: String,
    #[serde(default)]
    ambient: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification)]
#[notification(method = "_claude/sdkMessage")]
pub(super) struct ClaudeSdkMessageNotification {
    #[serde(rename = "sessionId")]
    pub(super) session_id: SessionId,
    pub(super) message: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification)]
#[notification(method = "_x.ai/session/update")]
pub(super) struct GrokUsageNotification {
    #[serde(rename = "sessionId")]
    pub(super) session_id: SessionId,
    pub(super) update: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification)]
#[notification(method = "session/update")]
pub(super) struct RawSessionNotification {
    #[serde(rename = "sessionId")]
    pub(super) session_id: SessionId,
    pub(super) update: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaudeAsyncTaskControlUpdate {
    Set { task_id: String, can_stop: bool },
    Ignore,
}

pub(super) fn claude_async_task_control_update(
    update: &serde_json::Value,
) -> std::result::Result<Option<ClaudeAsyncTaskControlUpdate>, String> {
    let Some(kind) = update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    if !kind.starts_with("async_task_") {
        return Ok(None);
    }
    let task_id = || {
        update
            .get("asyncTaskId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{kind} requires a non-empty asyncTaskId"))
    };
    match kind {
        "async_task_spawned" => Ok(Some(ClaudeAsyncTaskControlUpdate::Set {
            task_id: task_id()?,
            can_stop: update
                .get("canStop")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| "async_task_spawned requires canStop".to_owned())?,
        })),
        "async_task_state_update" => {
            let state = update
                .get("state")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "async_task_state_update requires state".to_owned())?;
            if matches!(state, "completed" | "failed" | "stopped") {
                Ok(Some(ClaudeAsyncTaskControlUpdate::Set {
                    task_id: task_id()?,
                    can_stop: false,
                }))
            } else if matches!(state, "running" | "paused") {
                Ok(Some(ClaudeAsyncTaskControlUpdate::Ignore))
            } else {
                Err(format!("unknown Claude async task state {state:?}"))
            }
        }
        "async_task_progress" => {
            task_id()?;
            Ok(Some(ClaudeAsyncTaskControlUpdate::Ignore))
        }
        _ => Err(format!("unknown Claude async task update {kind:?}")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "_session/async_task/stop", response = ClaudeAsyncTaskStopResponse)]
pub(super) struct ClaudeAsyncTaskStopRequest {
    #[serde(rename = "sessionId")]
    pub(super) session_id: SessionId,
    #[serde(rename = "asyncTaskId")]
    pub(super) async_task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(super) struct ClaudeAsyncTaskStopResponse {
    stopped: bool,
}

/// Extract the one Claude SDK level signal Hel subscribes to. Edge lifecycle
/// messages and foreground activity are deliberately ignored: their ordering
/// is unspecified and task starts include foreground work.
pub(super) fn claude_background_tasks(
    message: &serde_json::Value,
) -> std::result::Result<Option<Vec<ClaudeBackgroundTask>>, serde_json::Error> {
    if message.get("type").and_then(serde_json::Value::as_str) != Some("system")
        || message.get("subtype").and_then(serde_json::Value::as_str)
            != Some(CLAUDE_BACKGROUND_TASKS_CHANGED_SUBTYPE)
    {
        return Ok(None);
    }
    let payload = serde_json::from_value::<Vec<ClaudeBackgroundTaskPayload>>(
        message
            .get("tasks")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )?;
    Ok(Some(
        payload
            .into_iter()
            .filter(|task| !task.ambient)
            .map(|task| ClaudeBackgroundTask {
                task_id: task.task_id,
                description: task.description,
            })
            .collect(),
    ))
}

pub(super) const BACKGROUND_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn stop_background_task(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    terminals: &TerminalRegistry,
    target: mj_core::relay::BackgroundTaskStopTarget,
) -> std::result::Result<(), String> {
    match target {
        mj_core::relay::BackgroundTaskStopTarget::HostedTerminal { terminal_id } => {
            if terminals.kill(&terminal_id) {
                Ok(())
            } else {
                Err("background task is no longer running".into())
            }
        }
        mj_core::relay::BackgroundTaskStopTarget::ClaudeAsyncTask { task_id } => {
            let request = ClaudeAsyncTaskStopRequest {
                session_id: session_id.clone(),
                async_task_id: task_id,
            };
            match tokio::time::timeout(
                BACKGROUND_TASK_STOP_TIMEOUT,
                connection.send_request(request).block_task(),
            )
            .await
            {
                Ok(Ok(response)) if response.stopped => Ok(()),
                Ok(Ok(_)) => Err("background task is no longer stoppable".into()),
                Ok(Err(error)) => Err(format!("stop Claude background task: {error}")),
                Err(_) => Err("timed out stopping Claude background task".into()),
            }
        }
    }
}

pub(super) async fn resolve_background_task_stop(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    terminals: &TerminalRegistry,
    target: mj_core::relay::BackgroundTaskStopTarget,
    resolved: oneshot::Sender<std::result::Result<(), String>>,
) {
    let result = stop_background_task(connection, session_id, terminals, target).await;
    if resolved.send(result).is_err() {
        tracing::debug!(
            session_id = %session_id,
            operation = "stop_background_task",
            "background task stop receiver was already closed"
        );
    }
}
