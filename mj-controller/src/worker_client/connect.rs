use super::*;

impl RelayClient {
    pub async fn connect(spec: &CommandSpec, expected_session_id: &str) -> Result<Self> {
        Self::connect_with_timeouts(
            spec,
            expected_session_id,
            RELAY_RPC_TIMEOUT,
            RELAY_HANDSHAKE_TIMEOUT,
        )
        .await
    }

    #[cfg(all(test, unix))]
    pub(super) async fn connect_with_timeout(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
    ) -> Result<Self> {
        Self::connect_with_timeouts(spec, expected_session_id, request_timeout, request_timeout)
            .await
    }

    /// Start a relay proxy and complete its handshake, retrying while the
    /// remote `sshd` is turning fresh connections away before authentication.
    ///
    /// The whole daemon reconnects at once after a restart, which is exactly
    /// when a host at its `MaxStartups` ceiling drops the surplus. Those
    /// rejections say nothing about the worker, so escalating one to worker
    /// recovery would destroy a healthy session.
    pub(super) async fn connect_with_timeouts(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        for attempt in 1..=SSH_RETRY_ATTEMPTS {
            let outcome = Self::connect_attempt(
                spec,
                expected_session_id,
                request_timeout,
                handshake_timeout,
            )
            .await;
            let error = match outcome {
                Ok(client) => return Ok(client),
                Err(ConnectFailure {
                    error,
                    transport_rejected,
                }) => {
                    if attempt == SSH_RETRY_ATTEMPTS || !transport_rejected {
                        return Err(error);
                    }
                    error
                }
            };
            let delay = mj_core::targets::ssh_retry_delay(attempt);
            tracing::warn!(
                session_id = %expected_session_id,
                destination = spec.ssh_destination.as_deref().unwrap_or_default(),
                purpose = %spec.purpose,
                attempt,
                attempts = SSH_RETRY_ATTEMPTS,
                delay_ms = delay.as_millis() as u64,
                error = %error,
                "relay proxy was refused by the SSH server before authentication; retrying"
            );
            tokio::time::sleep(delay).await;
        }
        unreachable!("the final attempt always returns");
    }

    /// One proxy launch and handshake.
    ///
    /// An admission permit is taken before the proxy is spawned and released
    /// once hello completes: `sshd` counts only unauthenticated connections
    /// against `MaxStartups`, so the long-lived relay stops occupying a slot
    /// as soon as it is authenticated and talking.
    pub(super) async fn connect_attempt(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
    ) -> std::result::Result<Self, ConnectFailure> {
        let permit = match spec.ssh_destination.clone() {
            Some(destination) => {
                match tokio::task::spawn_blocking(move || SshAdmission::acquire(&destination)).await
                {
                    Ok(permit) => Some(permit),
                    Err(error) => {
                        return Err(ConnectFailure::plain(anyhow!(
                            "SSH admission for the relay proxy was cancelled: {error}"
                        )));
                    }
                }
            }
            None => None,
        };
        Self::spawn_and_handshake(
            spec,
            expected_session_id,
            request_timeout,
            handshake_timeout,
            permit,
        )
        .await
    }

