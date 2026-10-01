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
    let (quotas_tx, quotas_rx) = tokio::sync::watch::channel(Default::default());
    let (config_tx, config_rx) = tokio::sync::watch::channel(mj_core::config::Config::default());
    let (health_tx, health_rx) = tokio::sync::watch::channel(RuntimeFeedHealth::default());
    tokio::spawn(async move {
        let replica = Arc::new(tokio::sync::Mutex::new(
            mj_client::runtime_feed::RuntimeReplica::default(),
        ));
        let mut feed = spawn_runtime_feed_with(
            workspace_id,
            move |_, revision| {
                let replica = replica.clone();
                async move { poll_daemon_runtime(replica, revision).await }
            },
            load_runtime_projection,
        );
        let mut native = super::native_agents::NativeAgentLoader::default();
        let mut request_order = crate::session_manager::SessionRequestOrder::new();
        loop {
            tokio::select! {
                _ = state_tx.closed() => return,
                () = native.next(), if native.has_work() => {
                    let views = native.views_snapshot();
                    state_tx.send_if_modified(|state| {
                        if state.native_agents == views { return false; }
                        state.native_agents = views;
                        true
                    });
                    health_tx.send_if_modified(|health| {
                        let error = native.error();
                        if health.native_error == error { false } else { health.native_error = error; true }
                    });
                },
                request = requests.recv() => {
                    let Some(request) = request else { return; };
                    request_order.dispatch(request, forward_remote_session_request);
                }
                update = feed.updates.recv() => {
                    match update {
                        Some(RuntimeFeedUpdate::Snapshot(snapshot)) => {
                            let metadata = snapshot.metadata;
                            send_if_changed(&config_tx, metadata.config);
                            native.update_snapshot(snapshot.native_agents);
                            health_tx.send_if_modified(|health| {
                                let recovered = health.refresh_error.take().is_some();
                                let native_error = native.error();
                                let changed = health.native_error != native_error;
                                health.native_error = native_error;
                                recovered || changed
                            });
                            publish_runtime_state(&state_tx, RuntimeStateUpdate {
                                last_subagent_policy: metadata.last_subagent_policy,
                                native_agents: native.views_snapshot(),
                                workspace_names: metadata.workspace_names,
                                revision: snapshot.revision,
                                records: snapshot.records,
                                lifecycles: metadata.lifecycles,
                                moves: snapshot.moves,
                                subagents: snapshot.subagents,
                                launch_recency: metadata.launch_recency,
                            });
                            send_if_changed(&reviews_tx, metadata.reviews);
                            send_if_changed(&notices_tx, metadata.notices);
                            send_if_changed(&quotas_tx, metadata.quotas);
                        }
                        Some(RuntimeFeedUpdate::Session { session_id, view }) => {
                            if mirror_daemon_view(&targets, &publisher, session_id, *view).await.is_err() { return; }
                        }
                        Some(RuntimeFeedUpdate::SessionRemoved(session_id)) => {
                            mirror_daemon_removal(&targets, &session_id);
                        }
                        Some(RuntimeFeedUpdate::Error(error)) => {
                            health_tx.send_if_modified(|health| {
                                if health.refresh_error.as_ref() == Some(&error) { return false; }
                                tracing::warn!(%error, "could not refresh sessions from controller daemon");
                                health.refresh_error = Some(error);
                                true
                            });
                        }
                        None => {
                            health_tx.send_modify(|health| health.refresh_error = Some("Session updates stopped; reconnect to the controller.".into()));
                            return;
                        },
                    }
                }
            }
        }
    });
    Ok(RemoteDashboardWorkerPoller {
        updates,
        control,
        shutdown,
        state: state_rx,
        reviews: reviews_rx,
        notices: notices_rx,
        quotas: quotas_rx,
        config: config_rx,
        health: health_rx,
    })
}

/// Every send on these watches wakes the dashboard loop, and the daemon
/// republishes the whole snapshot whenever any session moves. Sending only
/// what changed keeps a streaming session from waking the surface for
/// reviews, notices, or quotas it already has.
pub(super) fn send_if_changed<T: PartialEq>(tx: &tokio::sync::watch::Sender<T>, value: T) -> bool {
    tx.send_if_modified(|current| {
        if *current == value {
            return false;
        }
        *current = value;
        true
    })
}

/// The same for the records snapshot, whose revision moves with every
/// daemon publication even when nothing the surface shows did. The revision
/// is still kept current for the next reader, without a wakeup.
pub(super) fn publish_runtime_state(
    tx: &tokio::sync::watch::Sender<RuntimeStateUpdate>,
    next: RuntimeStateUpdate,
) -> bool {
    tx.send_if_modified(|current| {
        current.revision = next.revision;
        if *current == next {
            return false;
        }
        *current = next;
        true
    })
}

