//! Controller-side client for a session relay's JSON-lines proxy.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};

use hel::hel_config::harness_authentication_marker;
use hel::hel_credentials::{
    CredentialSnapshot, CredentialSyncAction, CredentialSyncHandle, CredentialSyncOutcome,
    CredentialSyncResult, CredentialSyncTarget, SYNC_INTERVAL, SyncAction, SyncTrigger, enqueue,
    profiles_with_targets, read_credential_file, reconcile, validate_credential_payload,
    write_credential_file,
};
use hel::hel_elicitation::ElicitationResponse;
use hel::hel_targets::CommandSpec;
use hel::hel_worker::{
    MAX_FRAME_BYTES, RELAY_EVENT_GENESIS_DIGEST, RELAY_MIN_PROTOCOL_VERSION,
    RELAY_PROTOCOL_VERSION, RelayCommand, RelayCursor, RelayErrorCode, RelayEvent,
    RelayOperationalState, RelayProtocolError, RelayRequest, RelayRequestEnvelope,
    RelayResponseBody, RelayResponseEnvelope, RelayResponsePayload, RelayVersionRange,
    ReviewerRequest, validate_relay_event,
};
use hel::hel_worker_launch::ReviewerLaunchConfig;

const RELAY_RPC_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_SLOW_OPERATION_WARNING: Duration = Duration::from_secs(5);
/// Starting a target-side proxy may page the full worker executable in and
/// traverse a container runtime before the relay sees `hello`. That is worker
/// startup latency, not an ordinary in-connection RPC.
const RELAY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(300);
/// An attachment can decompress a transport-sized page from cold journal
/// segments. It remains bounded by the relay frame budget, but cold or loaded
/// storage needs a filesystem deadline rather than an in-memory RPC deadline.
const RELAY_HISTORY_TIMEOUT: Duration = Duration::from_secs(900);
/// Advancing an acknowledgement can durably prune a large relay journal. The
/// worker performs that maintenance before replying, so it needs a deadline
/// sized for filesystem work rather than ordinary relay bookkeeping.
const RELAY_ACKNOWLEDGE_TIMEOUT: Duration = Duration::from_secs(300);
/// A compaction request runs a full model turn in a scratch ACP session, so it
/// outlives the deadline that suits the relay's bookkeeping calls.
const RELAY_COMPACT_TIMEOUT: Duration = Duration::from_secs(600);
/// Capturing a review delta runs Git over every workspace repository, which is
/// filesystem work on a possibly large tree rather than relay bookkeeping.
const REVIEW_CAPTURE_TIMEOUT: Duration = Duration::from_secs(300);
/// Bifrost's semantic diff analysis has its own 600-second budget inside the
/// worker; this leaves room for it to report a timeout as an error rather than
/// having the call time out underneath it.
const REVIEW_ANALYSIS_TIMEOUT: Duration = Duration::from_secs(660);
const RELAY_PROXY_DETACH_GRACE: Duration = Duration::from_millis(500);
const RELAY_PROXY_REAP_POLL: Duration = Duration::from_millis(10);

/// Forward a relay proxy's stderr to the log, one line at a time, until the
/// child closes it. Reporting rather than dropping keeps connect failures
/// diagnosable now that the controller no longer shares its terminal.
async fn drain_proxy_stderr(
    errors: tokio::process::ChildStderr,
    purpose: String,
    session_id: String,
) {
    let mut lines = BufReader::new(errors).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => {
                tracing::warn!(%session_id, %purpose, %line, "relay proxy stderr")
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%session_id, %purpose, %error, "read relay proxy stderr");
                return;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelayAttachment {
    pub state: RelayOperationalState,
    pub events: Vec<RelayEvent>,
    pub through_ordinal: u64,
    pub through_digest: String,
}

/// What the reviewer sidecar reports once it is running.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StartedReviewer {
    /// The reviewer's own native session, distinct from the primary's.
    pub native_session_id: Option<String>,
    /// What the reviewer's harness advertises right now, which is what the
    /// selection waterfall offers the user.
    pub config_options: Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
    /// Whether an already-running reviewer served this request.
    pub reused: bool,
    pub state: RelayOperationalState,
}

/// One bounded page in a catch-up whose upper frontier was fixed before any
/// page was applied. The relay may return newer events on later `Attach`
/// calls; those are deliberately left for the next catch-up.
#[derive(Debug, Clone)]
pub struct RelayEventPage {
    pub events: Vec<RelayEvent>,
    pub through_ordinal: u64,
    pub through_digest: String,
}

#[derive(Debug, Clone)]
pub struct RelayCatchUp {
    pub state: RelayOperationalState,
    pub frontier: RelayCursor,
    pub first_page: RelayEventPage,
}

#[derive(Debug)]
pub struct RelayRejected(pub RelayProtocolError);

impl std::fmt::Display for RelayRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "relay rejected request ({:?}): {}",
            self.0.code, self.0.message
        )
    }
}

impl std::error::Error for RelayRejected {}

impl RelayRejected {
    pub fn is_desynchronized(&self) -> bool {
        self.0.code == RelayErrorCode::Desynchronized
    }

    /// Whether the relay itself said the same request could succeed later.
    /// Validation rejections say no; transient internal failures say yes.
    pub fn is_retryable(&self) -> bool {
        self.0.retryable
    }
}

/// A relay transport that can no longer carry requests: the proxy exited, one
/// of its pipes failed, or the handshake never completed.
///
/// Every site that can prove this attaches the marker, and recovery decisions
/// such as worker auto-restart downcast for it. Nothing reads the message text,
/// so rewording a diagnostic can never silently disable recovery.
#[derive(Debug)]
pub struct RelayTransportDead {
    message: String,
    handshake_failed: bool,
}

impl std::fmt::Display for RelayTransportDead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RelayTransportDead {}

