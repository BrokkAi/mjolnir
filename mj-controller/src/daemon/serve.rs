use super::*;

pub(super) async fn refresh_runtime_workspaces(state: &RuntimeState) -> Result<()> {
    let workspaces = tokio::task::spawn_blocking(crate::database::list_workspaces)
        .await
        .context("daemon workspace refresh task panicked")??;
    state.publish_workspaces(workspaces);
    Ok(())
}

pub(super) fn spawn_phone_server(
    config: mj_core::config::PhoneConfig,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
    worker: SessionManagerChannels,
) -> tokio::task::JoinHandle<()> {
    state.set_phone_status(WebViewerStatus::Starting);
    let workspaces = state.workspaces();
    tokio::spawn(async move {
        match crate::server_runtime::run_server(
            (&config).into(),
            cancellation.clone(),
            worker,
            state.clone(),
            workspaces,
        )
        .await
        {
            Ok(()) if cancellation.is_cancelled() => {}
            Ok(()) => {
                state.set_phone_status(WebViewerStatus::Stopped);
                state.web_viewer.publish(crate::server::WebViewerAccess::Unavailable("The web viewer stopped unexpectedly. Restart the daemon to restore web access.".into()));
            }
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "phone server stopped");
                state.publish_web_access(crate::server::WebViewerAccess::Unavailable(format!(
                    "Could not start the web viewer: {error:#}"
                )));
            }
        }
    })
}

pub(super) fn spawn_remote_request_bridge(
    mut requests: crate::session_manager::RemoteSessionRequests,
    manager: SessionManagerControl,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // One session's requests reach its relay actor in the order they were
        // made; different sessions still overlap.
        let mut request_order = crate::session_manager::SessionRequestOrder::new();
        while let Some(request) = requests.recv().await {
            let manager = manager.clone();
            request_order.dispatch(request, move |request| {
                forward_in_process_session_request(request, manager)
            });
        }
    })
}

pub(super) async fn forward_in_process_session_request(
    request: RemoteSessionRequest,
    manager: SessionManagerControl,
) {
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
                manager
                    .wait_for_session(&session_id, Duration::from_secs(5))
                    .await?
                    .submit(command_id, command)
                    .await
            }
            .await
            .map_err(|error| mj_client::session::SubmitFailure {
                unconfirmed: error.is::<mj_client::session::DeliveryUnconfirmed>(),
                message: format!("{error:#}"),
            });
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Sync { session_id, reply } => {
            let result = async { manager.session(session_id).await?.sync_now().await }
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
                manager
                    .session(session_id)
                    .await?
                    .respond_elicitation(elicitation_id, response)
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
                manager
                    .session(session_id)
                    .await?
                    .stop_background_task(background_task_id)
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
                    manager
                        .session(session_id)
                        .await?
                        .reviewer_as(role, action)
                        .await
                } => result,
            }
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
    }
}

