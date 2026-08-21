//! Authenticated IPC boundary for the Mjolnir Computer host.
//!
//! The terminal-facing `mj` process keeps the model/session policy. The
//! separate app host keeps macOS Screen Recording and Accessibility grants.
//! This module defines the small protocol between them and enforces a random,
//! session-scoped capability before a host backend can observe or inject input.

use std::{fmt, io, path::PathBuf};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

use crate::computer::{
    BackendAction, ComputerBackend, ComputerError, ComputerPermission, HostLockState, Observation,
    ObserveArgs, PermissionReadiness,
};

/// Protocol version for the Mjolnir-to-host IPC. Bump this only for an
/// incompatible message change.
pub const HOST_PROTOCOL_VERSION: u16 = 1;

/// A host never accepts an unbounded JSON allocation from a local peer.
pub const MAX_HOST_FRAME_BYTES: usize = 6 * 1024 * 1024;

const CAPABILITY_BYTES: usize = 32;
const MAX_SESSION_ID_BYTES: usize = 512;

/// Opaque identifier for one authenticated Mjolnir ACP session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostSessionId(pub String);

impl HostSessionId {
    pub fn validate(&self) -> Result<(), HostProtocolError> {
        if self.0.is_empty() || self.0.len() > MAX_SESSION_ID_BYTES {
            return Err(HostProtocolError::InvalidSessionId);
        }
        Ok(())
    }
}

/// A random, single-session bearer capability. It is created by `mj`, written
/// to the private host launch descriptor, and invalidated on every session
/// transition/cancel/disable/shutdown boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostCapability(String);

impl HostCapability {
    pub fn generate() -> Result<Self, HostProtocolError> {
        let mut bytes = [0_u8; CAPABILITY_BYTES];
        getrandom::fill(&mut bytes)
            .map_err(|error| HostProtocolError::Random(error.to_string()))?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// The value intentionally has no public string accessor. Serialization is
    /// only for the 0600 launch descriptor and the host handshake.
    fn matches(&self, candidate: &Self) -> bool {
        let Ok(expected) = URL_SAFE_NO_PAD.decode(&self.0) else {
            return false;
        };
        let Ok(actual) = URL_SAFE_NO_PAD.decode(&candidate.0) else {
            return false;
        };
        expected.len() == CAPABILITY_BYTES
            && actual.len() == CAPABILITY_BYTES
            && expected.ct_eq(&actual).into()
    }
}

/// First request on every host connection. A later connection may authenticate
/// with the same active capability so that cancellation can interrupt an
/// in-flight operation on another connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    pub protocol_version: u16,
    pub session_id: HostSessionId,
    pub capability: HostCapability,
}

