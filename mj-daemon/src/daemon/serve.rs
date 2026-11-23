use super::*;

// Worker quiescence is unbounded. Only the synchronous controller operations
// leave async workers; the wait must never occupy a blocking-pool thread.
pub(super) fn spawn_adoption_operation<F, T>(operation: F) -> tokio::task::JoinHandle<Result<T>>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    mj_core::runtime::spawn(async move {
        use futures::FutureExt;
        let result = match std::panic::AssertUnwindSafe(operation).catch_unwind().await {
            Ok(result) => result,
            Err(panic) => {
                tracing::error!(?panic, "adoption controller task panicked");
                Err(anyhow::anyhow!(
                    "adoption controller task panicked; check session state before retrying"
                ))
            }
        };
        if let Err(error) = &result {
            tracing::error!(%error, "adoption controller task failed");
        }
        result
    })
}

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
            if let DaemonAction::LoginTerminal {
                profile_id,
                cols,
                rows,
            } = &request.action
            {
                let prepared = (|| -> Result<portable_pty::CommandBuilder> {
                    let config = state.with_config(Clone::clone);
                    let profile = config.named_profile(profile_id)?;
                    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe()?);
                    if let Some(instance) = mj_core::config::instance_name() {
                        command.args(["--instance", &instance]);
                    }
                    command.args(["login", "--daemon-local", "--profile", profile_id]);
                    for (name, value) in profile.environment.resolved() {
                        command.env(name, value);
                    }
                    command.env("MJ_LOGIN_TERMINAL", "1");
                    command.env("MJ_CONFIG_DIR", mj_core::config::config_dir());
                    command.env("MJ_DATA_DIR", mj_core::config::data_dir());
                    Ok(command)
                })();
                let command = match prepared {
                    Ok(command) => command,
                    Err(error) => {
                        write_response(
                            &mut stream,
                            ResponseEnvelope {
                                protocol_version: request.protocol_version,
                                request_id,
                                result: Err(format!("{error:#}")),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                };
                write_response(
                    &mut stream,
                    ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id,
                        result: Ok(DaemonReply::LoginTerminalReady),
                    },
                )
                .await?;
                return mj_controller::login_terminal::serve(
                    stream,
                    command,
                    *cols,
                    *rows,
                    cancellation,
                )
                .await;
            }
            if let DaemonAction::AdoptRecovery(adoption) = request.action {
                let wait_cancel = cancellation.child_token();
                let _cancel_on_drop = wait_cancel.clone().drop_guard();
                let cancelled = Arc::new(AtomicBool::new(false));
                let _cancel_processes =
                    crate::server_runtime::preflight::ProcessCancellationGuard(cancelled.clone());
                let (progress, mut waiting) = tokio::sync::watch::channel(None);
                let (installed, mut installation) = tokio::sync::oneshot::channel();
                let mut installation_open = true;
                let work_state = state.clone();
                let work_adoption = adoption.clone();
                let work_cancelled = cancelled.clone();
                let work_wait_cancel = wait_cancel.clone();
                let _cancel_processes =
                    crate::server_runtime::preflight::ProcessCancellationGuard(cancelled.clone());
                let operation = async move {
                    let mut controller = blocking(Controller::load).await?;
                    let executor = CancellableProcessExecutor::new(work_cancelled);
                    let outcome = controller
                        .adopt_orphan_worker_controlled(
                            &work_adoption,
                            &executor,
                            mj_controller::controller::AdoptionControl {
                                cancelled: work_wait_cancel,
                                progress: Some(progress),
                                installed: Some(installed),
                                lifecycle_gate: work_state.session_gate.clone(),
                            },
                        )
                        .await;
                    refresh_runtime_controller(&work_state).await;
                    Ok::<_, anyhow::Error>(outcome)
                };
                let mut work = spawn_adoption_operation(operation);
                let mut disconnected = false;
                let mut draining = false;
                let mut progress_open = true;
                let mut previous_blockers = None;
                let mut byte = [0u8; 1];
                use tokio::io::AsyncReadExt;
                let outcome = loop {
                    tokio::select! {
                        biased;
                        closed = stream.read(&mut byte), if !disconnected => {
                            if let Err(error) = closed { tracing::debug!(%error, "adoption connection closed"); }
                            disconnected = true;
                            cancelled.store(true, Ordering::Release);
                            wait_cancel.cancel();
                        }

                        result = &mut work => break match result {
                            Ok(result) => result,
                            Err(error) => {
                                tracing::error!(%error, "adoption supervisor task failed");
                                Err(anyhow::anyhow!("adoption task failed: {error}"))
                            },
                        },
                        () = mj_controller::upgrade::gate().wait_until_draining(), if !draining => {
                            draining = true;
                            cancelled.store(true, Ordering::Release);
                            wait_cancel.cancel();
                        }
                        publication = &mut installation, if installation_open => {
                            installation_open = false;
                            progress_open = false;
                            if let Ok(publication) = publication {
                                refresh_runtime_controller(&state).await;
                                let delivered = if disconnected { Ok(()) } else { write_adoption_response(&mut stream, ResponseEnvelope {
                                    protocol_version: request.protocol_version, request_id,
                                    result: Ok(DaemonReply::AdoptionInstalled { session_id: adoption.session_id.clone() }),
                                }, &wait_cancel).await };
                                if let Err(error) = delivered {
                                    tracing::debug!(%error, "adoption installed notice connection closed");
                                    disconnected = true;
                                    cancelled.store(true, Ordering::Release);
                                    wait_cancel.cancel();
                                }
                                drop(publication.admission);
                                let _ = publication.acknowledged.send(());
                            }
                        }
                        changed = waiting.changed(), if progress_open => {
                            if changed.is_ok() {
                                let blockers = waiting.borrow_and_update().clone();
                                if blockers != previous_blockers {
                                    previous_blockers = blockers.clone();
                                    if let Some(blockers) = blockers && !disconnected {
                                        let advice = adoption.clone();
                                        let destroy_command = blocking(move || mj_controller::controller::adoption_destroy_command(
                                            &advice.session_id, &advice.target_id, advice.all_instances,
                                        )).await?;
                                        if let Err(error) = write_adoption_response(&mut stream, ResponseEnvelope {
                                            protocol_version: request.protocol_version, request_id,
                                            result: Ok(DaemonReply::AdoptionWaiting { blockers, destroy_command: Some(destroy_command) }),
                                        }, &wait_cancel).await {
                                            tracing::debug!(%error, "adoption progress connection closed");
                                            disconnected = true;
                                            cancelled.store(true, Ordering::Release);
                                            wait_cancel.cancel();
                                        }
                                    }
                                }
                            } else { progress_open = false; }
                        }
                    }
                };
                let (result, admission) = match outcome {
                    Ok(outcome) => (
                        outcome.result.map(|()| DaemonReply::Done),
                        outcome.admission,
                    ),
                    Err(error) => (Err(error), None),
                };
                if disconnected {
                    tracing::info!(
                        session_id = adoption.session_id,
                        "adoption connection closed; pending recovery cut was cancelled"
                    );
                    if let Err(error) = &result {
                        tracing::debug!(%error, "disconnected adoption finished");
                    }
                    drop(admission);
                    return Ok(());
                }
                write_adoption_response(
                    &mut stream,
                    ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id,
                        result: result.map_err(|error| format!("{error:#}")),
                    },
                    &wait_cancel,
                )
                .await?;
                // The install guard spans final response delivery, as other
                // admitted requests do; no worker wait or catch-up owns it.
                drop(admission);
                continue;
            }
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

