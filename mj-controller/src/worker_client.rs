//! Controller-side client for a session relay's JSON-lines proxy.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};

use crate::targets::{
    BoundedProcessExecutor, CommandSpec, SSH_MASTER_OPEN_TIMEOUT, SSH_RETRY_ATTEMPTS, SshAdmission,
    SshPermit, SshSessionLease, is_transport_rejection,
};
use mj_core::config::harness_authentication_marker;
use mj_core::credentials::{
    CredentialSnapshot, CredentialSyncAction, CredentialSyncHandle, CredentialSyncOutcome,
    CredentialSyncResult, CredentialSyncTarget, SYNC_INTERVAL, SyncAction, SyncTrigger, enqueue,
    profiles_with_targets, read_credential_file, reconcile, validate_credential_payload,
    write_credential_file,
};
use mj_core::elicitation::ElicitationResponse;
use mj_core::relay::{
    MAX_FRAME_BYTES, RELAY_EVENT_GENESIS_DIGEST, RELAY_MIN_PROTOCOL_VERSION,
    RELAY_PROTOCOL_VERSION, RelayCommand, RelayCursor, RelayErrorCode, RelayEvent,
    RelayOperationalState, RelayProtocolError, RelayRequest, RelayRequestEnvelope,
    RelayResponseBody, RelayResponseEnvelope, RelayResponsePayload, RelayVersionRange,
    ReviewerRequest, validate_relay_event,
};

pub use mj_client::session::{RelayAttachment, StartedReviewer};
use mj_core::worker_launch::ReviewerLaunchConfig;

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
/// Capturing a review delta runs Git over every workspace repository, which is
/// filesystem work on a possibly large tree rather than relay bookkeeping.
const REVIEW_CAPTURE_TIMEOUT: Duration = Duration::from_secs(300);
/// Bifrost's semantic diff analysis has its own 600-second budget inside the
/// worker; this leaves room for it to report a timeout as an error rather than
/// having the call time out underneath it.
const REVIEW_ANALYSIS_TIMEOUT: Duration = Duration::from_secs(660);
const RELAY_PROXY_DETACH_GRACE: Duration = Duration::from_millis(500);
const RELAY_PROXY_REAP_POLL: Duration = Duration::from_millis(10);

/// How many trailing stderr lines a failed connect reports back to its caller.
const RELAY_PROXY_STDERR_TAIL: usize = 10;

/// The proxy's last [`RELAY_PROXY_STDERR_TAIL`] non-empty stderr lines, shared
/// with whoever has to report them.
///
/// The drain publishes each line here as it reads it, rather than returning
/// the whole tail when it finishes. A failed connect has to bound how long it
/// waits for the drain, because a proxy that leaves a grandchild holding
/// stderr never reaches EOF. Reading the tail from here means that bound costs
/// only the lines not yet read, instead of discarding every line already
/// collected.
type ProxyStderrTail = Arc<std::sync::Mutex<VecDeque<String>>>;

/// Forward a relay proxy's stderr to the log, one line at a time, until the
/// child closes it, keeping the tail in `tail`. Reporting rather than dropping
/// keeps connect failures diagnosable now that the controller no longer shares
/// its terminal, and lets a failed connect put the proxy's own complaint in
/// the error the caller sees rather than only in the log.
async fn drain_proxy_stderr(
    errors: tokio::process::ChildStderr,
    purpose: String,
    session_id: String,
    tail: ProxyStderrTail,
) {
    let mut lines = BufReader::new(errors).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => {
                tracing::warn!(%session_id, %purpose, %line, "relay proxy stderr");
                let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
                if tail.len() == RELAY_PROXY_STDERR_TAIL {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%session_id, %purpose, %error, "read relay proxy stderr");
                return;
            }
        }
    }
}

mod errors;
pub use errors::*;
mod connect;
mod exchange;
mod relay;
mod reviewer;
mod transport;
use transport::*;
mod credential_sync;
pub use credential_sync::*;

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
    /// The shared-connection session the proxy runs on. Unlike the admission
    /// permit, which is released once hello completes, the session is in use
    /// for as long as the proxy runs, so the lease lives as long as `child`.
    ssh_session: Option<SshSessionLease>,
}

#[cfg(test)]
mod tests;