impl RelayTransportDead {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            handshake_failed: false,
        }
    }

    /// Mark an I/O failure on the relay's pipes. The marker reports exactly
    /// what the I/O error reported, so it adds a type without adding text.
    fn from_io(error: std::io::Error, kind: ExchangeKind) -> Self {
        Self::during_exchange(error.to_string(), kind)
    }

    fn during_exchange(message: impl Into<String>, kind: ExchangeKind) -> Self {
        Self {
            message: message.into(),
            handshake_failed: kind == ExchangeKind::Handshake,
        }
    }

    /// Whether this error, or any cause behind it, is a dead relay transport.
    pub fn marks(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }

    /// Whether the worker was reachable enough to run its liveness probe but
    /// the proxy then disconnected or failed I/O during a fresh handshake.
    /// Timeouts are deliberately not marked: a live proxy can be waiting on a
    /// loaded container runtime or filesystem, which restarting only worsens.
    pub fn marks_failed_handshake(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<Self>()
            .is_some_and(|failure| failure.handshake_failed)
    }
}

/// Whether an exchange is the handshake that proves the transport carries
/// traffic at all.
///
/// A disconnected handshake proves the new transport never became usable. A
/// timeout does not: the proxy launcher or worker can still be alive and slow,
/// so timeout classification is handled separately in [`RelayClient::exchange`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExchangeKind {
    Handshake,
    Call,
}

