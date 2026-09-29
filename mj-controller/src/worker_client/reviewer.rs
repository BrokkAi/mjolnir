use super::*;

impl RelayClient {
    /// Start the second-opinion reviewer beside this session, or report the
    /// running one when it already matches `config`.
    ///
    /// The reviewer's profile must already be staged on the target. Starting
    /// can take as long as opening any harness session, so this uses the
    /// handshake deadline rather than the bookkeeping one.
    pub async fn start_reviewer(
        &mut self,
        role: Option<&str>,
        config: ReviewerLaunchConfig,
    ) -> Result<StartedReviewer> {
        let request = self.reviewer_request(
            role,
            ReviewerRequest::Start {
                config: Box::new(config),
            },
        )?;
        match self
            .call_with_timeout(request, RELAY_HANDSHAKE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewerStarted {
                native_session_id,
                config_options,
                reused,
                state,
            } => Ok(StartedReviewer {
                native_session_id,
                config_options,
                reused,
                state: *state,
            }),
            _ => bail!("relay returned an unexpected reviewer start response"),
        }
    }

    /// Replay the reviewer's journal from a cursor, exactly as [`Self::attach`]
    /// does for the primary.
    pub async fn attach_reviewer(
        &mut self,
        role: Option<&str>,
        after_ordinal: u64,
        after_digest: impl Into<String>,
    ) -> Result<RelayAttachment> {
        let after_digest = after_digest.into();
        let request = self.reviewer_request(
            role,
            ReviewerRequest::Attach {
                after_ordinal,
                after_digest: after_digest.clone(),
            },
        )?;
        let payload = self
            .call_with_timeout(request, RELAY_HISTORY_TIMEOUT)
            .await?;
        let RelayResponsePayload::Attached {
            state,
            events,
            through_ordinal,
            through_digest,
        } = payload
        else {
            bail!("relay returned an unexpected reviewer attach response");
        };
        // The reviewer's journal is verified the same way the primary's is: a
        // sidecar's history is not exempt from the chain check.
        let mut cursor = RelayCursor {
            ordinal: after_ordinal,
            digest: after_digest,
        };
        for event in &events {
            validate_relay_event(cursor.ordinal, &cursor.digest, event)
                .context("verify reviewer attachment event chain")?;
            cursor.ordinal = event.ordinal;
            cursor.digest.clone_from(&event.digest);
        }
        if cursor.ordinal != through_ordinal || cursor.digest != through_digest {
            bail!("reviewer attachment frontier does not match its event chain");
        }
        Ok(RelayAttachment {
            state,
            events,
            through_ordinal,
            through_digest,
        })
    }

    /// Advance the reviewer's acknowledged frontier so its journal can be
    /// pruned once the controller has the events durably.
    pub async fn acknowledge_reviewer(
        &mut self,
        role: Option<&str>,
        through_ordinal: u64,
        through_digest: impl Into<String>,
    ) -> Result<RelayCursor> {
        let request = self.reviewer_request(
            role,
            ReviewerRequest::Acknowledge {
                through_ordinal,
                through_digest: through_digest.into(),
            },
        )?;
        match self
            .call_with_timeout(request, RELAY_ACKNOWLEDGE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::Acknowledged {
                through_ordinal,
                through_digest,
            } => Ok(RelayCursor {
                ordinal: through_ordinal,
                digest: through_digest,
            }),
            _ => bail!("relay returned an unexpected reviewer acknowledgement response"),
        }
    }

    /// Queue one command on the reviewer's own relay.
    pub async fn submit_to_reviewer(
        &mut self,
        role: Option<&str>,
        command_id: impl Into<String>,
        command: RelayCommand,
    ) -> Result<u64> {
        let command_id = command_id.into();
        let request = self.reviewer_request(
            role,
            ReviewerRequest::Submit {
                command_id: command_id.clone(),
                command,
            },
        )?;
        match self.call(request).await? {
            RelayResponsePayload::Accepted {
                command_id: accepted_id,
                ordinal,
            } if accepted_id == command_id => Ok(ordinal),
            RelayResponsePayload::Accepted {
                command_id: accepted_id,
                ..
            } => bail!("reviewer accepted command under ID {accepted_id}, expected {command_id}"),
            _ => bail!("relay returned an unexpected reviewer command response"),
        }
    }

