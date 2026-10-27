use super::*;

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

/// Decode AIR tasks independently of the harness and owning session.
pub(super) fn air_task_update(
    update: &serde_json::Value,
) -> std::result::Result<Option<Option<AirTaskUpdate>>, String> {
    let Some(kind) = update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    if !kind.starts_with("async_task_") {
        return Ok(None);
    }
    if update.get("taskType").and_then(serde_json::Value::as_str) == Some("turn") {
        return Ok(Some(None));
    }
    let task_id = update
        .get("asyncTaskId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| format!("{kind} requires a non-empty asyncTaskId"))?
        .to_owned();
    let description = update
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .or_else(|| update.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_owned);
    let can_stop = update.get("canStop").and_then(serde_json::Value::as_bool);
    let change = match kind {
        "async_task_spawned" => AirTaskUpdate::Spawned {
            task_id,
            description: description.ok_or("async_task_spawned requires name or description")?,
            can_stop: can_stop.ok_or("async_task_spawned requires canStop")?,
        },
        "async_task_progress" => AirTaskUpdate::Changed {
            task_id,
            description,
            can_stop,
        },
        "async_task_state_update" => {
            match update.get("state").and_then(serde_json::Value::as_str) {
                Some("running" | "paused") => AirTaskUpdate::Changed {
                    task_id,
                    description,
                    can_stop,
                },
                Some(state @ ("completed" | "failed" | "stopped")) => AirTaskUpdate::Finished {
                    task_id,
                    settled: state != "stopped",
                },
                state => return Err(format!("unknown AIR async task state {state:?}")),
            }
        }
        _ => return Err(format!("unknown AIR async task update {kind:?}")),
    };
    Ok(Some(Some(change)))
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "_session/async_task/stop", response = AirAsyncTaskStopResponse)]
pub(super) struct AirAsyncTaskStopRequest {
    #[serde(rename = "sessionId")]
    pub(super) session_id: SessionId,
    #[serde(rename = "asyncTaskId")]
    pub(super) async_task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(super) struct AirAsyncTaskStopResponse {
    stopped: bool,
}

pub(super) const BACKGROUND_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn stop_background_task(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    terminals: &TerminalRegistry,
    target: mj_core::relay::BackgroundTaskStopTarget,
) -> std::result::Result<(), String> {
    match target {
        mj_core::relay::BackgroundTaskStopTarget::NativeAgent {
            session_id: child_id,
        } => {
            let request = NativeAgentCancelRequest {
                session_id: session_id.clone(),
                subagent_session_id: child_id,
            };
            match tokio::time::timeout(
                BACKGROUND_TASK_STOP_TIMEOUT,
                connection.send_request(request).block_task(),
            )
            .await
            {
                Ok(Ok(response)) if response.cancelled => Ok(()),
                Ok(Ok(_)) => Err("native agent is no longer running".into()),
                Ok(Err(error)) => Err(format!("cancel native agent: {error}")),
                Err(_) => Err("timed out cancelling native agent".into()),
            }
        }
        mj_core::relay::BackgroundTaskStopTarget::HostedTerminal { terminal_id } => {
            if terminals.kill(&terminal_id) {
                Ok(())
            } else {
                Err("background task is no longer running".into())
            }
        }
        mj_core::relay::BackgroundTaskStopTarget::AirAsyncTask {
            session_id: owner,
            task_id,
        } => {
            let request = AirAsyncTaskStopRequest {
                session_id: owner.into(),
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
                Ok(Err(error)) => Err(format!("stop AIR background task: {error}")),
                Err(_) => Err("timed out stopping AIR background task".into()),
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

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "_session/subagent/cancel", response = NativeAgentCancelResponse)]
struct NativeAgentCancelRequest {
    #[serde(rename = "sessionId")]
    session_id: SessionId,
    #[serde(rename = "subagentSessionId")]
    subagent_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
struct NativeAgentCancelResponse {
    cancelled: bool,
}