/// Controller-side connection to the durable ACP relay protocol.
///
/// This type does not construct transcript state or request unbounded history.
/// Callers persist bounded attachment pages, then acknowledge only a frontier
/// that is already durable locally.
pub struct RelayClient {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    request_timeout: Duration,
    /// Why this connection can no longer be used, once a call gave up on a
    /// reply that is still in flight. See [`RelayClient::exchange`].
    abandoned: Option<String>,
    next_request: u64,
    connection_nonce: u64,
    protocol_version: u32,
    session_id: String,
    relay_version: String,
    /// Content address of the executable the worker is running, as reported in
    /// hello. `None` from a worker built before the field existed.
    worker_build: Option<String>,
    latest_ordinal: u64,
    latest_digest: String,
}

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
    async fn connect_with_timeout(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
    ) -> Result<Self> {
        Self::connect_with_timeouts(spec, expected_session_id, request_timeout, request_timeout)
            .await
    }

    async fn connect_with_timeouts(
        spec: &CommandSpec,
        expected_session_id: &str,
        request_timeout: Duration,
        handshake_timeout: Duration,
    ) -> Result<Self> {
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
        if let Some(errors) = child.stderr.take() {
            let purpose = spec.purpose.clone();
            let session_id = expected_session_id.to_owned();
            tokio::spawn(drain_proxy_stderr(errors, purpose, session_id));
        }
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
        let response = client
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
            log_relay_client_failure(&client, "hello", "relay-hello", &error);
            return Err(error);
        };
        if session_id != expected_session_id {
            let error = anyhow!("relay belongs to session {session_id}, not {expected_session_id}");
            log_relay_client_failure(&client, "hello", "relay-hello", &error);
            return Err(error);
        }
        if !RelayVersionRange::CURRENT.contains(negotiated) {
            let error = anyhow!(
                "relay negotiated unsupported protocol {negotiated}; this controller supports {}-{}",
                RELAY_MIN_PROTOCOL_VERSION,
                RELAY_PROTOCOL_VERSION
            );
            log_relay_client_failure(&client, "hello", "relay-hello", &error);
            return Err(error);
        }
        client.protocol_version = negotiated;
        client.session_id = session_id;
        client.relay_version = relay_version;
        client.worker_build = worker_build;
        Ok(client)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub const fn supports_project_memory_sync(&self) -> bool {
        self.protocol_version >= 4
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
    ) -> Result<hel::hel_credentials::GithubTokenSnapshot> {
        github_token_snapshot(self.call(RelayRequest::GithubTokenState).await?)
    }

    pub async fn install_github_token(
        &mut self,
        token: &str,
    ) -> Result<hel::hel_credentials::GithubTokenSnapshot> {
        github_token_snapshot(
            self.call(RelayRequest::InstallGithubToken {
                data: BASE64.encode(token.as_bytes()),
            })
            .await?,
        )
    }

    pub async fn remove_github_token(
        &mut self,
    ) -> Result<hel::hel_credentials::GithubTokenSnapshot> {
        github_token_snapshot(self.call(RelayRequest::RemoveGithubToken).await?)
    }

    /// Return the fingerprint of this session's synced skills trees without
    /// transferring the tree itself.
    pub async fn skills_state(&mut self) -> Result<hel::hel_skills::SkillsSyncState> {
        skills_sync_state(self.call(RelayRequest::SkillsState).await?)
    }

    /// Install background text that only the target harness sees, prepended
    /// to the next real prompt without creating a synthetic transcript turn.
    pub async fn install_prompt_context(&mut self, text: String) -> Result<()> {
        let request = RelayRequest::InstallPromptContext { text };
        if !request.supported_at(self.protocol_version) {
            bail!(
                "hidden prompt context requires relay protocol {}; this session negotiated {}",
                request.minimum_protocol(),
                self.protocol_version
            );
        }
        match self.call(request).await? {
            RelayResponsePayload::PromptContextInstalled => Ok(()),
            _ => bail!("relay returned an unexpected prompt-context response"),
        }
    }

    pub async fn project_memory_snapshot(
        &mut self,
    ) -> Result<(
        hel::hel_project_memory::ProjectMemorySnapshot,
        hel::hel_project_memory::ProjectMemorySnapshot,
    )> {
        let request = RelayRequest::ProjectMemorySnapshot;
        if !request.supported_at(self.protocol_version) {
            bail!(
                "project memory synchronization requires relay protocol {}; this session negotiated {}",
                request.minimum_protocol(),
                self.protocol_version
            );
        }
        match self.call(request).await? {
            RelayResponsePayload::ProjectMemorySnapshot { baseline, replica } => {
                Ok((baseline, replica))
            }
            _ => bail!("relay returned an unexpected project-memory response"),
        }
    }

    pub async fn install_project_memory_snapshot(
        &mut self,
        snapshot: hel::hel_project_memory::ProjectMemorySnapshot,
    ) -> Result<()> {
        let request = RelayRequest::InstallProjectMemorySnapshot { snapshot };
        if !request.supported_at(self.protocol_version) {
            bail!(
                "project memory synchronization requires relay protocol {}; this session negotiated {}",
                request.minimum_protocol(),
                self.protocol_version
            );
        }
        match self.call(request).await? {
            RelayResponsePayload::ProjectMemorySnapshotInstalled => Ok(()),
            _ => bail!("relay returned an unexpected project-memory install response"),
        }
    }

    /// Replace this session's synced skills trees with an encoded
    /// `hel_skills::SkillsArchive`. The destination directories are fixed by
    /// the session's launch config and the harness skills whitelist.
    pub async fn install_skills(
        &mut self,
        archive_bytes: &[u8],
    ) -> Result<hel::hel_skills::SkillsSyncState> {
        skills_sync_state(
            self.call(RelayRequest::InstallSkills {
                data: BASE64.encode(archive_bytes),
            })
            .await?,
        )
    }

    pub async fn submit(
        &mut self,
        command_id: impl Into<String>,
        command: RelayCommand,
    ) -> Result<u64> {
        let command_id = command_id.into();
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

    /// Run a prompt in a disposable ACP session and return its agent text.
    /// The relay serves this on the connection, so it never becomes session
    /// history.
    pub async fn compact(&mut self, prompt: String) -> Result<String> {
        match self
            .call_with_timeout(RelayRequest::Compact { prompt }, RELAY_COMPACT_TIMEOUT)
            .await?
        {
            RelayResponsePayload::Compacted { text } => Ok(text),
            _ => bail!("relay returned an unexpected compaction response"),
        }
    }

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

    /// Report what every workspace repository changed since the review
    /// baselines the controller holds.
    pub async fn capture_review_delta(
        &mut self,
        role: Option<&str>,
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    ) -> Result<Vec<hel::hel_worker::RepoDelta>> {
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
        repositories: Vec<hel::hel_worker::AnalyzeDeltaRepository>,
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
    ) -> Result<Vec<hel::hel_review::lanes::ReviewSubagentRequest>> {
        let request = self.reviewer_request(None, ReviewerRequest::TakeLaneDispatches)?;
        match self.call(request).await? {
            RelayResponsePayload::LaneDispatches { requests } => Ok(requests),
            _ => bail!("relay returned an unexpected lane dispatch response"),
        }
    }

    /// Wraps a reviewer action, refusing it on a worker too old to know what a
    /// reviewer is rather than sending a method it would reject as unknown.
    fn reviewer_request(
        &self,
        role: Option<&str>,
        request: ReviewerRequest,
    ) -> Result<RelayRequest> {
        let request = RelayRequest::Reviewer {
            role: role.map(str::to_owned),
            request,
        };
        if !request.supported_at(self.protocol_version) {
            bail!(
                "a second opinion requires relay protocol {}; this session negotiated {}",
                request.minimum_protocol(),
                self.protocol_version
            );
        }
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
        if !request.supported_at(self.protocol_version) {
            bail!(
                "elicitation responses require relay protocol {}; this session negotiated {}",
                request.minimum_protocol(),
                self.protocol_version
            );
        }
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

    async fn call(&mut self, request: RelayRequest) -> Result<RelayResponsePayload> {
        self.call_with_timeout(request, self.request_timeout).await
    }

    async fn call_with_timeout(
        &mut self,
        request: RelayRequest,
        timeout: Duration,
    ) -> Result<RelayResponsePayload> {
        let operation = request.method_name();
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

    async fn call_hello(
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
        let line = match self
            .exchange(&envelope, operation, timeout, ExchangeKind::Handshake)
            .await
        {
            Ok(line) => line,
            Err(error) => {
                log_relay_client_failure(self, operation, &request_id, &error);
                return Err(error);
            }
        };
        let result = decode_relay_hello_response(&line, &request_id);
        if let Err(error) = &result {
            log_relay_client_failure(self, operation, &request_id, error);
        }
        result
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
    async fn exchange(
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

    fn request_id(&mut self) -> String {
        let id = format!("relay-{:016x}-{}", self.connection_nonce, self.next_request);
        self.next_request = self.next_request.wrapping_add(1);
        id
    }
}

/// Keep transport, protocol, and explicit relay rejections visible at the
/// point where a request fails. Callers often turn these into a user-facing
/// string or a retry, which otherwise loses the operation and request ID that
/// make concurrent session failures diagnosable.
fn log_relay_client_failure(
    client: &RelayClient,
    operation: &str,
    request_id: &str,
    error: &anyhow::Error,
) {
    let rejection = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RelayRejected>()
            .map(|rejected| &rejected.0)
    });
    let transport_dead = RelayTransportDead::marks(error);
    match rejection {
        Some(rejection) => tracing::warn!(
            session_id = %client.session_id,
            relay_version = %client.relay_version,
            %operation,
            %request_id,
            relay_error_code = ?rejection.code,
            relay_retryable = rejection.retryable,
            transport_dead,
            error = %error,
            "relay request rejected"
        ),
        None => tracing::warn!(
            session_id = %client.session_id,
            relay_version = %client.relay_version,
            %operation,
            %request_id,
            transport_dead,
            error = %error,
            "relay request failed"
        ),
    }
}

impl Drop for RelayClient {
    fn drop(&mut self) {
        // Async owners call `detach` so EOF has a bounded chance to propagate
        // through Podman or SSH before the launcher is stopped. Drop is the
        // shutdown-safe fallback: it may run while Tokio's drivers are already
        // gone, so its bounded reaper cannot use runtime work or Tokio timers.
        drop(self.input.take());
        let Some(child) = self.child.take() else {
            return;
        };
        let session_id = self.session_id.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("hel-relay-reaper".into())
            .spawn(move || reap_dropped_relay_proxy(child, session_id))
        {
            tracing::warn!(
                session_id = %self.session_id,
                %error,
                "could not start dropped relay proxy reaper"
            );
        }
    }
}

/// Let EOF traverse a proxy launcher, then stop and reap it without relying on
/// an async runtime that may already be shutting down.
fn reap_dropped_relay_proxy(mut child: Child, session_id: String) {
    let deadline = Instant::now() + RELAY_PROXY_DETACH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::warn!(
                        %session_id,
                        %status,
                        "dropped relay proxy exited unsuccessfully"
                    );
                }
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(RELAY_PROXY_REAP_POLL);
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%session_id, %error, "could not reap dropped relay proxy");
                return;
            }
        }
    }

    if let Err(error) = child.start_kill()
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%session_id, %error, "could not stop dropped relay proxy");
        return;
    }
    let deadline = Instant::now() + RELAY_PROXY_DETACH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(RELAY_PROXY_REAP_POLL);
            }
            Ok(None) => {
                tracing::warn!(%session_id, "stopped relay proxy could not be reaped in time");
                return;
            }
            Err(error) => {
                tracing::warn!(%session_id, %error, "could not reap stopped relay proxy");
                return;
            }
        }
    }
}