    pub async fn reviewer_status(&mut self, role: Option<&str>) -> Result<RelayOperationalState> {
        let request = self.reviewer_request(role, ReviewerRequest::Status)?;
        match self.call(request).await? {
            RelayResponsePayload::Status(status) => Ok(status),
            _ => bail!("relay returned an unexpected reviewer status response"),
        }
    }

    /// Answer a form the reviewer's harness is waiting on.
    pub async fn respond_to_reviewer(
        &mut self,
        role: Option<&str>,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        let request = self.reviewer_request(
            role,
            ReviewerRequest::RespondElicitation {
                elicitation_id: elicitation_id.clone(),
                response,
            },
        )?;
        match self.call(request).await? {
            RelayResponsePayload::ElicitationResolved {
                elicitation_id: resolved,
            } if resolved == elicitation_id => Ok(()),
            RelayResponsePayload::ElicitationResolved {
                elicitation_id: resolved,
            } => bail!("reviewer resolved elicitation {resolved:?}, expected {elicitation_id:?}"),
            _ => bail!("relay returned an unexpected reviewer elicitation response"),
        }
    }

    /// Cancel any reviewer turn in flight and stop its process group, keeping
    /// its staged profile, native session and journal for the next review.
    pub async fn pause_reviewer(&mut self, role: Option<&str>) -> Result<()> {
        let request = self.reviewer_request(role, ReviewerRequest::Pause)?;
        match self
            .call_with_timeout(request, RELAY_ACKNOWLEDGE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewerPaused => Ok(()),
            _ => bail!("relay returned an unexpected reviewer pause response"),
        }
    }

    pub async fn pause_reviewer_generation(
        &mut self,
        role: Option<&str>,
        generation: u64,
    ) -> Result<()> {
        let request =
            self.reviewer_request(role, ReviewerRequest::PauseGeneration { generation })?;
        match self
            .call_with_timeout(request, RELAY_ACKNOWLEDGE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewerPaused => Ok(()),
            _ => bail!("relay returned an unexpected reviewer pause response"),
        }
    }

    /// Report what every workspace repository changed since the review
    /// baselines the controller holds.
    pub async fn capture_review_delta(
        &mut self,
        role: Option<&str>,
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    ) -> Result<Vec<mj_core::relay::RepoDelta>> {
        let request = self.reviewer_request(role, ReviewerRequest::CaptureDelta { baselines })?;
        match self
            .call_with_timeout(request, REVIEW_CAPTURE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewDelta { repositories } => Ok(repositories),
            _ => bail!("relay returned an unexpected review capture response"),
        }
    }

    /// Record the trees a completed review reviewed through, so the next
    /// review starts from them.
    pub async fn advance_review_baseline(
        &mut self,
        role: Option<&str>,
        trees: std::collections::BTreeMap<std::path::PathBuf, String>,
    ) -> Result<()> {
        let request = self.reviewer_request(role, ReviewerRequest::AdvanceBaseline { trees })?;
        match self
            .call_with_timeout(request, REVIEW_CAPTURE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewBaselineAdvanced => Ok(()),
            _ => bail!("relay returned an unexpected review baseline response"),
        }
    }

    /// Run Bifrost's semantic diff analysis over the captured trees. It can
    /// take minutes on a large changeset, so it carries its own budget.
    pub async fn analyze_review_delta(
        &mut self,
        role: Option<&str>,
        repositories: Vec<mj_core::relay::AnalyzeDeltaRepository>,
    ) -> Result<String> {
        let request =
            self.reviewer_request(role, ReviewerRequest::AnalyzeDelta { repositories })?;
        match self
            .call_with_timeout(request, REVIEW_ANALYSIS_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewChangedFunctions { packet } => Ok(packet),
            _ => bail!("relay returned an unexpected review analysis response"),
        }
    }

