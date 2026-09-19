use super::*;

pub fn spawn_remote_dashboard_worker_poller(
    workspace_id: String,
) -> Result<RemoteDashboardWorkerPoller> {
    let channels = spawn_remote_session_manager()?;
    let crate::session_manager::RemoteSessionManagerChannels {
        targets,
        control,
        updates,
        shutdown,
        publisher,
        mut requests,
    } = channels;
    let (state_tx, state_rx) = tokio::sync::watch::channel(RuntimeStateUpdate::default());
    let (reviews_tx, reviews_rx) = tokio::sync::watch::channel(Vec::new());
    let (notices_tx, notices_rx) = tokio::sync::watch::channel(Vec::new());
    let (config_tx, config_rx) = tokio::sync::watch::channel(mj_core::config::Config::default());
    tokio::spawn(async move {
        let mut feed = spawn_runtime_feed_with(
            workspace_id,
            |workspace, revision| poll_daemon_runtime(workspace, revision, true),
            load_runtime_projection,
        );
        let mut request_order = crate::session_manager::SessionRequestOrder::new();
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { return; };
                    request_order.dispatch(request, forward_remote_session_request);
                }
                update = feed.updates.recv() => {
                    match update {
                        Some(RuntimeFeedUpdate::Snapshot(snapshot)) => {
                            config_tx.send_if_modified(|config| {
                                if *config == snapshot.config { false }
                                else { *config = snapshot.config.clone(); true }
                            });
                            state_tx.send_replace(RuntimeStateUpdate {
                                native_agents: snapshot.native_agents,
                                workspace_names: snapshot.workspace_names,
                                revision: snapshot.revision,
                                records: snapshot.records,
                                lifecycles: snapshot.lifecycles,
                                moves: snapshot.moves,
                                subagents: snapshot.subagents,
                            });
                            reviews_tx.send_replace(snapshot.reviews);
                            notices_tx.send_replace(snapshot.notices);
                        }
                        Some(RuntimeFeedUpdate::Session { session_id, view }) => {
                            if publisher.publish(session_id, *view).await.is_err() { return; }
                        }
                        Some(RuntimeFeedUpdate::Error(error)) => {
                            tracing::warn!(%error, "could not refresh sessions from controller daemon");
                        }
                        None => return,
                    }
                }
            }
        }
    });
    Ok(RemoteDashboardWorkerPoller {
        targets,
        updates,
        control,
        shutdown,
        state: state_rx,
        reviews: reviews_rx,
        notices: notices_rx,
        config: config_rx,
    })
}

pub(super) async fn poll_daemon_runtime(
    workspace_id: String,
    after_revision: u64,
    all_workspaces: bool,
) -> Result<daemon::RuntimeSnapshot> {
    let mut daemon = mj_client::daemon::connect_existing().await?;
    daemon
        .runtime_snapshot(workspace_id, after_revision, all_workspaces)
        .await
}

pub(super) async fn forward_remote_session_request(request: RemoteSessionRequest) {
    match request {
        RemoteSessionRequest::Submit {
            session_id,
            command_id,
            command,
            admission,
            reply,
        } => {
            if admission.is_some() {
                let _ = reply.send(Err(
                    "review delivery admissions cannot cross the daemon request bridge".into(),
                ));
                return;
            }
            let result = async {
                mj_client::daemon::connect_existing()
                    .await?
                    .submit_session_command(session_id, command_id, command, None)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Sync { session_id, reply } => {
            let result = async {
                mj_client::daemon::connect_existing()
                    .await?
                    .sync_session(session_id)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::RespondElicitation {
            session_id,
            elicitation_id,
            response,
            reply,
        } => {
            let result = async {
                mj_client::daemon::connect_existing()
                    .await?
                    .respond_elicitation(session_id, elicitation_id, response)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::StopBackgroundTask {
            session_id,
            background_task_id,
            reply,
        } => {
            let result = async {
                mj_client::daemon::connect_existing()
                    .await?
                    .stop_background_task(session_id, background_task_id)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Reviewer {
            session_id,
            role,
            action,
            mut reply,
        } => {
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = async {
                    mj_client::daemon::connect_existing()
                        .await?
                        .reviewer_action(session_id, role, action)
                        .await
                } => result,
            }
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
    }
}

pub fn queued_prompt_projection(
    session: &MaterializedSession,
) -> Vec<mj_core::relay::QueuedPrompt> {
    queued_prompt_entries(&session.queued_prompts)
}

pub(super) fn queued_prompt_entries(
    prompts: &[mj_core::state::MaterializedQueuedPrompt],
) -> Vec<mj_core::relay::QueuedPrompt> {
    prompts
        .iter()
        .map(|prompt| mj_core::relay::QueuedPrompt {
            id: prompt.command_id.clone(),
            text: mj_core::transcript::materialized_content_text(&prompt.content),
            attachments: Vec::new(),
            created_at_ms: prompt.queued_at_ms,
        })
        .collect()
}