/// A partial frame cannot be resumed: callers close the adoption connection
/// on interruption, then finish or release any already accepted installation.
pub(super) async fn write_adoption_response(
    stream: &mut TcpStream,
    response: ResponseEnvelope,
    cancelled: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        // A ready socket can still deliver the final failure during drain.
        biased;
        result = write_response(stream, response) => result,
        () = cancelled.cancelled() => anyhow::bail!("adoption response interrupted"),
        () = mj_controller::upgrade::gate().wait_until_draining() => anyhow::bail!("adoption response interrupted for daemon handoff"),
    }
}

/// Encoding failure is a reply, not an unexplained connection close.
pub(super) async fn write_response(
    stream: &mut TcpStream,
    mut response: ResponseEnvelope,
) -> Result<()> {
    let result = std::mem::replace(&mut response.result, Ok(DaemonReply::Done));
    match result {
        Ok(reply) if reply.is_chunked() => {
            // These replies grow with the store (dashboard startup, a runtime
            // bootstrap, the resume dialog's history). Encode away from the
            // control loop, then bound each write without rebuilding the
            // reply per page.
            let body = mj_core::runtime::spawn_blocking(move || serde_json::to_vec(&reply))
                .await
                .context("reply encoder task failed")??;
            let mut chunks = body.chunks(64 * 1024).peekable();
            while let Some(bytes) = chunks.next() {
                write_frame(
                    stream,
                    &ResponseEnvelope {
                        protocol_version: response.protocol_version,
                        request_id: response.request_id,
                        result: Ok(DaemonReply::ReplyChunk {
                            bytes: bytes.to_vec(),
                            finished: chunks.peek().is_none(),
                        }),
                    },
                )
                .await?;
            }
            return Ok(());
        }
        result => response.result = result,
    }
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
