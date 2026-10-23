use super::*;

impl RelayClient {
    /// Report what every workspace repository changed since the review
    /// baselines the controller holds.
    pub async fn capture_review_delta(
        &mut self,
        role: Option<&str>,
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
        scope: mj_core::relay::CaptureScope,
    ) -> Result<Vec<mj_core::relay::RepoDelta>> {
        if scope == mj_core::relay::CaptureScope::PushableCommits
            && self.protocol_version < mj_core::relay::RELAY_PUSH_REVIEW_PROTOCOL
        {
            anyhow::bail!("the session's worker predates push review; restart the session");
        }
        let request =
            self.reviewer_request(role, ReviewerRequest::CaptureDelta { baselines, scope })?;
        match self
            .call_with_timeout(request, REVIEW_CAPTURE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewDelta { repositories } => Ok(repositories),
            _ => bail!("relay returned an unexpected review capture response"),
        }
    }

    pub async fn record_reviewed(
        &mut self,
        role: Option<&str>,
        trees: std::collections::BTreeMap<std::path::PathBuf, (String, String)>,
    ) -> Result<()> {
        let request = self.reviewer_request(role, ReviewerRequest::RecordReviewed { trees })?;
        match self
            .call_with_timeout(request, REVIEW_CAPTURE_TIMEOUT)
            .await?
        {
            RelayResponsePayload::ReviewBaselineAdvanced => Ok(()),
            _ => bail!("relay returned an unexpected reviewed-tree response"),
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
        Vec<mj_core::subagent::SubagentCompletion>,
    )> {
        if self.protocol_version >= mj_core::relay::RELAY_SUBAGENT_FEED_PROTOCOL {
            use mj_core::subagent::SubagentQueueChange;
            loop {
                let payload = self
                    .call(RelayRequest::PollSubagentQueue {
                        acknowledged: self.subagent_acknowledged,
                    })
                    .await?;
                let RelayResponsePayload::SubagentQueue { page } = payload else {
                    bail!("relay returned an unexpected sub-agent queue page");
                };
                anyhow::ensure!(
                    self.subagent_acknowledged
                        .is_none_or(|previous| page.sequence > previous),
                    "sub-agent queue page did not advance"
                );
                for change in page.changes {
                    let id = change.request_id().to_owned();
                    match change {
                        SubagentQueueChange::Retired { .. } => {
                            self.subagent_queue.remove(&id);
                        }
                        _ => {
                            self.subagent_queue.insert(id, change);
                        }
                    }
                }
                self.subagent_acknowledged = Some(page.sequence);
                if !page.has_more {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let mut requests = Vec::new();
            let mut completions = Vec::new();
            for entry in self.subagent_queue.values() {
                match entry {
                    SubagentQueueChange::Request(request) => requests.push(request.clone()),
                    SubagentQueueChange::Completed(receipt) => completions.push(receipt.clone()),
                    SubagentQueueChange::Retired { .. } => {
                        unreachable!("retirement removes the entry")
                    }
                }
            }
            return Ok((requests, completions));
        }
        let request = RelayRequest::SubagentRequests;
        if !request.supported_at(self.protocol_version) {
            return Ok((Vec::new(), Vec::new()));
        }
        match self.call(request).await {
            Ok(RelayResponsePayload::SubagentRequests { requests, results }) => Ok((
                requests,
                results
                    .iter()
                    .map(mj_core::subagent::SubagentCompletion::from)
                    .collect(),
            )),
            Ok(_) => bail!("relay returned an unexpected sub-agent request response"),
            // The worker refuses this only while its harness preparation is
            // pending or has failed. The harness and its MCP server are not
            // running then, so no request can exist. Failing here would make
            // the session unsyncable, and so impossible to suspend or close;
            // the preparation failure stays visible in the worker's state.
            Err(error)
                if error
                    .downcast_ref::<RelayRejected>()
                    .is_some_and(|rejected| rejected.0.code == RelayErrorCode::InvalidState) =>
            {
                Ok((Vec::new(), Vec::new()))
            }
            Err(error) => Err(error),
        }
    }

    pub async fn set_subagent_admission(&mut self, open: bool) -> Result<()> {
        let request = RelayRequest::SetSubagentAdmission { open };
        if !request.supported_at(self.protocol_version) {
            bail!(
                "source worker does not support safe in-place sub-agent draining; update or restart the session worker, then retry Move"
            );
        }
        match self.call(request).await? {
            RelayResponsePayload::SubagentAdmissionChanged { open: returned }
                if returned == open =>
            {
                Ok(())
            }
            RelayResponsePayload::SubagentAdmissionChanged { open: returned } => {
                bail!("worker set sub-agent admission to {returned}, expected {open}")
            }
            _ => bail!("relay returned an unexpected sub-agent admission response"),
        }
    }

    pub async fn complete_subagent_request(
        &mut self,
        result: mj_core::subagent::SubagentToolResult,
    ) -> Result<bool> {
        let request = RelayRequest::CompleteSubagentRequest { result };
        subagent_completion_delivery(self.call(request).await?)
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

fn subagent_completion_delivery(payload: RelayResponsePayload) -> Result<bool> {
    match payload {
        // Old workers return the original unit response. Treat it as
        // delivered: they cannot tell us that their local wait timed out.
        RelayResponsePayload::SubagentRequestCompleted => Ok(true),
        RelayResponsePayload::SubagentRequestCompletedWithDelivery {
            delivered_to_waiter,
        } => Ok(delivered_to_waiter),
        _ => bail!("relay returned an unexpected sub-agent completion response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_worker_completion_without_delivery_status_counts_as_delivered() {
        assert!(
            subagent_completion_delivery(RelayResponsePayload::SubagentRequestCompleted).unwrap()
        );
        assert!(
            !subagent_completion_delivery(
                RelayResponsePayload::SubagentRequestCompletedWithDelivery {
                    delivered_to_waiter: false,
                }
            )
            .unwrap()
        );
    }
}
