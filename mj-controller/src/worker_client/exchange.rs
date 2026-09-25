use super::*;

impl RelayClient {
    pub(super) async fn call(&mut self, request: RelayRequest) -> Result<RelayResponsePayload> {
        self.call_with_timeout(request, self.request_timeout).await
    }

    pub(super) async fn call_with_timeout(
        &mut self,
        request: RelayRequest,
        timeout: Duration,
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

    pub(super) async fn call_hello(
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
            .exchange(&envelope, operation, timeout, ExchangeKind::Handshake)
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
    pub(super) async fn exchange(
        &mut self,
        envelope: &RelayRequestEnvelope,
        operation: &str,
        timeout: Duration,
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
        let started = Instant::now();
        let exchanged = tokio::time::timeout(timeout, async {
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
            let response = read_bounded_frame(&mut self.output, kind);
            tokio::pin!(response);
            let response = tokio::select! {
                response = &mut response => response,
                () = tokio::time::sleep(RELAY_SLOW_OPERATION_WARNING) => {
                    tracing::warn!(
                        %session_id,
                        %operation,
                        warning_after_seconds = RELAY_SLOW_OPERATION_WARNING.as_secs_f64(),
                        timeout_seconds = timeout.as_secs_f64(),
                        "relay operation is still waiting for its response"
                    );
                    response.await
                }
            };
            response
                .with_context(|| format!("read relay {operation} response"))?
                .ok_or_else(|| {
                    anyhow::Error::new(RelayTransportDead::during_exchange(
                        format!("relay proxy disconnected during {operation}"),
                        kind,
                    ))
                })
        })
        .await;
        tracing::debug!(target: "mj_controller::latency", %session_id, %operation,
            request_id = %envelope.request_id, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "relay exchange completed");
        match exchanged {
            Ok(line) => line,
            Err(_elapsed) => {
                let seconds = timeout.as_secs_f64();
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

    pub(super) fn request_id(&mut self) -> String {
        let id = format!("relay-{:016x}-{}", self.connection_nonce, self.next_request);
        self.next_request = self.next_request.wrapping_add(1);
        id
    }
}