/// Private, mode-0600 launch input from Mjolnir to the app bundle. The random
/// capability never appears in command-line arguments or a reusable service
/// configuration; the host deletes this file as soon as it has read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLaunchDescriptor {
    pub socket_path: PathBuf,
    pub session_id: HostSessionId,
    pub capability: HostCapability,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum HostRequest {
    Hello(HostHello),
    PermissionReadiness,
    RequestPermission(ComputerPermission),
    HostLockState,
    Observe(ObserveArgs),
    Execute(BackendAction),
    /// Revokes the entire active capability, including work running on another
    /// authenticated connection, then asks the host process to exit.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum HostResponse {
    Hello { protocol_version: u16 },
    PermissionReadiness(PermissionReadiness),
    PermissionRequested(PermissionReadiness),
    HostLockState(HostLockState),
    Observation(Observation),
    Completed,
    Error(HostCallError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum HostCallError {
    AuthenticationRequired,
    Unauthorized,
    UnsupportedProtocol { expected: u16, received: u16 },
    Revoked,
    Backend(String),
}

/// Length-delimited envelope so concurrent client calls can correlate a reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostRequestEnvelope {
    pub id: u64,
    pub request: HostRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostResponseEnvelope {
    pub id: u64,
    pub response: HostResponse,
}

#[derive(Debug)]
enum CapabilityState {
    AwaitingAuthentication,
    Active { cancellation: CancellationToken },
    Revoked,
}

/// A host-owned gate around a platform backend. It has no model policy: the
/// caller must already have made approval and observation-target decisions.
/// It exists solely to ensure the host never performs an operation for a peer
/// outside the live Mjolnir session.
pub struct HostService<B> {
    backend: B,
    session_id: HostSessionId,
    capability: HostCapability,
    state: Mutex<CapabilityState>,
    revoked: CancellationToken,
    authenticated: CancellationToken,
}

impl<B> HostService<B> {
    pub fn new(
        backend: B,
        session_id: HostSessionId,
        capability: HostCapability,
    ) -> Result<Self, HostProtocolError> {
        session_id.validate()?;
        Ok(Self {
            backend,
            session_id,
            capability,
            state: Mutex::new(CapabilityState::AwaitingAuthentication),
            revoked: CancellationToken::new(),
            authenticated: CancellationToken::new(),
        })
    }

    /// Cancels in-flight backend work and prevents all future requests. This is
    /// intentionally idempotent because session teardown has several routes.
    pub async fn revoke(&self) {
        let mut state = self.state.lock().await;
        match &*state {
            CapabilityState::Active { cancellation } => cancellation.cancel(),
            CapabilityState::AwaitingAuthentication | CapabilityState::Revoked => {}
        }
        *state = CapabilityState::Revoked;
        self.revoked.cancel();
    }

    /// A host process uses this token to stop accepting connections as soon as
    /// the session is revoked, even while a different connection is handling
    /// an in-flight operation.
    pub fn revocation_token(&self) -> CancellationToken {
        self.revoked.clone()
    }

    /// A host process uses this token to enforce a short startup timeout. It
    /// is cancelled after the first successful capability handshake.
    pub fn authentication_token(&self) -> CancellationToken {
        self.authenticated.clone()
    }

    async fn authenticate(&self, hello: HostHello) -> Result<(), HostCallError> {
        if hello.protocol_version != HOST_PROTOCOL_VERSION {
            return Err(HostCallError::UnsupportedProtocol {
                expected: HOST_PROTOCOL_VERSION,
                received: hello.protocol_version,
            });
        }
        if hello.session_id != self.session_id || !self.capability.matches(&hello.capability) {
            // Deliberately avoid revealing which credential was wrong.
            return Err(HostCallError::Unauthorized);
        }
        let mut state = self.state.lock().await;
        match &*state {
            CapabilityState::AwaitingAuthentication => {
                *state = CapabilityState::Active {
                    cancellation: CancellationToken::new(),
                };
                self.authenticated.cancel();
                Ok(())
            }
            CapabilityState::Active { .. } => Ok(()),
            CapabilityState::Revoked => Err(HostCallError::Revoked),
        }
    }

    async fn active_cancellation(&self) -> Result<CancellationToken, HostCallError> {
        let state = self.state.lock().await;
        match &*state {
            CapabilityState::AwaitingAuthentication => Err(HostCallError::AuthenticationRequired),
            CapabilityState::Active { cancellation } => Ok(cancellation.clone()),
            CapabilityState::Revoked => Err(HostCallError::Revoked),
        }
    }

    async fn completed_while_active(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), HostCallError> {
        if cancellation.is_cancelled() {
            Err(HostCallError::Revoked)
        } else {
            Ok(())
        }
    }
}

impl<B: ComputerBackend> HostService<B> {
    /// Applies one authenticated IPC request. Backends receive the shared
    /// cancellation token, so a `Shutdown` from another connection interrupts
    /// capture and compound input immediately.
    pub async fn handle(&self, request: HostRequest) -> HostResponse {
        match request {
            HostRequest::Hello(hello) => match self.authenticate(hello).await {
                Ok(()) => HostResponse::Hello {
                    protocol_version: HOST_PROTOCOL_VERSION,
                },
                Err(error) => HostResponse::Error(error),
            },
            HostRequest::PermissionReadiness => {
                let cancellation = match self.active_cancellation().await {
                    Ok(cancellation) => cancellation,
                    Err(error) => return HostResponse::Error(error),
                };
                match self
                    .backend
                    .permission_readiness(cancellation.clone())
                    .await
                {
                    Ok(readiness) => match self.completed_while_active(&cancellation).await {
                        Ok(()) => HostResponse::PermissionReadiness(readiness),
                        Err(error) => HostResponse::Error(error),
                    },
                    Err(error) => HostResponse::Error(backend_error(error)),
                }
            }
            HostRequest::RequestPermission(permission) => {
                let cancellation = match self.active_cancellation().await {
                    Ok(cancellation) => cancellation,
                    Err(error) => return HostResponse::Error(error),
                };
                match self
                    .backend
                    .request_permission(permission, cancellation.clone())
                    .await
                {
                    Ok(readiness) => match self.completed_while_active(&cancellation).await {
                        Ok(()) => HostResponse::PermissionRequested(readiness),
                        Err(error) => HostResponse::Error(error),
                    },
                    Err(error) => HostResponse::Error(backend_error(error)),
                }
            }
            HostRequest::HostLockState => {
                let cancellation = match self.active_cancellation().await {
                    Ok(cancellation) => cancellation,
                    Err(error) => return HostResponse::Error(error),
                };
                match self.backend.host_lock_state(cancellation.clone()).await {
                    Ok(lock_state) => match self.completed_while_active(&cancellation).await {
                        Ok(()) => HostResponse::HostLockState(lock_state),
                        Err(error) => HostResponse::Error(error),
                    },
                    Err(error) => HostResponse::Error(backend_error(error)),
                }
            }
            HostRequest::Observe(args) => {
                let cancellation = match self.active_cancellation().await {
                    Ok(cancellation) => cancellation,
                    Err(error) => return HostResponse::Error(error),
                };
                match self.backend.observe(args, cancellation.clone()).await {
                    Ok(observation) => match self.completed_while_active(&cancellation).await {
                        Ok(()) => HostResponse::Observation(observation),
                        Err(error) => HostResponse::Error(error),
                    },
                    Err(error) => HostResponse::Error(backend_error(error)),
                }
            }
            HostRequest::Execute(action) => {
                let cancellation = match self.active_cancellation().await {
                    Ok(cancellation) => cancellation,
                    Err(error) => return HostResponse::Error(error),
                };
                match self.backend.execute(action, cancellation.clone()).await {
                    Ok(()) => match self.completed_while_active(&cancellation).await {
                        Ok(()) => HostResponse::Completed,
                        Err(error) => HostResponse::Error(error),
                    },
                    Err(error) => HostResponse::Error(backend_error(error)),
                }
            }
            HostRequest::Shutdown => {
                if let Err(error) = self.active_cancellation().await {
                    return HostResponse::Error(error);
                }
                self.revoke().await;
                HostResponse::Completed
            }
        }
    }
}

fn backend_error(error: ComputerError) -> HostCallError {
    if error == ComputerError::Cancelled {
        HostCallError::Revoked
    } else {
        HostCallError::Backend(error.to_string())
    }
}

/// Serves a framed control connection. The caller owns listener lifecycle and
/// may run a second authenticated connection solely to send `shutdown` while
/// a long operation runs on this one. A control connection ending without an
/// explicit shutdown revokes the whole session: a disconnected `mj` process
/// must never leave a usable automation host behind.
pub async fn serve_connection<B, S>(
    service: &HostService<B>,
    stream: &mut S,
) -> Result<(), HostWireError>
where
    B: ComputerBackend,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut authenticated = false;
    loop {
        let request = match read_frame::<_, HostRequestEnvelope>(&mut reader).await {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(error) => {
                if authenticated {
                    service.revoke().await;
                }
                return Err(error);
            }
        };
        if !authenticated && !matches!(request.request, HostRequest::Hello(_)) {
            write_frame(
                &mut writer,
                &HostResponseEnvelope {
                    id: request.id,
                    response: HostResponse::Error(HostCallError::AuthenticationRequired),
                },
            )
            .await?;
            continue;
        }

        let shutdown = matches!(request.request, HostRequest::Shutdown);
        let response = if authenticated && !shutdown {
            // Continue reading while a host operation runs. A disconnected
            // control peer is itself a revocation boundary, so no native input
            // may continue merely because the backend is still busy.
            tokio::select! {
                response = service.handle(request.request) => response,
                inbound = read_frame::<_, HostRequestEnvelope>(&mut reader) => {
                    service.revoke().await;
                    match inbound {
                        Ok(None) | Ok(Some(_)) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                }
            }
        } else {
            service.handle(request.request).await
        };
        authenticated = authenticated || matches!(response, HostResponse::Hello { .. });
        if let Err(error) = write_frame(
            &mut writer,
            &HostResponseEnvelope {
                id: request.id,
                response,
            },
        )
        .await
        {
            if authenticated {
                service.revoke().await;
            }
            return Err(error);
        }
        if shutdown {
            return Ok(());
        }
    }
    if authenticated {
        service.revoke().await;
    }
    Ok(())
}

/// One persistent client connection from Mjolnir to its per-session host.
/// Dropping it revokes the host capability; use a separate short-lived client
/// only for the cancellation path while a call is in flight.
pub struct HostClient<S> {
    stream: S,
    next_id: u64,
}

impl<S> HostClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub async fn authenticate(
        mut stream: S,
        session_id: HostSessionId,
        capability: HostCapability,
    ) -> Result<Self, HostClientError> {
        let hello = HostRequestEnvelope {
            id: 1,
            request: HostRequest::Hello(HostHello {
                protocol_version: HOST_PROTOCOL_VERSION,
                session_id,
                capability,
            }),
        };
        write_frame(&mut stream, &hello)
            .await
            .map_err(HostClientError::Wire)?;
        let response: HostResponseEnvelope = read_frame(&mut stream)
            .await
            .map_err(HostClientError::Wire)?
            .ok_or(HostClientError::Disconnected)?;
        if response.id != hello.id {
            return Err(HostClientError::UnexpectedResponseId {
                expected: hello.id,
                received: response.id,
            });
        }
        match response.response {
            HostResponse::Hello { protocol_version }
                if protocol_version == HOST_PROTOCOL_VERSION =>
            {
                Ok(Self {
                    stream,
                    next_id: hello.id + 1,
                })
            }
            HostResponse::Hello { protocol_version } => Err(HostClientError::ProtocolMismatch {
                expected: HOST_PROTOCOL_VERSION,
                received: protocol_version,
            }),
            HostResponse::Error(error) => Err(HostClientError::Host(error)),
            response => Err(HostClientError::UnexpectedResponse(response_name(
                &response,
            ))),
        }
    }

    pub async fn call(&mut self, request: HostRequest) -> Result<HostResponse, HostClientError> {
        if matches!(request, HostRequest::Hello(_)) {
            return Err(HostClientError::HelloAfterAuthentication);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(HostClientError::RequestIdExhausted)?;
        write_frame(&mut self.stream, &HostRequestEnvelope { id, request })
            .await
            .map_err(HostClientError::Wire)?;
        let response: HostResponseEnvelope = read_frame(&mut self.stream)
            .await
            .map_err(HostClientError::Wire)?
            .ok_or(HostClientError::Disconnected)?;
        if response.id != id {
            return Err(HostClientError::UnexpectedResponseId {
                expected: id,
                received: response.id,
            });
        }
        match response.response {
            HostResponse::Error(error) => Err(HostClientError::Host(error)),
            response => Ok(response),
        }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

fn response_name(response: &HostResponse) -> &'static str {
    match response {
        HostResponse::Hello { .. } => "hello",
        HostResponse::PermissionReadiness(_) => "permission_readiness",
        HostResponse::PermissionRequested(_) => "permission_requested",
        HostResponse::HostLockState(_) => "host_lock_state",
        HostResponse::Observation(_) => "observation",
        HostResponse::Completed => "completed",
        HostResponse::Error(_) => "error",
    }
}

/// Reads one capped JSON frame. A clean EOF before a frame returns `None`.
pub async fn read_frame<S, T>(stream: &mut S) -> Result<Option<T>, HostWireError>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0_u8; 4];
    match stream.read(&mut header).await.map_err(HostWireError::Io)? {
        0 => return Ok(None),
        read => stream
            .read_exact(&mut header[read..])
            .await
            .map_err(HostWireError::Io)?,
    };
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_HOST_FRAME_BYTES {
        return Err(HostWireError::FrameTooLarge(len));
    }
    let mut bytes = vec![0_u8; len];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(HostWireError::Io)?;
    serde_json::from_slice(&bytes)
        .map_err(HostWireError::Json)
        .map(Some)
}

/// Writes one capped JSON frame and flushes it so permission/readiness state
/// reaches the setup surface without waiting for a later request.
pub async fn write_frame<S, T>(stream: &mut S, value: &T) -> Result<(), HostWireError>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(HostWireError::Json)?;
    if bytes.len() > MAX_HOST_FRAME_BYTES {
        return Err(HostWireError::FrameTooLarge(bytes.len()));
    }
    let len = u32::try_from(bytes.len()).map_err(|_| HostWireError::FrameTooLarge(bytes.len()))?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(HostWireError::Io)?;
    stream.write_all(&bytes).await.map_err(HostWireError::Io)?;
    stream.flush().await.map_err(HostWireError::Io)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostProtocolError {
    InvalidSessionId,
    Random(String),
}

impl fmt::Display for HostProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId => f.write_str("host session id must be non-empty and bounded"),
            Self::Random(error) => write!(f, "generate host capability: {error}"),
        }
    }
}

