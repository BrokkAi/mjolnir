//! Which node serves this daemon's local work.
//!
//! A node (`mj node`) connects to the daemon and turns its connection into the
//! node protocol (`mj_core::node`). This registry is the one owner of the
//! answer to "which node runs this daemon's commands": it publishes the
//! connected node into the slot that the daemon's command host reads, and
//! clears it when that connection ends. A newer connection replaces an older
//! one, so a node that redials after a network blip, or a node of a newer
//! build, takes over without a second answer existing in between.

use anyhow::Result;
use mj_core::node::{NodeClient, NodeCommandHost, NodeSlot};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tokio::net::TcpStream;

pub struct NodeRegistry {
    slot: NodeSlot,
    current: Mutex<Option<Connection>>,
    /// Set when this daemon stops for good rather than handing off.
    dismiss_on_exit: std::sync::atomic::AtomicBool,
}

struct Connection {
    generation: u64,
    node_id: String,
    driver: tokio::task::AbortHandle,
}

static REGISTRY: OnceLock<Arc<NodeRegistry>> = OnceLock::new();

/// This process's registry.
pub fn registry() -> &'static Arc<NodeRegistry> {
    REGISTRY.get_or_init(|| {
        Arc::new(NodeRegistry {
            slot: tokio::sync::watch::channel(None).0,
            current: Mutex::new(None),
            dismiss_on_exit: std::sync::atomic::AtomicBool::new(false),
        })
    })
}

/// Route every command this daemon process runs to its node. Called once by
/// the `mj daemon-run` process before it runs any command; daemons that tests
/// run inside their own process keep running commands locally.
pub fn install_command_host() -> Result<()> {
    let host = NodeCommandHost::new(
        registry().slot.subscribe(),
        tokio::runtime::Handle::current(),
    );
    mj_core::targets::install_command_host(Arc::new(host))
}

impl NodeRegistry {
    /// Take over `stream`, which has just authenticated as node `node_id`, and
    /// make it the node this daemon uses.
    pub async fn adopt(&self, stream: TcpStream, node_id: String) -> Result<()> {
        let (client, driver) = NodeClient::handshake(stream).await?;
        let generation = {
            static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
        };
        let registry = registry().clone();
        let ended_node = node_id.clone();
        let task = tokio::spawn(async move {
            let result = driver.await;
            registry.connection_ended(generation, &ended_node, result);
        });
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = current.take() {
            tracing::info!(
                previous = %previous.node_id,
                node = %node_id,
                "a newer node connection replaces the previous one"
            );
            previous.driver.abort();
        } else {
            tracing::info!(node = %node_id, "a node connected");
        }
        *current = Some(Connection {
            generation,
            node_id,
            driver: task.abort_handle(),
        });
        // Published under the same lock that names the connection, so the slot
        // and `current` never describe different nodes.
        self.slot.send_replace(Some(client));
        Ok(())
    }

    fn connection_ended(&self, generation: u64, node_id: &str, result: Result<()>) {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if current
            .as_ref()
            .is_some_and(|connection| connection.generation == generation)
        {
            *current = None;
            self.slot.send_replace(None);
            match result {
                Ok(()) => tracing::info!(node = %node_id, "the node disconnected"),
                Err(error) => tracing::warn!(
                    node = %node_id,
                    error = %format!("{error:#}"),
                    "the node connection failed"
                ),
            }
        }
    }

    /// The id of the connected node, if one is connected.
    pub fn connected_node(&self) -> Option<String> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|connection| connection.node_id.clone())
    }

    /// This daemon is stopping for good (an explicit stop or idle exit, not a
    /// handoff), so its node should exit with it.
    pub fn dismiss_node_on_exit(&self) {
        self.dismiss_on_exit
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// At daemon exit: dismiss the node if this daemon stopped for good, and
    /// wait briefly for it to go, so no node outlives a stopped daemon.
    pub async fn finish(&self) {
        if !self
            .dismiss_on_exit
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let Some(client) = self.slot.borrow().clone() else {
            return;
        };
        let bound = std::time::Duration::from_secs(2);
        match tokio::time::timeout(bound, client.dismiss()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(error = format!("{error:#}"), "could not dismiss the node");
                return;
            }
            Err(_) => {
                tracing::warn!("the node did not acknowledge its dismissal in time");
                return;
            }
        }
        let mut slot = self.slot.subscribe();
        if tokio::time::timeout(bound, slot.wait_for(Option::is_none))
            .await
            .is_err()
        {
            tracing::warn!("the dismissed node kept its connection open");
        }
    }
}