fn credential_snapshot(payload: RelayResponsePayload) -> Result<CredentialSnapshot> {
    match payload {
        RelayResponsePayload::CredentialState {
            present,
            fingerprint,
            freshness_epoch_ms,
        } => Ok(CredentialSnapshot {
            present,
            fingerprint,
            freshness_epoch_ms,
        }),
        _ => bail!("relay returned an unexpected credential state response"),
    }
}

fn skills_sync_state(payload: RelayResponsePayload) -> Result<hel::hel_skills::SkillsSyncState> {
    match payload {
        RelayResponsePayload::SkillsState {
            present,
            fingerprint,
        } => Ok(hel::hel_skills::SkillsSyncState {
            present,
            fingerprint,
        }),
        _ => bail!("relay returned an unexpected skills state response"),
    }
}

fn github_token_snapshot(
    payload: RelayResponsePayload,
) -> Result<hel::hel_credentials::GithubTokenSnapshot> {
    match payload {
        RelayResponsePayload::GithubTokenState {
            present,
            fingerprint,
        } => Ok(hel::hel_credentials::GithubTokenSnapshot {
            present,
            fingerprint,
        }),
        _ => bail!("relay returned an unexpected GitHub token state response"),
    }
}

async fn read_bounded_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    kind: ExchangeKind,
) -> Result<Option<String>> {
    read_bounded_frame_with_limit(reader, MAX_FRAME_BYTES, kind).await
}

async fn read_bounded_frame_with_limit(
    reader: &mut (impl AsyncBufRead + Unpin),
    maximum_bytes: usize,
    kind: ExchangeKind,
) -> Result<Option<String>> {
    let mut frame = Vec::new();
    loop {
        // A failed read and a half-written frame are transport deaths; the
        // limit and encoding failures below are protocol violations that a
        // worker restart would not fix, so only these two carry the marker.
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| RelayTransportDead::from_io(error, kind))?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(anyhow::Error::new(RelayTransportDead::during_exchange(
                "relay proxy disconnected in the middle of a response frame",
                kind,
            )));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let payload = newline.map_or(available, |position| &available[..position]);
        if frame.len().saturating_add(payload.len()) > maximum_bytes {
            bail!("relay response frame is too large");
        }
        frame.extend_from_slice(payload);
        reader.consume(consumed);
        if newline.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return String::from_utf8(frame)
                .context("relay response is not UTF-8")
                .map(Some);
        }
    }
}

fn clip_catch_up_page(
    page: RelayAttachment,
    previous: &RelayCursor,
    frontier: &RelayCursor,
) -> Result<RelayEventPage> {
    if previous.ordinal > frontier.ordinal {
        bail!("relay catch-up starts beyond its fixed frontier");
    }
    if previous.ordinal == frontier.ordinal {
        if previous != frontier {
            bail!("relay catch-up cursor digest differs from its fixed frontier");
        }
        if !page.events.is_empty() || page.through_ordinal != previous.ordinal {
            bail!("relay attachment advanced beyond its advertised frontier");
        }
        return Ok(RelayEventPage {
            events: Vec::new(),
            through_ordinal: previous.ordinal,
            through_digest: previous.digest.clone(),
        });
    }
    if page.through_ordinal <= previous.ordinal || page.events.is_empty() {
        bail!("relay catch-up page did not advance");
    }
    if page.through_ordinal <= frontier.ordinal {
        let through = RelayCursor {
            ordinal: page.through_ordinal,
            digest: page.through_digest.clone(),
        };
        if through.ordinal == frontier.ordinal && through != *frontier {
            bail!("relay catch-up page digest differs from its fixed frontier");
        }
        return Ok(RelayEventPage {
            events: page.events,
            through_ordinal: through.ordinal,
            through_digest: through.digest,
        });
    }

    let events = page
        .events
        .into_iter()
        .take_while(|event| event.ordinal <= frontier.ordinal)
        .collect::<Vec<_>>();
    let reached = events
        .last()
        .map(|event| RelayCursor {
            ordinal: event.ordinal,
            digest: event.digest.clone(),
        })
        .ok_or_else(|| anyhow!("relay catch-up page skipped its fixed frontier"))?;
    if reached != *frontier {
        bail!("relay catch-up page does not contain its fixed frontier");
    }
    Ok(RelayEventPage {
        events,
        through_ordinal: reached.ordinal,
        through_digest: reached.digest,
    })
}

fn decode_relay_response(
    line: &str,
    request_id: &str,
    protocol: u32,
) -> Result<RelayResponsePayload> {
    let response: RelayResponseEnvelope =
        serde_json::from_str(line).context("decode relay response")?;
    if response.request_id != request_id {
        bail!(
            "relay response ID mismatch: expected {request_id}, got {}",
            response.request_id
        );
    }
    if response.protocol_version != protocol {
        bail!(
            "relay response protocol mismatch: expected {protocol}, got {}",
            response.protocol_version
        );
    }
    match response.body {
        RelayResponseBody::Ok { payload } => Ok(payload),
        RelayResponseBody::Error { error } => Err(RelayRejected(error).into()),
    }
}

fn decode_relay_hello_response(line: &str, request_id: &str) -> Result<RelayResponsePayload> {
    let response: RelayResponseEnvelope =
        serde_json::from_str(line).context("decode relay hello response")?;
    if response.request_id != request_id {
        bail!(
            "relay response ID mismatch: expected {request_id}, got {}",
            response.request_id
        );
    }
    match response.body {
        RelayResponseBody::Ok {
            payload: payload @ RelayResponsePayload::Hello { negotiated, .. },
        } => {
            if response.protocol_version != negotiated {
                bail!(
                    "relay hello envelope uses protocol {}, negotiated {negotiated}",
                    response.protocol_version
                );
            }
            Ok(payload)
        }
        RelayResponseBody::Ok { .. } => bail!("relay returned an unexpected hello response"),
        RelayResponseBody::Error { error } => Err(RelayRejected(error).into()),
    }
}