impl std::error::Error for HostProtocolError {}

#[derive(Debug)]
pub enum HostWireError {
    FrameTooLarge(usize),
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for HostWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge(size) => {
                write!(
                    f,
                    "computer host frame exceeds {MAX_HOST_FRAME_BYTES} bytes: {size}"
                )
            }
            Self::Io(error) => write!(f, "computer host IPC: {error}"),
            Self::Json(error) => write!(f, "computer host JSON: {error}"),
        }
    }
}

impl std::error::Error for HostWireError {}

#[derive(Debug)]
pub enum HostClientError {
    Wire(HostWireError),
    Disconnected,
    ProtocolMismatch { expected: u16, received: u16 },
    UnexpectedResponseId { expected: u64, received: u64 },
    UnexpectedResponse(&'static str),
    HelloAfterAuthentication,
    RequestIdExhausted,
    Host(HostCallError),
}

impl fmt::Display for HostClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "computer host connection: {error}"),
            Self::Disconnected => f.write_str("computer host closed the connection"),
            Self::ProtocolMismatch { expected, received } => write!(
                f,
                "computer host protocol mismatch: expected version {expected}, received {received}"
            ),
            Self::UnexpectedResponseId { expected, received } => write!(
                f,
                "computer host response id mismatch: expected {expected}, received {received}"
            ),
            Self::UnexpectedResponse(response) => {
                write!(f, "unexpected computer host response: {response}")
            }
            Self::HelloAfterAuthentication => {
                f.write_str("computer host hello is valid only when connecting")
            }
            Self::RequestIdExhausted => f.write_str("computer host request id exhausted"),
            Self::Host(error) => write!(f, "computer host rejected request: {error:?}"),
        }
    }
}

