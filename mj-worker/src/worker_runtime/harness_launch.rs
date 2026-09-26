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
    let bridge = npm_bridge(harness).filter(|bridge| {
        (spec.command == Path::new(bridge.command) && spec.args.is_empty())
            || bridge.is_legacy_launcher(&spec.command, &spec.args)
    });
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
            && let Some(provider) = environment.get("CODEX_PATH")
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
