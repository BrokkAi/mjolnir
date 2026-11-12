//! Process-owned daemon transport. Remote credentials never enter local metadata.
use super::{
    DaemonClient, DaemonMetadata,
    ssh::{self, DiscoveryIntent, SshLease},
};
use anyhow::{Context, Result};
use mj_core::config::{DaemonConfig, DaemonRuntime};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

pub struct DaemonConnector {
    placement: DaemonConfig,
    lease: Mutex<Option<Arc<SshLease>>>,
}

static CONNECTOR: Mutex<Weak<DaemonConnector>> = Mutex::const_new(Weak::new());

impl DaemonConnector {
    #[cfg(test)]
    pub(super) fn for_placement(placement: DaemonConfig) -> Arc<Self> {
        Arc::new(Self {
            placement,
            lease: Mutex::new(None),
        })
    }

    pub async fn shared() -> Result<Arc<Self>> {
        let mut shared = CONNECTOR.lock().await;
        if let Some(connector) = shared.upgrade() {
            return Ok(connector);
        }
        let placement = tokio::task::spawn_blocking(|| {
            DaemonConfig::load_placement(&mj_core::config::config_path())
        })
        .await
        .context("load daemon placement task")??;
        let connector = Arc::new(Self {
            placement,
            lease: Mutex::new(None),
        });
        *shared = Arc::downgrade(&connector);
        Ok(connector)
    }

    pub fn is_ssh(&self) -> bool {
        self.placement.runtime == DaemonRuntime::Ssh
    }

    /// Validate a browser/HTTP destination against the transport's machine origin.
    pub async fn validate_viewer_url(&self, viewer_url: &str) -> Result<()> {
        if !self.is_ssh() {
            return Ok(());
        }
        let url = url::Url::parse(viewer_url)?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
            Some(url::Host::Domain(host)) => {
                host.eq_ignore_ascii_case("localhost")
                    || host.to_ascii_lowercase().ends_with(".localhost")
            }
            None => false,
        };
        if loopback {
            let wildcard = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_unspecified())
                || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_unspecified());
            let detail = if wildcard {
                " the advertised address is a wildcard; [phone] bind must name a reachable address or use Tailscale;"
            } else {
                ""
            };
            let lease = self.lease.lock().await;
            let path = lease
                .as_ref()
                .and_then(|lease| lease.endpoint.config_path.as_ref())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "the remote config.toml".into());
            anyhow::bail!(
                "the viewer is unreachable from this computer;{detail} on {} enable [phone] tailscale_detect with working Tailscale, or set [phone] bind to a reachable address with tls_cert and tls_key; remote config: {path}",
                self.placement.host.as_deref().unwrap_or("the SSH host")
            );
        }
        Ok(())
    }

    /// The CLI supplies local lifecycle policy without a client dependency on it.
    pub async fn connect_or_start<F, Fut>(self: &Arc<Self>, local: F) -> Result<DaemonClient>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<DaemonClient>>,
    {
        if self.is_ssh() {
            self.remote(DiscoveryIntent::Start, None).await
        } else {
            local().await
        }
    }

    pub async fn connect_existing(self: &Arc<Self>) -> Result<DaemonClient> {
        if self.is_ssh() {
            return self.remote(DiscoveryIntent::Existing, None).await;
        }
        let metadata = tokio::task::spawn_blocking(super::read_metadata)
            .await
            .context("read daemon metadata task failed")??;
        DaemonClient::connect(metadata).await
    }

    pub(super) async fn refresh(
        self: &Arc<Self>,
        previous: &DaemonMetadata,
    ) -> Result<DaemonClient> {
        if self.is_ssh() {
            self.remote(DiscoveryIntent::Start, Some(previous.address))
                .await
        } else {
            self.connect_existing().await
        }
    }

    async fn remote(
        self: &Arc<Self>,
        intent: DiscoveryIntent,
        stale: Option<std::net::SocketAddr>,
    ) -> Result<DaemonClient> {
        // Hold the decision point through retirement and authenticated replacement.
        // Callers holding an older endpoint reuse a replacement already published.
        let mut lease = self.lease.lock().await;
        if let Some(current) = lease.as_ref() {
            let metadata = current.metadata();
            if stale != Some(metadata.address)
                && let Ok(mut client) = DaemonClient::connect(metadata).await
                && matches!(
                    client.request_once(super::DaemonAction::Ping).await,
                    Ok(super::DaemonReply::Pong)
                )
            {
                client.connector = Some(self.clone());
                client.ssh_lease = Some(current.clone());
                return Ok(client);
            }
        }
        let instance = self
            .placement
            .instance
            .clone()
            .or_else(mj_core::config::instance_name)
            .unwrap_or_else(|| "default".into());
        let replacement =
            ssh::open(&self.placement, &instance, intent, std::future::pending()).await?;
        let metadata = replacement.metadata();
        let replacement = Arc::new(replacement);
        *lease = Some(replacement.clone());
        let mut client = DaemonClient::connect(metadata).await?;
        client.connector = Some(self.clone());
        client.ssh_lease = Some(replacement);
        Ok(client)
    }
}