    pub(super) async fn spawn_and_handshake(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
        permit: Option<SshPermit>,
    ) -> std::result::Result<Self, ConnectFailure> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Never inherit: the controller owns a TUI alternate screen, so a
            // child writing to the shared stderr corrupts the display outside
            // the renderer's buffer. Drain it into the log instead.
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start session relay proxy for {}", spec.purpose))
            .map_err(|error| {
                tracing::warn!(
                    session_id = %expected_session_id,
                    operation = "connect",
                    purpose = %spec.purpose,
                    error = %error,
                    "could not start relay proxy"
                );
                error
            })?;
        let stderr_tail = child.stderr.take().map(|errors| {
            let purpose = spec.purpose.clone();
            let session_id = expected_session_id.to_owned();
            tokio::spawn(drain_proxy_stderr(errors, purpose, session_id))
        });
        let input = child
            .stdin
            .take()
            .context("relay proxy stdin unavailable")
            .map_err(|error| {
                tracing::warn!(
                    session_id = %expected_session_id,
                    operation = "connect",
                    purpose = %spec.purpose,
                    error = %error,
                    "relay proxy did not provide stdin"
                );
                error
            })?;
        let output = child
            .stdout
            .take()
            .context("relay proxy stdout unavailable")
            .map_err(|error| {
                tracing::warn!(
                    session_id = %expected_session_id,
                    operation = "connect",
                    purpose = %spec.purpose,
                    error = %error,
                    "relay proxy did not provide stdout"
                );
                error
            })?;
        let mut nonce_bytes = [0_u8; 8];
        getrandom::fill(&mut nonce_bytes).map_err(|error| {
            let error = anyhow!("generate relay request nonce: {error}");
            tracing::warn!(
                session_id = %expected_session_id,
                operation = "connect",
                error = %error,
                "could not initialize relay request nonce"
            );
            error
        })?;
        let mut client = Self {
            child: Some(child),
            input: Some(input),
            output: BufReader::new(output),
            request_timeout,
            abandoned: None,
            next_request: 1,
            connection_nonce: u64::from_le_bytes(nonce_bytes),
            protocol_version: RELAY_PROTOCOL_VERSION,
            // Keep the expected identity from process creation onward so a
            // handshake failure and the dropped proxy that follows it remain
            // attributable even when Hello never returns a session ID.
            session_id: expected_session_id.to_owned(),
            relay_version: String::new(),
            worker_build: None,
            latest_ordinal: 0,
            latest_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        };
        match client
            .complete_handshake(expected_session_id, handshake_timeout)
            .await
        {
            Ok(()) => {
                // Hello succeeded, so this connection is past authentication
                // and no longer counts against the server's startup budget.
                drop(permit);
                // The drain task keeps logging for the life of the connection.
                Ok(client)
            }
            Err(error) => {
                // Read the proxy's exit status before killing it: a connection
                // the server dropped has already exited 255, and that status
                // is what separates a refused connection from a broken worker.
                let status = match client.child.as_mut() {
                    Some(child) => {
                        match tokio::time::timeout(RELAY_PROXY_DETACH_GRACE, child.wait()).await {
                            Ok(Ok(status)) => status.code(),
                            // Still running, or unwaitable. Stop the proxy so
                            // it closes stderr; otherwise a proxy that is
                            // merely slow would hold the drain task open past
                            // its grace period and the tail would be lost. The
                            // child stays in place so dropping `client` reaps
                            // it as usual.
                            _ => {
                                let _ = child.start_kill();
                                None
                            }
                        }
                    }
                    None => None,
                };
                let tail = Self::proxy_stderr_tail(stderr_tail).await;
                let transport_rejected = permit.is_some()
                    && status
                        .is_some_and(|status| is_transport_rejection(status, &tail.join("\n")));
                drop(permit);
                Err(ConnectFailure {
                    error: Self::attach_proxy_stderr(error, tail),
                    transport_rejected,
                })
            }
        }
    }

    /// Collect the proxy's trailing stderr, or nothing if it is still open
    /// past its detach grace period.
    pub(super) async fn proxy_stderr_tail(
        stderr_tail: Option<tokio::task::JoinHandle<VecDeque<String>>>,
    ) -> Vec<String> {
        let Some(handle) = stderr_tail else {
            return Vec::new();
        };
        match tokio::time::timeout(RELAY_PROXY_DETACH_GRACE, handle).await {
            Ok(Ok(lines)) => lines.into(),
            _ => Vec::new(),
        }
    }

    /// Attach the proxy's own stderr tail to a failed connect. The proxy
    /// explains failures the controller cannot see any other way, such as a
    /// worker socket path longer than `sun_path`.
    pub(super) fn attach_proxy_stderr(error: anyhow::Error, lines: Vec<String>) -> anyhow::Error {
        if lines.is_empty() {
            return error;
        }
        error.context(format!(
            "relay proxy stderr (last {} lines):\n{}",
            lines.len(),
            lines.join("\n")
        ))
    }

    /// Exchange `Hello` and record what the relay negotiated.
    pub(super) async fn complete_handshake(
        &mut self,
        expected_session_id: &str,
        handshake_timeout: Duration,
    ) -> Result<()> {
        let response = self
            .call_hello(
                RelayRequest::Hello {
                    controller_version: env!("CARGO_PKG_VERSION").to_owned(),
                    supported: RelayVersionRange::CURRENT,
                },
                handshake_timeout,
            )
            .await?;
        let RelayResponsePayload::Hello {
            negotiated,
            relay_version,
            session_id,
            worker_build,
        } = response
        else {
            let error = anyhow!("relay returned an unexpected hello response");
            log_relay_client_failure(self, "hello", "relay-hello", &error);
            return Err(error);
        };
        if session_id != expected_session_id {
            let error = anyhow!("relay belongs to session {session_id}, not {expected_session_id}");
            log_relay_client_failure(self, "hello", "relay-hello", &error);
            return Err(error);
        }
        if !RelayVersionRange::CURRENT.contains(negotiated) {
            let error = anyhow!(
                "relay negotiated unsupported protocol {negotiated}; this controller supports {}-{}",
                RELAY_MIN_PROTOCOL_VERSION,
                RELAY_PROTOCOL_VERSION
            );
            log_relay_client_failure(self, "hello", "relay-hello", &error);
            return Err(error);
        }
        self.protocol_version = negotiated;
        self.session_id = session_id;
        self.relay_version = relay_version;
        self.worker_build = worker_build;
        Ok(())
    }
}
