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
    worker: SessionManagerControl,
    services: super::delegation::Services,
) -> tokio::task::JoinHandle<()> {
    state.set_phone_status(WebViewerStatus::Starting);
    mj_core::runtime::spawn(async move {
        let push = crate::daemon::push_delivery::run(state.clone());
        let server = crate::server_runtime::run_server(
            (&config).into(),
            cancellation.clone(),
            worker,
            state.clone(),
            services,
        );
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
                        mj_core::runtime::spawn_blocking(move || tempfile::tempdir_in(root))
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
                let (file, path) = mj_core::runtime::spawn_blocking(move || {
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
        } else if matches!(request.action, DaemonAction::CheckpointSession { .. }) {
            // Decode the legacy action only to explain its removal, including
            // to clients from before the protocol bump. Authentication still applies.
            Err(mj_client::daemon::REMOVED_CHECKPOINT_MESSAGE.to_owned())
        } else if activity.is_err() {
            // No work was accepted. The client can wait and retry this exact
            // request without risking a duplicate operation.
            Ok(DaemonReply::UpgradePending)
        } else if request.protocol_version != PROTOCOL_VERSION && !is_management {
            Err(format!(
                "incompatible daemon protocol {}; expected {}",
                request.protocol_version, PROTOCOL_VERSION
            ))
        } else if matches!(&request.action, DaemonAction::WikiSyncWatch { .. }) {
            // A read-only watch owns no admission. Closing the transport
            // on handoff makes the client reconnect and retain its target.
            let mut byte = [0];
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                result = handle_action(request.action, &metadata, &state, &cancellation) =>
                    result.map_err(|error| format!("{error:#}")),
                closed = stream.peek(&mut byte) => {
                    match closed {
                        Ok(0) => return Ok(()),
                        Ok(_) => return Err(anyhow::anyhow!("unexpected input during SessionWiki watch")),
                        Err(error) => return Err(error.into()),
                    }
                }
            }
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
            if request.action.is_cancellable_machine_action() {
                let cancelled = Arc::new(AtomicBool::new(false));
                let guard =
                    crate::server_runtime::preflight::ProcessCancellationGuard(cancelled.clone());
                let mut work = Box::pin(super::machine::execute(request.action, &state, cancelled));
                let mut byte = [0u8; 1];
                use tokio::io::AsyncReadExt;
                let mut disconnected = false;
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => None,
                    closed = stream.read(&mut byte) => {
                        if let Err(error) = closed { tracing::debug!(%error, "machine request connection closed"); }
                        disconnected = true;
                        None
                    },
                    result = &mut work => Some(result),
                };
                let result = match result {
                    Some(result) => result,
                    None => {
                        drop(guard);
                        // Stop subprocess groups before their staging files are removed.
                        work.await
                    }
                };
                if disconnected {
                    if let Err(error) = result {
                        tracing::debug!(%error, "disconnected machine request finished");
                    }
                    return Ok(());
                }
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
                        let mut prepare = mj_core::runtime::spawn_blocking(move || {
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
                                match mj_controller::upgrade::activity_unless_draining(
                                    "import registration",
                                ) {
                                    Err(_) => {
                                        drop(staged);
                                        Ok(DaemonReply::UpgradePending)
                                    }
                                    Ok(registration) => {
                                        // The transaction and its acknowledgement own admission.
                                        let held = registration.clone();
                                        let registered = mj_core::runtime::spawn_blocking(move || -> Result<_> {
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
                                                mj_controller::database::save_imported_session(&session, &materialized)
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

    // Hard-won: c6ac3b7aa: ordinary client admission accepted sync requests during drain.
    #[test]
    fn wiki_sync_admission_is_named_and_deferrable() {
        const CHILD: &str = "MJ_TEST_WIKI_SYNC_ADMISSION";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "wiki_sync_admission_is_named_and_deferrable",
                ),
            )
            .isolated_store(root.path())
            .env(CHILD, "1")
            .run();
            return;
        }
        let work = upgrade_request_activity(&DaemonAction::WikiSyncRequest).unwrap();
        assert!(
            mj_controller::upgrade::gate()
                .blockers()
                .iter()
                .any(|label| label.label.starts_with("wiki sync request"))
        );
        drop(work);
        mj_controller::upgrade::gate().begin_draining();
        assert!(upgrade_request_activity(&DaemonAction::WikiSyncRequest).is_err());
    }

    // Hard-won: eb0fbd8: a watch sent between handoff polls received a terminal refusal.
    #[tokio::test]
    async fn wiki_sync_watch_closes_transport_when_handoff_begins_between_polls() {
        let mut state = super::super::tests::test_runtime_state();
        Arc::get_mut(&mut state).unwrap().wiki = mj_controller::sessionwiki::WikiIndexer::inert();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = super::super::tests::test_metadata(listener.local_addr().unwrap());
        let server_metadata = metadata.clone();
        let cancellation = CancellationToken::new();
        let server_cancellation = cancellation.clone();
        let server = mj_core::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_metadata, state, server_cancellation).await
        });
        let mut client = mj_client::daemon::DaemonClient::connect(metadata)
            .await
            .unwrap();
        let first = client
            .wiki_sync_watch("another-epoch".into(), 0)
            .await
            .unwrap();
        // The first reply is delivered; handoff starts before the next poll.
        cancellation.cancel();
        let error = client
            .wiki_sync_watch(first.epoch, first.version)
            .await
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<mj_client::daemon::DaemonTransportLost>()
                .is_some(),
            "{error:#}"
        );
        server.await.unwrap().unwrap();
    }
}
