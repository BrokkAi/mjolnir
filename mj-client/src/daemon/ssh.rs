//! SSH endpoint discovery, independent of CLI startup and local schema stores.
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::DaemonConfig;
use mj_core::targets::daemon_login_command;

use super::DaemonEndpoint;
use crate::build_identity::{BuildIdentity, this_build};

/// Whether discovery may start the remote daemon.
#[derive(Clone, Copy)]
pub enum DiscoveryIntent {
    Start,
    Existing,
}

/// Discover process-private remote credentials. No client-side daemon or store
/// is consulted. Dropping this future cancels the SSH process group.
pub async fn discover(
    placement: &DaemonConfig,
    remote_instance: &str,
    intent: DiscoveryIntent,
) -> Result<DaemonEndpoint> {
    let host = placement
        .host
        .as_deref()
        .context("[daemon] host is required for SSH placement")?;
    ensure!(
        !host.starts_with('-'),
        "[daemon] host cannot be an SSH option"
    );
    let mut args = vec![
        placement.remote_command().to_owned(),
        "--instance".into(),
        remote_instance.into(),
        "daemon".into(),
        "endpoint".into(),
        "--json".into(),
    ];
    if matches!(intent, DiscoveryIntent::Existing) {
        args.push("--existing".into());
    }
    let mut command = tokio::process::Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .args(&placement.ssh_args)
        .arg(host)
        .arg(daemon_login_command(&args));
    let output = mj_core::subprocess::capture(&mut command)
        .await
        .context("discover SSH daemon")?;
    if output.status.code() == Some(127) {
        bail!(
            "[daemon] remote_command {:?} was not found on {host}; install mj remotely with scripts/install.sh (or install the same release). SSH: {}",
            placement.remote_command(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    ensure!(
        output.status.success(),
        "SSH daemon discovery failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let endpoint: DaemonEndpoint =
        serde_json::from_slice(&output.stdout).context("parse SSH daemon endpoint JSON")?;
    let remote = BuildIdentity::parse(&endpoint.build_version)?;
    ensure!(
        this_build().same_source(&remote),
        "SSH daemon build {} differs from client build {}; run scripts/dev-remote.ps1 or install the same release on both machines",
        remote.describe(),
        this_build().describe()
    );
    Ok(endpoint)
}

/// Process-private transport ownership. Dropping the lease stops and reaps SSH;
/// it never stops the remote daemon.
pub struct SshLease {
    pub local_addr: std::net::SocketAddr,
    pub endpoint: DaemonEndpoint,
    child: mj_core::subprocess::SupervisedChild,
}

impl SshLease {
    /// Credentials for the forwarded connection, never persisted to disk.
    /// PID zero deliberately carries no local process authority.
    pub fn metadata(&self) -> super::DaemonMetadata {
        super::DaemonMetadata {
            protocol_version: self.endpoint.protocol_version,
            pid: 0,
            address: self.local_addr,
            token: self.endpoint.token.clone(),
            started_at: String::new(),
            build_version: self.endpoint.build_version.clone(),
            instance_id: self.endpoint.instance_id.clone(),
            placement: super::DaemonPlacement::Host,
        }
    }

    pub async fn terminate(&mut self) -> Result<()> {
        self.child.terminate().await?;
        Ok(())
    }
}

/// Discover and authenticate a supervised tunnel. Cancellation (including
/// dropping this future) releases the process group without remote teardown.
pub async fn open(
    placement: &DaemonConfig,
    remote_instance: &str,
    intent: DiscoveryIntent,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<SshLease> {
    tokio::pin!(cancel);
    tokio::select! {
        result = open_with_port(placement, remote_instance, intent, select_port) => result,
        () = &mut cancel => bail!("SSH daemon connection cancelled"),
    }
}

fn select_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

async fn open_with_port(
    placement: &DaemonConfig,
    remote_instance: &str,
    intent: DiscoveryIntent,
    mut port: impl FnMut() -> Result<u16>,
) -> Result<SshLease> {
    let endpoint = discover(placement, remote_instance, intent).await?;
    loop {
        let local_addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port()?));
        let mut command = tokio::process::Command::new("ssh");
        command
            .args([
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "BatchMode=yes",
            ])
            .args(&placement.ssh_args)
            .arg("-L")
            .arg(format!(
                "127.0.0.1:{}:127.0.0.1:{}",
                local_addr.port(),
                endpoint.control_address.port()
            ))
            .arg(placement.host.as_deref().context("missing SSH host")?);
        let child = mj_core::subprocess::SupervisedChild::spawn(&mut command)?;
        let mut lease = SshLease {
            local_addr,
            endpoint: endpoint.clone(),
            child,
        };
        let metadata = lease.metadata();
        let ready = async {
            loop {
                if let Ok(mut client) = super::DaemonClient::connect(metadata.clone()).await
                    && matches!(
                        client.request_once(super::DaemonAction::Ping).await,
                        Ok(super::DaemonReply::Pong)
                    )
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        tokio::select! {
            () = ready => return Ok(lease),
            status = lease.child.wait() => {
                let status = status?;
                let stderr = String::from_utf8_lossy(&lease.child.stderr()).into_owned();
                let collision = stderr.lines().any(|line|
                    (line.contains("bind") && line.contains("Address already in use"))
                    || line.contains("cannot listen to port"));
                if collision { continue; }
                bail!("SSH daemon tunnel exited ({status}): {}", stderr.trim());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Run against a disposable sshd with a real, matching mj, never a live
    /// instance. The driver supplies isolated remote config/data directories.
    #[tokio::test]
    #[ignore = "requires MJ_SSH_DISCOVERY_PLACEMENT pointing at a disposable sshd fixture"]
    async fn real_ssh_endpoint_discovery() {
        let path = std::env::var_os("MJ_SSH_DISCOVERY_PLACEMENT").unwrap();
        let placement =
            mj_core::config::DaemonConfig::load_placement(std::path::Path::new(&path)).unwrap();
        let mut connector_placement = placement.clone();
        connector_placement.instance = Some("ssh-discovery-e2e".into());
        let connector =
            super::super::connector::DaemonConnector::for_placement(connector_placement);
        let mut connected = connector
            .connect_or_start(|| async { panic!("SSH must not run local startup") })
            .await
            .unwrap();
        let first_identity = connected.identity().await.unwrap();
        let mut second_client = connector.connect_existing().await.unwrap();
        assert_eq!(
            second_client.identity().await.unwrap().instance_id,
            first_identity.instance_id
        );
        let argv = vec![
            placement.remote_command().to_owned(),
            "--instance".into(),
            "ssh-discovery-e2e".into(),
            "daemon".into(),
            "restart".into(),
        ];
        let mut restart = tokio::process::Command::new("ssh");
        restart
            .args(["-o", "BatchMode=yes"])
            .args(&placement.ssh_args)
            .arg(placement.host.as_ref().unwrap())
            .arg(mj_core::targets::daemon_login_command(&argv));
        let restarted = mj_core::subprocess::capture(&mut restart).await.unwrap();
        assert!(
            restarted.status.success(),
            "{}",
            String::from_utf8_lossy(&restarted.stderr)
        );
        let (left, right) =
            tokio::join!(connector.connect_existing(), connector.connect_existing());
        let new_identity = left.unwrap().identity().await.unwrap();
        assert_ne!(new_identity.instance_id, first_identity.instance_id);
        assert_eq!(
            right.unwrap().identity().await.unwrap().instance_id,
            new_identity.instance_id
        );
        drop(connected);
        drop(second_client);
        drop(connector);
        let endpoint = super::discover(
            &placement,
            "ssh-discovery-e2e",
            super::DiscoveryIntent::Start,
        )
        .await
        .unwrap();
        assert!(endpoint.control_address.ip().is_loopback());
        let existing = super::discover(
            &placement,
            "ssh-discovery-e2e",
            super::DiscoveryIntent::Existing,
        )
        .await
        .unwrap();
        assert_eq!(existing.token, endpoint.token);
        assert_eq!(existing.instance_id, endpoint.instance_id);
        let mut lease = super::open(
            &placement,
            "ssh-discovery-e2e",
            super::DiscoveryIntent::Existing,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(lease.endpoint.instance_id, endpoint.instance_id);
        lease.terminate().await.unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let mut first = true;
        let mut second = super::open_with_port(
            &placement,
            "ssh-discovery-e2e",
            super::DiscoveryIntent::Existing,
            || {
                if first {
                    first = false;
                    Ok(occupied_port)
                } else {
                    super::select_port()
                }
            },
        )
        .await
        .unwrap();
        assert!(!first);
        assert_ne!(second.local_addr.port(), occupied_port);
        assert_eq!(second.endpoint.instance_id, endpoint.instance_id);
        second.terminate().await.unwrap();
        let mut missing = placement.clone();
        missing.remote_command = Some("/missing/mj-discovery-test".into());
        let error = super::discover(
            &missing,
            "ssh-discovery-e2e",
            super::DiscoveryIntent::Existing,
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("[daemon] remote_command"), "{error}");
        assert!(error.contains("scripts/install.sh"), "{error}");
    }
}
