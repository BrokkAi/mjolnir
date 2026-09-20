use super::*;

impl RelayClient {
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

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn supports_project_memory_sync(&self) -> bool {
        RelayRequest::ProjectMemorySnapshot.supported_at(self.protocol_version)
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
        let after_digest = after_digest.into();
        match self
            .call_with_timeout(
                RelayRequest::Attach {
                    after_ordinal,
                    after_digest: after_digest.clone(),
                },
                RELAY_HISTORY_TIMEOUT,
            )
            .await?
        {
            RelayResponsePayload::Attached {
                state,
                events,
                through_ordinal,
                through_digest,
            } => {
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
            _ => bail!("relay returned an unexpected attach response"),
        }
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
        let frontier = RelayCursor {
            ordinal: first.state.latest_ordinal,
            digest: first.state.latest_digest.clone(),
        };
        let previous = RelayCursor {
            ordinal: after_ordinal,
            digest: after_digest,
        };
        let state = first.state.clone();
        let first_page = clip_catch_up_page(first, &previous, &frontier)?;
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
        let attachment = self
            .attach(previous.ordinal, previous.digest.clone())
            .await?;
        clip_catch_up_page(attachment, previous, frontier)
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
            RelayResponsePayload::Status(status) => {
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

    /// Read this session's credential file. Callers must keep these bytes out
    /// of durable relay observations, logs, and archives.
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
        let request = RelayRequest::InstallPromptContext { text };
        match self.call(request).await? {
            RelayResponsePayload::PromptContextInstalled => Ok(()),
            _ => bail!("relay returned an unexpected prompt-context response"),
        }
    }

    pub async fn project_memory_snapshot(
        &mut self,
    ) -> Result<(
        mj_core::project_memory::ProjectMemorySnapshot,
        mj_core::project_memory::ProjectMemorySnapshot,
    )> {
        let request = RelayRequest::ProjectMemorySnapshot;
        match self.call(request).await? {
            RelayResponsePayload::ProjectMemorySnapshot { baseline, replica } => {
                Ok((baseline, replica))
            }
            _ => bail!("relay returned an unexpected project-memory response"),
        }
    }

    pub async fn install_project_memory_snapshot(
        &mut self,
        snapshot: mj_core::project_memory::ProjectMemorySnapshot,
    ) -> Result<()> {
        let request = RelayRequest::InstallProjectMemorySnapshot { snapshot };
        match self.call(request).await? {
            RelayResponsePayload::ProjectMemorySnapshotInstalled => Ok(()),
            _ => bail!("relay returned an unexpected project-memory install response"),
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
            self.call(RelayRequest::InstallSkills {
                data: BASE64.encode(archive_bytes),
            })
            .await?,
        )
    }

    /// Copy a verified controller blob to this session before admitting its reference.
    pub async fn ensure_attachment(
        &mut self,
        reference: &mj_core::attachment::AttachmentRef,
    ) -> Result<()> {
        match self
            .call(RelayRequest::AttachmentPresent {
                reference: reference.clone(),
            })
            .await?
        {
            RelayResponsePayload::AttachmentPresent { present: true } => return Ok(()),
            RelayResponsePayload::AttachmentPresent { present: false } => {}
            _ => bail!("unexpected image presence response"),
        }
        let store = mj_core::attachment::AttachmentStore::controller(&self.session_id)?;
        let reference_copy = reference.clone();
        let bytes = tokio::task::spawn_blocking(move || store.read(&reference_copy))
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
        if tokio::task::spawn_blocking(move || local.contains(&reference_copy))
            .await
            .context("image lookup task failed")??
        {
            return Ok(());
        }
        let RelayResponsePayload::AttachmentData { data } = self
            .call(RelayRequest::ReadAttachment {
                reference: reference.clone(),
            })
            .await?
        else {
            bail!("unexpected image download response")
        };
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || {
            anyhow::ensure!(
                data.len() <= mj_core::attachment::MAX_IMAGE_BYTES.div_ceil(3) * 4,
                "image download is too large"
            );
            store.install(&reference, &BASE64.decode(data)?)
        })
        .await
        .context("image caching task failed")?
    }

    pub async fn submit(
        &mut self,
        command_id: impl Into<String>,
        command: RelayCommand,
    ) -> Result<u64> {
        let command_id = command_id.into();
        if let RelayCommand::Prompt { prompt } = &command {
            for reference in mj_core::attachment::references(prompt)? {
                self.ensure_attachment(&reference).await?;
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
}
