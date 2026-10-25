//! Select the concrete executable before starting a harness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use mj_core::config::{ExecutionPolicy, HarnessKind};
use mj_core::harness_runtime::{harness_uses_node_runtime, npm_bridge};
use mj_core::worker_launch::HarnessRuntimePolicy;

use super::{AcpSupervisorSpec, harness};

/// Keep this preparation alive until the supervisor holds its installation lease.
pub struct PreparedHarnessLaunch {
    pub spec: AcpSupervisorSpec,
    pub(crate) environment: BTreeMap<String, String>,
    pub(crate) managed: Option<harness::ManagedHarness>,
    pub(crate) managed_node: Option<harness::ManagedNodeRuntime>,
}

pub async fn prepare_harness_launch(
    harness: HarnessKind,
    policy: HarnessRuntimePolicy,
    execution_policy: ExecutionPolicy,
    mut spec: AcpSupervisorSpec,
) -> Result<PreparedHarnessLaunch> {
    let mut environment = mj_core::login_environment::with_overrides(&spec.environment).await?;
    link_build_cache_configuration(&environment).await?;
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
    let managed_node = if needs_node_runtime(harness, &environment)? {
        let node = harness::ensure_node_runtime(&mut environment)
            .await
            .with_context(|| format!("prepare Node.js for {}", harness.display_name()))?;
        if let Some(node) = &node {
            spec.node_lease = Some(node.lease_path().to_path_buf());
            spec.environment.insert(
                "PATH".into(),
                environment
                    .get("PATH")
                    .cloned()
                    .context("managed Node.js runtime did not produce PATH")?,
            );
        }
        node
    } else {
        None
    };
    let (prepared_spec, prepared_environment) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let before = environment.clone();
            super::tool_cache::prepare(&spec.cwd, &mut environment)?;
            spec.environment.extend(
                environment
                    .iter()
                    .filter(|(name, value)| before.get(*name) != Some(*value))
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
            Ok((spec, environment))
        })
        .await
        .context("build cache preparation task failed")??;
    spec = prepared_spec;
    environment = prepared_environment;
    let bridge =
        npm_bridge(harness).filter(|bridge| bridge.matches_launcher(&spec.command, &spec.args));
    let mut selected_policy = policy;
    if policy == HarnessRuntimePolicy::Ambient
        && let Some(bridge) = bridge
    {
        let search_environment = environment.clone();
        let selected = tokio::task::spawn_blocking(move || {
            find_command(Path::new(bridge.command), &search_environment)
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
        spec.command = resolve_command(&command, &environment)?;
        // Freeze the explicit provider selection as well as the bridge.
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
            if let Some(provider) = find_command(&provider, &environment)? {
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
        managed,
        managed_node,
    })
}

/// DSH can launch its own native binary. Its npm fallback is the only branch
/// that needs Node.js. Other Node-using harnesses always need the runtime,
/// even when their adapter is already installed.
pub(super) fn needs_node_runtime(
    harness: HarnessKind,
    environment: &BTreeMap<String, String>,
) -> Result<bool> {
    let dsh_is_native = if harness == HarnessKind::Dsh {
        if environment.contains_key("PATH") {
            find_command(Path::new("dsh-acp"), environment)?.is_some()
        } else {
            false
        }
    } else {
        false
    };
    Ok(harness_uses_node_runtime(harness) && !dsh_is_native)
}

/// Reviewer homes can be created after provisioning. Link their mbx lookup to
/// machine policy too, without changing XDG_CONFIG_HOME for any other program.
async fn link_build_cache_configuration(environment: &BTreeMap<String, String>) -> Result<()> {
    let Some(source) = environment.get("MJ_MBX_CONFIG_DIR") else {
        return Ok(());
    };
    let source = Path::new(source).join("config.toml");
    let root = match environment
        .get("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
    {
        Some(root) => std::path::PathBuf::from(root),
        None => Path::new(
            environment
                .get("HOME")
                .context("mbx configuration needs HOME")?,
        )
        .join(".config"),
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        // The shared cache is optional. The controller reports why it could not
        // prepare the policy and starts the session without the mbx launchers,
        // so a policy that is absent here must not refuse the harness. Report
        // the OS error rather than calling every failure "missing": a host file
        // the container cannot reach is a different problem from one that was
        // never published.
        let directory = root.join("mbx");
        let destination = directory.join("config.toml");
        match std::fs::metadata(&source) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                remove_stale_configuration_link(&destination, &source);
                tracing::warn!(
                    configuration = %source.display(),
                    "shared machine mbx configuration is not a file; mbx uses its own defaults"
                );
                return Ok(());
            }
            Err(error) => {
                remove_stale_configuration_link(&destination, &source);
                tracing::warn!(
                    configuration = %source.display(),
                    %error,
                    "shared machine mbx configuration is unavailable; mbx uses its own defaults"
                );
                return Ok(());
            }
        }
        std::fs::create_dir_all(&directory).context("create mbx configuration lookup directory")?;
        if std::fs::read_link(&destination).ok().as_ref() == Some(&source) {
            return Ok(());
        }
        let staging = tempfile::tempdir_in(&directory).context("stage mbx configuration link")?;
        let link = staging.path().join("config.toml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).context("link shared mbx configuration")?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&source, &link)
            .context("link shared mbx configuration")?;
        std::fs::rename(link, destination).context("publish mbx configuration link")?;
        Ok(())
    })
    .await
    .context("mbx configuration link task failed")?
}

