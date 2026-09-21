use super::*;

/// Answer one relay request, keeping the relay lock for exactly as long as
/// the request needs it.
///
/// Every request but attach is a bounded mutation of durable state and is
/// served under the lock. Attach is not: validating a controller's cursor
/// can decompress a sealed segment and the reply carries up to a
/// [`mj_core::relay::RELAY_REPLAY_BYTE_BUDGET`] page read from disk, and
/// a controller catching up over a long offline history asks for page
/// after page. Holding the lock across that stalls the coordinator's
/// `record_runtime_event`, and once its bounded event channel fills the
/// agent's turn stalls with it. So attach captures a plan under the lock
/// and does its reading on a blocking thread with the lock released.
pub(crate) async fn handle_request(
    relay: &Arc<Mutex<DurableRelay>>,
    envelope: RelayRequestEnvelope,
) -> Result<RelayResponseEnvelope> {
    if let RelayRequest::JevDecisions { decision_id } = &envelope.request {
        if !envelope.request.supported_at(envelope.protocol_version) {
            return Ok(relay
                .lock()
                .expect("relay state lock poisoned")
                .handle(envelope));
        }
        let (directory, session) = {
            let relay = relay.lock().expect("relay state lock poisoned");
            (
                relay.root().join("jev-decisions"),
                relay.session_id().to_owned(),
            )
        };
        let decision_id = decision_id.clone();
        let page = tokio::task::spawn_blocking(move || {
            mj_core::jev::read(&directory, &session, decision_id.as_deref())
        })
        .await
        .context("read worker Jev diagnostics")??;
        return Ok(RelayResponseEnvelope {
            request_id: envelope.request_id,
            protocol_version: envelope.protocol_version,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::JevDecisions(page),
            },
        });
    }
    // Sealing and garbage collection can move the segments a plan named
    // while it is being read. That is not the controller's fault and not
    // its problem: plan again against the journal as it now stands. The
    // journal only moves that way once per sealed megabyte or per
    // acknowledgement, so a couple of attempts always outrun it.
    const REPLAN_ATTEMPTS: usize = 3;

    let mut deferred = {
        let mut guard = relay.lock().expect("relay state lock poisoned");
        match guard.take_deferred_attach(&envelope) {
            Some(deferred) => deferred,
            None => return Ok(guard.handle(envelope)),
        }
    };
    for _ in 0..REPLAN_ATTEMPTS {
        let generation = deferred.journal_generation();
        let response = tokio::task::spawn_blocking(move || deferred.finish())
            .await
            .context("assemble relay replay page")?;
        if !matches!(response.body, RelayResponseBody::Error { .. }) {
            let mut guard = relay.lock().expect("relay state lock poisoned");
            if guard.journal_generation() == generation {
                guard.remember_replay_cursor(&response);
            }
            return Ok(response);
        }
        let replanned = {
            let guard = relay.lock().expect("relay state lock poisoned");
            if guard.journal_generation() == generation {
                // The journal never moved, so this really is the answer.
                return Ok(response);
            }
            guard.take_deferred_attach(&envelope)
        };
        match replanned {
            Some(next) => deferred = next,
            None => break,
        }
    }
    Ok(DeferredRelayAttach::stale_journal_response(
        envelope.request_id,
        envelope.protocol_version,
    ))
}

/// Serves the review dispatch socket for as long as the returned guard lives.
///
/// One line in, one line out, one connection per call: the supervisor's MCP
/// server connects, hands over the lanes it wants, and reads the answer. The
/// socket lives inside the worker root, so nothing outside this container can
/// reach it, and it is removed when the worker stops.
pub(crate) fn serve_review_dispatch(
    root: &std::path::Path,
    reviewer: Arc<ReviewerSidecar>,
) -> Result<SocketGuard> {
    let directory = root.join(crate::worker_runtime::REVIEWER_DIR);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create the reviewer directory {}", directory.display()))?;
    let path = directory.join(mj_core::review::mcp::REVIEW_DISPATCH_SOCKET);
    // A socket left behind by a previous worker would refuse the bind; the
    // previous worker is gone, so its socket is stale by definition.
    let _ = std::fs::remove_file(&path);
    let listener = bind_unix_listener(&path)
        .with_context(|| format!("bind the review dispatch socket {}", path.display()))?;
    listener.set_nonblocking(true).with_context(|| {
        format!(
            "set the review dispatch socket {} nonblocking",
            path.display()
        )
    })?;
    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("register the review dispatch socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let reviewer = reviewer.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_one_review_dispatch(stream, reviewer).await {
                    // Reported rather than dropped: a supervisor whose
                    // dispatch was lost would wait for lanes that never run.
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "a review dispatch could not be answered"
                    );
                }
            });
        }
    });
    Ok(SocketGuard(path))
}

