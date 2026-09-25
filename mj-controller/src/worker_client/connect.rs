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
    /// remote `sshd` is turning fresh connections away before authentication,
    /// and while the worker has not bound its control socket yet.
    ///
    /// The whole daemon reconnects at once after a restart, which is exactly
    /// when a host at its `MaxStartups` ceiling drops the surplus. Those
    /// rejections say nothing about the worker, so escalating one to worker
    /// recovery would destroy a healthy session. A worker that was started
    /// moments ago has usually not bound its socket yet; that is routine too,
    /// as long as the socket appears within [`WORKER_SOCKET_RETRY_DELAYS`].
    pub(super) async fn connect_with_timeouts(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        let purpose = format!("{} for session {expected_session_id}", spec.purpose);
        let mut refusals = 0;
        let mut socket_waits = 0;
        loop {
            let outcome = Self::connect_attempt(
                spec,
                expected_session_id,
                request_timeout,
                handshake_timeout,
            )
            .await;
            let (error, retry) = match outcome {
                Ok(client) => return Ok(client),
                Err(ConnectFailure { error, retry: None }) => return Err(error),
                Err(ConnectFailure {
                    error,
                    retry: Some(retry),
                }) => (error, retry),
            };
            let delay = match retry {
                // Logged by the routine every other refused ssh command uses:
                // a MaxSessions refusal is routine while sessions start and
                // goes to debug, one before authentication stays a warning
                // (R7-1).
                ConnectRetry::Refused(refusal, stderr) => {
                    refusals += 1;
                    let destination = spec.ssh_destination.as_deref().unwrap_or_default();
                    if refusals == SSH_RETRY_ATTEMPTS {
                        refusal.log_exhausted(destination, &purpose, &stderr);
                        return Err(error);
                    }
                    let delay = mj_core::targets::ssh_retry_delay(refusals);
                    refusal.log_retry(destination, &purpose, refusals, delay, &stderr);
                    delay
                }
                ConnectRetry::SocketMissing => {
                    let Some(&delay) = WORKER_SOCKET_RETRY_DELAYS.get(socket_waits) else {
                        tracing::warn!(
                            session_id = %expected_session_id,
                            purpose = %purpose,
                            operation = "hello",
                            attempts = socket_waits + 1,
                            error = format!("{error:#}"),
                            "the worker's control socket is still missing; giving up on this connection"
                        );
                        return Err(error);
                    };
                    socket_waits += 1;
                    tracing::debug!(
                        session_id = %expected_session_id,
                        purpose = %purpose,
                        attempt = socket_waits,
                        delay_ms = delay.as_millis() as u64,
                        "the worker has not bound its control socket yet; retrying"
                    );
                    delay
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// One proxy launch and handshake.
    ///
    /// When the proxy runs over a shared SSH connection, a session is leased
    /// first (opening a master if needed) and kept for the life of the
    /// proxy. An admission permit is then taken before the proxy is spawned
    /// and released once hello completes: `sshd` counts only unauthenticated
    /// connections against `MaxStartups`, so the long-lived relay stops
    /// occupying a slot as soon as it is authenticated and talking.
    pub(super) async fn connect_attempt(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
    ) -> std::result::Result<Self, ConnectFailure> {
        let Some(destination) = spec.ssh_destination.clone() else {
            return Self::spawn_and_handshake(
                spec,
                expected_session_id,
                request_timeout,
                handshake_timeout,
                None,
                None,
            )
            .await;
        };
        let requested = spec.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            // Lease before taking the permit: opening a master takes a
            // permit of its own.
            let (spec, lease) = requested
                .open_ssh_session(&BoundedProcessExecutor::new(SSH_MASTER_OPEN_TIMEOUT))?
                .into_parts();
            let permit = SshAdmission::acquire(&destination);
            Ok::<_, anyhow::Error>((spec, lease, permit))
        })
        .await;
        let (spec, lease, permit) = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                return Err(ConnectFailure::plain(
                    error.context("open the SSH session for the relay proxy"),
                ));
            }
            Err(error) => {
                return Err(ConnectFailure::plain(anyhow!(
                    "SSH admission for the relay proxy was cancelled: {error}"
                )));
            }
        };
        Self::spawn_and_handshake(
            &spec,
            expected_session_id,
            request_timeout,
            handshake_timeout,
            Some(permit),
            lease,
        )
        .await
    }

    pub(super) async fn spawn_and_handshake(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
        permit: Option<SshPermit>,
        ssh_session: Option<SshSessionLease>,
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
        let stderr_tail: ProxyStderrTail = Default::default();
        let handshake_done = Arc::new(AtomicBool::new(false));
        let draining = child.stderr.take().map(|errors| {
            let purpose = spec.purpose.clone();
            let session_id = expected_session_id.to_owned();
            let tail = stderr_tail.clone();
            let handshake_done = handshake_done.clone();
            tokio::spawn(drain_proxy_stderr(
                errors,
                purpose,
                session_id,
                tail,
                handshake_done,
            ))
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
            ssh_session,
        };
        match client
            .complete_handshake(expected_session_id, handshake_timeout)
            .await
        {
            Ok(()) => {
                // Hello succeeded, so this connection is past authentication
                // and no longer counts against the server's startup budget.
                drop(permit);
                handshake_done.store(true, Ordering::Release);
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
                let tail = Self::proxy_stderr_tail(draining, &stderr_tail).await;
                let stderr = tail.join("\n");
                let refusal = permit
                    .as_ref()
                    .and(status)
                    .and_then(|status| ssh_refusal(status, &stderr));
                drop(permit);
                // A session turned away by the transport usually means its
                // master died; make the retry check and reopen it.
                if refusal == Some(SshRefusal::BeforeAuthentication)
                    && let Some(lease) = &client.ssh_session
                {
                    lease.invalidate();
                }
                let error = Self::attach_proxy_stderr(error, tail);
                let retry = match refusal {
                    Some(refusal) => Some(ConnectRetry::Refused(refusal, stderr)),
                    None if status.is_some() && worker_socket_missing(&stderr) => {
                        Some(ConnectRetry::SocketMissing)
                    }
                    None => None,
                };
                match retry {
                    // A launch that will be retried is logged by the retry.
                    // Its proxy has exited and was reaped above, so dropping
                    // `client` must not report that exit as a second warning.
                    Some(_) => drop(client.child.take()),
                    // Anything else is a failure of this proxy or its worker.
                    None => log_relay_client_failure(&client, "hello", "relay-hello", &error),
                }
                Err(ConnectFailure { error, retry })
            }
        }
    }

    /// Collect the proxy's trailing stderr.
    ///
    /// The caller has already waited for the proxy or killed it, so the drain
    /// normally reaches EOF at once. The grace period covers the case it
    /// cannot: a grandchild that inherited stderr keeps the pipe open for as
    /// long as it lives. Either way the lines already read are returned, since
    /// the drain publishes them as it goes.
    pub(super) async fn proxy_stderr_tail(
        draining: Option<tokio::task::JoinHandle<()>>,
        tail: &ProxyStderrTail,
    ) -> Vec<String> {
        if let Some(handle) = draining
            && tokio::time::timeout(RELAY_PROXY_DETACH_GRACE, handle)
                .await
                .is_err()
        {
            tracing::debug!("relay proxy stderr is still open; reporting the lines read so far");
        }
        tail.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
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
            bail!("relay returned an unexpected hello response");
        };
        if session_id != expected_session_id {
            bail!("relay belongs to session {session_id}, not {expected_session_id}");
        }
        if !RelayVersionRange::CURRENT.contains(negotiated) {
            bail!(
                "relay negotiated unsupported protocol {negotiated}; this controller supports {}-{}",
                RELAY_MIN_PROTOCOL_VERSION,
                RELAY_PROTOCOL_VERSION
            );
        }
        self.protocol_version = negotiated;
        self.session_id = session_id;
        self.relay_version = relay_version;
        self.worker_build = worker_build;
        Ok(())
    }
}

/// Whether a relay proxy that exited during hello found no control socket to
/// connect to. The worker's proxy reports a failed connect as "connect worker
/// socket <path>" (`mj-worker`'s `proxy`), and a socket file that does not
/// exist yet as ENOENT.
fn worker_socket_missing(stderr: &str) -> bool {
    stderr.contains("connect worker socket") && stderr.contains("No such file or directory")
}
