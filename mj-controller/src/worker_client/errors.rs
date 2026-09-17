use super::*;

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
    pub(super) message: String,
    pub(super) handshake_failed: bool,
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
    pub(super) fn from_io(error: std::io::Error, kind: ExchangeKind) -> Self {
        Self::during_exchange(error.to_string(), kind)
    }

    pub(super) fn during_exchange(message: impl Into<String>, kind: ExchangeKind) -> Self {
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
pub(super) enum ExchangeKind {
    Handshake,
    Call,
}

/// Why one relay proxy launch failed, and whether the SSH server refused the
/// connection before authentication rather than the worker being unreachable.
pub(super) struct ConnectFailure {
    pub(super) error: anyhow::Error,
    pub(super) transport_rejected: bool,
}

impl ConnectFailure {
    pub(super) fn plain(error: anyhow::Error) -> Self {
        Self {
            error,
            transport_rejected: false,
        }
    }
}

impl From<anyhow::Error> for ConnectFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::plain(error)
    }
}