impl std::error::Error for HostClientError {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::{io::duplex, sync::Notify};

    use super::*;
    use crate::computer::{HostLockState, PermissionState, PixelSize, SourceRegion};

    #[derive(Default)]
    struct MockBackendState {
        execute_calls: AtomicUsize,
        execute_started: Notify,
    }

    #[derive(Clone, Default)]
    struct MockBackend {
        state: Arc<MockBackendState>,
    }

    #[async_trait::async_trait]
    impl ComputerBackend for MockBackend {
        async fn observe(
            &self,
            _request: ObserveArgs,
            _cancellation: CancellationToken,
        ) -> Result<Observation, ComputerError> {
            Ok(Observation {
                metadata: crate::computer::ObservationMetadata {
                    observation_id: crate::computer::ObservationId("observation".to_string()),
                    display_id: crate::computer::DisplayId("display".to_string()),
                    display_origin: crate::computer::DesktopPoint { x: 0, y: 0 },
                    display_pixel_size: PixelSize {
                        width: 1,
                        height: 1,
                    },
                    display_scale_x: 1.0,
                    display_scale_y: 1.0,
                    source_region: SourceRegion {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    },
                    returned_image_size: PixelSize {
                        width: 1,
                        height: 1,
                    },
                    mime_type: "image/png".to_string(),
                    created_at_unix_ms: 1,
                    expires_at_unix_ms: 2,
                },
                image: crate::computer::EncodedImage {
                    data_base64: "AA==".to_string(),
                },
            })
        }

