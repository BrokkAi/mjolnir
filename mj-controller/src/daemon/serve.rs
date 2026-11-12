use super::*;

pub(super) async fn refresh_runtime_workspaces(state: &RuntimeState) -> Result<()> {
    state.refresh_workspaces().await
}

pub(super) fn spawn_phone_server(
    config: mj_core::config::PhoneConfig,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
    worker: SessionManagerChannels,
    services: super::delegation::Services,
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
            services,
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
            dispatch_in_process_request(
                &mut request_order,
                request,
                manager.clone(),
                crate::upgrade::gate(),
            );
        }
        if let Err(error) = request_order.drain().await {
            tracing::error!(%error, "remote request bridge drain failed");
        }
    })
}

/// Only daemon-owned mutations hold replacement admission. Reviewer operations
/// live in workers, and synchronization is a repeatable read of their journals.
fn dispatch_in_process_request(
    order: &mut crate::session_manager::SessionRequestOrder,
    request: RemoteSessionRequest,
    manager: SessionManagerControl,
    gate: &Arc<crate::upgrade::Gate>,
) {
    let label = match &request {
        RemoteSessionRequest::Submit { .. } => Some("web relay submit"),
        RemoteSessionRequest::RespondElicitation { .. } => Some("web elicitation response"),
        RemoteSessionRequest::StopBackgroundTask { .. } => Some("web background task stop"),
        RemoteSessionRequest::Sync { .. } | RemoteSessionRequest::Reviewer { .. } => None,
    };
    let work = match label.map(|label| gate.enter(label)).transpose() {
        Ok(work) => work,
        Err(error) => {
            request.reject(&format!("request was not accepted: {error:#}"));
            return;
        }
    };
    order.dispatch(request, move |request| async move {
        // Queue ownership includes the reply, even if its receiver disconnects.
        let _work = work;
        forward_in_process_session_request(request, manager).await;
    });
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
            reply,
        } => {
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
                refused: error.is::<mj_client::session::Refused>(),
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
            role: _,
            action,
            mut reply,
        } => {
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = async {
                    manager
                        .session(session_id)
                        .await?
                        .reviewer(action)
                        .await
                } => result,
            };
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
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;
    let mut directory: Option<Arc<tempfile::TempDir>> = None;
    let mut blobs = std::collections::HashMap::new();
    let mut upload = None;

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
        if let DaemonAction::UploadChunk { bytes, sha256 } = &request.action {
            ensure!(
                request.token == metadata.token,
                "daemon authentication failed"
            );
            ensure!(
                request.protocol_version == PROTOCOL_VERSION,
                "incompatible upload protocol"
            );
            if upload.is_none() {
                if directory.is_none() {
                    let root = mj_core::config::data_dir().join("uploads");
                    tokio::fs::create_dir_all(&root).await?;
                    directory = Some(Arc::new(
                        tokio::task::spawn_blocking(move || tempfile::tempdir_in(root))
                            .await
                            .context("upload directory task failed")??,
                    ));
                }
                let root = directory
                    .as_ref()
                    .context("upload directory missing")?
                    .path()
                    .to_owned();
                // Connection storage must remain available while admission drains.
                let (file, path) = tokio::task::spawn_blocking(move || {
                    Ok::<_, std::io::Error>(tempfile::NamedTempFile::new_in(root)?.into_parts())
                })
                .await
                .context("upload file task failed")??;
                let id = path
                    .file_name()
                    .context("blob filename missing")?
                    .to_string_lossy()
                    .into_owned();
                let file = tokio::fs::File::from_std(file);
                upload = Some((request.request_id, id, path, file, Sha256::new()));
            }
            let (request_id, _, _, file, digest) = upload.as_mut().expect("upload exists");
            ensure!(
                *request_id == request.request_id,
                "interleaved upload request"
            );
            file.write_all(bytes).await?;
            digest.update(bytes);
            if let Some(expected) = sha256 {
                let (_, id, path, mut file, digest) = upload.take().expect("upload exists");
                file.flush().await?;
                drop(file);
                ensure!(
                    digest
                        .finalize()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                        == *expected,
                    "upload SHA-256 mismatch"
                );
                blobs.insert(id.clone(), path);
                write_response(
                    &mut stream,
                    ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id: request.request_id,
                        result: Ok(DaemonReply::BlobUploaded(id)),
                    },
                )
                .await?;
            }
            continue;
        } else {
            ensure!(upload.is_none(), "interrupted upload request");
        }
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
        let mut import_registration = None;
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
            if request.action.is_cancellable_machine_action() {
                let cancelled = Arc::new(AtomicBool::new(false));
                let guard =
                    crate::server_runtime::preflight::ProcessCancellationGuard(cancelled.clone());
                let mut work = Box::pin(super::machine::execute(request.action, &state, cancelled));
                let mut byte = [0u8; 1];
                use tokio::io::AsyncReadExt;
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => None,
                    closed = stream.read(&mut byte) => {
                        if let Err(error) = closed { tracing::debug!(%error, "machine request connection closed"); }
                        None
                    },
                    result = &mut work => Some(result),
                };
                let Some(result) = result else {
                    drop(guard);
                    if let Err(error) = work.await {
                        tracing::debug!(%error, "cancelled machine request finished");
                    }
                    return Ok(());
                };
                write_response(
                    &mut stream,
                    ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id,
                        result: result.map_err(|error| format!("{error:#}")),
                    },
                )
                .await?;
                return Ok(());
            }
            // Lifecycle actions contain large checkpoint/restore futures. Keep
            // them off the connection task's stack, including in debug builds.
            if let DaemonAction::InstallAttachment {
                session_id,
                reference,
                blob,
            } = request.action
            {
                match blobs.remove(&blob) {
                    Some(path) => blocking(move || {
                        let bytes = std::fs::read(&path)?;
                        let result = mj_core::attachment::AttachmentStore::controller(&session_id)?
                            .install(&reference, &bytes);
                        drop(path);
                        result?;
                        Ok(DaemonReply::AttachmentInstalled(reference))
                    })
                    .await
                    .map_err(|error| format!("{error:#}")),
                    None => Err("unknown upload blob".into()),
                }
            } else if let DaemonAction::ImportSession {
                blob,
                mut session,
                bundle,
            } = request.action
            {
                match blobs.remove(&blob) {
                    Some(path) => {
                        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let _cancel = CancelImportPreparation(stopped.clone());
                        let storage = directory.clone();
                        let mut prepare = tokio::task::spawn_blocking(move || {
                            let outcome = (|| -> Result<_> {
                                // Keep connection storage alive until every reader has stopped.
                                let _storage = storage;
                                anyhow::ensure!(
                                    session
                                        .id
                                        .chars()
                                        .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                                    "invalid imported session id"
                                );
                                let verified =
                                    mj_checkpoint::archive::verify_archive_streaming_cancellable(
                                        &path,
                                        stopped.clone(),
                                    )?;
                                let checkpoint = session
                                    .checkpoint
                                    .as_mut()
                                    .context("imported session has no checkpoint")?;
                                anyhow::ensure!(
                                    verified.archive_sha256 == checkpoint.sha256,
                                    "import archive digest mismatch"
                                );
                                let materialized =
                                    mj_transcript::projection::materialized_session_from_canonical(
                                        session.id.clone(),
                                        &verified.canonical_session,
                                    )?;
                                let root = mj_core::config::sessions_dir();
                                std::fs::create_dir_all(&root)?;
                                let mut staged = tempfile::NamedTempFile::new_in(&root)?;
                                let mut input = std::fs::File::open(&path)?;
                                let mut buffer = [0u8; 64 * 1024];
                                loop {
                                    anyhow::ensure!(
                                        !stopped.load(std::sync::atomic::Ordering::Acquire),
                                        "import preparation cancelled"
                                    );
                                    let count = std::io::Read::read(&mut input, &mut buffer)?;
                                    if count == 0 {
                                        break;
                                    }
                                    std::io::Write::write_all(&mut staged, &buffer[..count])?;
                                }
                                staged
                                    .as_file()
                                    .sync_all()
                                    .context("sync imported archive")?;
                                Ok((session, bundle, materialized, staged))
                            })();
                            if let Err(error) = &outcome {
                                tracing::debug!(error = %format!("{error:#}"), "import preparation stopped");
                            }
                            outcome
                        });
                        let prepared = tokio::select! {
                            result = &mut prepare => result.context("import preparation task panicked")?,
                            _ = cancellation.cancelled() => {
                                // The task owns its files and removes the partial only after stopping.
                                return Err(anyhow::anyhow!("daemon stopped during import preparation; retry the import"));
                            }
                        };
                        let result = match prepared {
                            Err(error) => Err(error),
                            Ok((mut session, bundle, materialized, staged)) => {
                                match crate::upgrade::activity_unless_draining(
                                    "import registration",
                                ) {
                                    Err(_) => {
                                        drop(staged);
                                        Ok(DaemonReply::UpgradePending)
                                    }
                                    Ok(registration) => {
                                        // The transaction and its acknowledgement own admission.
                                        let held = registration.clone();
                                        let registered = tokio::task::spawn_blocking(move || -> Result<_> {
                                            let _registration = held;
                                            let destination = mj_core::config::sessions_dir().join(format!("{}.hel.zip", session.id));
                                            session.checkpoint.as_mut().context("imported session has no checkpoint")?.archive_path = destination.clone();
                                            let installed = staged.persist_noclobber(&destination).map_err(|error| error.error)?;
                                            let registered = (|| -> Result<()> {
                                                if let Some(ImportedBundle { bundle_id, bundle }) = bundle {
                                                    Config::update(|config| {
                                                        if let Some(existing) = config.bundles.get(&bundle_id) {
                                                            anyhow::ensure!(existing == &bundle, "bundle {bundle_id:?} changed during import; retry the import");
                                                        } else { config.bundles.insert(bundle_id.clone(), bundle); }
                                                        Ok(())
                                                    })?;
                                                }
                                                crate::database::save_imported_session(&session, &materialized)
                                            })();
                                            drop(installed);
                                            if registered.is_err() { std::fs::remove_file(&destination).context("remove failed import archive")?; }
                                            registered?;
                                            Ok(DaemonReply::Done)
                                        }).await.context("import registration task panicked")?;
                                        // Retain admission through write_response below.
                                        import_registration = Some(registration);
                                        registered
                                    }
                                }
                            }
                        };
                        if result.is_ok() {
                            refresh_runtime_controller(&state).await;
                        }
                        result.map_err(|error| format!("{error:#}"))
                    }
                    None => Err("unknown upload blob".into()),
                }
            } else if let DaemonAction::TranscribeAudio {
                preferred_profile,
                blob,
            } = request.action
            {
                match blobs.remove(&blob) {
                    Some(path) => {
                        let config = state.with_config(Clone::clone);
                        // This disposable request owns no admission. Dropping its future
                        // on disconnect or handoff cancels the network operation.
                        let transcription = async move {
                            let (auth, bytes) = blocking(move || {
                                let bytes = std::fs::read(&path)?;
                                let auth = super::actions::transcription_auth(&config, &preferred_profile).context("voice transcription requires a signed-in ChatGPT Codex profile on the daemon")?;
                                Ok((auth, bytes))
                            }).await?;
                            let client =
                                anvil_client::codex_client::CodexClient::with_auth_path(auth);
                            let response = client
                                .transcribe(anvil_client::transcribe::TranscribeRequest::new(
                                    bytes.into(),
                                    "audio.wav",
                                    "audio/wav",
                                ))
                                .await
                                .context("daemon transcribe captured audio")?;
                            let text = response.text.trim().to_owned();
                            ensure!(!text.is_empty(), "daemon transcription returned no text");
                            Ok::<_, anyhow::Error>(DaemonReply::Transcription(text))
                        };
                        let mut byte = [0];
                        tokio::select! {
                            result = transcription => result.map_err(|error| format!("{error:#}")),
                            _ = cancellation.cancelled() => Err("daemon transcription cancelled by handoff".into()),
                            closed = stream.peek(&mut byte) => {
                                match closed {
                                    Ok(0) => return Ok(()),
                                    Ok(_) => Err("unexpected input during transcription".into()),
                                    Err(error) => return Err(error.into()),
                                }
                            }
                        }
                    }
                    None => Err("unknown upload blob".into()),
                }
            } else {
                Box::pin(handle_action(
                    request.action,
                    &metadata,
                    &state,
                    &cancellation,
                ))
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
        drop(import_registration);
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
            let body = tokio::task::spawn_blocking(move || serde_json::to_vec(&reply))
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

/// Dropping an aborted connection asks its file-owning preparation task to stop.
struct CancelImportPreparation(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelImportPreparation {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod bridge_admission_tests {
    use super::*;

    #[tokio::test]
    async fn queued_web_mutations_hold_handoff_until_reply_and_late_requests_are_refused() {
        let mut remote = crate::session_manager::spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "bridge-test".into(),
            spec: CommandSpec::new("true", Vec::<String>::new()),
            worker_recovery: None,
            project_memory: None,
        }]);
        remote
            .publisher
            .publish("bridge-test".into(), ManagedSessionView::default())
            .await
            .unwrap();
        remote
            .control
            .wait_for_session("bridge-test", Duration::from_secs(5))
            .await
            .unwrap();
        let gate = Arc::new(crate::upgrade::Gate::default());
        let mut order = crate::session_manager::SessionRequestOrder::new();
        let request = |id: &str| {
            let (reply, response) = tokio::sync::oneshot::channel();
            (
                RemoteSessionRequest::Submit {
                    session_id: "bridge-test".into(),
                    command_id: id.into(),
                    command: mj_core::relay::RelayCommand::Cancel,
                    reply,
                },
                response,
            )
        };
        let (first, disconnected) = request("first");
        dispatch_in_process_request(&mut order, first, remote.control.clone(), &gate);
        let (second, completed) = request("second");
        dispatch_in_process_request(&mut order, second, remote.control.clone(), &gate);
        assert_eq!(gate.active_labels(), ["web relay submit x2"]);
        assert!(!gate.try_close());
        drop(disconnected);
        for ordinal in 1..=2 {
            let RemoteSessionRequest::Submit { reply, .. } = remote.requests.recv().await.unwrap()
            else {
                panic!("expected submit");
            };
            assert!(!gate.try_close());
            reply.send(Ok(ordinal)).unwrap();
        }
        assert_eq!(completed.await.unwrap().unwrap(), 2);
        order.drain().await.unwrap();
        assert!(gate.try_close());
        let mut order = crate::session_manager::SessionRequestOrder::new();
        let (late, refused) = request("late");
        dispatch_in_process_request(&mut order, late, remote.control.clone(), &gate);
        let refused = refused.await.unwrap().unwrap_err();
        assert!(!refused.unconfirmed);
        assert!(refused.message.contains("not accepted"));
        order.drain().await.unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}
