use super::*;
use mj_core::relay::{
    ProjectMemoryTransferAction as MemoryAction, ProjectMemoryTransferResult as MemoryResult,
};

async fn memory_cancelled(cancel: Option<&tokio_util::sync::CancellationToken>) {
    match cancel {
        Some(cancel) => cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Owns a staged transfer until completion or an acknowledged abort. Dropping
/// its future closes the input and invalidates the existing connection owner.
struct ProjectMemoryTransfer<'a> {
    client: Option<&'a mut RelayClient>,
    id: u64,
}

impl ProjectMemoryTransfer<'_> {
    async fn call(&mut self, action: MemoryAction) -> Result<MemoryResult> {
        match self
            .client
            .as_mut()
            .unwrap()
            .call(RelayRequest::ProjectMemoryTransfer { action })
            .await?
        {
            RelayResponsePayload::ProjectMemoryTransfer { result } => Ok(result),
            _ => bail!("relay returned an unexpected project-memory transfer response"),
        }
    }

    async fn abort(mut self, error: anyhow::Error) -> anyhow::Error {
        let client = self.client.as_ref().unwrap();
        if client.abandoned.is_some() || error.downcast_ref::<RelayTransportDead>().is_some() {
            // Drop closes the connection and lets its worker clean up staging.
            // An abort cannot succeed here and would obscure the primary cause.
            return error;
        }
        match self
            .call(MemoryAction::Abort {
                transfer_id: self.id,
            })
            .await
        {
            Ok(MemoryResult::Aborted { transfer_id }) if transfer_id == self.id => {
                self.client.take();
                error
            }
            result => error.context(format!("abort project-memory transfer failed: {result:?}")),
        }
    }

    fn complete(mut self) {
        self.client.take();
    }
}

impl Drop for ProjectMemoryTransfer<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.abandoned =
                Some("project-memory transfer was abandoned; reconnect the relay".into());
            client.input.take();
        }
    }
}

// A disposable proxy cannot prove that the worker or its Control transport died.
// Preserve typed protocol/application errors, but keep restart markers local.
fn bulk_connection_error(error: anyhow::Error) -> anyhow::Error {
    if RelayTransportDead::marks(&error) {
        anyhow!("bulk relay connection failed: {error:#}")
    } else {
        error
    }
}

impl RelayClient {
    /// Bulk failures belong to the disposable connection, never the Control owner.
    async fn with_bulk<T>(
        &mut self,
        operation: impl for<'a> FnOnce(
            &'a mut Self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>,
        > + Send,
    ) -> Result<T> {
        if let Some(reason) = &self.abandoned {
            bail!("{reason}");
        }
        let Some(spec) = &self.bulk_spec else {
            return operation(self).await;
        };
        let mut bulk = Self::connect_with_timeouts(
            spec,
            &self.session_id,
            self.request_timeout,
            RELAY_HANDSHAKE_TIMEOUT,
        )
        .await
        .map_err(bulk_connection_error)?;
        bulk.set_adoption_wait_progress(self.adoption_wait_progress.clone());
        let result = operation(&mut bulk).await.map_err(bulk_connection_error);
        if let Err(error) = bulk.detach().await {
            tracing::warn!(session_id = %self.session_id, %error, "detach bulk relay");
        }
        result
    }

    async fn call_bulk(&mut self, request: RelayRequest) -> Result<RelayResponsePayload> {
        if !request.supported_at(self.protocol_version) {
            return self.call(request).await;
        }
        self.with_bulk(move |bulk| Box::pin(async move { bulk.call(request).await }))
            .await
    }

    pub async fn cpu_usage(&mut self) -> Result<Option<mj_core::cpu_usage::SessionCpuUsage>> {
        if !RelayRequest::CpuUsage.supported_at(self.protocol_version) {
            return Ok(None);
        }
        match self.call(RelayRequest::CpuUsage).await? {
            RelayResponsePayload::CpuUsage { usage } => Ok(usage),
            _ => bail!("relay returned an unexpected CPU usage response"),
        }
    }

    pub async fn history_requests(&mut self) -> Result<Vec<mj_core::history::HistoryRequest>> {
        if !RelayRequest::HistoryRequests.supported_at(self.protocol_version) {
            return Ok(Vec::new());
        }
        match self.call(RelayRequest::HistoryRequests).await? {
            RelayResponsePayload::HistoryRequests { requests } => Ok(requests),
            _ => bail!("relay returned an unexpected history queue response"),
        }
    }

    pub async fn complete_history_request(
        &mut self,
        result: mj_core::history::HistoryResult,
    ) -> Result<()> {
        match self
            .call(RelayRequest::CompleteHistoryRequest { result })
            .await?
        {
            RelayResponsePayload::HistoryRequestCompleted => Ok(()),
            _ => bail!("relay returned an unexpected history completion response"),
        }
    }

    pub async fn guardian_requests(
        &mut self,
    ) -> Result<Vec<mj_core::guardian::GuardianReviewRequest>> {
        if !RelayRequest::GuardianRequests.supported_at(self.protocol_version) {
            return Ok(Vec::new());
        }
        match self.call(RelayRequest::GuardianRequests).await? {
            RelayResponsePayload::GuardianRequests { requests } => Ok(requests),
            _ => bail!("relay returned an unexpected guardian queue response"),
        }
    }