pub struct CredentialSyncCoordinator {
    handle: CredentialSyncHandle,
    results: mpsc::UnboundedReceiver<CredentialSyncResult>,
}

impl CredentialSyncCoordinator {
    pub fn spawn() -> Self {
        let (targets_tx, mut targets_rx) = watch::channel(Vec::new());
        let (triggers_tx, mut triggers_rx) = mpsc::unbounded_channel::<SyncTrigger>();
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<CredentialSyncResult>();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + SYNC_INTERVAL,
                SYNC_INTERVAL,
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // A pull rewrites the canonical file, so one profile is never
            // reconciled twice at once.
            let mut busy = BTreeSet::<String>::new();
            let mut queue = VecDeque::<SyncTrigger>::new();
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    changed = targets_rx.changed() => {
                        if changed.is_err() { break; }
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    trigger = triggers_rx.recv() => {
                        let Some(trigger) = trigger else { break };
                        enqueue(&mut queue, trigger);
                    }
                    completed = completed_rx.recv() => {
                        let Some(result) = completed else { break };
                        busy.remove(&result.profile_id);
                        if result.trigger.is_some()
                            || result.failure.is_some()
                            || !result.outcomes.is_empty()
                        {
                            let profile_id = result.profile_id.clone();
                            if results_tx.send(result).is_err() {
                                tracing::debug!(
                                    %profile_id,
                                    operation = "credential_sync_result",
                                    "credential sync result receiver was already closed"
                                );
                            }
                        }
                    }
                }

                let mut deferred = VecDeque::new();
                while let Some(trigger) = queue.pop_front() {
                    if busy.contains(&trigger.profile_id) {
                        deferred.push_back(trigger);
                        continue;
                    }
                    let targets: Vec<_> = targets_rx
                        .borrow()
                        .iter()
                        .filter(|target| target.profile_id == trigger.profile_id)
                        .cloned()
                        .collect();
                    if targets.is_empty() {
                        if trigger.cause.is_some() {
                            let profile_id = trigger.profile_id.clone();
                            if results_tx
                                .send(CredentialSyncResult {
                                    profile_id: trigger.profile_id,
                                    trigger: trigger.cause,
                                    failure: None,
                                    outcomes: Vec::new(),
                                })
                                .is_err()
                            {
                                tracing::debug!(
                                    %profile_id,
                                    operation = "credential_sync_result",
                                    "credential sync result receiver was already closed"
                                );
                            }
                        }
                        continue;
                    }
                    busy.insert(trigger.profile_id.clone());
                    let completed_tx = completed_tx.clone();
                    let handle = tokio::runtime::Handle::current();
                    // The blocking join is awaited so a panicked reconcile is
                    // reported and its profile always leaves the busy set.
                    tokio::spawn(async move {
                        let joined = tokio::task::spawn_blocking(move || {
                            handle.block_on(reconcile_profile(&targets))
                        })
                        .await;
                        let (failure, outcomes) = match joined {
                            Ok(outcomes) => (None, outcomes),
                            Err(error) => (Some(format!("sync task stopped: {error}")), Vec::new()),
                        };
                        let profile_id = trigger.profile_id.clone();
                        if completed_tx
                            .send(CredentialSyncResult {
                                profile_id: trigger.profile_id,
                                trigger: trigger.cause,
                                failure,
                                outcomes,
                            })
                            .is_err()
                        {
                            tracing::debug!(
                                %profile_id,
                                operation = "credential_sync_completion",
                                "credential sync coordinator stopped before receiving completion"
                            );
                        }
                    });
                }
                queue = deferred;
            }
        });
        Self {
            handle: CredentialSyncHandle {
                targets: Arc::new(targets_tx),
                triggers: triggers_tx,
            },
            results: results_rx,
        }
    }

    pub fn handle(&self) -> CredentialSyncHandle {
        self.handle.clone()
    }

    pub fn try_result(&mut self) -> Option<CredentialSyncResult> {
        self.results.try_recv().ok()
    }

    /// Waits for the next finished sync.
    ///
    /// Event-driven loops select on this instead of polling; `None` means the
    /// coordinator task has stopped. Cancel-safe, so a lost `select!` race
    /// keeps the result queued.
    pub async fn result(&mut self) -> Option<CredentialSyncResult> {
        self.results.recv().await
    }
}

/// Reconcile one profile with every live session that runs it.
///
/// A pull makes every other session's copy stale by definition, so the pass
/// runs again once with the new canonical bytes. Two passes are enough: the
/// second cannot pull anything the first did not already see unless a harness
/// refreshed mid-cycle, and that lands in the next cycle.
async fn reconcile_profile(targets: &[CredentialSyncTarget]) -> Vec<CredentialSyncOutcome> {
    let github_token = targets
        .iter()
        .any(|target| target.sync_github_token)
        .then(crate::hel_controller::controller_github_token)
        .flatten();
    let mut outcomes = BTreeMap::<String, CredentialSyncOutcome>::new();
    for pass in 0..2 {
        let mut pulled = false;
        for target in targets {
            match reconcile_session(target, github_token.as_deref()).await {
                Ok(actions) if actions.is_empty() => {}
                Ok(actions) => {
                    pulled |= actions.contains(&CredentialSyncAction::Pulled);
                    outcomes.insert(
                        target.session_id.clone(),
                        CredentialSyncOutcome {
                            session_id: target.session_id.clone(),
                            outcome: Ok(actions),
                        },
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %target.session_id,
                        profile_id = %target.profile_id,
                        pass = pass + 1,
                        error = %error,
                        "credential synchronization failed for relay session"
                    );
                    outcomes.insert(
                        target.session_id.clone(),
                        CredentialSyncOutcome {
                            session_id: target.session_id.clone(),
                            outcome: Err(format!("{error:#}")),
                        },
                    );
                }
            }
        }
        if !pulled || pass == 1 {
            break;
        }
    }
    outcomes.into_values().collect()
}