pub(crate) async fn serve_one_review_dispatch(
    stream: UnixStream,
    reviewer: Arc<ReviewerSidecar>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (read, mut write) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("read a review dispatch")?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let reply = match serde_json::from_str::<mj_core::review::lanes::LaneDispatch>(line.trim()) {
        Ok(dispatch) => reviewer.record_dispatch(dispatch),
        Err(error) => mj_core::review::lanes::LaneDispatchReply {
            started: Vec::new(),
            error: Some(format!("could not read the dispatch: {error}")),
        },
    };
    let mut body = serde_json::to_vec(&reply)?;
    body.push(b'\n');
    write
        .write_all(&body)
        .await
        .context("answer a review dispatch")?;
    write.flush().await.context("flush a review dispatch")
}

pub(crate) async fn reviewer_response(
    envelope: RelayRequestEnvelope,
    reviewer: Option<&Arc<ReviewerSidecar>>,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> RelayResponseEnvelope {
    let Some(reviewer) = reviewer else {
        return RelayResponseEnvelope {
            request_id: envelope.request_id,
            protocol_version: envelope.protocol_version,
            body: compaction_error(
                RelayErrorCode::InvalidState,
                "session is closed; no reviewer can run beside it",
            ),
        };
    };
    let RelayRequest::Reviewer { role, request } = envelope.request.clone() else {
        unreachable!("reviewer_response only serves reviewer requests");
    };
    // Operations on one role are sequential by construction; operations on
    // different roles are not, so a lane launching cannot block the controller
    // reading the supervisor's journal.
    reviewer
        .handle_cancellable(
            envelope,
            role,
            request,
            wait_for_reviewer_disconnect(reader),
        )
        .await
}

/// Watches a review request's owning connection until it closes.
///
/// Do not consume input: cancellation of a line-reading future could discard
/// half of the next frame. Buffered pipelined input is left for the ordinary
/// request loop; only an idle input stream can signal EOF during this action.
pub(crate) async fn wait_for_reviewer_disconnect(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> ReviewerCancellation {
    match reader.fill_buf().await {
        Ok(bytes) if !bytes.is_empty() => std::future::pending().await,
        Ok(_) => ReviewerCancellation::ClientDisconnected,
        Err(error) => {
            tracing::debug!(%error, "review client read failed during an in-flight operation");
            ReviewerCancellation::ClientDisconnected
        }
    }
}

pub(crate) fn compaction_error(code: RelayErrorCode, message: &str) -> RelayResponseBody {
    RelayResponseBody::Error {
        error: RelayProtocolError {
            code,
            message: message.to_owned(),
            retryable: false,
            detail: None,
        },
    }
}

pub(crate) async fn elicitation_response(
    envelope: RelayRequestEnvelope,
    commands: Option<&mpsc::Sender<CommandRequest>>,
) -> RelayResponseEnvelope {
    let protocol_version = envelope.protocol_version;
    let request_id = envelope.request_id;
    let RelayRequest::RespondElicitation {
        elicitation_id,
        response,
    } = envelope.request
    else {
        unreachable!("elicitation_response only serves form answers")
    };
    let body = match commands {
        None => compaction_error(
            RelayErrorCode::InvalidState,
            "session is closed; no ACP runtime can answer the elicitation",
        ),
        Some(commands) => {
            let (resolved, resolution) = tokio::sync::oneshot::channel();
            match commands
                .send(CommandRequest::ResolveElicitation {
                    elicitation_id: elicitation_id.clone(),
                    response,
                    resolved,
                })
                .await
            {
                Ok(()) => match resolution.await {
                    Ok(Ok(())) => RelayResponseBody::Ok {
                        payload: RelayResponsePayload::ElicitationResolved { elicitation_id },
                    },
                    Ok(Err(message)) => compaction_error(RelayErrorCode::InvalidState, &message),
                    Err(_) => compaction_error(
                        RelayErrorCode::Internal,
                        "ACP runtime stopped before resolving the elicitation",
                    ),
                },
                Err(_) => compaction_error(
                    RelayErrorCode::Internal,
                    "ACP runtime stopped before accepting the elicitation answer",
                ),
            }
        }
    };
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body,
    }
}

pub(crate) async fn background_task_stop_response(
    envelope: RelayRequestEnvelope,
    commands: Option<&mpsc::Sender<CommandRequest>>,
    relay: &Arc<Mutex<DurableRelay>>,
) -> RelayResponseEnvelope {
    let protocol_version = envelope.protocol_version;
    let request_id = envelope.request_id;
    let RelayRequest::StopBackgroundTask { background_task_id } = envelope.request else {
        unreachable!("background_task_stop_response only serves task stops")
    };
    let target = relay
        .lock()
        .expect("relay state lock poisoned")
        .background_task_stop_target(&background_task_id);
    let body = match (commands, target) {
        (None, _) => compaction_error(
            RelayErrorCode::InvalidState,
            "session is closed; no ACP runtime can stop the background task",
        ),
        (_, Err(error)) => compaction_error(RelayErrorCode::InvalidState, &format!("{error:#}")),
        (Some(commands), Ok(target)) => {
            // The relay expects the adapter's acknowledgement once a Claude
            // stop is requested; forget it if the stop never gets there.
            let claude_task_id = match &target {
                mj_core::relay::BackgroundTaskStopTarget::ClaudeAsyncTask { task_id } => {
                    Some(task_id.clone())
                }
                _ => None,
            };
            let stop_not_sent = || {
                if let Some(task_id) = &claude_task_id {
                    relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .claude_stop_not_sent(task_id);
                }
            };
            let (resolved, resolution) = tokio::sync::oneshot::channel();
            match commands
                .send(CommandRequest::StopBackgroundTask { target, resolved })
                .await
            {
                Ok(()) => match resolution.await {
                    Ok(Ok(())) => RelayResponseBody::Ok {
                        payload: RelayResponsePayload::BackgroundTaskStopRequested {
                            background_task_id,
                        },
                    },
                    Ok(Err(message)) => {
                        stop_not_sent();
                        compaction_error(RelayErrorCode::InvalidState, &message)
                    }
                    Err(_) => {
                        stop_not_sent();
                        compaction_error(
                            RelayErrorCode::Internal,
                            "ACP runtime stopped before resolving the background task stop",
                        )
                    }
                },
                Err(_) => {
                    stop_not_sent();
                    compaction_error(
                        RelayErrorCode::Internal,
                        "ACP runtime stopped before accepting the background task stop",
                    )
                }
            }
        }
    };
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body,
    }
}