pub(super) async fn serve_client(
    mut stream: TcpStream,
    metadata: DaemonMetadata,
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> Result<()> {
    loop {
        let request: RequestEnvelope = match read_frame(&mut stream).await {
            Ok(request) => request,
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    )
                }) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let request_id = request.request_id;
        // The frozen management subset is served for every protocol version so
        // any Mjolnir build can inspect, stop, or replace this daemon; everything
        // else requires an exact protocol match.
        let is_management = matches!(
            request.action,
            DaemonAction::Ping
                | DaemonAction::Status
                | DaemonAction::Stop
                | DaemonAction::PrepareUpgrade
                | DaemonAction::UpgradeBlockers
        );
        // Hold through the acknowledgement, not merely the action's result.
        let activity = upgrade_request_activity(&request.action);
        let result = if request.token != metadata.token {
            Err("daemon authentication failed".to_owned())
        } else if activity.is_err() {
            // No work was accepted. The client can wait and retry this exact
            // request without risking a duplicate operation.
            Ok(DaemonReply::UpgradePending)
        } else if request.protocol_version != PROTOCOL_VERSION && !is_management {
            Err(format!(
                "incompatible daemon protocol {}; expected {}",
                request.protocol_version, PROTOCOL_VERSION
            ))
        } else if cancellation.is_cancelled() && !is_management {
            // A daemon in its epilogue still holds a snapshot in memory and
            // would happily serve it, from a store it has stopped reading and
            // may no longer be able to. The retry reaches a fresh daemon,
            // which either migrates the store or reports the mismatch with the
            // numbers it read itself. Ping, Status, and Stop stay answered:
            // they touch no store, and a client asking a stopping daemon to
            // stop should not be refused.
            Err("daemon is shutting down; retry to reach a fresh daemon".to_owned())
        } else {
            let reviewer = matches!(&request.action, DaemonAction::ReviewerAction { .. });
            if reviewer {
                // A reviewer action is a long-lived sidecar operation. If its
                // client goes away, drop the future so the session actor sees
                // its reply receiver close and tears down the reviewer. A
                // one-byte peek observes EOF without consuming a pipelined
                // frame; buffered work therefore remains for the next loop.
                let mut peer_probe = [0_u8; 1];
                let mut action = Box::pin(handle_action(
                    request.action,
                    &metadata,
                    &state,
                    &cancellation,
                ));
                tokio::select! {
                    result = &mut action => result.map_err(|error| format!("{error:#}")),
                    peer = stream.peek(&mut peer_probe) => {
                        match peer {
                            Ok(0) => return Ok(()),
                            Ok(_) => action.await.map_err(|error| format!("{error:#}")),
                            Err(error) => {
                                tracing::debug!(%error, "reviewer client connection became unreadable");
                                return Ok(());
                            }
                        }
                    }
                }
            } else {
                handle_action(request.action, &metadata, &state, &cancellation)
                    .await
                    .map_err(|error| format!("{error:#}"))
            }
        };
        // Echo the caller's protocol version: replies must stay readable in the
        // client's own dialect, and the shapes it can receive here are frozen.
        write_response(
            &mut stream,
            ResponseEnvelope {
                protocol_version: request.protocol_version,
                request_id,
                result,
            },
        )
        .await?;
    }
}

/// Encoding failure is a reply, not an unexplained connection close.
pub(super) async fn write_response(
    stream: &mut TcpStream,
    mut response: ResponseEnvelope,
) -> Result<()> {
    let body = serde_json::to_vec(&response)?;
    if body.len() <= MAX_FRAME_BYTES {
        return write_encoded_frame(stream, &body).await;
    }
    // Inspect only the reply tag; serde skips the potentially huge value.
    #[derive(serde::Deserialize)]
    struct ReplyKind {
        reply: String,
    }
    #[derive(serde::Deserialize)]
    struct ResponseKind {
        result: std::result::Result<ReplyKind, serde::de::IgnoredAny>,
    }
    let operation = serde_json::from_slice::<ResponseKind>(&body)?
        .result
        .map(|reply| reply.reply)
        .unwrap_or_else(|_| "error".into());
    let message = format!(
        "Daemon {operation} response for request {} is too large: {} bytes exceeds the {MAX_FRAME_BYTES}-byte limit",
        response.request_id,
        body.len()
    );
    tracing::warn!(request_id = response.request_id, %operation, encoded_bytes = body.len(), limit = MAX_FRAME_BYTES, "daemon response exceeded frame limit");
    response.result = Err(message);
    write_frame(stream, &response).await
}

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let activity = crate::upgrade::activity("database operation")?;
    tokio::task::spawn_blocking(move || {
        let _activity = activity;
        work()
    })
    .await
    .context("daemon background database task panicked")?
}

pub(super) async fn reach_test_hook(name: &'static str) -> Result<()> {
    #[cfg(feature = "test-hooks")]
    {
        tokio::task::spawn_blocking(move || mj_core::test_hooks::reach_test_hook(name))
            .await
            .context("test hook task panicked")??;
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = name;
    Ok(())
}
