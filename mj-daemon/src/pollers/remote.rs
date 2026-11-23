use super::*;

pub fn spawn_remote_dashboard_worker_poller(
    workspace_id: String,
    bootstrap: mj_client::runtime_feed::RuntimeReplica,
) -> Result<RemoteDashboardWorkerPoller> {
    let channels = spawn_remote_session_manager()?;
    let mj_controller::session_manager::RemoteSessionManagerChannels {
        targets,
        control,
        updates,
        shutdown,
        publisher,
        mut requests,
    } = channels;
    let metadata = &bootstrap.projection.metadata;
    let (alerts_tx, alerts_rx) = tokio::sync::mpsc::unbounded_channel();
    alerts_tx
        .send((metadata.alert_high_sequence, Vec::new()))
        .context("initialize alert stream")?;
    let mut alert_sequence = metadata.alert_high_sequence;
    let (reviews_tx, reviews_rx) = tokio::sync::watch::channel(metadata.reviews.clone());
    let (notices_tx, notices_rx) = tokio::sync::watch::channel(metadata.notices.clone());
    let (ui_requests_tx, ui_requests_rx) =
        tokio::sync::watch::channel(metadata.ui_requests.clone());
    let (quotas_tx, quotas_rx) = tokio::sync::watch::channel(metadata.quotas.clone().into_inner());
    let (capabilities_tx, capabilities_rx) =
        tokio::sync::watch::channel(metadata.profile_capabilities.clone().into_inner());
    let (config_tx, config_rx) = tokio::sync::watch::channel(RuntimeLaunchConfig::from(metadata));
    let initial_native_agents = bootstrap.projection.native_agents.clone();
    let (state_tx, state_rx) = tokio::sync::watch::channel(runtime_state_update(
        bootstrap.projection.clone(),
        Default::default(),
    ));
    let (health_tx, health_rx) = tokio::sync::watch::channel(RuntimeFeedHealth::default());
    let initial_scope = bootstrap
        .cursor
        .as_ref()
        .map(|cursor| cursor.subagent_parents.clone())
        .unwrap_or_default();
    let (subagent_parents, scopes) = tokio::sync::watch::channel(initial_scope);
    mj_core::runtime::spawn(async move {
        let replica = Arc::new(tokio::sync::Mutex::new(bootstrap));
        let tails = Arc::clone(&replica);
        let mut feed = spawn_runtime_feed_with(
            workspace_id,
            move |_, revision| {
                let replica = replica.clone();
                let scopes = scopes.clone();
                async move { poll_daemon_runtime(replica, revision, scopes).await }
            },
            move |session_id| load_session_tail(Arc::clone(&tails), session_id),
        );
        let mut native = super::native_agents::NativeAgentLoader::default();
        native.update_snapshot(initial_native_agents);
        let mut request_order = mj_controller::session_manager::SessionRequestOrder::new();
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
                            let metadata = &snapshot.metadata;
                            if publish_alerts(&alerts_tx, &mut alert_sequence, metadata).is_err() { return; }
                            send_if_changed(&config_tx, RuntimeLaunchConfig::from(metadata));
                            native.update_snapshot(snapshot.native_agents.clone());
                            health_tx.send_if_modified(|health| {
                                let recovered = health.refresh_error.take().is_some();
                                let native_error = native.error();
                                let changed = health.native_error != native_error;
                                health.native_error = native_error;
                                recovered || changed
                            });
                            let next = runtime_state_update((*snapshot).clone(), native.views_snapshot());
                            publish_runtime_state(&state_tx, next);
                            send_if_changed(&reviews_tx, metadata.reviews.clone());
                            send_if_changed(&capabilities_tx, metadata.profile_capabilities.clone().into_inner());
                            send_if_changed(&notices_tx, metadata.notices.clone());
                            send_if_changed(&ui_requests_tx, metadata.ui_requests.clone());
                            send_if_changed(&quotas_tx, metadata.quotas.clone().into_inner());
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
        alerts: alerts_rx,
        updates,
        control,
        shutdown,
        state: state_rx,
        subagent_parents,
        reviews: reviews_rx,
        notices: notices_rx,
        ui_requests: ui_requests_rx,
        quotas: quotas_rx,
        profile_capabilities: capabilities_rx,
        config: config_rx,
        health: health_rx,
    })
}