/// Returns every action taken; an empty list means the copies already agree.
async fn reconcile_session(
    target: &CredentialSyncTarget,
    github_token: Option<&str>,
) -> Result<Vec<CredentialSyncAction>> {
    let canonical_path = harness_authentication_marker(target.harness, &target.profile_home);
    let (canonical, canonical_bytes) = read_credential_file(target.harness, &canonical_path)?;
    let canonical_skills = hel::hel_skills::collect_skills(target.harness, &target.profile_home)
        .with_context(|| {
            format!(
                "collect canonical skills for profile {} from {}",
                target.profile_id,
                target.profile_home.display()
            )
        })?;
    let mut client = RelayClient::connect(&target.spec, &target.session_id).await?;
    let result = reconcile_connected(
        &mut client,
        target,
        &canonical_path,
        &canonical,
        &canonical_bytes,
        &canonical_skills,
        github_token,
    )
    .await;
    // Detach even when the exchange failed; the worker and harness keep
    // running either way. A failed detach only leaks a short-lived proxy, so it
    // is reported rather than turned into a sync failure.
    if let Err(error) = client.detach().await {
        tracing::warn!(
            session_id = %target.session_id,
            "could not close the credential sync connection: {error:#}"
        );
    }
    result
}

async fn reconcile_connected(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical_path: &Path,
    canonical: &CredentialSnapshot,
    canonical_bytes: &[u8],
    canonical_skills: &hel::hel_skills::SkillsArchive,
    github_token: Option<&str>,
) -> Result<Vec<CredentialSyncAction>> {
    let mut actions = Vec::new();
    let session = client.credential_state().await?;
    match reconcile(canonical, &session) {
        SyncAction::None => {
            if canonical.present
                && session.present
                && canonical.fingerprint != session.fingerprint
                && canonical.freshness_epoch_ms.is_none()
                && session.freshness_epoch_ms.is_none()
            {
                tracing::warn!(
                    session_id = %target.session_id,
                    profile_id = %target.profile_id,
                    "credential copies differ but neither reports a refresh time; leaving both alone"
                );
            }
        }
        SyncAction::Push => {
            client.install_credentials(canonical_bytes).await?;
            actions.push(CredentialSyncAction::Pushed);
        }
        SyncAction::Pull => {
            let bytes = client.read_credentials().await?;
            validate_credential_payload(target.harness, &bytes).with_context(|| {
                format!(
                    "session {} returned an unusable credential file",
                    target.session_id
                )
            })?;
            write_credential_file(target.harness, canonical_path, &bytes).with_context(|| {
                format!(
                    "install fresher credentials from session {} for profile {}",
                    target.session_id, target.profile_id
                )
            })?;
            actions.push(CredentialSyncAction::Pulled);
        }
    }
    if reconcile_skills(client, target, canonical_skills).await? {
        actions.push(CredentialSyncAction::SkillsPushed);
    }
    if target.sync_github_token
        && let Some(action) = reconcile_github_token(client, target, github_token).await?
    {
        actions.push(action);
    }
    Ok(actions)
}

