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

/// The `-L` destination for the discovered control address, as seen from the
/// remote host. A daemon listening on a wildcard is reached through loopback
/// of the same family; IPv6 needs brackets in OpenSSH forwarding syntax.
fn forward_destination(address: std::net::SocketAddr) -> String {
    let ip = match address.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    match ip {
        std::net::IpAddr::V4(ip) => format!("{ip}:{}", address.port()),
        std::net::IpAddr::V6(ip) => format!("[{ip}]:{}", address.port()),
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
    let mut endpoint = discover(placement, remote_instance, intent).await?;
    loop {
        let local_addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port()?));
        let mut command = tokio::process::Command::new("ssh");
        // The tunnel needs its own connection. Through a shared ControlMaster
        // connection the master would own the listener, and stopping this child would
        // not remove the forward. OpenSSH keeps the first value of an option,
        // so these override the user's `ssh_args` and ssh_config.
        command
            .args([
                "-N",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "BatchMode=yes",
            ])
            .args(&placement.ssh_args)
            .arg("-L")
            .arg(format!(
                "127.0.0.1:{}:{}",
                local_addr.port(),
                forward_destination(endpoint.control_address)
            ))
            .arg(placement.host.as_deref().context("missing SSH host")?);
        let child = mj_core::subprocess::SupervisedChild::spawn(&mut command)?;
        let mut lease = SshLease {
            local_addr,
            endpoint: endpoint.clone(),
            child,
        };
        let ready = async {
            loop {
                // Bound a silent competing listener, not SSH startup.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    prove_listener(local_addr, &endpoint.token),
                )
                .await
                {
                    Ok(Ok(true)) => return true,
                    Ok(Err(ref error))
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    _ => {
                        // Give SSH's bind failure the connection deadline to
                        // finish; send no more bytes to the unproven peer.
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        return false;
                    }
                }
            }
        };
        tokio::select! {
            answered = ready => {
                if answered {
                    return Ok(lease);
                }
                // The remote daemon stopped or was replaced after discovery.
                // Discover again; the new endpoint is the state change that
                // ends this loop, and an unchanged one is an error.
                lease.terminate().await?;
                let stale = std::mem::replace(
                    &mut endpoint,
                    discover(placement, remote_instance, intent).await?,
                );
                if stale.instance_id == endpoint.instance_id
                    && stale.control_address == endpoint.control_address
                {
                    bail!(
                        "the daemon at {} on {} answers discovery but not through the SSH tunnel",
                        endpoint.control_address,
                        placement.host.as_deref().unwrap_or_default()
                    );
                }
            }
            status = lease.child.wait() => {
                let status = status?;
                let stderr = String::from_utf8_lossy(&lease.child.stderr()).into_owned();
                let collision = stderr.lines().any(|line|
                    (line.contains("bind") && line.contains("Address already in use"))
                    || line.contains("cannot listen to port"));
                if collision { continue; }
                bail!("SSH daemon tunnel exited ({status}): {}", stderr.lines().filter(|line| !line.starts_with("debug") && !line.starts_with("OpenSSH")).collect::<Vec<_>>().join("\n"));
            }
        }
    }
}

async fn prove_listener(address: std::net::SocketAddr, token: &str) -> Result<bool> {
    use hmac::Mac;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("{error}"))?;
    let nonce = mj_core::hex::lower_hex(random);
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    super::write_frame(
        &mut stream,
        &super::RequestEnvelope {
            protocol_version: super::PROTOCOL_VERSION,
            request_id: 0,
            token: String::new(),
            action: super::DaemonAction::ProveIdentity {
                nonce: nonce.clone(),
            },
        },
    )
    .await?;
    let response = super::read_response(&mut stream).await?;
    Ok(
        matches!(response.result, Ok(super::DaemonReply::IdentityProof { mac })
        if super::identity_proof(token, &nonce).verify_slice(&mac).is_ok()),
    )
}

#[cfg(test)]
mod tests {
    // Hard-won: 5159ed294: a forwarding diagnostic printed before bind() released the token to another listener
    #[tokio::test]
    async fn competing_listener_never_receives_control_token() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = "private-control-token-that-must-never-cross-the-wire";
        let peer = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let length = stream.read_u32().await.unwrap();
            let mut bytes = vec![0; length as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(token));
            let request: super::super::RequestEnvelope = serde_json::from_slice(&bytes).unwrap();
            assert!(request.token.is_empty());
            assert!(matches!(
                request.action,
                super::super::DaemonAction::ProveIdentity { .. }
            ));
            stream.write_all(&[0, 0, 0, 1, b'!']).await.unwrap();
            let mut remainder = Vec::new();
            stream.read_to_end(&mut remainder).await.unwrap();
            assert!(remainder.is_empty());
        };
        let (result, ()) = tokio::join!(super::prove_listener(address, token), peer);
        assert!(result.is_err());
    }

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
        let old_lease = std::sync::Arc::downgrade(connected.ssh_lease.as_ref().unwrap());
        let old_address = connected.metadata.address;
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
        // Hard-won: #1514: refreshing one client killed another client's tunnel.
        assert!(old_lease.upgrade().is_some());
        tokio::net::TcpStream::connect(old_address).await.unwrap();
        drop(connected);
        drop(second_client);
        assert!(old_lease.upgrade().is_none());
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
        occupied.set_nonblocking(true).unwrap();
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