    pub async fn complete_guardian_request(
        &mut self,
        result: mj_core::guardian::GuardianReviewResult,
    ) -> Result<()> {
        if !RelayRequest::GuardianRequests.supported_at(self.protocol_version) {
            bail!("worker does not support guardian review completion");
        }
        match self
            .call(RelayRequest::CompleteGuardianRequest { result })
            .await?
        {
            RelayResponsePayload::GuardianRequestCompleted => Ok(()),
            _ => bail!("relay returned an unexpected guardian completion response"),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn supports_project_memory_sync(&self) -> bool {
        RelayRequest::ProjectMemorySnapshot.supported_at(self.protocol_version)
    }

    pub fn supports_slopcop_login_sync(&self) -> bool {
        RelayRequest::SlopcopLoginState.supported_at(self.protocol_version)
    }

    /// The skills archive format this session's worker reads. A worker from
    /// before `RELAY_GZIP_SKILLS_PROTOCOL` reads only the uncompressed format.
    pub fn skills_archive_format(&self) -> mj_core::skills::SkillsArchiveFormat {
        if self.protocol_version >= mj_core::relay::RELAY_GZIP_SKILLS_PROTOCOL {
            mj_core::skills::SkillsArchiveFormat::Gzip
        } else {
            mj_core::skills::SkillsArchiveFormat::Plain
        }
    }

    pub fn relay_version(&self) -> &str {
        &self.relay_version
    }

    /// Content address of the executable serving this connection, or `None`
    /// from a worker too old to report one. A controller reads `None` as
    /// outdated: it predates the field, so it predates this controller.
    pub fn worker_build(&self) -> Option<&str> {
        self.worker_build.as_deref()
    }

    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// The digest of the operational state this connection last received.
    pub fn known_state_digest(&self) -> Option<&str> {
        self.known_state.as_ref().map(|(digest, _)| digest.as_str())
    }

    pub fn latest_ordinal(&self) -> u64 {
        self.latest_ordinal
    }

    pub fn latest_digest(&self) -> &str {
        &self.latest_digest
    }

    pub async fn attach(
        &mut self,
        after_ordinal: u64,
        after_digest: impl Into<String>,
    ) -> Result<RelayAttachment> {
        let request = self.attach_request(after_ordinal, after_digest.into());
        let response = self
            .call_with_timeout(request.clone(), RELAY_HISTORY_TIMEOUT)
            .await?;
        self.decode_attachment(request, response)
    }

    pub(crate) fn attach_request(&self, after_ordinal: u64, after_digest: String) -> RelayRequest {
        let known_state_digest = (self.protocol_version
            >= mj_core::relay::RELAY_STATE_DIGEST_PROTOCOL)
            .then(|| self.known_state.as_ref().map(|(digest, _)| digest.clone()))
            .flatten();
        RelayRequest::Attach {
            after_ordinal,
            after_digest,
            known_state_digest,
        }
    }

    pub(crate) fn decode_attachment(
        &mut self,
        request: RelayRequest,
        response: RelayResponsePayload,
    ) -> Result<RelayAttachment> {
        let RelayRequest::Attach {
            after_ordinal,
            after_digest,
            known_state_digest,
        } = request
        else {
            bail!("expected attach request");
        };
        let (state, events, through_ordinal, through_digest) = match response {
            RelayResponsePayload::Attached {
                mut state,
                events,
                through_ordinal,
                through_digest,
                state_digest,
            } => {
                state.relay_protocol_version = Some(self.protocol_version);
                let state = mj_core::snapshot_map::SharedSnapshot::from(state);
                self.known_state = state_digest.map(|digest| (digest, state.clone()));
                (state, events, through_ordinal, through_digest)
            }
            RelayResponsePayload::AttachedWithKnownState {
                events,
                through_ordinal,
                through_digest,
            } => {
                let held = self
                    .known_state
                    .as_ref()
                    .filter(|(digest, _)| Some(digest) == known_state_digest.as_ref())
                    .context("relay answered with a state this connection does not hold")?;
                (held.1.clone(), events, through_ordinal, through_digest)
            }
            _ => bail!("relay returned an unexpected attach response"),
        };
        let mut cursor = RelayCursor {
            ordinal: after_ordinal,
            digest: after_digest,
        };
        for event in &events {
            validate_relay_event(cursor.ordinal, &cursor.digest, event)
                .context("verify relay attachment event chain")?;
            cursor.ordinal = event.ordinal;
            cursor.digest.clone_from(&event.digest);
        }
        if cursor.ordinal != through_ordinal || cursor.digest != through_digest {
            bail!("relay attachment frontier does not match its event chain");
        }
        self.latest_ordinal = state.latest_ordinal;
        self.latest_digest = state.latest_digest.clone();
        Ok(RelayAttachment {
            state,
            events,
            through_ordinal,
            through_digest,
        })
    }

    /// Start a bounded catch-up by capturing the relay frontier before the
    /// caller applies anything. Callers persist `first_page`, request further
    /// pages with [`Self::next_catch_up_page`], and may acknowledge the fixed
    /// frontier after all of those pages are durable.
    pub async fn begin_catch_up(
        &mut self,
        after_ordinal: u64,
        after_digest: impl Into<String>,
    ) -> Result<RelayCatchUp> {
        let after_digest = after_digest.into();
        let first = self.attach(after_ordinal, after_digest.clone()).await?;
        Self::catch_up_from_attachment(after_ordinal, after_digest, first)
    }

    pub(crate) fn catch_up_from_attachment(
        after_ordinal: u64,
        after_digest: String,
        first: RelayAttachment,
    ) -> Result<RelayCatchUp> {
        let frontier = RelayCursor {
            ordinal: first.state.latest_ordinal,
            digest: first.state.latest_digest.clone(),
        };
        let previous = RelayCursor {
            ordinal: after_ordinal,
            digest: after_digest,
        };
        let RelayAttachment {
            state,
            events,
            through_ordinal,
            through_digest,
        } = first;
        let first_page = clip_catch_up_page(
            RelayEventPage {
                events,
                through_ordinal,
                through_digest,
            },
            &previous,
            &frontier,
        )?;
        Ok(RelayCatchUp {
            state,
            frontier,
            first_page,
        })
    }

    /// Fetch the next bounded page without chasing events that arrived after
    /// `frontier` was captured. A response may contain such newer events; the
    /// returned page is clipped at the exact ordinal-and-digest frontier.
    pub async fn next_catch_up_page(
        &mut self,
        previous: &RelayCursor,
        frontier: &RelayCursor,
    ) -> Result<RelayEventPage> {
        if previous.ordinal >= frontier.ordinal {
            bail!("relay catch-up is already at its fixed frontier");
        }
        let RelayAttachment {
            events,
            through_ordinal,
            through_digest,
            ..
        } = self
            .attach(previous.ordinal, previous.digest.clone())
            .await?;
        clip_catch_up_page(
            RelayEventPage {
                events,
                through_ordinal,
                through_digest,
            },
            previous,
            frontier,
        )
    }

    pub async fn acknowledge(
        &mut self,
        through_ordinal: u64,
        through_digest: impl Into<String>,
    ) -> Result<RelayCursor> {
        match self
            .call_with_timeout(
                RelayRequest::Acknowledge {
                    through_ordinal,
                    through_digest: through_digest.into(),
                },
                RELAY_ACKNOWLEDGE_TIMEOUT,
            )
            .await?
        {
            RelayResponsePayload::Acknowledged {
                through_ordinal,
                through_digest,
            } => Ok(RelayCursor {
                ordinal: through_ordinal,
                digest: through_digest,
            }),
            _ => bail!("relay returned an unexpected acknowledgement response"),
        }
    }

    pub async fn status(&mut self) -> Result<RelayOperationalState> {
        match self.call(RelayRequest::Status).await? {
            RelayResponsePayload::Status(mut status) => {
                status.relay_protocol_version = Some(self.protocol_version);
                self.latest_ordinal = status.latest_ordinal;
                self.latest_digest = status.latest_digest.clone();
                Ok(status)
            }
            _ => bail!("relay returned an unexpected status response"),
        }
    }

    /// Return the fingerprint and freshness of this session's harness
    /// credentials without exposing the credential bytes.
    pub async fn credential_state(&mut self) -> Result<CredentialSnapshot> {
        credential_snapshot(self.call(RelayRequest::CredentialState).await?)
    }

    pub async fn aws_credentials_state(&mut self) -> Result<CredentialSnapshot> {
        credential_snapshot(self.call(RelayRequest::AwsCredentialsState).await?)
    }

    pub async fn install_aws_credentials(
        &mut self,
        credentials: &mj_core::aws_credentials::AwsCredentials,
    ) -> Result<CredentialSnapshot> {
        credential_snapshot(
            self.call(RelayRequest::InstallAwsCredentials {
                data: BASE64.encode(credentials.encode()?),
                // A worker before protocol 55 rejects the unknown field.
                scope: credentials
                    .scope()
                    .filter(|_| {
                        self.protocol_version
                            >= mj_core::relay::RELAY_AWS_CREDENTIALS_REMOVAL_PROTOCOL
                    })
                    .cloned(),
            })
            .await?,
        )
    }

    /// The scope the worker recorded beside the session's AWS credentials.
    /// Needs a worker at relay protocol 55 or later.
    pub async fn aws_credentials_scope(
        &mut self,
    ) -> Result<Option<mj_core::aws_credentials::AwsCredentialScope>> {
        match self.call(RelayRequest::AwsCredentialsScope).await? {
            RelayResponsePayload::AwsCredentialsScope { scope, .. } => Ok(scope),
            _ => bail!("relay returned an unexpected AWS credential scope response"),
        }
    }

    /// Delete the session's AWS credentials. Needs a worker at relay protocol
    /// 55 or later.
    pub async fn remove_aws_credentials(&mut self) -> Result<CredentialSnapshot> {
        credential_snapshot(self.call(RelayRequest::RemoveAwsCredentials).await?)
    }

    /// Read this session's credential file. Callers must keep these bytes out
    /// of durable relay observations, logs, and archives.
    pub async fn read_retired_logins_after(
        &mut self,
        after: Option<&str>,
    ) -> Result<(Vec<mj_core::credentials::RetiredLoginSnapshot>, bool, bool)> {
        let request = RelayRequest::ReadRetiredLogins {
            after: after.map(str::to_owned),
        };
        if !request.supported_at(self.protocol_version) {
            return Ok((Vec::new(), false, false));
        }
        match self.call(request).await? {
            RelayResponsePayload::RetiredLogins {
                logins,
                has_more,
                deferred,
            } => Ok((logins, has_more, deferred)),
            _ => bail!("relay returned an unexpected retired login response"),
        }
    }

    pub async fn ack_retired_login(&mut self, id: &str, fingerprint: &str) -> Result<()> {
        match self
            .call(RelayRequest::AckRetiredLogin {
                id: id.to_owned(),
                fingerprint: fingerprint.to_owned(),
            })
            .await?
        {
            RelayResponsePayload::RetiredLoginAcknowledged => Ok(()),
            _ => bail!("relay returned an unexpected retired login acknowledgement"),
        }
    }

    pub async fn read_credentials(&mut self) -> Result<Vec<u8>> {
        match self.call(RelayRequest::ReadCredentials).await? {
            RelayResponsePayload::Credentials { data } => BASE64
                .decode(data.as_bytes())
                .context("decode relay credential payload"),
            _ => bail!("relay returned an unexpected credential response"),
        }
    }

    /// Install credentials into the harness home fixed by this session's
    /// launch config.
    pub async fn install_credentials(&mut self, bytes: &[u8]) -> Result<CredentialSnapshot> {
        credential_snapshot(
            self.call(RelayRequest::InstallCredentials {
                data: BASE64.encode(bytes),
            })
            .await?,
        )
    }

    pub async fn slopcop_login_state(&mut self) -> Result<CredentialSnapshot> {
        credential_snapshot(self.call(RelayRequest::SlopcopLoginState).await?)
    }

    pub async fn install_slopcop_login(
        &mut self,
        login: &mj_core::slopcop::SlopcopLogin,
    ) -> Result<CredentialSnapshot> {
        credential_snapshot(
            self.call(RelayRequest::InstallSlopcopLogin {
                data: BASE64.encode(login.encode()),
            })
            .await?,
        )
    }

    pub async fn remove_slopcop_login(&mut self) -> Result<CredentialSnapshot> {
        credential_snapshot(self.call(RelayRequest::RemoveSlopcopLogin).await?)
    }

    pub async fn github_token_state(
        &mut self,
    ) -> Result<mj_core::credentials::GithubTokenSnapshot> {
        github_token_snapshot(self.call(RelayRequest::GithubTokenState).await?)
    }

    pub async fn install_github_token(
        &mut self,
        token: &str,
    ) -> Result<mj_core::credentials::GithubTokenSnapshot> {
        github_token_snapshot(
            self.call(RelayRequest::InstallGithubToken {
                data: BASE64.encode(token.as_bytes()),
            })
            .await?,
        )
    }

    pub async fn remove_github_token(
        &mut self,
    ) -> Result<mj_core::credentials::GithubTokenSnapshot> {
        github_token_snapshot(self.call(RelayRequest::RemoveGithubToken).await?)
    }

    /// Return the fingerprint of this session's synced skills trees without
    /// transferring the tree itself.
    pub async fn skills_state(&mut self) -> Result<mj_core::skills::SkillsSyncState> {
        skills_sync_state(self.call(RelayRequest::SkillsState).await?)
    }

    /// Install background text that only the target harness sees, prepended
    /// to the next real prompt without creating a synthetic transcript turn.
    pub async fn install_prompt_context(&mut self, text: String) -> Result<()> {
        self.submit(
            crate::session_manager::new_command_id("prompt-context")?,
            RelayCommand::InstallPromptContext { text },
        )
        .await
        .map(|_| ())
    }

    async fn call_legacy_project_memory(
        &mut self,
        request: RelayRequest,
    ) -> Result<RelayResponsePayload> {
        // Legacy workers send one whole tree. Keep their connection protected
        // from a late response when checkpoint cancellation drops this future.
        let mut transfer = ProjectMemoryTransfer {
            client: Some(self),
            id: 0,
        };
        let result = transfer.client.as_mut().unwrap().call(request).await;
        transfer.complete();
        result
    }

    pub async fn project_memory_snapshot(
        &mut self,
    ) -> Result<(
        mj_core::project_memory::ProjectMemorySnapshot,
        mj_core::project_memory::ProjectMemorySnapshot,
    )> {
        if self.protocol_version >= mj_core::relay::RELAY_PROJECT_MEMORY_TRANSFER_PROTOCOL {
            return self.download_project_memory().await;
        }
        let request = RelayRequest::ProjectMemorySnapshot;
        match self.call_legacy_project_memory(request).await? {
            RelayResponsePayload::ProjectMemorySnapshot { baseline, replica } => {
                mj_core::runtime::spawn_blocking(move || {
                    Ok((baseline.decode()?, replica.decode()?))
                })
                .await
                .context("decode project memory task failed")?
            }
            _ => bail!("relay returned an unexpected project-memory response"),
        }
    }

    async fn begin_memory_transfer(&mut self, action: MemoryAction) -> Result<MemoryResult> {
        let mut transfer = ProjectMemoryTransfer {
            client: Some(self),
            id: 0,
        };
        let result = transfer.call(action).await;
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<RelayRejected>().is_some())
        {
            transfer.complete();
        }
        result
    }

    async fn download_project_memory(
        &mut self,
    ) -> Result<(
        mj_core::project_memory::ProjectMemorySnapshot,
        mj_core::project_memory::ProjectMemorySnapshot,
    )> {
        let started = self
            .begin_memory_transfer(MemoryAction::BeginSnapshot)
            .await?;
        let bytes = self.download_memory_pages(started, None).await?;
        mj_core::runtime::spawn_blocking(move || {
            mj_core::relay::ProjectMemoryWireSnapshot::decode(&bytes)
        })
        .await
        .context("decode project-memory snapshot task failed")?
    }

    pub async fn project_memory_turn_snapshot(
        &mut self,
        absorbed: Option<String>,
        head_version: mj_core::project_memory::TreeVersion,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        Option<(
            mj_core::project_memory::ProjectMemorySnapshot,
            String,
            mj_core::project_memory::ProjectMemorySnapshot,
        )>,
    > {
        let started = self
            .begin_memory_transfer(MemoryAction::BeginTurnSnapshot {
                absorbed,
                head_version,
            })
            .await?;
        if matches!(started, MemoryResult::InSync | MemoryResult::Busy) {
            return Ok(None);
        }
        let bytes = self.download_memory_pages(started, Some(cancel)).await?;
        mj_core::runtime::spawn_blocking(move || {
            let snapshot: mj_core::relay::ProjectMemoryTurnSnapshot =
                serde_json::from_slice(&bytes)?;
            Ok(Some((
                snapshot.base.decode()?,
                snapshot.id,
                snapshot.tree.decode()?,
            )))
        })
        .await
        .context("decode project-memory turn snapshot task failed")?
    }

    async fn download_memory_pages(
        &mut self,
        started: MemoryResult,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<u8>> {
        let MemoryResult::SnapshotStarted {
            transfer_id,
            total_bytes,
        } = started
        else {
            self.abandoned =
                Some("invalid project-memory snapshot start; reconnect the relay".into());
            self.input.take();
            bail!("relay returned an unexpected project-memory snapshot start response");
        };
        let started = Instant::now();
        let session_id = self.session_id.clone();
        let mut transfer = ProjectMemoryTransfer {
            client: Some(self),
            id: transfer_id,
        };
        let result = async {
            let mut bytes = Vec::new();
            let mut offset = 0u64;
            let mut pages = 0u64;
            loop {
                let MemoryResult::SnapshotPage {
                    transfer_id: page_id,
                    offset: page_offset,
                    next_offset,
                    complete,
                    chunk,
                } = tokio::select! (
                    () = memory_cancelled(cancel) => bail!("project-memory synchronization cancelled"),
                    result = transfer.call(MemoryAction::ReadSnapshotPage {
                        transfer_id,
                        offset,
                    }) => result?,
                )
                else {
                    bail!("relay returned an unexpected project-memory page response");
                };
                let chunk = mj_core::runtime::spawn_blocking(move || BASE64.decode(chunk))
                    .await
                    .context("decode project-memory page task failed")??;
                anyhow::ensure!(
                    page_id == transfer_id && page_offset == offset,
                    "project-memory page identity or offset changed"
                );
                anyhow::ensure!(
                    offset.checked_add(chunk.len() as u64) == Some(next_offset)
                        && next_offset <= total_bytes,
                    "project-memory page length is inconsistent"
                );
                anyhow::ensure!(
                    complete == (next_offset == total_bytes) && (complete || next_offset > offset),
                    "project-memory page did not advance to its declared end"
                );
                bytes.extend_from_slice(&chunk);
                offset = next_offset;
                pages += 1;
                if complete {
                    return Ok((bytes, pages));
                }
            }
        }
        .await;
        let bytes = match result {
            Ok((bytes, pages)) => {
                transfer.complete();
                tracing::info!(
                    %session_id,
                    direction = "download",
                    bytes = bytes.len(),
                    pages,
                    elapsed_ms = started.elapsed().as_millis(),
                    "project-memory transfer finished"
                );
                bytes
            }
            Err(error) if cancel.is_some_and(|cancel| cancel.is_cancelled()) => return Err(error),
            Err(error) => return Err(transfer.abort(error).await),
        };
        Ok(bytes)
    }

    async fn upload_project_memory(
        &mut self,
        kind: mj_core::relay::ProjectMemoryUploadKind,
        tree: mj_core::project_memory::ProjectMemorySnapshot,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<MemoryResult> {
        let protocol = self.protocol_version;
        let bytes = tokio::select! {
            () = memory_cancelled(cancel) => bail!("project-memory synchronization cancelled"),
            result = mj_core::runtime::spawn_blocking(move || -> Result<Vec<u8>> {
            Ok(serde_json::to_vec(
                &mj_core::relay::ProjectMemoryWireTree::encode(tree, protocol)?,
            )?)
            }) => result.context("encode project-memory upload task failed")??,
        };
        // The counter's type bounds the actual ID width for every page.
        let chunk_bytes = mj_core::relay::project_memory_transfer_request_chunk_bytes(
            protocol,
            &self.request_id_for(u64::MAX),
        )?;
        let MemoryResult::UploadStarted { transfer_id } = tokio::select! (
            () = memory_cancelled(cancel) => bail!("project-memory synchronization cancelled"),
            result = self.begin_memory_transfer(MemoryAction::BeginUpload {
                kind,
                total_bytes: bytes.len() as u64,
            }) => result?,
        ) else {
            self.abandoned =
                Some("invalid project-memory upload start; reconnect the relay".into());
            self.input.take();
            bail!("relay returned an unexpected project-memory upload start response");
        };
        let started = Instant::now();
        let session_id = self.session_id.clone();
        let total_bytes = bytes.len();
        let mut pages = 0u64;
        let mut transfer = ProjectMemoryTransfer {
            client: Some(self),
            id: transfer_id,
        };
        let result = async {
            let mut offset = 0u64;
            for chunk in bytes.chunks(chunk_bytes) {
                let next = offset + chunk.len() as u64;
                let chunk = BASE64.encode(chunk);
                let MemoryResult::UploadProgress {
                    transfer_id: upload_id,
                    next_offset,
                } = tokio::select! (
                    () = memory_cancelled(cancel) => bail!("project-memory synchronization cancelled"),
                    result = transfer.call(MemoryAction::AppendUpload {
                        transfer_id,
                        offset,
                        chunk,
                    }) => result?,
                )
                else {
                    bail!("relay returned an unexpected project-memory upload progress response");
                };
                anyhow::ensure!(
                    upload_id == transfer_id && next_offset == next,
                    "project-memory upload offset changed"
                );
                offset = next_offset;
                pages += 1;
            }
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                bail!("project-memory synchronization cancelled");
            }
            match transfer
                .call(MemoryAction::CommitUpload { transfer_id })
                .await?
            {
                outcome @ (MemoryResult::Installed
                | MemoryResult::Replaced { .. }
                | MemoryResult::Delivered { .. }) => Ok(outcome),
                _ => bail!("relay returned an unexpected project-memory upload commit response"),
            }
        }
        .await;
        match result {
            Ok(outcome) => {
                transfer.complete();
                tracing::info!(
                    %session_id,
                    direction = "upload",
                    bytes = total_bytes,
                    pages,
                    elapsed_ms = started.elapsed().as_millis(),
                    "project-memory transfer finished"
                );
                Ok(outcome)
            }
            Err(error) if cancel.is_some_and(|cancel| cancel.is_cancelled()) => Err(error),
            Err(error) => Err(transfer.abort(error).await),
        }
    }

    pub async fn deliver_project_memory(
        &mut self,
        id: String,
        tree: mj_core::project_memory::ProjectMemorySnapshot,
        version: mj_core::project_memory::TreeVersion,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<mj_core::relay::ProjectMemoryDeliveryOutcome> {
        match self
            .upload_project_memory(
                mj_core::relay::ProjectMemoryUploadKind::Deliver { id, version },
                tree,
                Some(cancel),
            )
            .await?
        {
            MemoryResult::Delivered { outcome } => Ok(outcome),
            _ => bail!("relay returned an unexpected project-memory delivery response"),
        }
    }

    pub async fn install_project_memory_snapshot(
        &mut self,
        snapshot: mj_core::project_memory::ProjectMemorySnapshot,
    ) -> Result<()> {
        if self.protocol_version >= mj_core::relay::RELAY_PROJECT_MEMORY_TRANSFER_PROTOCOL {
            return match self
                .upload_project_memory(
                    mj_core::relay::ProjectMemoryUploadKind::InstallSnapshot,
                    snapshot,
                    None,
                )
                .await?
            {
                MemoryResult::Installed => Ok(()),
                _ => bail!("relay returned an unexpected project-memory install terminal response"),
            };
        }
        let protocol = self.protocol_version;
        let snapshot = mj_core::runtime::spawn_blocking(move || {
            mj_core::relay::ProjectMemoryWireTree::encode(snapshot, protocol)
        })
        .await
        .context("encode project memory task failed")??;
        let request = RelayRequest::InstallProjectMemorySnapshot { snapshot };
        match self.call_legacy_project_memory(request).await? {
            RelayResponsePayload::ProjectMemorySnapshotInstalled => Ok(()),
            _ => bail!("relay returned an unexpected project-memory install response"),
        }
    }

    /// Whether the worker can replace its whole memory tree, deletions included.
    pub fn supports_project_memory_replace(&self) -> bool {
        mj_core::relay::RelayVersionRange::CURRENT.contains(self.protocol_version)
            && self.protocol_version >= mj_core::relay::RELAY_PROJECT_MEMORY_REPLACE_PROTOCOL
    }

    /// Replace the worker's replica and baseline with `tree`, only if the
    /// replica still has `expected_replica`.
    pub async fn replace_project_memory_tree(
        &mut self,
        expected_replica: mj_core::project_memory::TreeVersion,
        tree: mj_core::project_memory::ProjectMemorySnapshot,
    ) -> Result<mj_core::project_memory::ReplicaReplaceOutcome> {
        if self.protocol_version >= mj_core::relay::RELAY_PROJECT_MEMORY_TRANSFER_PROTOCOL {
            return match self
                .upload_project_memory(
                    mj_core::relay::ProjectMemoryUploadKind::ReplaceTree { expected_replica },
                    tree,
                    None,
                )
                .await?
            {
                MemoryResult::Replaced { outcome } => Ok(outcome),
                _ => bail!("relay returned an unexpected project-memory replace terminal response"),
            };
        }
        let protocol = self.protocol_version;
        let tree = mj_core::runtime::spawn_blocking(move || {
            mj_core::relay::ProjectMemoryWireTree::encode(tree, protocol)
        })
        .await
        .context("encode project memory task failed")??;
        let request = RelayRequest::ReplaceProjectMemoryTree {
            expected_replica,
            tree,
        };
        match self.call_legacy_project_memory(request).await? {
            RelayResponsePayload::ProjectMemoryTreeReplaced { outcome } => Ok(outcome),
            _ => bail!("relay returned an unexpected project-memory replace response"),
        }
    }

    /// Replace this session's synced skills trees with an encoded
    /// `skills::SkillsArchive`. The destination directories are fixed by
    /// the session's launch config and the harness skills whitelist.
    pub async fn install_skills(
        &mut self,
        archive_bytes: &[u8],
    ) -> Result<mj_core::skills::SkillsSyncState> {
        skills_sync_state(
            self.call_bulk(RelayRequest::InstallSkills {
                data: BASE64.encode(archive_bytes),
            })
            .await?,
        )
    }

    pub async fn attachment_present(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<bool> {
        match self
            .call(RelayRequest::AttachmentPresent {
                reference: reference.clone(),
            })
            .await?
        {
            RelayResponsePayload::AttachmentPresent { present } => Ok(present),
            _ => bail!("unexpected image presence response"),
        }
    }

    /// Copy a verified controller blob to this session before admitting its reference.
    pub async fn ensure_attachment(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<()> {
        if self.attachment_present(reference).await? {
            return Ok(());
        }
        let reference = reference.clone();
        self.with_bulk(move |bulk| {
            Box::pin(async move { bulk.upload_attachment(&reference).await })
        })
        .await
    }

    async fn upload_attachment(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<()> {
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let reference_copy = reference.clone();
        let bytes = mj_core::runtime::spawn_blocking(move || store.read(&reference_copy))
            .await
            .context("image loading task failed")??;
        match self
            .call(RelayRequest::InstallAttachment {
                reference: reference.clone(),
                data: BASE64.encode(bytes),
            })
            .await?
        {
            RelayResponsePayload::AttachmentInstalled => Ok(()),
            _ => bail!("unexpected image upload response"),
        }
    }

    /// Recover the local copy needed for queue editing and resubmission.
    pub async fn cache_attachment(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<()> {
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let local = store.clone();
        let reference_copy = reference.clone();
        if mj_core::runtime::spawn_blocking(move || local.contains(&reference_copy))
            .await
            .context("image lookup task failed")??
        {
            return Ok(());
        }
        let reference = reference.clone();
        self.with_bulk(move |bulk| {
            Box::pin(async move { bulk.download_attachment(&reference).await })
        })
        .await
    }

    async fn download_attachment(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<()> {
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let RelayResponsePayload::AttachmentData { data } = self
            .call(RelayRequest::ReadAttachment {
                reference: reference.clone(),
            })
            .await?
        else {
            bail!("unexpected image download response")
        };
        let reference = reference.clone();
        mj_core::runtime::spawn_blocking(move || {
            anyhow::ensure!(
                data.len() <= mj_core::attachment::MAX_IMAGE_BYTES.div_ceil(3) * 4,
                "image download is too large"
            );
            store.install(&reference, &BASE64.decode(data)?)
        })
        .await
        .context("image caching task failed")?
    }

    pub async fn file_attachment_present(
        &mut self,
        reference: &mj_core::attachment::FileAttachmentRef,
    ) -> Result<bool> {
        self.require_file_attachment_protocol()?;
        match self
            .call(RelayRequest::FileAttachmentPresent {
                reference: reference.clone(),
            })
            .await?
        {
            RelayResponsePayload::AttachmentPresent { present } => Ok(present),
            _ => bail!("unexpected file presence response"),
        }
    }

    /// Copy a verified controller blob to this session before admitting its reference.
    pub async fn ensure_file_attachment(
        &mut self,
        reference: &mj_core::attachment::FileAttachmentRef,
    ) -> Result<()> {
        if self.file_attachment_present(reference).await? {
            return Ok(());
        }
        let reference = reference.clone();
        self.with_bulk(move |bulk| {
            Box::pin(async move { bulk.upload_file_attachment(&reference).await })
        })
        .await
    }

    async fn upload_file_attachment(
        &mut self,
        reference: &mj_core::attachment::FileAttachmentRef,
    ) -> Result<()> {
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let reference_copy = reference.clone();
        let bytes = mj_core::runtime::spawn_blocking(move || store.read_file(&reference_copy))
            .await
            .context("file loading task failed")??;
        match self
            .call(RelayRequest::InstallFileAttachment {
                reference: reference.clone(),
                data: BASE64.encode(bytes),
            })
            .await?
        {
            RelayResponsePayload::AttachmentInstalled => Ok(()),
            _ => bail!("unexpected file upload response"),
        }
    }

    /// Recover the local copy needed for queue editing and resubmission.
    pub async fn cache_file_attachment(
        &mut self,
        reference: &mj_core::attachment::FileAttachmentRef,
    ) -> Result<()> {
        self.require_file_attachment_protocol()?;
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let local = store.clone();
        let reference_copy = reference.clone();
        if mj_core::runtime::spawn_blocking(move || local.contains_file(&reference_copy))
            .await
            .context("file lookup task failed")??
        {
            return Ok(());
        }
        let reference = reference.clone();
        self.with_bulk(move |bulk| {
            Box::pin(async move { bulk.download_file_attachment(&reference).await })
        })
        .await
    }

    async fn download_file_attachment(
        &mut self,
        reference: &mj_core::attachment::FileAttachmentRef,
    ) -> Result<()> {
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let RelayResponsePayload::AttachmentData { data } = self
            .call(RelayRequest::ReadFileAttachment {
                reference: reference.clone(),
            })
            .await?
        else {
            bail!("unexpected file download response")
        };
        let reference = reference.clone();
        mj_core::runtime::spawn_blocking(move || {
            anyhow::ensure!(
                data.len() <= mj_core::attachment::MAX_FILE_BYTES.div_ceil(3) * 4,
                "file download is too large"
            );
            store.install_file(&reference, &BASE64.decode(data)?)
        })
        .await
        .context("file caching task failed")?
    }

    /// Best-effort recovery caching for editing and resubmitting externally
    /// queued prompts. A Bulk failure must never block session adoption or event
    /// projection while Control is healthy. Stop the batch on a dead proxy;
    /// report permanent per-blob failures and continue with the remaining blobs.
    pub async fn cache_attachments(
        &mut self,
        images: &[mj_core::attachment::AttachmentRef],
        files: &[mj_core::attachment::FileAttachmentRef],
    ) -> Result<()> {
        if images.is_empty() && files.is_empty() {
            return Ok(());
        }
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let images = images.to_vec();
        let files = files.to_vec();
        let session_id = self.session_id.clone();
        let (images, mut files) = mj_core::runtime::spawn_blocking(move || {
            let mut seen = BTreeSet::new();
            let images = images.into_iter().filter(|reference| {
                if !seen.insert(reference.sha256.clone()) { return false; }
                match store.contains(reference) {
                    Ok(present) => !present,
                    Err(error) => {
                        tracing::warn!(%session_id, attachment = %reference.sha256, %error, "could not cache image attachment during recovery");
                        false
                    }
                }
            }).collect::<Vec<_>>();
            seen.clear();
            let files = files.into_iter().filter(|reference| {
                if !seen.insert(reference.sha256.clone()) { return false; }
                match store.contains_file(reference) {
                    Ok(present) => !present,
                    Err(error) => {
                        tracing::warn!(%session_id, attachment = %reference.sha256, %error, "could not cache file attachment during recovery");
                        false
                    }
                }
            }).collect::<Vec<_>>();
            (images, files)
        }).await.context("attachment lookup task failed")?;
        if !files.is_empty()
            && let Err(error) = self.require_file_attachment_protocol()
        {
            for reference in &files {
                tracing::warn!(session_id = %self.session_id, attachment = %reference.sha256, %error, "could not cache file attachment during recovery");
            }
            files.clear();
        }
        if images.is_empty() && files.is_empty() {
            return Ok(());
        }
        let result = self.with_bulk(move |bulk| Box::pin(async move {
            for reference in images {
                if let Err(error) = bulk.download_attachment(&reference).await {
                    if bulk.abandoned.is_some() || RelayTransportDead::marks(&error) {
                        return Err(error);
                    }
                    tracing::warn!(session_id = %bulk.session_id, attachment = %reference.sha256, %error, "could not cache image attachment during recovery");
                }
            }
            for reference in files {
                if let Err(error) = bulk.download_file_attachment(&reference).await {
                    if bulk.abandoned.is_some() || RelayTransportDead::marks(&error) {
                        return Err(error);
                    }
                    tracing::warn!(session_id = %bulk.session_id, attachment = %reference.sha256, %error, "could not cache file attachment during recovery");
                }
            }
            Ok(())
        })).await;
        if let Err(error) = result {
            tracing::warn!(session_id = %self.session_id, %error, "could not cache attachments during recovery");
        }
        Ok(())
    }

    fn require_file_attachment_protocol(&self) -> Result<()> {
        if self.protocol_version < mj_core::relay::RELAY_FILE_ATTACHMENT_PROTOCOL {
            return Err(mj_core::refusal::Refusal::precondition(
                "this session's worker predates file attachments; restart the session to attach files",
            )
            .into());
        }
        Ok(())
    }

    /// Query presence in reference order before any upload mutates the worker.
    pub async fn attachment_presence(
        &mut self,
        images: &[mj_core::attachment::AttachmentRef],
        files: &[mj_core::attachment::FileAttachmentRef],
    ) -> Result<(Vec<bool>, Vec<bool>)> {
        if !files.is_empty() {
            self.require_file_attachment_protocol()?;
        }
        let requests = images
            .iter()
            .map(|reference| RelayRequest::AttachmentPresent {
                reference: reference.clone(),
            })
            .chain(
                files
                    .iter()
                    .map(|reference| RelayRequest::FileAttachmentPresent {
                        reference: reference.clone(),
                    }),
            )
            .collect();
        let mut present = self
            .call_many(requests)
            .await?
            .into_iter()
            .map(|reply| match reply? {
                RelayResponsePayload::AttachmentPresent { present } => Ok(present),
                _ => bail!("unexpected attachment presence response"),
            })
            .collect::<Result<Vec<_>>>()?;
        let files = present.split_off(images.len());
        Ok((present, files))
    }

    pub async fn submit(
        &mut self,
        command_id: impl Into<String>,
        command: RelayCommand,
    ) -> Result<u64> {
        let command_id = command_id.into();
        if let RelayCommand::Prompt { prompt } = &command {
            let files = mj_core::attachment::file_references(prompt)?;
            if !files.is_empty() {
                self.require_file_attachment_protocol()?;
            }
            let mut seen_images = BTreeSet::new();
            let mut seen_files = BTreeSet::new();
            let mut missing_images = Vec::new();
            let mut missing_files = Vec::new();
            let images: Vec<_> = mj_core::attachment::references(prompt)?
                .into_iter()
                .filter(|reference| seen_images.insert(reference.sha256.clone()))
                .collect();
            let files: Vec<_> = files
                .into_iter()
                .filter(|reference| seen_files.insert(reference.sha256.clone()))
                .collect();
            let (image_presence, file_presence) = self.attachment_presence(&images, &files).await?;
            for (reference, present) in images.into_iter().zip(image_presence) {
                if !present {
                    missing_images.push(reference);
                }
            }
            for (reference, present) in files.into_iter().zip(file_presence) {
                if !present {
                    missing_files.push(reference);
                }
            }
            if !missing_images.is_empty() || !missing_files.is_empty() {
                self.with_bulk(move |bulk| {
                    Box::pin(async move {
                        for reference in missing_images {
                            bulk.upload_attachment(&reference).await?;
                        }
                        for reference in missing_files {
                            bulk.upload_file_attachment(&reference).await?;
                        }
                        Ok(())
                    })
                })
                .await?;
            }
        }
        match self
            .call(RelayRequest::Submit {
                command_id: command_id.clone(),
                command,
            })
            .await?
        {
            RelayResponsePayload::Accepted {
                command_id: accepted_id,
                ordinal,
            } if accepted_id == command_id => Ok(ordinal),
            RelayResponsePayload::Accepted {
                command_id: accepted_id,
                ..
            } => bail!("relay accepted command under ID {accepted_id}, expected {command_id}"),
            _ => bail!("relay returned an unexpected command response"),
        }
    }

    pub async fn read_queued_command(
        &mut self,
        command_id: String,
    ) -> Result<(Option<RelayCommand>, Option<u64>)> {
        match self
            .call(RelayRequest::ReadQueuedCommand { command_id })
            .await?
        {
            RelayResponsePayload::QueuedCommand {
                command,
                accepted_ordinal,
            } => Ok((command, accepted_ordinal)),
            _ => bail!("relay returned an unexpected queued command response"),
        }
    }

    pub async fn read_checkpoint_mailbox(
        &mut self,
        barrier_command_id: String,
        cut: mj_core::relay::RelayCursor,
    ) -> Result<mj_core::archive::CanonicalMailbox> {
        match self
            .call(RelayRequest::ReadCheckpointMailbox {
                barrier_command_id,
                cut,
            })
            .await?
        {
            RelayResponsePayload::CheckpointMailbox { mailbox } => Ok(mailbox),
            _ => bail!("relay returned an unexpected checkpoint mailbox response"),
        }
    }

    /// The whole mailbox at the cut and the prompts of its queued mailbox
    /// wakes, in one cut-bound read. The worker serializes the cut once and
    /// serves its pages; the read fails if the relay moves past the cut.
    pub async fn read_checkpoint_mailbox_pages(
        &mut self,
        barrier_command_id: String,
        cut: mj_core::relay::RelayCursor,
    ) -> Result<mj_core::relay::CheckpointMailboxRead> {
        let mut offset = 0u64;
        let mut json = String::new();
        loop {
            let RelayResponsePayload::CheckpointMailboxPage { page, next_offset } = self
                .call(RelayRequest::ReadCheckpointMailboxPage {
                    barrier_command_id: barrier_command_id.clone(),
                    cut: cut.clone(),
                    offset,
                })
                .await?
            else {
                bail!("relay returned an unexpected checkpoint mailbox page response")
            };
            json.push_str(&page);
            match next_offset {
                None => return Ok(serde_json::from_str(&json)?),
                Some(next) => {
                    anyhow::ensure!(
                        next > offset && usize::try_from(next).ok() == Some(json.len()),
                        "checkpoint mailbox page returned an invalid next offset"
                    );
                    offset = next;
                }
            }
        }
    }

    pub async fn read_checkpoint_quota_input(
        &mut self,
        barrier_command_id: String,
        cut: mj_core::relay::RelayCursor,
    ) -> Result<mj_core::archive::CanonicalQuotaInput> {
        let mut offset = 0u64;
        let mut json = String::new();
        loop {
            let RelayResponsePayload::CheckpointQuotaInput { page, next_offset } = self
                .call(RelayRequest::ReadCheckpointQuotaInput {
                    barrier_command_id: barrier_command_id.clone(),
                    cut: cut.clone(),
                    offset,
                })
                .await?
            else {
                bail!("relay returned an unexpected checkpoint quota input response")
            };
            json.push_str(&page);
            match next_offset {
                None => return Ok(serde_json::from_str(&json)?),
                Some(next) => {
                    anyhow::ensure!(
                        next > offset && usize::try_from(next).ok() == Some(json.len()),
                        "checkpoint quota input page returned an invalid next offset"
                    );
                    offset = next;
                }
            }
        }
    }

    pub async fn read_checkpoint_native_use(
        &mut self,
        barrier_command_id: String,
        cut: mj_core::relay::RelayCursor,
    ) -> Result<(
        Option<mj_core::relay::NativeSessionUseEvidence>,
        Option<String>,
    )> {
        anyhow::ensure!(
            self.protocol_version >= mj_core::relay::RELAY_CHECKPOINT_PROMPT_CONTEXT_PROTOCOL,
            "worker cannot report pending prompt context at a checkpoint cut"
        );
        let mut offset = None;
        let mut evidence = None;
        let mut context: Option<String> = None;
        loop {
            let RelayResponsePayload::CheckpointNativeUse {
                native_use_evidence,
                pending_prompt_context,
                next_context_offset,
            } = self
                .call(RelayRequest::ReadCheckpointNativeUse {
                    barrier_command_id: barrier_command_id.clone(),
                    cut: cut.clone(),
                    pending_context_offset: offset,
                })
                .await?
            else {
                bail!("relay returned an unexpected checkpoint native-use response");
            };
            if let Some(first) = &evidence {
                anyhow::ensure!(
                    *first == native_use_evidence,
                    "checkpoint native-use evidence changed between context pages"
                );
            } else {
                evidence = Some(native_use_evidence);
            }
            match pending_prompt_context {
                Some(page) => context.get_or_insert_with(String::new).push_str(&page),
                None => anyhow::ensure!(
                    offset.is_none() && next_context_offset.is_none(),
                    "checkpoint context disappeared between pages"
                ),
            }
            let Some(next) = next_context_offset else {
                return Ok((evidence.flatten(), context));
            };
            let length = context.as_ref().map_or(0, String::len);
            anyhow::ensure!(
                next > offset.unwrap_or(0) && usize::try_from(next).ok() == Some(length),
                "checkpoint context page returned an invalid next offset"
            );
            offset = Some(next);
        }
    }

    pub async fn read_pending_elicitation(
        &mut self,
        elicitation_id: String,
    ) -> Result<Option<mj_core::elicitation::ElicitationRequest>> {
        match self
            .call(RelayRequest::ReadPendingElicitation { elicitation_id })
            .await?
        {
            RelayResponsePayload::PendingElicitation { request } => Ok(request),
            _ => bail!("relay returned an unexpected pending question response"),
        }
    }

    pub fn set_adoption_wait_progress(
        &mut self,
        progress: Option<tokio::sync::watch::Sender<Option<Vec<String>>>>,
    ) {
        self.adoption_wait_progress = progress;
    }

    pub async fn live_session_baseline(&mut self) -> Result<mj_core::relay::LiveSessionBaseline> {
        if self.protocol_version() < mj_core::relay::RELAY_LIVE_BASELINE_PROTOCOL {
            return Err(LiveBaselineUnsupported.into());
        }
        self.with_bulk(|bulk| Box::pin(bulk.read_live_session_baseline()))
            .await
    }

    async fn read_live_session_baseline(&mut self) -> Result<mj_core::relay::LiveSessionBaseline> {
        let RelayResponsePayload::LiveBaselineStarted { total_bytes } =
            self.call(RelayRequest::BeginLiveBaseline).await?
        else {
            bail!("relay returned an unexpected live baseline response")
        };
        let mut bytes = Vec::new();
        let mut offset = 0;
        loop {
            let RelayResponsePayload::LiveBaselinePage { page } = self
                .call(RelayRequest::ReadLiveBaselinePage { offset })
                .await?
            else {
                bail!("relay returned an unexpected live baseline page")
            };
            anyhow::ensure!(
                page.offset == offset && page.next_offset <= total_bytes,
                "live baseline page cursor mismatch"
            );
            use base64::Engine as _;
            let chunk = mj_core::runtime::spawn_blocking(move || {
                base64::engine::general_purpose::STANDARD.decode(page.chunk)
            })
            .await
            .context("decode live baseline page task failed")??;
            anyhow::ensure!(
                page.next_offset == offset + u64::try_from(chunk.len())?,
                "live baseline page length mismatch"
            );
            anyhow::ensure!(
                page.complete || !chunk.is_empty(),
                "live baseline download made no progress"
            );
            bytes.extend(chunk);
            offset = page.next_offset;
            if page.complete {
                anyhow::ensure!(offset == total_bytes, "live baseline download ended early");
                break;
            }
            anyhow::ensure!(
                offset < total_bytes,
                "live baseline download made no progress"
            );
        }
        mj_core::runtime::spawn_blocking(move || {
            serde_json::from_slice(&bytes).context("decode live recovery baseline")
        })
        .await
        .context("live recovery baseline decode task failed")?
    }

    pub async fn command_retention_status(&mut self) -> Result<Option<(bool, BTreeSet<String>)>> {
        if self.protocol_version() < mj_core::relay::RELAY_COMMAND_RETENTION_PROTOCOL {
            return Ok(None);
        }
        match self.call(RelayRequest::CommandRetentionStatus).await? {
            RelayResponsePayload::CommandRetentionState {
                legacy_pending,
                operation_ids,
            } => Ok(Some((legacy_pending, operation_ids))),
            _ => bail!("relay returned an unexpected command-retention status"),
        }
    }

    pub async fn update_command_retention(
        &mut self,
        operation_id: Option<String>,
        command_ids: BTreeSet<String>,
        reconcile_legacy: bool,
    ) -> Result<()> {
        if self.protocol_version() < mj_core::relay::RELAY_COMMAND_RETENTION_PROTOCOL {
            // Older workers retain receipts behind their recovery floor.
            return Ok(());
        }
        match self
            .call(RelayRequest::UpdateCommandRetention {
                operation_id,
                command_ids,
                reconcile_legacy,
            })
            .await?
        {
            RelayResponsePayload::CommandRetentionUpdated => Ok(()),
            _ => bail!("relay returned an unexpected command-retention response"),
        }
    }

    pub async fn reserve_idle(&mut self, command_id: String) -> Result<bool> {
        match self.call(RelayRequest::ReserveIdle { command_id }).await? {
            RelayResponsePayload::IdleReservation { ordinal } => Ok(ordinal.is_some()),
            _ => bail!("relay returned an unexpected idle reservation response"),
        }
    }
}