/// A policy that existed when the home was prepared can disappear, leaving a
/// link to nothing. Remove only Mjolnir's own link; a real file belongs to the
/// user.
fn remove_stale_configuration_link(destination: &Path, source: &Path) {
    if std::fs::read_link(destination).ok().as_deref() != Some(source) {
        return;
    }
    if let Err(error) = std::fs::remove_file(destination) {
        tracing::warn!(
            configuration = %destination.display(),
            %error,
            "could not remove a stale shared mbx configuration link"
        );
    }
}

fn resolve_command(command: &Path, environment: &BTreeMap<String, String>) -> Result<PathBuf> {
    find_command(command, environment)?
        .context("runtime command is not executable on the selected PATH")
}

pub(super) fn find_command(
    command: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>> {
    let selected = if command.is_absolute() {
        command.to_path_buf()
    } else {
        ensure!(
            command.components().count() == 1,
            "relative runtime command is ambiguous"
        );
        let selected = std::env::split_paths(
            environment
                .get("PATH")
                .context("runtime PATH is unavailable")?,
        )
        .map(|directory| directory.join(command))
        .find(|path| super::harness::entrypoint_is_executable(path));
        let Some(selected) = selected else {
            return Ok(None);
        };
        selected
    };
    if !super::harness::entrypoint_is_executable(&selected) {
        return Ok(None);
    }
    selected
        .canonicalize()
        .map(Some)
        .context("resolve selected runtime command")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn dsh_without_path_uses_the_managed_node_runtime() {
        assert!(
            needs_node_runtime(HarnessKind::Dsh, &BTreeMap::new()).unwrap(),
            "a missing PATH must select the managed Node fallback, not fail discovery"
        );

        let tools = tempfile::tempdir().unwrap();
        mj_core::test_hooks::install_fake_command(tools.path(), "dsh-acp", "#!/bin/sh\nexit 0\n");
        let environment = BTreeMap::from([(
            "PATH".to_owned(),
            tools.path().to_string_lossy().into_owned(),
        )]);
        assert!(!needs_node_runtime(HarnessKind::Dsh, &environment).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reviewer_and_primary_homes_read_the_same_replaced_machine_policy() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let policy = shared.join("config.toml");
        mj_core::config::atomic_write(&policy, b"[target]\nmax_size = '100GiB'\n").unwrap();
        for home in ["primary", "reviewer"] {
            let environment = BTreeMap::from([
                (
                    "MJ_MBX_CONFIG_DIR".into(),
                    shared.to_string_lossy().into_owned(),
                ),
                (
                    "XDG_CONFIG_HOME".into(),
                    root.path().join(home).to_string_lossy().into_owned(),
                ),
            ]);
            link_build_cache_configuration(&environment).await.unwrap();
            assert_eq!(
                environment["XDG_CONFIG_HOME"],
                root.path().join(home).to_string_lossy()
            );
        }
        mj_core::config::atomic_write(&policy, b"[target]\nmax_size = '300GiB'\n").unwrap();
        for home in ["primary", "reviewer"] {
            assert_eq!(
                std::fs::read(root.path().join(home).join("mbx/config.toml")).unwrap(),
                b"[target]\nmax_size = '300GiB'\n"
            );
        }
    }

    // A failed optional cache must not refuse the harness: the controller
    // deliberately starts a session without the shared cache when it cannot
    // verify the in-container mbx copy, but the worker then refused to start
    // because the unpublished policy was absent.
    #[tokio::test]
    async fn a_missing_machine_policy_does_not_refuse_the_harness() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let missing = root.path().join("missing");
        std::fs::create_dir_all(home.join("mbx")).unwrap();
        std::os::unix::fs::symlink(missing.join("config.toml"), home.join("mbx/config.toml"))
            .unwrap();
        let environment = BTreeMap::from([
            (
                "MJ_MBX_CONFIG_DIR".into(),
                missing.to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                home.to_string_lossy().into_owned(),
            ),
        ]);
        link_build_cache_configuration(&environment).await.unwrap();
        assert!(!home.join("mbx/config.toml").exists());
    }
}

/// Only a supported primary may enforce the repository's review gate.
pub(super) fn primary_supervisor_spec(
    config: &mj_core::worker_launch::WorkerLaunchConfig,
) -> AcpSupervisorSpec {
    let mut spec = AcpSupervisorSpec::from(config);
    spec.environment.remove("MJ_REVIEW_GATE");
    if config.review_gate() {
        spec.excluded_environment
            .retain(|name| name != "MJ_REVIEW_GATE");
        spec.environment.insert("MJ_REVIEW_GATE".into(), "1".into());
    } else if !spec
        .excluded_environment
        .iter()
        .any(|name| name == "MJ_REVIEW_GATE")
    {
        spec.excluded_environment.push("MJ_REVIEW_GATE".into());
    }
    spec
}

#[cfg(unix)]
pub(super) fn prepare_review_gate(
    launch: &mj_core::worker_launch::WorkerLaunchConfig,
) -> Result<Vec<String>> {
    if launch.review_trigger != mj_core::config::ReviewTrigger::Push
        || launch.is_managed_subagent()
        || launch.handback_tool
        || launch.reviewer_launch.is_some()
    {
        return Ok(Vec::new());
    }
    let mut notices = Vec::new();
    if launch.review_gate() {
        let git = crate::review::capture::BoundedGit::new(
            crate::review::capture::WORKSPACE_STATE_TIMEOUT,
        );
        let _cancel_git = git.cancellation_guard();
        let mut roots = vec![launch.cwd.clone()];
        roots.extend(super::reviewer_workspace_directories(launch));
        for repository in crate::review::capture::discover_repositories(&git, &roots) {
            crate::review::capture::ensure_session_start_head(
                &git,
                &repository,
                &launch.session_id,
            )?;
            use crate::review::hook::{HookInstall, install_pre_push_hook};
            let reason = match install_pre_push_hook(&git, &repository) {
                Ok(HookInstall::Installed | HookInstall::Rewritten) => None,
                Ok(HookInstall::RefusedExistingBackup(path)) => {
                    Some(format!("{} already exists", path.display()))
                }
                Ok(HookInstall::RefusedWorkTreeHooksDirectory(path)) => Some(format!(
                    "{} is inside the work tree and is not ignored",
                    path.display()
                )),
                Err(error) => Some(format!("{error:#}")),
            };
            if let Some(reason) = reason {
                notices.push(format!(
                    "Before-push review in {}: {reason}; the gate is not installed.",
                    repository.display()
                ));
            }
        }
    } else {
        notices.push(format!(
            "Before-push review is not available for {}; this session runs without review.",
            launch.harness.display_name()
        ));
    }
    Ok(notices)
}
