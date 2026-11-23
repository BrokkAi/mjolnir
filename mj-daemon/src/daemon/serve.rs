use super::*;

pub(super) async fn refresh_runtime_workspaces(state: &RuntimeState) -> Result<()> {
    state.refresh_workspaces().await
}

pub(super) fn spawn_phone_server(
    config: mj_core::config::PhoneConfig,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
    mut options: crate::server::ServerOptions,
) -> tokio::task::JoinHandle<()> {
    state.set_phone_status(WebViewerStatus::Starting);
    // The API runtime owns shared resources used by both listeners. A
    // shutdown of the phone listener must not cancel that runtime.
    options.shutdown = options.shutdown.child_token();
    mj_core::runtime::spawn(async move {
        let push = crate::daemon::push_delivery::run(state.clone());
        let server = crate::server_runtime::serve_phone((&config).into(), options, state.clone());
        tokio::pin!(server);
        tokio::pin!(push);
        let result = tokio::select! {
            result = &mut server => result,
            result = &mut push => {
                if let Err(error) = result { tracing::error!(error=format!("{error:#}"), "Web Push service stopped"); }
                // The HTTP server still drains accepted control work on shutdown.
                server.await
            }
        };
        match result {
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
        let result = if let DaemonAction::ProveIdentity { ref nonce } = request.action {
            use hmac::Mac;
            Ok(DaemonReply::IdentityProof {
                mac: mj_client::daemon::identity_proof(&metadata.token, nonce)
                    .finalize()
                    .into_bytes()
                    .to_vec(),
            })
        } else if request.token != metadata.token {
            Err("daemon authentication failed".to_owned())
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
            Box::pin(handle_action(
                request.action,
                &metadata,
                &state,
                &cancellation,
            ))
            .await
            .map_err(|error| format!("{error:#}"))
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
    let activity = mj_controller::upgrade::activity("database operation")?;
    let owner = mj_controller::worker_lifecycle::capture();
    mj_core::runtime::spawn_blocking(move || {
        let _activity = activity;
        match owner {
            Some(owner) => owner.scope_blocking(work),
            None => work(),
        }
    })
    .await
    .context("daemon background database task panicked")?
}

pub(super) async fn reach_test_hook(name: &'static str) -> Result<()> {
    if mj_core::test_hooks::test_hooks_enabled() {
        mj_core::runtime::spawn_blocking(move || mj_core::test_hooks::reach_test_hook(name))
            .await
            .context("test hook task panicked")??;
    }
    Ok(())
}