async fn reconcile_github_token(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: Option<&str>,
) -> Result<Option<CredentialSyncAction>> {
    let session = match client.github_token_state().await {
        Ok(state) => state,
        Err(error) if sync_method_unsupported(&error) => {
            tracing::debug!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                "worker predates GitHub token sync; skipping until the target is re-provisioned"
            );
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    match canonical {
        Some(token) => {
            let canonical = hel::hel_credentials::GithubTokenSnapshot::of(token);
            if session == canonical {
                return Ok(None);
            }
            let installed = client.install_github_token(token).await?;
            if installed != canonical {
                bail!(
                    "session {} GitHub token fingerprint does not match the controller after install",
                    target.session_id
                );
            }
            Ok(Some(CredentialSyncAction::GithubTokenPushed))
        }
        None if session.present => {
            let removed = client.remove_github_token().await?;
            if removed.present {
                bail!(
                    "session {} retained its GitHub token after removal",
                    target.session_id
                );
            }
            Ok(Some(CredentialSyncAction::GithubTokenRemoved))
        }
        None => Ok(None),
    }
}

/// Converge the session's synced skills trees onto the canonical archive.
/// Returns true when a push happened. Workers old enough to predate skills
/// sync answer the unknown method with `InvalidRequest`; those sessions are
/// skipped quietly until their target is re-provisioned.
async fn reconcile_skills(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: &hel::hel_skills::SkillsArchive,
) -> Result<bool> {
    let canonical_state = canonical.state();
    let session = match client.skills_state().await {
        Ok(state) => state,
        Err(error) if sync_method_unsupported(&error) => {
            tracing::debug!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                "worker predates skills sync; skipping until the target is re-provisioned"
            );
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if session == canonical_state {
        return Ok(false);
    }
    let installed = client.install_skills(&canonical.encode()).await?;
    if installed != canonical_state {
        bail!(
            "session {} skills fingerprint {} does not match the canonical {} after install",
            target.session_id,
            installed.fingerprint,
            canonical_state.fingerprint
        );
    }
    Ok(true)
}

fn sync_method_unsupported(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| rejected.0.code == RelayErrorCode::InvalidRequest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_worker::{DurableRelay, RelayObservation};
    const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

    #[test]
    fn relay_decoder_preserves_explicit_desynchronization() {
        let response = RelayResponseEnvelope {
            request_id: "relay-1".into(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            body: RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Desynchronized,
                    message: "journal gap".into(),
                    retryable: false,
                    detail: None,
                },
            },
        };
        let encoded = serde_json::to_string(&response).unwrap();
        let error = decode_relay_response(&encoded, "relay-1", RELAY_PROTOCOL_VERSION).unwrap_err();
        assert!(
            error
                .downcast_ref::<RelayRejected>()
                .is_some_and(RelayRejected::is_desynchronized)
        );
    }

    #[test]
    fn relay_decoder_rejects_crossed_request_ids() {
        let response = RelayResponseEnvelope {
            request_id: "other".into(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            body: RelayResponseBody::Ok {
                payload: RelayResponsePayload::Acknowledged {
                    through_ordinal: 4,
                    through_digest: "a".repeat(64),
                },
            },
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(
            decode_relay_response(&encoded, "wanted", RELAY_PROTOCOL_VERSION)
                .unwrap_err()
                .to_string()
                .contains("ID mismatch")
        );
    }

    #[test]
    fn command_spec_preserves_argv_boundaries() {
        let spec = CommandSpec::new("ssh", ["host", "hel worker proxy --root '/odd path'"]);
        assert_eq!(spec.program, "ssh");
        assert_eq!(spec.args.len(), 2);
        assert_eq!(spec.args[1], "hel worker proxy --root '/odd path'");
    }

    #[test]
    fn relay_protocol_version_range_contains_current_version() {
        assert_eq!(
            RelayVersionRange::CURRENT.negotiate(RelayVersionRange::CURRENT),
            Some(RELAY_PROTOCOL_VERSION)
        );
        assert_eq!(
            RelayVersionRange::CURRENT.negotiate(RelayVersionRange { min: 1, max: 1 }),
            Some(1)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn controller_accepts_negotiated_protocol_v1() {
        let script = format!(
            r#"python3 -c '
import json, sys
session = {session:?}
req = json.loads(sys.stdin.readline())
assert req["request"]["method"] == "hello"
supported = req["request"]["params"]["supported"]
assert supported["min"] <= 1 <= supported["max"]
print(json.dumps({{
    "request_id": req["request_id"],
    "protocol_version": 1,
    "result": "ok",
    "payload": {{
        "type": "hello",
        "data": {{
            "negotiated": 1,
            "relay_version": "v1-fixture",
            "session_id": session,
        }},
    }},
}}), flush=True)
sys.stdin.read()
'"#,
            session = SESSION_ID
        );
        let spec = CommandSpec::new("sh", ["-c", &script]).purpose("v1 relay fixture");
        let client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
            .await
            .expect("protocol v1 hello must be accepted");
        assert_eq!(client.protocol_version(), 1);
        assert_eq!(client.relay_version(), "v1-fixture");
    }

    /// The build a worker reports is what decides whether it is replaced, so a
    /// controller has to read it from hello - and read a worker that reports
    /// none as exactly that, rather than failing the handshake.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hello_reports_the_worker_build_or_none_from_an_older_worker() {
        let hello = |build: Option<&str>| {
            let data = match build {
                Some(build) => format!(
                    r#"{{"negotiated":1,"relay_version":"build-fixture","session_id":"%s","worker_build":"{build}"}}"#
                ),
                None => r#"{"negotiated":1,"relay_version":"build-fixture","session_id":"%s"}"#
                    .to_owned(),
            };
            format!(
                r#"
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{{"request_id":"%s","protocol_version":1,"result":"ok","payload":{{"type":"hello","data":{data}}}}}
' "$id" "$1"
sh -c 'while :; do sleep 30; done'
"#
            )
        };
        for reported in [None, Some("a".repeat(64).as_str())] {
            let spec = CommandSpec::new(
                "sh",
                [
                    "-c".to_owned(),
                    hello(reported),
                    "hel-relay-build-fixture".to_owned(),
                    SESSION_ID.to_owned(),
                ],
            )
            .purpose("relay worker build fixture");
            let client =
                RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
                    .await
                    .expect("hello must be accepted with and without a worker build");
            assert_eq!(client.worker_build(), reported);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_client_delivers_eof_before_stopping_its_proxy_launcher() {
        let directory = tempfile::tempdir().unwrap();
        let eof = directory.path().join("proxy-saw-eof");
        let script = r#"
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"request_id":"%s","protocol_version":1,"result":"ok","payload":{"type":"hello","data":{"negotiated":1,"relay_version":"eof-fixture","session_id":"%s"}}}\n' "$id" "$1"
if IFS= read -r _; then exit 9; fi
: > "$2"
"#;
        let spec = CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                script.to_owned(),
                "hel-relay-eof-fixture".to_owned(),
                SESSION_ID.to_owned(),
                eof.to_string_lossy().into_owned(),
            ],
        )
        .purpose("relay proxy EOF fixture");
        let client = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
            .await
            .unwrap();

        drop(client);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !eof.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("proxy launcher was killed before it observed stdin EOF");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn controller_rejects_negotiated_protocol_outside_supported_range() {
        let future_protocol = RELAY_PROTOCOL_VERSION + 1;
        let script = format!(
            r#"python3 -c '
import json, sys
session = {session:?}
req = json.loads(sys.stdin.readline())
print(json.dumps({{
    "request_id": req["request_id"],
    "protocol_version": {future_protocol},
    "result": "ok",
    "payload": {{
        "type": "hello",
        "data": {{
            "negotiated": {future_protocol},
            "relay_version": "future",
            "session_id": session,
        }},
    }},
}}), flush=True)
sys.stdin.read()
'"#,
            session = SESSION_ID,
            future_protocol = future_protocol,
        );
        let spec = CommandSpec::new("sh", ["-c", &script]).purpose("future relay fixture");
        let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
            .await
            .err()
            .expect("a future protocol hello must be rejected");
        assert!(
            error.to_string().contains(&format!(
                "negotiated unsupported protocol {future_protocol}"
            )),
            "{error:#}"
        );
        // The transport carried the answer perfectly well; restarting the
        // worker cannot make it speak a protocol it does not implement.
        assert!(!RelayTransportDead::marks(&error), "{error:#}");
    }

    /// A proxy that exits without answering is the ordinary shape of a dead
    /// worker. Recovery hangs on this being typed rather than read.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_proxy_that_exits_before_hello_reports_a_dead_transport() {
        let spec = CommandSpec::new("sh", ["-c", "exit 1"]).purpose("exiting relay proxy");

        let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_secs(5))
            .await
            .err()
            .expect("a proxy that exits cannot complete hello");

        assert!(RelayTransportDead::marks(&error), "{error:#}");
        assert!(RelayTransportDead::marks_failed_handshake(&error));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn silent_proxy_handshake_has_a_bounded_deadline() {
        let spec = CommandSpec::new("sh", ["-c", "sleep 30"]).purpose("test silent relay proxy");
        let started = std::time::Instant::now();

        let error = RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_millis(50))
            .await
            .err()
            .expect("silent relay must time out");

        assert!(error.to_string().contains("relay hello timed out"));
        // The launcher is still alive. A loaded target can look exactly like
        // this while starting its proxy, so worker recovery must not restart
        // the native session merely because the deadline elapsed.
        assert!(!RelayTransportDead::marks(&error), "{error:#}");
        assert!(!RelayTransportDead::marks_failed_handshake(&error));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A relay that answers `hello` at once and then stalls, replying to the
    /// next request long after any controller deadline. `$1` is the session id.
    #[cfg(unix)]
    const STALLING_RELAY: &str = r#"
IFS= read -r hello
id=$(printf '%s' "$hello" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{"request_id":"%s","protocol_version":1,"result":"ok","payload":{"type":"hello","data":{"negotiated":1,"relay_version":"stalling-fixture","session_id":"%s"}}}\n' "$id" "$1"
IFS= read -r stalled
id=$(printf '%s' "$stalled" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
sleep 5
printf '{"request_id":"%s","protocol_version":1,"result":"error","error":{"code":"internal","message":"late reply","retryable":false}}\n' "$id"
cat > /dev/null
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_call_abandons_the_connection_instead_of_desynchronizing_it() {
        let spec = CommandSpec::new(
            "sh",
            ["-c", STALLING_RELAY, "hel-relay-fixture", SESSION_ID],
        )
        .purpose("stalling relay fixture");
        let mut client =
            RelayClient::connect_with_timeout(&spec, SESSION_ID, Duration::from_millis(500))
                .await
                .expect("the fixture answers hello immediately");

        let timed_out = client
            .status()
            .await
            .expect_err("the stalled status call must time out");
        assert!(
            format!("{timed_out:#}").contains("relay status timed out"),
            "{timed_out:#}"
        );
        // A busy worker that misses one deadline is not a dead transport: it
        // answered the handshake, and killing it would be worse than waiting.
        assert!(!RelayTransportDead::marks(&timed_out), "{timed_out:#}");

        // The abandoned reply is still in flight. A later call must not read it
        // as its own response, so it fails at once with the real cause. The
        // compaction deadline is minutes long: without this the controller
        // would block on someone else's reply.
        let started = std::time::Instant::now();
        let compaction = client
            .compact("summarize".into())
            .await
            .expect_err("a call on an abandoned connection must fail");
        let elapsed = started.elapsed();
        assert!(
            format!("{compaction:#}").contains("relay connection abandoned after status timed out"),
            "{compaction:#}"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "an abandoned connection must fail fast, took {elapsed:?}"
        );

        let repeated = client
            .status()
            .await
            .expect_err("the connection stays abandoned");
        assert!(
            format!("{repeated:#}").contains("relay connection abandoned after status timed out"),
            "{repeated:#}"
        );
    }

    #[test]
    fn an_unsupported_method_answer_still_reads_as_missing_skills_sync() {
        // Workers that predate skills sync answer the unknown method with an
        // `InvalidRequest` rejection, and so does a current worker's structured
        // unsupported-method response. Both must skip the session quietly.
        let response = hel::hel_worker::unsupported_relay_method_response(
            "relay-1".into(),
            RELAY_PROTOCOL_VERSION,
            "skills_state".into(),
        );
        let encoded = serde_json::to_string(&response).unwrap();
        let error = decode_relay_response(&encoded, "relay-1", RELAY_PROTOCOL_VERSION).unwrap_err();
        assert!(sync_method_unsupported(&error), "{error:#}");
    }

    #[tokio::test]
    async fn publishing_new_targets_starts_reconciliation_without_waiting_for_the_tick() {
        let profile = tempfile::tempdir().unwrap();
        let mut coordinator = CredentialSyncCoordinator::spawn();
        coordinator.handle().set_targets(vec![CredentialSyncTarget {
            session_id: SESSION_ID.into(),
            profile_id: "work".into(),
            harness: hel::hel_config::HarnessKind::Codex,
            profile_home: profile.path().to_path_buf(),
            sync_github_token: false,
            spec: CommandSpec::new("sh", ["-c", "exit 1"]),
        }]);

        let result = tokio::time::timeout(Duration::from_secs(5), coordinator.result())
            .await
            .expect("target publication must not wait for the 60-second periodic tick")
            .expect("credential coordinator stopped");
        assert_eq!(result.profile_id, "work");
        assert_eq!(result.outcomes.len(), 1);
        assert!(result.outcomes[0].outcome.is_err());
    }

    #[tokio::test]
    async fn response_frame_limit_is_enforced_before_newline() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = tokio::spawn(async move {
            writer.write_all(b"123456789\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);

        let error = read_bounded_frame_with_limit(&mut reader, 8, ExchangeKind::Call)
            .await
            .unwrap_err();

        write.await.unwrap();
        assert!(error.to_string().contains("frame is too large"));
        // An oversized frame is a protocol violation, not a dead transport:
        // the same worker would send the same frame after a restart.
        assert!(!RelayTransportDead::marks(&error), "{error:#}");
    }

    #[tokio::test]
    async fn a_half_written_response_frame_reports_a_dead_transport() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"{\"partial\":").await.unwrap();
        drop(writer);
        let mut reader = BufReader::new(reader);

        let error = read_bounded_frame(&mut reader, ExchangeKind::Call)
            .await
            .unwrap_err();

        assert!(RelayTransportDead::marks(&error), "{error:#}");
        assert!(!RelayTransportDead::marks_failed_handshake(&error));
    }

    #[test]
    fn catch_up_page_stops_at_the_frontier_captured_before_stream_growth() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
        for message in ["one", "two", "arrived concurrently"] {
            relay
                .record_observation(RelayObservation::Warning {
                    message: message.into(),
                })
                .unwrap();
        }
        let all = relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
        let previous = RelayCursor {
            ordinal: all[0].ordinal,
            digest: all[0].digest.clone(),
        };
        let frontier = RelayCursor {
            ordinal: all[1].ordinal,
            digest: all[1].digest.clone(),
        };
        let page = RelayAttachment {
            state: relay.operational_state(),
            events: all[1..].to_vec(),
            through_ordinal: all[2].ordinal,
            through_digest: all[2].digest.clone(),
        };
        let clipped = clip_catch_up_page(page, &previous, &frontier).unwrap();
        assert_eq!(clipped.through_ordinal, frontier.ordinal);
        assert_eq!(clipped.through_digest, frontier.digest);
        assert_eq!(clipped.events.len(), 1);
        assert_eq!(clipped.events.last().unwrap().ordinal, 2);
    }
}
