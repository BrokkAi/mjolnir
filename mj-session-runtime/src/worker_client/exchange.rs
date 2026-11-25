use super::*;
use anyhow::ensure;

/// How long a request may wait before it counts as slow. A change wait is a
/// long poll: the worker holds it for up to `RELAY_CHANGE_WAIT_BOUND` (the
/// same constant the worker uses) before answering, so that hold is expected.
fn slow_operation_warning_after(request: &RelayRequest) -> Duration {
    RELAY_SLOW_OPERATION_WARNING
        + match request {
            RelayRequest::WaitForChange { .. } => mj_core::relay::RELAY_CHANGE_WAIT_BOUND,
            _ => Duration::ZERO,
        }
}

impl RelayClient {
    /// Pipeline independent observations over the existing ordered wire format.
    /// A framed rejection belongs to its slot; transport or framing failure
    /// invalidates the entire connection, including all outstanding replies.
    pub async fn call_many(
        &mut self,
        requests: Vec<RelayRequest>,
    ) -> Result<Vec<Result<RelayResponsePayload>>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(reason) = &self.abandoned {
            bail!("{reason}");
        }
        for request in &requests {
            ensure!(
                is_pipelineable_read(request),
                "relay {} is not a pipelineable read",
                request.method_name()
            );
            ensure!(
                request.supported_at(self.protocol_version),
                "relay {} requires protocol {} (worker speaks {})",
                request.method_name(),
                request.minimum_protocol(),
                self.protocol_version
            );
        }
        let mut frames = Vec::new();
        let mut ids = Vec::new();
        for request in requests {
            let timeout = match &request {
                RelayRequest::Attach { .. } => RELAY_HISTORY_TIMEOUT,
                RelayRequest::Status | RelayRequest::HistoryRequests => RELAY_OBSERVATION_TIMEOUT,
                _ => self.request_timeout,
            };
            let operation = request.method_name();
            let request_id = self.request_id();
            let envelope = RelayRequestEnvelope {
                request_id: request_id.clone(),
                protocol_version: self.protocol_version,
                request,
            };
            let frame = serde_json::to_vec(&envelope)?;
            ensure!(
                frame.len() <= MAX_FRAME_BYTES,
                "relay pipeline request frame is too large"
            );
            frames.extend(frame);
            frames.push(b'\n');
            ids.push((request_id, timeout, operation));
        }
        let session_id = self.session_id.clone();
        let protocol_version = self.protocol_version;
        let input = self
            .input
            .as_mut()
            .expect("connected relay owns proxy stdin");
        let output = &mut self.output;
        // The sequential worker writes each reply before reading the next
        // request. Drain concurrently so bounded pipes cannot block both ends.
        let write_all = async {
            input
                .write_all(&frames)
                .await
                .map_err(|error| RelayTransportDead::from_io(error, ExchangeKind::Call))?;
            input
                .flush()
                .await
                .map_err(|error| RelayTransportDead::from_io(error, ExchangeKind::Call))?;
            Ok::<_, anyhow::Error>(())
        };
        let read_all = async {
            let mut replies = Vec::new();
            for (id, timeout, operation) in ids {
                // Only this slot's wait counts against its budget. An earlier
                // cold Attach must not consume the next observation's deadline.
                let started = Instant::now();
                let response = read_bounded_frame(output, ExchangeKind::Call);
                tokio::pin!(response);
                let line = tokio::time::timeout(timeout, async {
                    tokio::select! {
                        result = &mut response => result,
                        () = tokio::time::sleep(RELAY_SLOW_OPERATION_WARNING) => {
                            tracing::warn!(%session_id, %operation, request_id = %id, "relay pipeline slot is still waiting for its response");
                            let result = response.await;
                            tracing::warn!(%session_id, %operation, request_id = %id, elapsed_seconds = started.elapsed().as_secs_f64(), "slow relay pipeline slot finished");
                            result
                        }
                    }
                }).await.with_context(|| format!("relay pipeline {operation} timed out after {} seconds", timeout.as_secs_f64()))??
                    .ok_or_else(|| anyhow::Error::new(RelayTransportDead::during_exchange(
                        format!("relay proxy disconnected during pipeline {operation}"), ExchangeKind::Call)))?;
                let reply = decode_relay_response(&line, &id, protocol_version);
                if let Err(error) = &reply
                    && error.downcast_ref::<RelayRejected>().is_none()
                {
                    return Err(anyhow!("invalid relay pipeline response: {error:#}"));
                }
                replies.push(reply);
            }
            Ok(replies)
        };
        let result = tokio::try_join!(write_all, read_all).map(|(_, replies)| replies);
        if let Err(error) = &result {
            self.abandoned = Some(format!(
                "relay connection abandoned after pipeline failure: {error:#}"
            ));
        }
        result
    }

    pub async fn call(&mut self, request: RelayRequest) -> Result<RelayResponsePayload> {
        // Memory RPCs include filesystem work and worker-side queueing. Neither
        // has a measured deadline. An in-place reload is answered only after
        // the replacement harness opens its session, which first launches take
        // minutes on a remote target (a 15 s deadline abandoned a reload the
        // worker then committed). The worker owns that expiry
        // (`NATIVE_SESSION_STARTUP_TIMEOUT`) and reports its failure or exits,
        // which ends the wait.
        let timeout = match &request {
            RelayRequest::RestartHarnessInPlace { .. }
            | RelayRequest::ProjectMemoryTransfer { .. }
            | RelayRequest::ProjectMemorySnapshot
            | RelayRequest::InstallProjectMemorySnapshot { .. }
            | RelayRequest::ReplaceProjectMemoryTree { .. }
            | RelayRequest::BeginLiveBaseline => None,
            RelayRequest::Status
            | RelayRequest::HistoryRequests
            | RelayRequest::PollSubagentQueue { .. }
            | RelayRequest::SubagentRequests
            | RelayRequest::CompleteSubagentRequest { .. }
            | RelayRequest::SetSubagentAdmission { .. } => Some(RELAY_OBSERVATION_TIMEOUT),
            _ => Some(self.request_timeout),
        };
        self.call_with_deadline(request, timeout).await
    }

    /// Run `call` unless `interrupted` completes first, for a request the
    /// caller must be able to leave while the worker is still answering it.
    ///
    /// Leaving an exchange abandons the connection, as a timeout does: the
    /// worker may still send the reply, and it would be read as the next
    /// call's response. Later calls on this client fail with that cause, so
    /// the caller reconnects deliberately. The request itself is not
    /// cancelled; whatever the worker already did stays durable on its side.
    /// Returns `None` when interrupted.
    pub async fn call_until<T>(
        &mut self,
        interrupted: impl std::future::Future<Output = ()>,
        call: impl AsyncFnOnce(&mut Self) -> Result<T>,
    ) -> Result<Option<T>> {
        let outcome = {
            let call = std::pin::pin!(call(&mut *self));
            tokio::select! {
                result = call => Some(result),
                () = interrupted => None,
            }
        };
        match outcome {
            Some(result) => result.map(Some),
            None => {
                self.abandoned = Some(
                    "relay connection abandoned after its operation was interrupted while a request was in flight"
                        .into(),
                );
                Ok(None)
            }
        }
    }

    pub async fn call_with_timeout(
        &mut self,
        request: RelayRequest,
        timeout: Duration,
    ) -> Result<RelayResponsePayload> {
        self.call_with_deadline(request, Some(timeout)).await
    }

    async fn call_with_deadline(
        &mut self,
        request: RelayRequest,
        timeout: Option<Duration>,
    ) -> Result<RelayResponsePayload> {
        let operation = request.method_name();
        if !request.supported_at(self.protocol_version) {
            // An older worker cannot decode this request. It is refused here,
            // with the same code the worker would use, and the worker is
            // replaced with the current build once the session is quiet.
            return Err(RelayRejected(mj_core::relay::relay_protocol_error(
                RelayErrorCode::IncompatibleProtocol,
                format!(
                    "{operation} requires relay protocol {}; this session's worker speaks \
                     protocol {} and is replaced with the current worker when the session is idle",
                    request.minimum_protocol(),
                    self.protocol_version
                ),
                false,
                None,
            ))
            .into());
        }
        let request_id = self.request_id();
        let envelope = RelayRequestEnvelope {
            request_id: request_id.clone(),
            protocol_version: self.protocol_version,
            request,
        };
        let line = match self
            .exchange(&envelope, operation, timeout, ExchangeKind::Call)
            .await
        {
            Ok(line) => line,
            Err(error) => {
                log_relay_client_failure(self, operation, &request_id, &error);
                return Err(error);
            }
        };
        let result = decode_relay_response(&line, &request_id, self.protocol_version)
            .with_context(|| format!("relay {} could not perform {operation}", self.relay_version));
        if let Err(error) = &result {
            log_relay_client_failure(self, operation, &request_id, error);
        }
        result
    }

    pub async fn call_hello(
        &mut self,
        request: RelayRequest,
        timeout: Duration,
    ) -> Result<RelayResponsePayload> {
        let operation = request.method_name();
        let request_id = self.request_id();
        let envelope = RelayRequestEnvelope {
            request_id: request_id.clone(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request,
        };
        // Not logged here: a proxy the SSH server turned away also fails
        // hello, and only the connect that classifies it knows which it was.
        let line = self
            .exchange(&envelope, operation, Some(timeout), ExchangeKind::Handshake)
            .await?;
        decode_relay_hello_response(&line, &request_id)
    }

    /// Write one request frame and read the reply that belongs to it.
    ///
    /// The connection is strictly sequential, so giving up on a reply does not
    /// cancel it: the relay may still answer, and that answer would be read as
    /// the *next* call's response. Timeouts therefore abandon the connection
    /// rather than the single call. Every later call fails immediately with the
    /// true cause, so callers reconnect deliberately instead of chasing a
    /// mismatched response ID. This matters most where a short bookkeeping
    /// deadline and a long compaction deadline share one connection.
    pub async fn exchange(
        &mut self,
        envelope: &RelayRequestEnvelope,
        operation: &str,
        timeout: Option<Duration>,
        kind: ExchangeKind,
    ) -> Result<String> {
        if let Some(reason) = &self.abandoned {
            bail!("{reason}");
        }
        let mut frame = serde_json::to_vec(envelope)?;
        if frame.len() > MAX_FRAME_BYTES {
            bail!("relay {operation} request frame is too large");
        }
        frame.push(b'\n');
        let session_id = self.session_id.clone();
        let warning_after = slow_operation_warning_after(&envelope.request);
        let started = Instant::now();
        let progress = self.adoption_wait_progress.clone();
        let exchange = async {
            self.input
                .as_mut()
                .expect("connected relay owns proxy stdin")
                .write_all(&frame)
                .await
                .map_err(|error| RelayTransportDead::from_io(error, kind))
                .with_context(|| format!("write relay {operation} request"))?;
            self.input
                .as_mut()
                .expect("connected relay owns proxy stdin")
                .flush()
                .await
                .map_err(|error| RelayTransportDead::from_io(error, kind))
                .with_context(|| format!("flush relay {operation} request"))?;
            loop {
                let response = read_bounded_frame(&mut self.output, kind);
                tokio::pin!(response);
                let response = tokio::select! {
                    response = &mut response => response,
                    () = tokio::time::sleep(warning_after) => {
                        tracing::warn!(
                            %session_id,
                            %operation,
                            warning_after_seconds = warning_after.as_secs_f64(),
                            timeout_seconds = timeout.map(|duration| duration.as_secs_f64()),
                            "relay operation is still waiting for its response"
                        );
                        let response = response.await;
                        // The warning above says an operation is slow; this says
                        // for how long, so a stall can be sized per target.
                        tracing::warn!(
                            %session_id,
                            %operation,
                            elapsed_seconds = started.elapsed().as_secs_f64(),
                            answered = response.is_ok(),
                            "slow relay operation finished"
                        );
                        response
                    }
                };
                let line = response
                    .with_context(|| format!("read relay {operation} response"))?
                    .ok_or_else(|| {
                        anyhow::Error::new(RelayTransportDead::during_exchange(
                            format!("relay proxy disconnected during {operation}"),
                            kind,
                        ))
                    })?;
                if matches!(envelope.request, RelayRequest::BeginLiveBaseline)
                    && let RelayResponsePayload::LiveBaselineWaiting { blockers } =
                        decode_relay_response(
                            &line,
                            &envelope.request_id,
                            envelope.protocol_version,
                        )?
                {
                    if let Some(progress) = &progress {
                        progress.send_replace(Some(blockers));
                    }
                    continue;
                }
                break Ok(line);
            }
        };
        let exchange = async {
            match timeout {
                Some(timeout) => tokio::time::timeout(timeout, exchange).await,
                None => Ok(exchange.await),
            }
        };
        let exchanged = exchange.await;
        tracing::debug!(target: "mj_controller::latency", %session_id, %operation,
            request_id = %envelope.request_id, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "relay exchange completed");
        match exchanged {
            Ok(line) => line,
            Err(_elapsed) => {
                let seconds = timeout
                    .expect("only a bounded exchange can time out")
                    .as_secs_f64();
                tracing::warn!(
                    %session_id,
                    %operation,
                    timeout_seconds = seconds,
                    "relay operation timed out; abandoning its sequential connection"
                );
                self.abandoned = Some(format!(
                    "relay connection abandoned after {operation} timed out after {seconds} seconds"
                ));
                let timed_out = format!("relay {operation} timed out after {seconds} seconds");
                Err(anyhow!(timed_out))
            }
        }
    }

    pub fn request_id_for(&self, sequence: u64) -> String {
        format!("relay-{:016x}-{sequence}", self.connection_nonce)
    }

    pub fn request_id(&mut self) -> String {
        let id = self.request_id_for(self.next_request);
        self.next_request = self.next_request.wrapping_add(1);
        id
    }
}

fn is_pipelineable_read(request: &RelayRequest) -> bool {
    matches!(
        request,
        RelayRequest::Status
            | RelayRequest::Attach { .. }
            | RelayRequest::HistoryRequests
            | RelayRequest::GuardianRequests
            | RelayRequest::CpuUsage
            | RelayRequest::CredentialState
            | RelayRequest::SkillsState
            | RelayRequest::GithubTokenState
            | RelayRequest::SlopcopLoginState
            | RelayRequest::AwsCredentialsScope
            | RelayRequest::AwsCredentialsState
            | RelayRequest::AttachmentPresent { .. }
            | RelayRequest::FileAttachmentPresent { .. }
    )
}