    /// Collect the specialist lanes the review supervisor asked for since the
    /// last call.
    pub async fn take_lane_dispatches(
        &mut self,
    ) -> Result<Vec<mj_core::review::lanes::ReviewSubagentRequest>> {
        let request = self.reviewer_request(None, ReviewerRequest::TakeLaneDispatches)?;
        match self.call(request).await? {
            RelayResponsePayload::LaneDispatches { requests } => Ok(requests),
            _ => bail!("relay returned an unexpected lane dispatch response"),
        }
    }

    /// Wraps a reviewer action, refusing it on a worker too old to know what a
    /// reviewer is rather than sending a method it would reject as unknown.
    pub(super) fn reviewer_request(
        &self,
        role: Option<&str>,
        request: ReviewerRequest,
    ) -> Result<RelayRequest> {
        let request = RelayRequest::Reviewer {
            role: role.map(str::to_owned),
            request,
        };
        Ok(request)
    }

    /// Answer an ACP form over the live relay connection. User-entered content
    /// is intentionally excluded from the relay's durable command path.
    pub async fn respond_elicitation(
        &mut self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        let request = RelayRequest::RespondElicitation {
            elicitation_id: elicitation_id.clone(),
            response,
        };
        match self.call(request).await? {
            RelayResponsePayload::ElicitationResolved {
                elicitation_id: resolved,
            } if resolved == elicitation_id => Ok(()),
            RelayResponsePayload::ElicitationResolved {
                elicitation_id: resolved,
            } => bail!("relay resolved elicitation {resolved:?}, expected {elicitation_id:?}"),
            _ => bail!("relay returned an unexpected elicitation response"),
        }
    }

    /// Ask the live worker to stop one process-local background task.
    pub async fn stop_background_task(&mut self, background_task_id: String) -> Result<()> {
        let request = RelayRequest::StopBackgroundTask {
            background_task_id: background_task_id.clone(),
        };
        match self.call(request).await? {
            RelayResponsePayload::BackgroundTaskStopRequested {
                background_task_id: stopped,
            } if stopped == background_task_id => Ok(()),
            RelayResponsePayload::BackgroundTaskStopRequested {
                background_task_id: stopped,
            } => {
                bail!("relay stopped background task {stopped:?}, expected {background_task_id:?}")
            }
            _ => bail!("relay returned an unexpected background task stop response"),
        }
    }

    pub async fn subagent_requests(
        &mut self,
    ) -> Result<(
        Vec<mj_core::subagent::SubagentToolRequest>,
        Vec<mj_core::subagent::SubagentToolResult>,
    )> {
        let request = RelayRequest::SubagentRequests;
        if !request.supported_at(self.protocol_version) {
            return Ok((Vec::new(), Vec::new()));
        }
        match self.call(request).await? {
            RelayResponsePayload::SubagentRequests { requests, results } => Ok((requests, results)),
            _ => bail!("relay returned an unexpected sub-agent request response"),
        }
    }

    pub async fn complete_subagent_request(
        &mut self,
        result: mj_core::subagent::SubagentToolResult,
    ) -> Result<()> {
        let request = RelayRequest::CompleteSubagentRequest { result };
        match self.call(request).await? {
            RelayResponsePayload::SubagentRequestCompleted => Ok(()),
            _ => bail!("relay returned an unexpected sub-agent completion response"),
        }
    }

    pub async fn detach(mut self) -> Result<()> {
        self.input
            .take()
            .expect("connected relay owns proxy stdin")
            .shutdown()
            .await
            .context("close relay proxy stdin")?;
        let mut child = self.child.take().expect("connected relay owns proxy child");
        match tokio::time::timeout(RELAY_PROXY_DETACH_GRACE, child.wait()).await {
            Ok(status) => {
                status.context("wait for relay proxy")?;
            }
            Err(_) => {
                if let Err(error) = child.start_kill().context("stop relay proxy") {
                    tracing::warn!(
                        session_id = %self.session_id,
                        operation = "detach",
                        %error,
                        "could not stop relay proxy after detach timeout"
                    );
                    return Err(error);
                }
                if let Err(error) = child.wait().await {
                    tracing::warn!(
                        session_id = %self.session_id,
                        operation = "detach",
                        %error,
                        "could not reap relay proxy after stopping it"
                    );
                }
            }
        }
        Ok(())
    }
}