/// Serve a credential or skills request against this relay's own harness
/// home. File work runs on a blocking thread so neither the socket task
/// nor the ACP coordinator is stalled by filesystem I/O.
pub(crate) async fn credential_response(
    envelope: RelayRequestEnvelope,
    credentials: &std::result::Result<CredentialEndpoint, String>,
    relay_root: &std::path::Path,
) -> RelayResponseEnvelope {
    let body = match credentials {
        Err(message) => RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                message: message.clone(),
                retryable: false,
                detail: None,
            },
        },
        Ok(endpoint) => {
            let endpoint = endpoint.clone();
            let github_token_path = relay_root.join("github-token");
            let request = envelope.request.clone();
            match tokio::task::spawn_blocking(move || {
                apply_credential_request_at(&endpoint, &github_token_path, &request)
            })
            .await
            {
                Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
                Ok(Err(error)) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::InvalidRequest,
                        message: format!("{error:#}"),
                        retryable: false,
                        detail: None,
                    },
                },
                Err(error) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::Internal,
                        message: format!("credential task stopped: {error}"),
                        retryable: true,
                        detail: None,
                    },
                },
            }
        }
    };
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body,
    }
}

/// Snapshot and install project memory off the socket task. These payloads
/// are connection-only and are never journaled as conversation history.
pub(crate) async fn run_serialized_project_memory_io<T, F>(
    project_memory_io: &Arc<tokio::sync::Semaphore>,
    operation: F,
) -> std::result::Result<Result<T>, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    // A timed-out client can abandon this future, but Tokio cannot cancel
    // blocking filesystem work after it starts. Keep the permit inside
    // that work so reconnects wait instead of piling more reads and fsyncs
    // onto the degraded storage device.
    let permit = project_memory_io
        .clone()
        .acquire_owned()
        .await
        .expect("project memory I/O semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
}