        async fn permission_readiness(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<PermissionReadiness, ComputerError> {
            Ok(PermissionReadiness {
                screen_recording: PermissionState::Granted,
                accessibility: PermissionState::Granted,
            })
        }

        async fn host_lock_state(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<HostLockState, ComputerError> {
            Ok(HostLockState::Unlocked)
        }

        async fn execute(
            &self,
            _action: BackendAction,
            cancellation: CancellationToken,
        ) -> Result<(), ComputerError> {
            self.state.execute_calls.fetch_add(1, Ordering::Relaxed);
            self.state.execute_started.notify_waiters();
            cancellation.cancelled().await;
            Err(ComputerError::Cancelled)
        }
    }

    fn hello(session_id: &HostSessionId, capability: &HostCapability) -> HostRequest {
        HostRequest::Hello(HostHello {
            protocol_version: HOST_PROTOCOL_VERSION,
            session_id: session_id.clone(),
            capability: capability.clone(),
        })
    }

    fn action() -> BackendAction {
        BackendAction::Move { x: 12.0, y: 34.0 }
    }

    #[tokio::test]
    async fn authenticated_session_is_required_before_backend_calls() {
        let capability = HostCapability::generate().unwrap();
        let service = HostService::new(
            MockBackend::default(),
            HostSessionId("session-a".to_string()),
            capability.clone(),
        )
        .unwrap();

        assert_eq!(
            service.handle(HostRequest::PermissionReadiness).await,
            HostResponse::Error(HostCallError::AuthenticationRequired)
        );
        assert_eq!(
            service
                .handle(HostRequest::Hello(HostHello {
                    protocol_version: HOST_PROTOCOL_VERSION,
                    session_id: HostSessionId("other-session".to_string()),
                    capability,
                }))
                .await,
            HostResponse::Error(HostCallError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn bad_protocol_cannot_activate_a_capability() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let service = HostService::new(
            MockBackend::default(),
            session_id.clone(),
            capability.clone(),
        )
        .unwrap();
        assert_eq!(
            service
                .handle(HostRequest::Hello(HostHello {
                    protocol_version: HOST_PROTOCOL_VERSION + 1,
                    session_id: session_id.clone(),
                    capability: capability.clone(),
                }))
                .await,
            HostResponse::Error(HostCallError::UnsupportedProtocol {
                expected: HOST_PROTOCOL_VERSION,
                received: HOST_PROTOCOL_VERSION + 1,
            })
        );
        assert!(matches!(
            service.handle(hello(&session_id, &capability)).await,
            HostResponse::Hello { .. }
        ));
    }

    #[tokio::test]
    async fn authentication_token_is_cancelled_only_after_a_valid_hello() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let service = HostService::new(
            MockBackend::default(),
            session_id.clone(),
            capability.clone(),
        )
        .unwrap();
        let authenticated = service.authentication_token();

        assert_eq!(
            service
                .handle(HostRequest::Hello(HostHello {
                    protocol_version: HOST_PROTOCOL_VERSION,
                    session_id: HostSessionId("other-session".to_string()),
                    capability: capability.clone(),
                }))
                .await,
            HostResponse::Error(HostCallError::Unauthorized)
        );
        assert!(!authenticated.is_cancelled());
        assert!(matches!(
            service.handle(hello(&session_id, &capability)).await,
            HostResponse::Hello { .. }
        ));
        assert!(authenticated.is_cancelled());
    }

    #[tokio::test]
    async fn revocation_cancels_in_flight_action_and_blocks_replay() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let backend = MockBackend::default();
        let backend_state = backend.state.clone();
        let service =
            Arc::new(HostService::new(backend, session_id.clone(), capability.clone()).unwrap());
        assert!(matches!(
            service.handle(hello(&session_id, &capability)).await,
            HostResponse::Hello { .. }
        ));

        let action_service = service.clone();
        let action_task =
            tokio::spawn(
                async move { action_service.handle(HostRequest::Execute(action())).await },
            );
        backend_state.execute_started.notified().await;
        service.revoke().await;
        assert_eq!(
            action_task.await.unwrap(),
            HostResponse::Error(HostCallError::Revoked)
        );
        assert_eq!(backend_state.execute_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            service.handle(HostRequest::Execute(action())).await,
            HostResponse::Error(HostCallError::Revoked)
        );
        assert_eq!(
            service.handle(hello(&session_id, &capability)).await,
            HostResponse::Error(HostCallError::Revoked)
        );
    }

    #[tokio::test]
    async fn framed_connection_requires_hello_and_preserves_request_ids() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let service = Arc::new(
            HostService::new(
                MockBackend::default(),
                session_id.clone(),
                capability.clone(),
            )
            .unwrap(),
        );
        let (mut client, mut server) = duplex(8 * 1024);
        let task = tokio::spawn(async move { serve_connection(&service, &mut server).await });

        write_frame(
            &mut client,
            &HostRequestEnvelope {
                id: 7,
                request: HostRequest::PermissionReadiness,
            },
        )
        .await
        .unwrap();
        let response: HostResponseEnvelope = read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(response.id, 7);
        assert_eq!(
            response.response,
            HostResponse::Error(HostCallError::AuthenticationRequired)
        );

        write_frame(
            &mut client,
            &HostRequestEnvelope {
                id: 8,
                request: hello(&session_id, &capability),
            },
        )
        .await
        .unwrap();
        let response: HostResponseEnvelope = read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(response.id, 8);
        assert_eq!(
            response.response,
            HostResponse::Hello {
                protocol_version: HOST_PROTOCOL_VERSION
            }
        );
        drop(client);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn persistent_client_handshake_and_disconnect_revoke_the_host() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let service = Arc::new(
            HostService::new(
                MockBackend::default(),
                session_id.clone(),
                capability.clone(),
            )
            .unwrap(),
        );
        let (client_stream, mut server_stream) = duplex(8 * 1024);
        let server = {
            let service = service.clone();
            tokio::spawn(async move { serve_connection(&service, &mut server_stream).await })
        };

        let mut client = HostClient::authenticate(client_stream, session_id, capability)
            .await
            .unwrap();
        assert_eq!(
            client.call(HostRequest::PermissionReadiness).await.unwrap(),
            HostResponse::PermissionReadiness(PermissionReadiness {
                screen_recording: PermissionState::Granted,
                accessibility: PermissionState::Granted,
            })
        );
        drop(client);
        server.await.unwrap().unwrap();
        assert_eq!(
            service.handle(HostRequest::PermissionReadiness).await,
            HostResponse::Error(HostCallError::Revoked)
        );
    }