/// A handle for a session exists exactly while the daemon publishes a view for
/// it, which is while the daemon runs its relay actor. The daemon decides; this
/// surface only mirrors it, so no local record or lifecycle can strand a handle.
pub(super) async fn mirror_daemon_view(
    targets: &tokio::sync::watch::Sender<Vec<WorkerPollTarget>>,
    publisher: &crate::session_manager::RemoteSessionPublisher,
    session_id: String,
    view: ManagedSessionView,
) -> Result<()> {
    targets.send_if_modified(|targets| {
        if targets.iter().any(|target| target.session_id == session_id) {
            return false;
        }
        targets.push(WorkerPollTarget {
            session_id: session_id.clone(),
            spec: crate::targets::CommandSpec::new("daemon-owned", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        });
        true
    });
    publisher.publish(session_id, view).await
}

pub(super) fn mirror_daemon_removal(
    targets: &tokio::sync::watch::Sender<Vec<WorkerPollTarget>>,
    session_id: &str,
) {
    targets.send_if_modified(|targets| {
        let before = targets.len();
        targets.retain(|target| target.session_id != session_id);
        targets.len() != before
    });
}

pub(super) async fn poll_daemon_runtime(
    replica: Arc<tokio::sync::Mutex<mj_client::runtime_feed::RuntimeReplica>>,
    after_revision: u64,
) -> Result<mj_client::runtime_feed::RuntimeProjection> {
    let mut replica = replica.lock().await;
    loop {
        let mut daemon = mj_client::daemon::connect_existing().await?;
        let wait = after_revision != 0 && after_revision == replica.projection.revision;
        let frame = daemon.runtime_changes(replica.cursor.clone(), wait).await?;
        if let Err(error) = replica.apply(frame) {
            replica.cursor = None;
            return Err(error);
        }
        if replica.cursor.is_none() {
            continue;
        }
        return Ok(replica.projection.clone());
    }
}

/// A submit the daemon answered with an error was refused, not lost, unless
/// the daemon itself says delivery is unconfirmed. Only a failed exchange with
/// the daemon leaves delivery unknown (I1-12).
fn remote_submit_failure(error: &anyhow::Error) -> mj_client::session::SubmitFailure {
    let unconfirmed = error
        .downcast_ref::<mj_client::daemon::DaemonRefusal>()
        .is_none_or(mj_client::daemon::DaemonRefusal::delivery_unconfirmed);
    // The daemon's refusal text does not say whether the worker itself
    // rejected the command, so never claim it did.
    mj_client::session::SubmitFailure {
        unconfirmed,
        refused: false,
        message: format!("{error:#}"),
    }
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
            .map_err(|error| remote_submit_failure(&error));
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

#[cfg(test)]
mod tests {
    use super::{publish_runtime_state, remote_submit_failure, send_if_changed};
    use crate::pollers::{Feed, RuntimeStateUpdate};
    use mj_client::daemon::{DaemonRefusal, RuntimeNotice};

    /// The daemon republishes its whole snapshot, at a new revision, whenever
    /// any session moves. A surface must not wake for the parts it already has.
    #[test]
    fn a_republished_snapshot_wakes_the_surface_only_for_what_changed() {
        let (state_tx, state_rx) = tokio::sync::watch::channel(RuntimeStateUpdate::default());
        let (notices_tx, notices_rx) = tokio::sync::watch::channel(Vec::<RuntimeNotice>::new());
        let mut state = Feed::new(state_rx);
        let mut notices = Feed::new(notices_rx);
        let first = RuntimeStateUpdate {
            revision: 1,
            workspace_names: [("w".to_owned(), "work".to_owned())].into(),
            ..Default::default()
        };
        let notice = RuntimeNotice {
            id: 1,
            session_id: "s".into(),
            text: "checkpoint saved".into(),
        };
        publish_runtime_state(&state_tx, first.clone());
        send_if_changed(&notices_tx, vec![notice.clone()]);
        assert_eq!(state.next_ready().map(|state| state.revision), Some(1));
        assert_eq!(notices.next_ready(), Some(vec![notice.clone()]));

        publish_runtime_state(
            &state_tx,
            RuntimeStateUpdate {
                revision: 2,
                ..first.clone()
            },
        );
        send_if_changed(&notices_tx, vec![notice.clone()]);
        assert!(
            state.next_ready().is_none(),
            "an equal snapshot woke the surface"
        );
        assert!(
            notices.next_ready().is_none(),
            "equal notices woke the surface"
        );
        assert_eq!(
            state_tx.borrow().revision,
            2,
            "the revision is still current"
        );

        let mut renamed = first;
        renamed.revision = 3;
        renamed
            .workspace_names
            .insert("w".to_owned(), "renamed".to_owned());
        publish_runtime_state(&state_tx, renamed.clone());
        assert_eq!(state.next_ready(), Some(renamed));
    }

    /// I1-12: the daemon's refusal of `/clear` reached the chat as
    /// "Delivery unconfirmed" and stayed pinned as an unconfirmed row.
    #[test]
    fn a_daemon_refusal_is_a_rejection_and_a_lost_exchange_is_unconfirmed() {
        let refused = remote_submit_failure(&anyhow::Error::new(DaemonRefusal(
            "/clear requires an idle session".into(),
        )));
        assert!(!refused.unconfirmed);
        assert_eq!(refused.message, "/clear requires an idle session");

        let unconfirmed = remote_submit_failure(&anyhow::Error::new(DaemonRefusal(
            "delivery unconfirmed: channel closed".into(),
        )));
        assert!(unconfirmed.unconfirmed);

        let lost = remote_submit_failure(&anyhow::anyhow!("daemon connection reset"));
        assert!(lost.unconfirmed);
    }
}
