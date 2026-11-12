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
