//! Select the concrete executable before identifying or starting a harness.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use mj_core::config::{ExecutionPolicy, HarnessKind};
use mj_core::harness_runtime::{RuntimeIdentity, npm_bridge};
use mj_core::worker_launch::HarnessRuntimePolicy;

use super::{AcpSupervisorSpec, harness, runtime_identity};

/// Keep this preparation alive until the supervisor holds its installation lease.
pub struct PreparedHarnessLaunch {
    pub spec: AcpSupervisorSpec,
    pub(crate) environment: BTreeMap<String, String>,
    harness: HarnessKind,
    pub(crate) managed: Option<harness::ManagedHarness>,
}

impl PreparedHarnessLaunch {
    /// The installer keeps its lease until the receiving process has acquired
    /// the same shared lock. Private files avoid secret-bearing stdout.
    pub async fn transfer_runtime(
        &self,
        info_path: std::path::PathBuf,
        ack_path: std::path::PathBuf,
    ) -> Result<()> {
        let info = mj_core::worker_launch::PreparedHarnessInfo {
            version: mj_core::worker_launch::PreparedHarnessInfo::VERSION,
            command: self.spec.command.clone(),
            environment: self.spec.environment.clone(),
            lease_path: self
                .spec
                .harness_lease
                .clone()
                .context("managed preparation did not acquire a runtime lease")?,
        };
        tokio::task::spawn_blocking(move || {
            mj_core::config::atomic_write(&info_path, &serde_json::to_vec(&info)?)
        })
        .await
        .context("publish prepared runtime task failed")??;
        tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                match tokio::fs::read(&ack_path).await {
                    Ok(ack) if ack == b"retained" => return Ok(()),
                    Ok(_) => anyhow::bail!("prepared runtime lease transfer was declined"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("read runtime lease acknowledgement"),
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("prepared runtime lease acknowledgement timed out")?
    }

    /// Inspect exactly the executable and environment selected for the supervisor.
    pub async fn runtime_identity(&self) -> Result<RuntimeIdentity> {
        let command = self.spec.command.clone();
        let environment = self.environment.clone();
        let harness = self.harness;
        let root = self
            .managed
            .as_ref()
            .and_then(|managed| managed.lease_path.parent())
            .map(std::path::Path::to_path_buf);
        tokio::task::spawn_blocking(move || {
            runtime_identity::inspect(harness, &command, &environment, root.as_deref())
        })
        .await
        .context("runtime identity inspection task failed")
    }
}

pub async fn prepare_harness_launch(
    harness: HarnessKind,
    policy: HarnessRuntimePolicy,
    execution_policy: ExecutionPolicy,
    mut spec: AcpSupervisorSpec,
) -> Result<PreparedHarnessLaunch> {
    let mut environment = mj_core::login_environment::with_overrides(&spec.environment).await?;
    super::exclude_from_harness_environment(
        harness,
        &spec.excluded_environment,
        &environment,
        &mut spec.environment,
    );
    for name in &spec.excluded_environment {
        environment.remove(name);
    }
    // Resolve relative PATH entries against the same directory the bridge uses.
    if let Some(path) = environment.get("PATH") {
        let path = std::env::join_paths(std::env::split_paths(path).map(|entry| {
            if entry.is_absolute() {
                entry
            } else {
                spec.cwd.join(entry)
            }
        }))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("runtime PATH is not UTF-8"))?;
        environment.insert("PATH".into(), path.clone());
        spec.environment.insert("PATH".into(), path);
    }
    let bridge =
        npm_bridge(harness).filter(|bridge| bridge.matches_launcher(&spec.command, &spec.args));
    let mut selected_policy = policy;
    if policy == HarnessRuntimePolicy::Ambient
        && let Some(bridge) = bridge
    {
        let search_environment = environment.clone();
        let selected = tokio::task::spawn_blocking(move || {
            runtime_identity::find_command(Path::new(bridge.command), &search_environment)
        })
        .await
        .context("find target bridge task failed")??;
        let usable = if let Some(command) = &selected {
            if harness == HarnessKind::Codex {
                let mut probe = tokio::process::Command::new(command);
                probe
                    .arg("--version")
                    .current_dir(&spec.cwd)
                    .env_clear()
                    .envs(&environment);
                let output = mj_core::subprocess::run_bounded(
                    &mut probe,
                    64 * 1024,
                    Duration::from_secs(10),
                )
                .await
                .context("check installed Codex bridge version")?;
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim_end_matches('\n')
                        == format!("{} {}", bridge.package, bridge.version)
            } else {
                true
            }
        } else {
            false
        };
        if usable {
            spec.command = selected.expect("usable bridge has a command");
            spec.args.clear();
        } else {
            tracing::info!(
                harness = harness.id(),
                "target bridge is missing or incompatible; preparing the pinned installation"
            );
            selected_policy = HarnessRuntimePolicy::Managed;
        }
    }
    let managed = harness::resolve(selected_policy, harness, execution_policy, &environment)
        .await
        .with_context(|| format!("prepare {}", harness.display_name()))?;
    if let Some(managed) = &managed {
        spec.command = managed.command.clone();
        spec.args = managed.args.clone();
        spec.environment.extend(managed.environment.clone());
        environment.extend(managed.environment.clone());
        spec.harness_lease = Some(managed.lease_path.clone());
    }
    let (spec, environment) = tokio::task::spawn_blocking(move || -> Result<_> {
        let command = if spec.command.is_relative() && spec.command.components().count() > 1 {
            spec.cwd.join(&spec.command)
        } else {
            spec.command.clone()
        };
        spec.command = runtime_identity::resolve_command(&command, &environment)?;
        // Freeze the explicit provider selection as well as the bridge. Unknown
        // provider metadata still yields an unavailable identity during inspection.
        if harness == HarnessKind::Codex
            && let Some(provider) = environment
                .get("CODEX_PATH")
                .filter(|path| !path.is_empty())
        {
            let provider = Path::new(provider);
            let provider = if provider.is_relative() && provider.components().count() > 1 {
                spec.cwd.join(provider)
            } else {
                provider.to_path_buf()
            };
            if let Some(provider) = runtime_identity::find_command(&provider, &environment)? {
                let provider = provider.to_string_lossy().into_owned();
                spec.environment
                    .insert("CODEX_PATH".into(), provider.clone());
                environment.insert("CODEX_PATH".into(), provider);
            }
        }
        Ok((spec, environment))
    })
    .await
    .context("harness executable resolution task failed")??;
    Ok(PreparedHarnessLaunch {
        spec,
        environment,
        harness,
        managed,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn kimi_preparation_keeps_its_managed_lease_until_the_receiver_acknowledges() {
        let root = tempfile::tempdir().unwrap();
        let pin = mj_core::harness_runtime::pin(HarnessKind::Kimi);
        let install = root
            .path()
            .join("mjolnir/harnesses/kimi")
            .join(pin.install_id);
        let executable = install.join(pin.entrypoint);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(install.join(".lease"), []).unwrap();
        std::fs::write(
            install.join("mj-harness.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": 1, "harness": "kimi", "install_id": pin.install_id,
            }))
            .unwrap(),
        )
        .unwrap();
        let managed = harness::resolve(
            HarnessRuntimePolicy::Managed,
            HarnessKind::Kimi,
            ExecutionPolicy::ConfiguredApprovals,
            &BTreeMap::from([("XDG_CACHE_HOME".into(), root.path().display().to_string())]),
        )
        .await
        .unwrap()
        .unwrap();
        let lease_path = managed.lease_path.clone();
        let prepared = PreparedHarnessLaunch {
            spec: AcpSupervisorSpec {
                command: managed.command.clone(),
                args: Vec::new(),
                environment: BTreeMap::new(),
                excluded_environment: Vec::new(),
                cwd: root.path().into(),
                harness_lease: Some(lease_path.clone()),
            },
            environment: BTreeMap::new(),
            harness: HarnessKind::Kimi,
            managed: Some(managed),
        };
        let info = root.path().join("runtime.json");
        let ack = root.path().join("runtime.ack");
        let sender = tokio::spawn({
            let info = info.clone();
            let ack = ack.clone();
            async move {
                prepared.transfer_runtime(info, ack).await.unwrap();
                drop(prepared);
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !tokio::fs::try_exists(&info).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(!sender.is_finished());
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lease_path)
            .unwrap();
        assert!(contender.try_lock().is_err());
        let receiver = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lease_path)
            .unwrap();
        receiver.lock_shared().unwrap();
        mj_core::config::atomic_write(&ack, b"retained").unwrap();
        sender.await.unwrap();
        assert!(contender.try_lock().is_err());
        drop(receiver);
        // Concurrent process tests can fork while this descriptor is open.
        // CLOEXEC closes their copies at exec, rather than at the parent's drop.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match contender.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("probe released runtime lease: {error}"),
                }
            }
        })
        .await
        .expect("runtime lease remained locked after intentional owners dropped and fork/exec copies should have closed");
    }
}