/// Only a new frontier or a nonempty batch can wake the alert consumer.
fn publish_alerts(
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, Vec<mj_client::runtime_feed::AlertEvent>)>,
    previous: &mut u64,
    metadata: &mj_client::runtime_feed::RuntimeMetadata,
) -> Result<()> {
    if *previous == metadata.alert_high_sequence && metadata.alert_events.is_empty() {
        return Ok(());
    }
    tx.send((metadata.alert_high_sequence, metadata.alert_events.clone()))
        .context("publish alert stream")?;
    *previous = metadata.alert_high_sequence;
    Ok(())
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

fn runtime_state_update(
    snapshot: mj_client::runtime_feed::RuntimeProjection,
    native_agents: mj_core::snapshot_map::SnapshotMap<
        String,
        mj_core::native_agent::NativeAgentView,
    >,
) -> RuntimeStateUpdate {
    let metadata = snapshot.metadata;
    RuntimeStateUpdate {
        subagent_parents: snapshot.subagent_parents,
        last_subagent_policies: metadata.last_subagent_policies,
        operator_presence: metadata.operator_presence,
        native_agents,
        session_cpu: snapshot.session_cpu,
        workspace_names: metadata.workspace_names,
        revision: snapshot.revision,
        records: snapshot.records,
        sessions: snapshot.sessions,
        verdicts: snapshot.verdicts,
        lifecycles: metadata.lifecycles,
        moves: snapshot.moves,
        subagents: snapshot.subagents,
        environment_owners: snapshot.environment_owners,
        launch_recency: metadata.launch_recency.into_inner(),
        form_holders: metadata.form_holders,
        session_attention: metadata.session_attention,
        notification_cards: metadata.notification_cards,
        unread_frontiers: metadata.unread_frontiers,
        notification_dismissals: metadata.notification_dismissals,
        recent_launch: metadata.recent_launch,
        container_upgrade_sessions: metadata.container_upgrade_sessions,
        failed_launch_retries: metadata.failed_launch_retries,
        host_capacities: metadata.host_capacities.into_inner(),
        storage: metadata.storage.into_inner(),
        container_capacities: metadata.container_capacities.into_inner(),
    }
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
    publisher: &mj_controller::session_manager::RemoteSessionPublisher,
    session_id: String,
    view: ManagedSessionView,
) -> Result<()> {
    targets.send_if_modified(|targets| {
        if targets.iter().any(|target| target.session_id == session_id) {
            return false;
        }
        targets.push(WorkerPollTarget {
            session_id: session_id.clone(),
            spec: mj_controller::targets::CommandSpec::new("daemon-owned", Vec::<String>::new()),
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

/// A live session's projection, from the transcript tail the replica holds.
///
/// Deltas keep a held tail at the replica's cursor, so it is current. A tail
/// not held yet is fetched from the daemon as it was at that same cursor, so
/// the deltas that follow apply to it. Nothing here reads the store.
pub(super) async fn load_session_tail(
    replica: Arc<tokio::sync::Mutex<mj_client::runtime_feed::RuntimeReplica>>,
    session_id: String,
) -> Result<StoredProjection> {
    let cursor = {
        let replica = replica.lock().await;
        if let Some(tail) = replica.projection.transcripts.get(&session_id) {
            return Ok(Some((tail.materialized(), tail.window.clone())));
        }
        replica
            .cursor
            .clone()
            .context("the runtime feed has no cursor to fetch a tail at")?
    };
    let reply = mj_client::daemon::connect_existing()
        .await?
        .http()
        .await?
        .session_tail(session_id.clone(), cursor.clone())
        .await?;
    let mut replica = replica.lock().await;
    match reply {
        mj_client::runtime_feed::SessionTailReply::Tail {
            header,
            window,
            items,
        } => {
            let tail = mj_client::runtime_feed::SessionTail::from_parts(*header, window, items);
            let projection = (tail.materialized(), tail.window.clone());
            // Only a tail for the cursor the replica still holds can follow
            // its deltas.
            if replica.cursor.as_ref() == Some(&cursor) {
                replica.projection.transcripts.insert(session_id, tail);
            }
            Ok(Some(projection))
        }
        mj_client::runtime_feed::SessionTailReply::NoTail => Ok(None),
        mj_client::runtime_feed::SessionTailReply::ResetRequired => {
            if replica.cursor.as_ref() == Some(&cursor) {
                replica.cursor = None;
            }
            Err(anyhow::Error::new(super::runtime_feed::TailCursorExpired))
        }
    }
}

pub(super) async fn poll_daemon_runtime(
    replica: Arc<tokio::sync::Mutex<mj_client::runtime_feed::RuntimeReplica>>,
    after_revision: u64,
    mut scopes: tokio::sync::watch::Receiver<std::collections::BTreeSet<String>>,
) -> Result<mj_client::runtime_feed::RuntimeProjection> {
    loop {
        let parents = scopes.borrow_and_update().clone();
        let (held_cursor, cursor, revision) =
            {
                let replica = replica.lock().await;
                // Invalidated history and scope changes keep the durable alert frontier.
                let cursor = replica.cursor.clone().unwrap_or_else(|| {
                    mj_client::runtime_feed::RuntimeCursor {
                        alert_sequence: replica.projection.metadata.alert_high_sequence,
                        incarnation: String::new(),
                        sequence: 0,
                        subagent_parents: parents.clone(),
                    }
                });
                (
                    replica.cursor.clone(),
                    Some(cursor),
                    replica.projection.revision,
                )
            };
        let daemon = mj_client::daemon::connect_existing().await?.http().await?;
        let wait = cursor.as_ref().is_some_and(|cursor| {
            !cursor.incarnation.is_empty() && cursor.subagent_parents == parents
        }) && after_revision != 0
            && after_revision == revision;
        // A pending read never owns the replica: transcript-tail requests must
        // be able to use its current cursor while the daemon waits for changes.
        let frame = tokio::select! {
            changed = scopes.changed() => {
                changed.context("dashboard family subscription stopped")?;
                continue;
            }
            frame = daemon.runtime_changes_for_subagent_parents(cursor.clone(), wait, parents.clone()) => frame?,
        };
        if *scopes.borrow() != parents {
            continue;
        }
        let mut replica = replica.lock().await;
        // An expired tail can invalidate a cursor while this request waits.
        if replica.cursor != held_cursor {
            continue;
        }
        // A snapshot can replace an expired base while preserving alerts.
        if let Err(error) = replica.apply(frame) {
            replica.cursor = None;
            return Err(error);
        }
        if replica.cursor.is_none() {
            continue;
        }
        // Cursor acknowledgement remains in the replica; the transient batch
        // belongs to this publication only, including across unchanged polls.
        let events = std::mem::take(&mut replica.projection.metadata.alert_events);
        let mut projection = replica.projection.clone();
        projection.metadata.alert_events = events;
        return Ok(projection);
    }
}

#[cfg(test)]
mod expired_tail_cursor_tests {
    use super::*;
    use std::collections::BTreeSet;

    fn answer_identity(
        identity: &mj_client::daemon::DaemonIdentity,
        request: mj_client::daemon::RequestEnvelope,
    ) -> mj_client::daemon::ResponseEnvelope {
        use mj_client::daemon::{DaemonAction, DaemonReply, ResponseEnvelope};
        assert!(matches!(request.action, DaemonAction::Identity));
        ResponseEnvelope {
            protocol_version: request.protocol_version,
            request_id: request.request_id,
            result: Ok(DaemonReply::Identity(identity.clone())),
        }
    }

    async fn tail_api(
        metadata: &mj_client::daemon::DaemonMetadata,
        state: Arc<crate::daemon::RuntimeState>,
    ) -> (
        mj_client::daemon::DaemonIdentity,
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_, shutdown, server) =
            crate::daemon::tests::start_test_dashboard_http_api_on(state, listener).await;
        mj_client::daemon::write_api_endpoint(&mj_client::daemon::ApiEndpointMetadata {
            address,
            token: "direct-test-token".into(),
            pid: metadata.pid,
            instance_id: metadata.instance_id.clone(),
            protocol_version: metadata.protocol_version,
        })
        .unwrap();
        (
            mj_client::daemon::DaemonIdentity {
                instance_id: metadata.instance_id.clone(),
                build_version: metadata.build_version.clone(),
                placement: metadata.placement.clone(),
            },
            shutdown,
            server,
        )
    }

    // Hard-won: 6b12ea8ea: acknowledged alert batches were copied into every later publication.
    #[tokio::test]
    async fn bootstrap_poll_keeps_tails_available_and_cancels_the_old_family_scope() {
        const CHILD: &str = "MJ_TEST_RUNTIME_BOOTSTRAP_SCOPE";
        const TEST: &str = "bootstrap_poll_keeps_tails_available_and_cancels_the_old_family_scope";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), TEST),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "bootstrap-family-scope")
            .isolated_store(root.path())
            .run();
            return;
        }
        use mj_client::daemon::DaemonMetadata;
        use mj_client::runtime_feed::{RuntimeBootstrap, RuntimeFrame, RuntimeProjection};
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let metadata = DaemonMetadata {
            protocol_version: mj_client::daemon::PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "bootstrap-scope".into(),
            started_at: "2026-11-04T00:00:00Z".into(),
            build_version: "test".into(),
            instance_id: "bootstrap-scope".into(),
            placement: mj_client::daemon::DaemonPlacement::Host,
        };
        let path = mj_client::daemon::metadata_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let _writer = mj_controller::database::install_isolated_test_writer();
        let state = crate::daemon::tests::test_runtime_state();
        let mut initial = RuntimeProjection::default();
        initial.metadata.notification_cards = Some(Default::default());
        mj_controller::database::notification_reconcile(initial).unwrap();
        let RuntimeFrame::Snapshot { cursor, projection } =
            state.runtime_changes(None, false).await.unwrap()
        else {
            panic!("expected runtime bootstrap");
        };
        let (identity, shutdown, http_server) = tail_api(&metadata, state).await;
        let replica = Arc::new(tokio::sync::Mutex::new(
            RuntimeBootstrap {
                cursor,
                projection: *projection,
            }
            .into_replica(),
        ));
        let (scopes, scope_rx) = tokio::sync::watch::channel(BTreeSet::new());
        let (accepted, identity_seen) = tokio::sync::oneshot::channel();
        let (scope_requested, new_scope) = tokio::sync::oneshot::channel();
        let (release_scope, scope_ready) = tokio::sync::oneshot::channel();
        let server = mj_core::runtime::spawn(async move {
            let mut scope_barrier = Some((scope_requested, scope_ready));
            let mut accepted = Some(accepted);
            // Long poll, transcript tail, changed scope, then its next poll.
            for request_index in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = mj_client::daemon::read_frame(&mut stream).await.unwrap();
                if request_index == 2 {
                    let (requested, ready) = scope_barrier.take().unwrap();
                    requested.send(()).unwrap();
                    ready.await.unwrap();
                }
                mj_client::daemon::write_frame(&mut stream, &answer_identity(&identity, request))
                    .await
                    .unwrap();
                if let Some(accepted) = accepted.take() {
                    accepted.send(()).unwrap();
                }
            }
        });
        let revision = replica.lock().await.projection.revision;
        let poll =
            mj_core::runtime::spawn(poll_daemon_runtime(replica.clone(), revision, scope_rx));
        tokio::time::timeout(Duration::from_secs(2), identity_seen)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                load_session_tail(replica.clone(), "session".into()),
            )
            .await
            .expect("pending poll must not hold the replica lock")
            .unwrap()
            .is_none()
        );
        scopes.send(BTreeSet::from(["parent".to_owned()])).unwrap();
        tokio::time::timeout(Duration::from_secs(2), new_scope)
            .await
            .unwrap()
            .unwrap();
        let mut publication = RuntimeProjection::default();
        publication.metadata.notification_cards =
            Some(mj_core::snapshot_map::SnapshotMap::from([(
                "parent".into(),
                mj_client::runtime_feed::NotificationCard {
                    since_ms: 1,
                    badge: mj_core::attention::NotificationKind::NeedsInput,
                    entries: vec![],
                    dismissal: None,
                    holder: Some("child".into()),
                    form: Some(mj_client::runtime_feed::CardForm {
                        source: mj_client::runtime_feed::CardFormSource::Elicitation,
                        raised_at_ms: 1,
                        request_id: "test".into(),
                        message: "Caught up".into(),
                    }),
                },
            )]));
        mj_controller::database::notification_reconcile(publication).unwrap();
        release_scope.send(()).unwrap();
        let projection = tokio::time::timeout(Duration::from_secs(2), poll)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            projection.subagent_parents,
            BTreeSet::from(["parent".to_owned()])
        );
        assert_eq!(projection.metadata.alert_events.len(), 1);
        assert!(
            replica
                .lock()
                .await
                .projection
                .metadata
                .alert_events
                .is_empty(),
            "the delivered batch is moved out before cloning subsequent projections"
        );
        let next = tokio::time::timeout(
            Duration::from_secs(2),
            poll_daemon_runtime(replica, 0, scopes.subscribe()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(next.metadata.alert_events.is_empty());
        server.await.unwrap();
        shutdown.cancel();
        http_server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn expired_session_tail_cursor_clears_the_replica_cursor_before_retry() {
        const CHILD: &str = "MJ_TEST_EXPIRED_SESSION_TAIL_CURSOR";
        const TEST: &str = "expired_session_tail_cursor_clears_the_replica_cursor_before_retry";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().expect("isolated data root");
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(module_path!(), TEST),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "expired-tail-cursor")
            .isolated_store(root.path())
            .run();
            return;
        }

        use mj_client::daemon::DaemonMetadata;
        use mj_client::runtime_feed::{RuntimeCursor, RuntimeReplica};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind fake daemon protocol endpoint");
        let address = listener.local_addr().unwrap();
        let metadata = DaemonMetadata {
            protocol_version: mj_client::daemon::PROTOCOL_VERSION,
            pid: std::process::id(),
            address,
            token: "tail-cursor-test-token".into(),
            started_at: "2026-10-05T00:00:00Z".into(),
            build_version: "test".into(),
            instance_id: "expired-tail-cursor".into(),
            placement: mj_client::daemon::DaemonPlacement::Host,
        };
        let metadata_path = mj_client::daemon::metadata_path();
        std::fs::create_dir_all(metadata_path.parent().unwrap()).expect("create isolated data dir");
        std::fs::write(
            &metadata_path,
            serde_json::to_vec(&metadata).expect("serialize daemon metadata"),
        )
        .expect("publish fake daemon metadata");

        let _writer = mj_controller::database::install_isolated_test_writer();
        let (identity, shutdown, http_server) =
            tail_api(&metadata, crate::daemon::tests::test_runtime_state()).await;
        let server = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = mj_client::daemon::read_frame(&mut stream).await.unwrap();
            mj_client::daemon::write_frame(&mut stream, &answer_identity(&identity, request))
                .await
                .unwrap();
        });

        let cursor = RuntimeCursor {
            alert_sequence: 0,
            incarnation: "previous-daemon".into(),
            sequence: 42,
            subagent_parents: Default::default(),
        };
        let replica = Arc::new(tokio::sync::Mutex::new(RuntimeReplica {
            cursor: Some(cursor.clone()),
            ..RuntimeReplica::default()
        }));
        let error = load_session_tail(Arc::clone(&replica), "session-7".into())
            .await
            .expect_err("an expired cursor requires a fresh runtime snapshot");
        assert_eq!(
            error.to_string(),
            "the daemon no longer holds this runtime cursor"
        );
        assert!(replica.lock().await.cursor.is_none());
        server.await.unwrap();
        shutdown.cancel();
        http_server.await.unwrap().unwrap();
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
            reply,
        } => {
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
            .map_err(|error| anyhow::anyhow!("{error:#}"));
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
                    .http()
                    .await?
                    .stop_background_task(&session_id, background_task_id)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Reviewer { reply, .. } => {
            let _ = reply.send(Err(anyhow::anyhow!(
                "review capture is owned by the daemon"
            )));
        }
    }
}