pub(crate) async fn project_memory_response(
    envelope: RelayRequestEnvelope,
    endpoint: &ProjectMemoryEndpoint,
) -> RelayResponseEnvelope {
    let body = match endpoint.config.as_ref() {
        None => RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                message: "this session has no project memory endpoint".into(),
                retryable: false,
                detail: None,
            },
        },
        Some(memory) => {
            let memory = memory.clone();
            let request = envelope.request.clone();
            match run_serialized_project_memory_io(&endpoint.io, move || {
                apply_project_memory_request(&memory, &request)
            })
            .await
            {
                Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
                Ok(Err(error)) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::InvalidRequest,
                        message: format!("{error:#}"),
                        retryable: false,
                        detail: None,
                    },
                },
                Err(error) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::Internal,
                        message: format!("project memory task stopped: {error}"),
                        retryable: true,
                        detail: None,
                    },
                },
            }
        }
    };
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body,
    }
}

pub(crate) fn apply_project_memory_request(
    memory: &crate::worker_runtime::ProjectMemoryLaunchConfig,
    request: &RelayRequest,
) -> Result<RelayResponsePayload> {
    let replica = mj_core::project_memory::ProjectMemoryStore::new(&memory.root);
    let baseline = mj_core::project_memory::ProjectMemoryStore::new(&memory.baseline_root);
    match request {
        RelayRequest::ProjectMemorySnapshot => Ok(RelayResponsePayload::ProjectMemorySnapshot {
            baseline: baseline.snapshot()?,
            replica: replica.snapshot()?,
        }),
        RelayRequest::InstallProjectMemorySnapshot { snapshot } => {
            replica.install_snapshot(snapshot)?;
            baseline.install_snapshot(snapshot)?;
            Ok(RelayResponsePayload::ProjectMemorySnapshotInstalled)
        }
        other => bail!("{} is not a project memory request", other.method_name()),
    }
}

#[cfg(test)]
pub(crate) fn apply_credential_request(
    endpoint: &CredentialEndpoint,
    request: &RelayRequest,
) -> Result<RelayResponsePayload> {
    apply_credential_request_at(endpoint, &endpoint.home.join("github-token"), request)
}

