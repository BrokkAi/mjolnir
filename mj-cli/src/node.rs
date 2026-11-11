//! `mj node`: the process on this computer that runs the daemon's local work.
//!
//! The daemon may run on this computer or in a container, so it does not run
//! commands, git or harness workers here itself; it asks the node, over a
//! connection the node opens (`mj_core::node`). One node runs per instance on
//! a computer. The launcher that starts the daemon writes the daemon's
//! endpoint to `node-endpoint.json` and starts the node; the node redials
//! whenever the connection drops, and steps aside when a launcher of another
//! build has taken over.

use anyhow::{Context, Result, bail};
use mj_client::daemon::{
    DaemonAction, DaemonReply, PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope, read_frame,
    write_frame,
};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Where the node finds the daemon. Written by the launcher before it starts
/// the daemon, so the node can connect while the daemon is still starting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeEndpoint {
    pub(crate) address: SocketAddr,
    pub(crate) token: String,
    /// The build of the launcher that wrote this file, and so of the node it
    /// wants serving.
    pub(crate) build_version: String,
}

pub(crate) fn endpoint_path() -> PathBuf {
    mj_core::config::data_dir().join("node-endpoint.json")
}

fn lock_path() -> PathBuf {
    mj_core::config::data_dir().join("node.lock")
}

fn id_path() -> PathBuf {
    mj_core::config::data_dir().join("node-id")
}

/// Publish where the daemon will listen, for this build's node.
pub(crate) fn write_endpoint(endpoint: &NodeEndpoint) -> Result<()> {
    let path = endpoint_path();
    if let Some(parent) = path.parent() {
        mj_core::config::create_owner_only_directory(parent)?;
    }
    mj_core::config::atomic_write(&path, &serde_json::to_vec_pretty(endpoint)?)
}

/// The daemon was stopped on purpose; withdrawing its endpoint tells the node
/// to exit instead of waiting for a daemon to return.
pub(crate) fn withdraw_endpoint() {
    match std::fs::remove_file(endpoint_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not withdraw the node endpoint"),
    }
}

fn read_endpoint() -> Result<Option<NodeEndpoint>> {
    match std::fs::read(endpoint_path()) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("parse the node endpoint")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("read the node endpoint"),
    }
}

/// How long a new node waits for the previous one to step aside.
const LOCK_WAIT: Duration = Duration::from_secs(60);
const FIRST_RETRY: Duration = Duration::from_millis(250);
const MAX_RETRY: Duration = Duration::from_secs(5);

/// Start a node for this instance if none runs. A node that finds another
/// already serving exits by itself, so starting one is always safe.
pub(crate) fn spawn_node() -> Result<()> {
    let executable = std::env::current_exe().context("locate the mj executable")?;
    let mut command = std::process::Command::new(executable);
    command.arg("node");
    #[cfg(unix)]
    std::os::unix::process::CommandExt::arg0(&mut command, "mj");
    let log = mj_core::config::data_dir().join("node.log");
    mj_core::subprocess::spawn_detached(&mut command, &log).context("start the Mjolnir node")?;
    Ok(())
}

/// Start a node if none holds this instance's node lock. A daemon of this
/// build needs one node to run local work on this computer.
pub(crate) async fn ensure_node() {
    let checked = tokio::task::spawn_blocking(|| -> Result<bool> {
        let path = lock_path();
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            // No lock file: no node has ever run for this instance.
            return Ok(true);
        };
        Ok(match file.try_lock() {
            Ok(()) => {
                file.unlock().ok();
                true
            }
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("probe the node lock");
            }
        })
    })
    .await;
    let start = match checked {
        Ok(Ok(start)) => start,
        Ok(Err(error)) => {
            tracing::warn!(error = format!("{error:#}"), "could not check for a node");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "node check task failed");
            return;
        }
    };
    if start && let Err(error) = spawn_node() {
        tracing::warn!(error = format!("{error:#}"), "could not start the node");
    }
}

pub(crate) async fn run_node() -> Result<()> {
    let Some(_lock) = acquire_lock().await? else {
        tracing::info!("another node serves this instance");
        return Ok(());
    };
    let node_id = node_id()?;
    let own_build = mj_client::build_identity::this_build().published();
    let mut retry = FIRST_RETRY;
    loop {
        let endpoint = tokio::task::spawn_blocking(read_endpoint)
            .await
            .context("read node endpoint task failed")??;
        if let Some(endpoint) = &endpoint
            && endpoint.build_version != own_build
        {
            tracing::info!(
                launcher = %endpoint.build_version,
                node = %own_build,
                "a launcher of another build took over; this node steps aside"
            );
            return Ok(());
        }
        let Some(endpoint) = endpoint else {
            tracing::info!("the daemon was stopped; the node exits");
            return Ok(());
        };
        {
            match serve_once(&endpoint, &node_id).await {
                Ok(mj_core::node::ServeEnd::Dismissed) => {
                    tracing::info!("the daemon stopped and dismissed its node");
                    return Ok(());
                }
                Ok(mj_core::node::ServeEnd::Closed) => {
                    tracing::info!("the daemon closed the node connection");
                    retry = FIRST_RETRY;
                    continue;
                }
                Err(error) => {
                    tracing::debug!(
                        error = format!("{error:#}"),
                        "node could not reach the daemon"
                    );
                }
            }
        }
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(MAX_RETRY);
    }
}

/// Connect, authenticate, and serve until the connection ends.
async fn serve_once(endpoint: &NodeEndpoint, node_id: &str) -> Result<mj_core::node::ServeEnd> {
    let mut stream =
        tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(endpoint.address))
            .await
            .context("connecting to the daemon timed out")?
            .with_context(|| format!("connect to the daemon at {}", endpoint.address))?;
    stream.set_nodelay(true).ok();
    write_frame(
        &mut stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            token: endpoint.token.clone(),
            action: DaemonAction::NodeConnect {
                node_id: node_id.to_owned(),
            },
        },
    )
    .await?;
    let response: ResponseEnvelope = read_frame(&mut stream).await?;
    match response.result {
        Ok(DaemonReply::Done) => {}
        Ok(reply) => bail!("the daemon answered the node with {reply:?}"),
        Err(error) => bail!("the daemon refused the node: {error}"),
    }
    tracing::info!(address = %endpoint.address, "serving the daemon");
    mj_core::node::serve(stream).await
}

/// This computer's node identity for this instance, kept across restarts so
/// the daemon can tell a reconnect from another computer.
fn node_id() -> Result<String> {
    let path = id_path();
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_owned()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("generate a node id: {error}"))?;
    let id = mj_core::hex::lower_hex(bytes);
    mj_core::config::atomic_write(&path, id.as_bytes())?;
    Ok(id)
}

/// The instance's node lock, waiting while a previous node steps aside.
/// `None` when another node still holds it after the wait.
async fn acquire_lock() -> Result<Option<mj_core::file_lock::FileLock>> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        let path = lock_path();
        let lock =
            tokio::task::spawn_blocking(move || -> Result<Option<mj_core::file_lock::FileLock>> {
                if let Some(parent) = path.parent() {
                    mj_core::config::create_owner_only_directory(parent)?;
                }
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                match file.try_lock() {
                    Ok(()) => Ok(Some(mj_core::file_lock::FileLock::new(file))),
                    Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                    Err(std::fs::TryLockError::Error(error)) => {
                        Err(error).context("lock the node lock file")
                    }
                }
            })
            .await
            .context("node lock task failed")??;
        if lock.is_some() || Instant::now() >= deadline {
            return Ok(lock);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
