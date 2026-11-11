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
    relay: &Arc<RelayCell>,
    envelope: RelayRequestEnvelope,
    harness_preparation: Option<mj_core::relay::HarnessPreparation>,
) -> Result<RelayResponseEnvelope> {
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
        deferred.set_harness_preparation(harness_preparation.clone());
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

pub(crate) async fn review_capture_response(
    envelope: RelayRequestEnvelope,
    capture: Option<Arc<ReviewCapture>>,
) -> RelayResponseEnvelope {
    let body = match capture {
        Some(capture) => {
            let RelayRequest::Reviewer { request, .. } = envelope.request else {
                unreachable!("review_capture_response only serves capture requests");
            };
            match capture.handle(request).await {
                Ok(body) => body,
                Err(error) => compaction_error(RelayErrorCode::InvalidState, &format!("{error:#}")),
            }
        }
        None => compaction_error(
            RelayErrorCode::InvalidState,
            "review capture is unavailable",
        ),
    };
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body,
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
    relay: &Arc<RelayCell>,
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
                mj_core::relay::BackgroundTaskStopTarget::AirAsyncTask {
                    session_id,
                    task_id,
                } => Some((session_id.clone(), task_id.clone())),
                _ => None,
            };
            let stop_not_sent = || {
                if let Some((session_id, task_id)) = &claude_task_id {
                    relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .claude_stop_not_sent(session_id, task_id);
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
            let result = match &envelope.request {
                RelayRequest::InstallCredentials { .. }
                    if endpoint.harness.keeps_logins_in_database() =>
                {
                    let request = envelope.request.clone();
                    tokio::task::spawn_blocking(move || {
                        apply_credential_request_at(&endpoint, &github_token_path, &request)
                    })
                    .await
                }
                RelayRequest::InstallCredentials { data } => {
                    Ok(apply_install_credentials(&endpoint, data).await)
                }
                request => {
                    let request = request.clone();
                    tokio::task::spawn_blocking(move || {
                        apply_credential_request_at(&endpoint, &github_token_path, &request)
                    })
                    .await
                }
            };
            match result {
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

async fn apply_install_credentials(
    endpoint: &CredentialEndpoint,
    data: &str,
) -> Result<RelayResponsePayload> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use mj_core::credentials::{MAX_CREDENTIAL_BYTES, validate_credential_payload};

    if endpoint.harness.keeps_logins_in_database() {
        bail!("database-backed credentials require the authoritative install path");
    }
    if data.len() > MAX_CREDENTIAL_BYTES * 2 {
        bail!("credential payload is above the {MAX_CREDENTIAL_BYTES} byte limit");
    }
    let bytes = BASE64
        .decode(data.as_bytes())
        .context("decode credential payload")?;
    validate_credential_payload(endpoint.harness, &bytes)?;
    let installed = mj_core::credentials::install_session_credentials(
        endpoint.harness,
        &endpoint.marker,
        &bytes,
    )
    .await
    .with_context(|| format!("install credentials at {}", endpoint.marker.display()))?;
    Ok(credential_state_payload(&installed.snapshot))
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

pub(super) async fn project_memory_response(
    envelope: RelayRequestEnvelope,
    endpoint: &ProjectMemoryEndpoint,
    transfer: &mut ConnectionProjectMemoryTransfer,
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
            let request_id = envelope.request_id.clone();
            let protocol_version = envelope.protocol_version;
            let mut connection_transfer = std::mem::take(transfer);
            match run_serialized_project_memory_io(&endpoint.io, move || {
                let result = match &request {
                    RelayRequest::ProjectMemoryTransfer { action } => {
                        apply_project_memory_transfer_request(
                            &memory,
                            &mut connection_transfer,
                            action,
                            &request_id,
                            protocol_version,
                        )
                    }
                    _ => apply_project_memory_request(&memory, &request),
                };
                Ok((result, connection_transfer))
            })
            .await
            {
                Ok(Ok((result, connection_transfer))) => {
                    *transfer = connection_transfer;
                    match result {
                        Ok(payload) => RelayResponseBody::Ok { payload },
                        Err(error) => RelayResponseBody::Error {
                            error: RelayProtocolError {
                                code: RelayErrorCode::InvalidRequest,
                                message: format!("{error:#}"),
                                retryable: false,
                                detail: None,
                            },
                        },
                    }
                }
                Ok(Err(error)) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::Internal,
                        message: format!("{error:#}"),
                        retryable: true,
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

#[derive(Default)]
pub(super) struct ConnectionProjectMemoryTransfer {
    next_transfer_id: u64,
    active: ActiveProjectMemoryTransfer,
}

#[derive(Default)]
enum ActiveProjectMemoryTransfer {
    #[default]
    None,
    Download {
        transfer_id: u64,
        bytes: Vec<u8>,
        next_offset: u64,
    },
    Upload {
        transfer_id: u64,
        kind: mj_core::relay::ProjectMemoryUploadKind,
        total_bytes: u64,
        bytes: Vec<u8>,
    },
}

impl ConnectionProjectMemoryTransfer {
    fn allocate_id(&mut self) -> Result<u64> {
        let id = self
            .next_transfer_id
            .checked_add(1)
            .context("project-memory transfer ID space exhausted for this connection")?;
        self.next_transfer_id = id;
        Ok(id)
    }
}

fn apply_project_memory_transfer_request(
    memory: &crate::worker_runtime::ProjectMemoryLaunchConfig,
    transfer: &mut ConnectionProjectMemoryTransfer,
    action: &mj_core::relay::ProjectMemoryTransferAction,
    request_id: &str,
    protocol_version: u32,
) -> Result<RelayResponsePayload> {
    use base64::Engine as _;
    use mj_core::relay::{
        ProjectMemoryTransferAction as Action, ProjectMemoryTransferResult as ResultKind,
        ProjectMemoryUploadKind as UploadKind,
    };

    match action {
        Action::BeginSnapshot => {
            anyhow::ensure!(
                matches!(&transfer.active, ActiveProjectMemoryTransfer::None),
                "this connection already owns a project-memory transfer; abort it first"
            );
            let baseline = mj_core::project_memory::ProjectMemoryStore::new(&memory.baseline_root)
                .snapshot()?;
            let replica =
                mj_core::project_memory::ProjectMemoryStore::new(&memory.root).snapshot()?;
            let bytes = mj_core::relay::ProjectMemoryWireSnapshot::encode(
                baseline,
                replica,
                protocol_version,
            )?;
            let total_bytes = u64::try_from(bytes.len())
                .context("project-memory snapshot exceeds the transfer offset range")?;
            let transfer_id = transfer.allocate_id()?;
            transfer.active = ActiveProjectMemoryTransfer::Download {
                transfer_id,
                bytes,
                next_offset: 0,
            };
            Ok(RelayResponsePayload::ProjectMemoryTransfer {
                result: ResultKind::SnapshotStarted {
                    transfer_id,
                    total_bytes,
                },
            })
        }
        Action::ReadSnapshotPage {
            transfer_id,
            offset,
        } => {
            let ActiveProjectMemoryTransfer::Download {
                transfer_id: active_id,
                bytes,
                next_offset,
            } = &mut transfer.active
            else {
                anyhow::bail!("this connection has no project-memory snapshot transfer")
            };
            anyhow::ensure!(
                transfer_id == active_id,
                "project-memory snapshot transfer ID does not match this connection"
            );
            anyhow::ensure!(
                offset == next_offset,
                "project-memory snapshot page offset mismatch: expected {next_offset}, got {offset}"
            );
            let capacity = mj_core::relay::project_memory_transfer_response_chunk_bytes(
                protocol_version,
                request_id,
            )?;
            let start = usize::try_from(*offset)
                .context("project-memory snapshot offset exceeds addressable memory")?;
            let end = start.saturating_add(capacity).min(bytes.len());
            let chunk = base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]);
            let next = u64::try_from(end)
                .context("project-memory snapshot offset exceeds the transfer range")?;
            let complete = end == bytes.len();
            let id = *active_id;
            *next_offset = next;
            if complete {
                transfer.active = ActiveProjectMemoryTransfer::None;
            }
            Ok(RelayResponsePayload::ProjectMemoryTransfer {
                result: ResultKind::SnapshotPage {
                    transfer_id: id,
                    offset: *offset,
                    next_offset: next,
                    complete,
                    chunk,
                },
            })
        }
        Action::BeginUpload { kind, total_bytes } => {
            anyhow::ensure!(
                matches!(&transfer.active, ActiveProjectMemoryTransfer::None),
                "this connection already owns a project-memory transfer; abort it first"
            );
            usize::try_from(*total_bytes)
                .context("project-memory upload exceeds addressable memory")?;
            let transfer_id = transfer.allocate_id()?;
            transfer.active = ActiveProjectMemoryTransfer::Upload {
                transfer_id,
                kind: kind.clone(),
                total_bytes: *total_bytes,
                bytes: Vec::new(),
            };
            Ok(RelayResponsePayload::ProjectMemoryTransfer {
                result: ResultKind::UploadStarted { transfer_id },
            })
        }
        Action::AppendUpload {
            transfer_id,
            offset,
            chunk,
        } => {
            let ActiveProjectMemoryTransfer::Upload {
                transfer_id: active_id,
                total_bytes,
                bytes,
                ..
            } = &mut transfer.active
            else {
                anyhow::bail!("this connection has no project-memory upload transfer")
            };
            anyhow::ensure!(
                transfer_id == active_id,
                "project-memory upload transfer ID does not match this connection"
            );
            let expected_offset = u64::try_from(bytes.len())
                .context("project-memory upload offset exceeds the transfer range")?;
            anyhow::ensure!(
                *offset == expected_offset,
                "project-memory upload offset mismatch: expected {expected_offset}, got {offset}"
            );
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .context("decode project-memory upload chunk")?;
            anyhow::ensure!(!decoded.is_empty(), "project-memory upload chunk is empty");
            let new_len = bytes
                .len()
                .checked_add(decoded.len())
                .context("project-memory upload length overflow")?;
            anyhow::ensure!(
                u64::try_from(new_len)? <= *total_bytes,
                "project-memory upload exceeds its declared byte length"
            );
            bytes.extend_from_slice(&decoded);
            Ok(RelayResponsePayload::ProjectMemoryTransfer {
                result: ResultKind::UploadProgress {
                    transfer_id: *active_id,
                    next_offset: u64::try_from(new_len)?,
                },
            })
        }
        Action::CommitUpload { transfer_id } => {
            let ActiveProjectMemoryTransfer::Upload {
                transfer_id: active_id,
                kind,
                total_bytes,
                bytes,
            } = std::mem::take(&mut transfer.active)
            else {
                anyhow::bail!("this connection has no project-memory upload transfer")
            };
            if *transfer_id != active_id {
                transfer.active = ActiveProjectMemoryTransfer::Upload {
                    transfer_id: active_id,
                    kind,
                    total_bytes,
                    bytes,
                };
                anyhow::bail!("project-memory upload transfer ID does not match this connection");
            }
            if u64::try_from(bytes.len())? != total_bytes {
                transfer.active = ActiveProjectMemoryTransfer::Upload {
                    transfer_id: active_id,
                    kind,
                    total_bytes,
                    bytes,
                };
                anyhow::bail!("project-memory upload is incomplete");
            }
            let tree: mj_core::relay::ProjectMemoryWireTree =
                serde_json::from_slice(&bytes).context("parse uploaded project-memory tree")?;
            match kind {
                UploadKind::InstallSnapshot => {
                    apply_project_memory_request(
                        memory,
                        &RelayRequest::InstallProjectMemorySnapshot { snapshot: tree },
                    )?;
                    Ok(RelayResponsePayload::ProjectMemoryTransfer {
                        result: ResultKind::Installed,
                    })
                }
                UploadKind::ReplaceTree { expected_replica } => {
                    let payload = apply_project_memory_request(
                        memory,
                        &RelayRequest::ReplaceProjectMemoryTree {
                            expected_replica,
                            tree,
                        },
                    )?;
                    let RelayResponsePayload::ProjectMemoryTreeReplaced { outcome } = payload
                    else {
                        anyhow::bail!("project-memory replacement returned an unexpected result")
                    };
                    Ok(RelayResponsePayload::ProjectMemoryTransfer {
                        result: ResultKind::Replaced { outcome },
                    })
                }
            }
        }
        Action::Abort { transfer_id } => {
            let active_id = match &transfer.active {
                ActiveProjectMemoryTransfer::Download { transfer_id, .. }
                | ActiveProjectMemoryTransfer::Upload { transfer_id, .. } => *transfer_id,
                ActiveProjectMemoryTransfer::None => {
                    anyhow::bail!("this connection has no project-memory transfer to abort")
                }
            };
            anyhow::ensure!(
                *transfer_id == active_id,
                "project-memory transfer ID does not match this connection"
            );
            transfer.active = ActiveProjectMemoryTransfer::None;
            Ok(RelayResponsePayload::ProjectMemoryTransfer {
                result: ResultKind::Aborted {
                    transfer_id: active_id,
                },
            })
        }
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
            baseline: mj_core::relay::ProjectMemoryWireTree::encode(
                baseline.snapshot()?,
                mj_core::relay::RELAY_PROTOCOL_VERSION,
            )?,
            replica: mj_core::relay::ProjectMemoryWireTree::encode(
                replica.snapshot()?,
                mj_core::relay::RELAY_PROTOCOL_VERSION,
            )?,
        }),
        RelayRequest::InstallProjectMemorySnapshot { snapshot } => {
            let snapshot = snapshot.clone().decode()?;
            replica.install_snapshot(&snapshot)?;
            baseline.install_snapshot(&snapshot)?;
            Ok(RelayResponsePayload::ProjectMemorySnapshotInstalled)
        }
        RelayRequest::ReplaceProjectMemoryTree {
            expected_replica,
            tree,
        } => {
            let current_replica = replica.snapshot()?;
            if current_replica.version() != *expected_replica {
                return Ok(RelayResponsePayload::ProjectMemoryTreeReplaced {
                    outcome: mj_core::project_memory::ReplicaReplaceOutcome::ReplicaChanged,
                });
            }
            let tree = tree.clone().decode()?;
            replica.replace_tree(&tree)?;
            baseline.replace_tree(&tree)?;
            Ok(RelayResponsePayload::ProjectMemoryTreeReplaced {
                outcome: mj_core::project_memory::ReplicaReplaceOutcome::Replaced,
            })
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

    let slopcop_login_path = endpoint.home.join(mj_core::slopcop::LOGIN_FILE);
    match request {
        RelayRequest::AwsCredentialsState => Ok(credential_state_payload(
            &mj_core::aws_credentials::state(&endpoint.home)?,
        )),
        RelayRequest::AwsCredentialsScope => Ok(RelayResponsePayload::AwsCredentialsScope {
            scope: mj_core::aws_credentials::scope(&endpoint.home)?,
            profile: mj_core::aws_credentials::staged_profile(&endpoint.home)?,
        }),
        RelayRequest::RemoveAwsCredentials => Ok(credential_state_payload(
            &mj_core::aws_credentials::remove(&endpoint.home)?,
        )),
        RelayRequest::SlopcopLoginState => Ok(credential_state_payload(
            &mj_core::slopcop::read_login(&slopcop_login_path)?.0,
        )),
        RelayRequest::InstallSlopcopLogin { data } => {
            if data.len() > mj_core::slopcop::MAX_LOGIN_BYTES * 2 {
                bail!(
                    "slopcop login is above the {} byte limit",
                    mj_core::slopcop::MAX_LOGIN_BYTES
                );
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode slopcop login payload")?;
            Ok(credential_state_payload(&mj_core::slopcop::write_login(
                &slopcop_login_path,
                &bytes,
            )?))
        }
        RelayRequest::RemoveSlopcopLogin => {
            mj_core::slopcop::remove_login(&slopcop_login_path)?;
            Ok(credential_state_payload(
                &mj_core::credentials::CredentialSnapshot::absent(),
            ))
        }
        RelayRequest::InstallAwsCredentials { data, scope } => {
            if data.len() > MAX_CREDENTIAL_BYTES * 2 {
                bail!("AWS credential payload is above the {MAX_CREDENTIAL_BYTES} byte limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode AWS credential payload")?;
            let credentials =
                mj_core::aws_credentials::AwsCredentials::decode(&bytes)?.with_scope(scope.clone());
            Ok(credential_state_payload(
                &credentials.install(&endpoint.home, &endpoint.home)?,
            ))
        }
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
            if !endpoint.harness.keeps_logins_in_database() {
                bail!("file credential installs must use the async destination-owned install path");
            }
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
            anyhow::ensure!(
                !endpoint.no_github_credentials,
                "GitHub credentials are disabled for this session"
            );
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
mod tests {
    use super::*;

    fn codex_login(refreshed: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": format!("access-{refreshed}")},
            "last_refresh": refreshed,
        }))
        .unwrap()
    }

    // Hard-won: #1485: an install used bytes captured before waiting for the harness refresh lock.
    #[tokio::test]
    async fn credential_install_reports_the_newer_file_found_after_waiting_for_native_lock() {
        use base64::Engine as _;

        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("auth.json");
        let initial = codex_login("2026-09-25T20:00:00Z");
        let incoming = codex_login("2026-09-25T21:00:00Z");
        let latest = codex_login("2026-09-25T22:00:00Z");
        std::fs::write(&marker, &initial).unwrap();
        let held = mj_core::credentials::CredentialWriteGuard::acquire(
            mj_core::config::HarnessKind::Codex,
            &marker,
        )
        .await
        .unwrap();
        let endpoint = CredentialEndpoint {
            no_github_credentials: false,
            harness: mj_core::config::HarnessKind::Codex,
            home: home.path().to_owned(),
            marker: marker.clone(),
        };
        let data = base64::engine::general_purpose::STANDARD.encode(incoming);
        let mut install = tokio::spawn(async move {
            credential_response(
                RelayRequestEnvelope {
                    request_id: "credential-lock-race".into(),
                    protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
                    request: RelayRequest::InstallCredentials { data },
                },
                &Ok(endpoint.clone()),
                &endpoint.home,
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut install)
                .await
                .is_err(),
            "install must wait for the held destination lock"
        );
        std::fs::write(&marker, &latest).unwrap();
        drop(held);

        let response = install.await.unwrap();
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::CredentialState {
                    fingerprint,
                    present,
                    ..
                },
        } = response.body
        else {
            panic!("unexpected credential install response: {response:?}");
        };
        assert!(present);
        assert_eq!(
            fingerprint,
            mj_core::credentials::credential_fingerprint(&latest)
        );
        assert_eq!(std::fs::read(marker).unwrap(), latest);
    }
}