    #[tokio::test]
    async fn control_disconnect_cancels_an_in_flight_backend_action() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let backend = MockBackend::default();
        let backend_state = backend.state.clone();
        let service =
            Arc::new(HostService::new(backend, session_id.clone(), capability.clone()).unwrap());
        let (mut client, mut server_stream) = duplex(8 * 1024);
        let server = {
            let service = service.clone();
            tokio::spawn(async move { serve_connection(&service, &mut server_stream).await })
        };

        write_frame(
            &mut client,
            &HostRequestEnvelope {
                id: 1,
                request: hello(&session_id, &capability),
            },
        )
        .await
        .unwrap();
        let _: HostResponseEnvelope = read_frame(&mut client).await.unwrap().unwrap();
        write_frame(
            &mut client,
            &HostRequestEnvelope {
                id: 2,
                request: HostRequest::Execute(action()),
            },
        )
        .await
        .unwrap();
        backend_state.execute_started.notified().await;
        drop(client);
        server.await.unwrap().unwrap();
        assert_eq!(backend_state.execute_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            service.handle(HostRequest::Execute(action())).await,
            HostResponse::Error(HostCallError::Revoked)
        );
    }

    #[tokio::test]
    async fn truncated_frame_after_authentication_revokes_the_host() {
        let session_id = HostSessionId("session-a".to_string());
        let capability = HostCapability::generate().unwrap();
        let service = Arc::new(
            HostService::new(
                MockBackend::default(),
                session_id.clone(),
                capability.clone(),
            )
            .unwrap(),
        );
        let (mut client, mut server_stream) = duplex(8 * 1024);
        let server = {
            let service = service.clone();
            tokio::spawn(async move { serve_connection(&service, &mut server_stream).await })
        };

        write_frame(
            &mut client,
            &HostRequestEnvelope {
                id: 1,
                request: hello(&session_id, &capability),
            },
        )
        .await
        .unwrap();
        let _: HostResponseEnvelope = read_frame(&mut client).await.unwrap().unwrap();
        client.write_all(&2_u32.to_be_bytes()).await.unwrap();
        client.write_all(b"{").await.unwrap();
        drop(client);

        assert!(matches!(server.await.unwrap(), Err(HostWireError::Io(_))));
        assert_eq!(
            service.handle(HostRequest::PermissionReadiness).await,
            HostResponse::Error(HostCallError::Revoked)
        );
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_allocating_payload() {
        let (mut writer, mut reader) = duplex(64);
        writer
            .write_all(
                &u32::try_from(MAX_HOST_FRAME_BYTES + 1)
                    .unwrap()
                    .to_be_bytes(),
            )
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, HostRequestEnvelope>(&mut reader).await,
            Err(HostWireError::FrameTooLarge(size)) if size == MAX_HOST_FRAME_BYTES + 1
        ));
    }

    #[test]
    fn host_capability_is_fixed_size_and_session_id_is_bounded() {
        let capability = HostCapability::generate().unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(&capability.0).unwrap();
        assert_eq!(decoded.len(), CAPABILITY_BYTES);
        assert_eq!(
            HostSessionId(String::new()).validate(),
            Err(HostProtocolError::InvalidSessionId)
        );
        assert_eq!(
            HostSessionId("x".repeat(MAX_SESSION_ID_BYTES + 1)).validate(),
            Err(HostProtocolError::InvalidSessionId)
        );
    }
}