pub(crate) fn apply_credential_request_at(
    endpoint: &CredentialEndpoint,
    github_token_path: &std::path::Path,
    request: &RelayRequest,
) -> Result<RelayResponsePayload> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use mj_core::credentials::{
        CredentialSnapshot, MAX_CREDENTIAL_BYTES, MAX_GITHUB_TOKEN_BYTES, read_credential_file,
        read_github_token, remove_github_token, write_credential_file, write_github_token,
    };
    use mj_core::skills::{
        MAX_SKILLS_ARCHIVE_BYTES, SkillsArchive, collect_skills, install_skills,
    };

    match request {
        RelayRequest::CredentialState => {
            let (snapshot, _) = read_credential_file(endpoint.harness, &endpoint.marker)?;
            Ok(credential_state_payload(&snapshot))
        }
        RelayRequest::ReadCredentials => {
            let (snapshot, bytes) = read_credential_file(endpoint.harness, &endpoint.marker)?;
            if !snapshot.present {
                bail!("session has no {} credentials", endpoint.marker.display());
            }
            Ok(RelayResponsePayload::Credentials {
                data: BASE64.encode(&bytes),
            })
        }
        RelayRequest::InstallCredentials { data } => {
            if data.len() > MAX_CREDENTIAL_BYTES * 2 {
                bail!("credential payload is above the {MAX_CREDENTIAL_BYTES} byte limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode credential payload")?;
            write_credential_file(endpoint.harness, &endpoint.marker, &bytes)?;
            Ok(credential_state_payload(&CredentialSnapshot::of(
                endpoint.harness,
                &bytes,
            )))
        }
        RelayRequest::SkillsState => {
            let archive = collect_skills(endpoint.harness, &endpoint.home)?;
            Ok(skills_state_payload(&archive.state()))
        }
        RelayRequest::InstallSkills { data } => {
            // Base64 inflates by a third; rejecting early keeps a hostile
            // controller from making the worker buffer an endless frame.
            if data.len() > MAX_SKILLS_ARCHIVE_BYTES * 2 {
                bail!("skills payload is above the {MAX_SKILLS_ARCHIVE_BYTES} byte archive limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode skills payload")?;
            let archive = SkillsArchive::decode(&bytes)?;
            install_skills(endpoint.harness, &endpoint.home, &archive)?;
            let installed = collect_skills(endpoint.harness, &endpoint.home)?;
            Ok(skills_state_payload(&installed.state()))
        }
        RelayRequest::GithubTokenState => {
            let (snapshot, _) = read_github_token(github_token_path)?;
            Ok(github_token_state_payload(&snapshot))
        }
        RelayRequest::InstallGithubToken { data } => {
            if data.len() > MAX_GITHUB_TOKEN_BYTES * 2 {
                bail!("GitHub token is above the {MAX_GITHUB_TOKEN_BYTES} byte limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode GitHub token payload")?;
            let snapshot = write_github_token(github_token_path, &bytes)?;
            Ok(github_token_state_payload(&snapshot))
        }
        RelayRequest::RemoveGithubToken => {
            remove_github_token(github_token_path)?;
            Ok(github_token_state_payload(
                &mj_core::credentials::GithubTokenSnapshot::absent(),
            ))
        }
        other => bail!(
            "{} is not a credential, GitHub token, or skills request",
            other.method_name()
        ),
    }
}

pub(crate) fn skills_state_payload(
    state: &mj_core::skills::SkillsSyncState,
) -> RelayResponsePayload {
    RelayResponsePayload::SkillsState {
        present: state.present,
        fingerprint: state.fingerprint.clone(),
    }
}

pub(crate) fn credential_state_payload(
    snapshot: &mj_core::credentials::CredentialSnapshot,
) -> RelayResponsePayload {
    RelayResponsePayload::CredentialState {
        present: snapshot.present,
        fingerprint: snapshot.fingerprint.clone(),
        freshness_epoch_ms: snapshot.freshness_epoch_ms,
    }
}

pub(crate) fn github_token_state_payload(
    snapshot: &mj_core::credentials::GithubTokenSnapshot,
) -> RelayResponsePayload {
    RelayResponsePayload::GithubTokenState {
        present: snapshot.present,
        fingerprint: snapshot.fingerprint.clone(),
    }
}

pub(crate) async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &RelayResponseEnvelope,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(response)?;
    if encoded.len().saturating_add(1) > mj_core::relay::MAX_FRAME_BYTES {
        bail!("relay response frame is too large");
    }
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Protocol rejections are ordinary responses rather than Rust errors, so
/// the socket loop must log them explicitly. Keep this next to the write
/// boundary so every request route (including credentials and old
/// protocol methods) gets the same session and operation context.
pub(crate) async fn write_logged_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &RelayResponseEnvelope,
    session_id: &str,
    operation: &str,
) -> Result<()> {
    if let RelayResponseBody::Error { error } = &response.body {
        tracing::warn!(
            %session_id,
            %operation,
            request_id = %response.request_id,
            protocol_version = response.protocol_version,
            relay_error_code = ?error.code,
            relay_retryable = error.retryable,
            error_message = %error.message,
            "relay request returned an error"
        );
    }
    write_response(writer, response).await
}

#[cfg(test)]
mod jev_tests {
    use super::*;
    #[tokio::test]
    async fn decision_reads_are_capability_gated_and_do_not_advance_the_relay() {
        let directory = tempfile::tempdir().unwrap();
        let relay = Arc::new(Mutex::new(
            DurableRelay::open(directory.path(), "jev-test", "test").unwrap(),
        ));
        let log = mj_core::jev::DecisionLog::open(directory.path().join("jev-decisions")).unwrap();
        let attempt = log.start("jev-test", "activity", "Work finished?", "Current request");
        attempt.update(
            Some("Finished"),
            serde_json::json!({"request":{"text":"Exact evidence"}}),
        );
        attempt.finish("applied", "Marked ready");
        let before = relay.lock().unwrap().operational_state();
        for (version, id) in [
            (18, None),
            (19, None),
            (19, Some(attempt.id())),
            (19, Some("rotated".into())),
        ] {
            let response = handle_request(
                &relay,
                RelayRequestEnvelope {
                    request_id: "read-only".into(),
                    protocol_version: version,
                    request: RelayRequest::JevDecisions {
                        decision_id: id.clone(),
                    },
                },
            )
            .await
            .unwrap();
            if version == 18 {
                assert!(matches!(response.body, RelayResponseBody::Error { .. }));
                continue;
            }
            let RelayResponseBody::Ok {
                payload: RelayResponsePayload::JevDecisions(page),
            } = response.body
            else {
                panic!("expected diagnostic page")
            };
            if id.as_deref() == Some("rotated") {
                assert!(page.decisions.is_empty());
            } else {
                assert_eq!(page.decisions[0].technical.is_some(), id.is_some());
            }
        }
        assert_eq!(relay.lock().unwrap().operational_state(), before);
    }
}