pub fn queued_prompt_projection(
    session: &MaterializedSession,
) -> Vec<mj_core::relay::QueuedPrompt> {
    queued_prompt_entries(&session.queued_prompts)
}

pub(crate) fn queued_prompt_entries(
    prompts: &[mj_core::state::MaterializedQueuedPrompt],
) -> Vec<mj_core::relay::QueuedPrompt> {
    prompts
        .iter()
        .map(|prompt| mj_core::relay::QueuedPrompt {
            id: prompt.command_id.clone(),
            kind: prompt.kind.clone(),
            text: mj_core::transcript::materialized_content_text(&prompt.content),
            attachments: Vec::new(),
            created_at_ms: prompt.queued_at_ms,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{publish_alerts, publish_runtime_state, remote_submit_failure, send_if_changed};
    use crate::pollers::{Feed, RuntimeStateUpdate};
    use mj_client::daemon::{DaemonRefusal, RuntimeNotice};

    /// A republished revision with equal content must not wake the surface,
    /// while changed content must still wake it.
    // Hard-won: eb348fefb: empty unchanged alert updates woke the dashboard on every stream revision.
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

        let (alerts_tx, mut alerts_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut previous = 7;
        let mut metadata = mj_client::runtime_feed::RuntimeMetadata {
            alert_high_sequence: 7,
            ..Default::default()
        };
        publish_alerts(&alerts_tx, &mut previous, &metadata).unwrap();
        assert!(
            alerts_rx.try_recv().is_err(),
            "unchanged empty snapshots must not wake alert rendering"
        );
        metadata.alert_high_sequence = 8;
        metadata
            .alert_events
            .push(mj_client::runtime_feed::AlertEvent {
                identity: "test".into(),
                sequence: 8,
                destination: "s".into(),
                session_title: "Session".into(),
                text: "Question".into(),
                kind: mj_core::attention::NotificationKind::NeedsInput,
            });
        publish_alerts(&alerts_tx, &mut previous, &metadata).unwrap();
        let (high, events) = alerts_rx.try_recv().unwrap();
        assert_eq!(high, 8);
        assert_eq!(events.len(), 1);
        metadata.alert_events.clear();
        publish_alerts(&alerts_tx, &mut previous, &metadata).unwrap();
        assert!(
            alerts_rx.try_recv().is_err(),
            "an unchanged frontier with no new events stays quiet"
        );
        metadata.alert_high_sequence = 9;
        publish_alerts(&alerts_tx, &mut previous, &metadata).unwrap();
        assert_eq!(
            alerts_rx.try_recv().unwrap(),
            (9, Vec::new()),
            "a frontier advance is delivered even if its event was deleted"
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
    // Hard-won: 8bf21ad0: a final daemon relay refusal was incorrectly rendered as delivery unconfirmed and left the row stuck.
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
