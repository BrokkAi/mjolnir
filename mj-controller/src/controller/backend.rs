//! Backend target, locator, and capacity conversion for provisioned sessions.

use mj_core::targets::SshTrafficClass;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub use crate::targets::backend_locator;
pub(super) use crate::targets::recorded_backend_locator;

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{Config, ProjectBundle, TargetTemplate, data_dir};
use mj_core::state::{
    PodmanWorkspaceLocator, ResourceAllocationKind, SessionRecord, SessionResourceAllocation,
    TargetLocator,
};

use crate::targets::{
    self, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate, ProjectBundleSpec,
    ProvisionStage, RepositorySpec, SshTarget,
};

use super::Controller;

impl Controller {
    /// Inspect the actual execution checkout in a background worker.
    pub fn session_working_context(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<(PathBuf, String)> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session is missing")?;
        let locator = session
            .target
            .as_ref()
            .context("target is still starting")?;
        let backend = backend_locator(locator, session, &self.config)?;
        let launch = self.current_worker_launch_config(session_id, &backend)?;
        if session
            .project
            .as_ref()
            .is_some_and(|project| project.primary_is_plain_directory())
        {
            return Ok((launch.cwd, "not a git checkout".to_owned()));
        }
        let output = executor.execute(&targets::command_on_locator(
            SshTrafficClass::Control,
            &backend,
            session_id,
            vec![
                "git".into(),
                "-C".into(),
                launch.cwd.to_string_lossy().into_owned(),
                "rev-parse".into(),
                "--abbrev-ref".into(),
                "HEAD".into(),
            ],
            "read current session branch",
        )?)?;
        let branch = if output.status == 0 {
            let branch = String::from_utf8(output.stdout).context("decode session branch")?;
            if branch.trim() == "HEAD" {
                "detached HEAD".to_owned()
            } else {
                branch.trim().to_owned()
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not a git repository") {
                // An older or untracked session can still be a plain folder.
                // Say so briefly rather than echoing multi-line git stderr,
                // which wraps the banner.
                "not a git checkout".to_owned()
            } else {
                format!(
                    "unavailable: {}",
                    stderr.lines().next().unwrap_or("").trim()
                )
            }
        };
        Ok((launch.cwd, branch))
    }

    /// The session checkout's branch, distance from upstream, and changed
    /// files, read with the target's own `git`. Builds on
    /// [`Self::session_working_context`], so it works wherever that does: a
    /// local checkout, a container, or an SSH host.
    pub fn session_git_status(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<mj_core::local_git::SessionGitStatus> {
        let (cwd, branch) = self.session_working_context(session_id, executor)?;
        if branch.starts_with("not a git") || branch.starts_with("unavailable") {
            return Ok(mj_core::local_git::parse_git_status(
                cwd, &branch, None, "", "",
            ));
        }
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session is missing")?;
        let locator = session
            .target
            .as_ref()
            .context("target is still starting")?;
        let backend = backend_locator(locator, session, &self.config)?;
        let cwd_text = cwd.to_string_lossy().into_owned();
        let run = |args: &[&str], purpose: &str| -> Result<Option<String>> {
            let mut command = vec!["git".to_owned(), "-C".to_owned(), cwd_text.clone()];
            command.extend(args.iter().map(|arg| (*arg).to_owned()));
            let output = executor.execute(&targets::command_on_locator(
                SshTrafficClass::Control,
                &backend,
                session_id,
                command,
                purpose,
            )?)?;
            Ok((output.status == 0).then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
        };
        // No upstream is an ordinary state, so a failing count is `None`
        // rather than an error.
        let ahead_behind = run(
            &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
            "count commits against upstream",
        )?;
        // A repository with no commit yet has no HEAD to diff against.
        let numstat =
            run(&mj_core::local_git::NUMSTAT_ARGS, "count changed lines")?.unwrap_or_default();
        let porcelain =
            run(&mj_core::local_git::STATUS_ARGS, "list changed files")?.unwrap_or_default();
        Ok(mj_core::local_git::parse_git_status(
            cwd,
            &branch,
            ahead_behind.as_deref(),
            &numstat,
            &porcelain,
        ))
    }

    pub fn reconnect_command(&self, session_id: &str) -> Result<CommandSpec> {
        crate::worker_launch::reconnect_command(&self.config, &self.state, session_id)
    }

    /// The directories the live sessions on one target write to.
    fn session_storage_paths(&self, target_id: &str) -> Vec<String> {
        self.state
            .sessions
            .values()
            .filter(|session| session.target_template_id == target_id && session.state.is_active())
            .filter_map(|session| {
                let checkout = self.state.checkout(&session.id).ok()?;
                let paths = mj_core::targets::storage::session_storage_paths(
                    session.target.as_ref()?,
                    &session.id,
                    checkout.project_directory(),
                );
                Some(paths.all().map(str::to_owned).collect::<Vec<_>>())
            })
            .flatten()
            .collect()
    }

    pub fn deployment_capacity_targets(&self) -> Vec<targets::DeploymentCapacityTarget> {
        use targets::{DeploymentCapacityKind, DeploymentCapacityTarget};

        let mut local_ids = Vec::new();
        // Per host: its target ids, and per SSH connection the paths to measure.
        // The flag marks a connection on which every target is a Nitro
        // sandbox, whose memory total is the engine's pool. A host shared
        // with other runtimes keeps its physical memory as the total, because
        // their sessions draw on all of it.
        type HostProbes = (Vec<String>, Vec<(SshTarget, Vec<String>, bool)>);
        let mut ssh_hosts: BTreeMap<String, HostProbes> = BTreeMap::new();
        let mut targets = Vec::new();
        for (target_id, template) in &self.config.targets {
            match template {
                TargetTemplate::LocalBare
                | TargetTemplate::LocalPodman { .. }
                | TargetTemplate::LocalDocker { .. }
                | TargetTemplate::AppleContainer { .. } => {
                    local_ids.push(target_id.clone());
                }
                TargetTemplate::SshBare { ssh, .. }
                | TargetTemplate::SshPodman { ssh, .. }
                | TargetTemplate::SshDocker { ssh, .. }
                | TargetTemplate::SshAppleContainer { ssh, .. }
                | TargetTemplate::SshNitro { ssh, .. } => {
                    let entry = ssh_hosts.entry(ssh.host.clone()).or_default();
                    entry.0.push(target_id.clone());
                    let connection = SshTarget::from(ssh);
                    let nitro = matches!(template, TargetTemplate::SshNitro { .. });
                    let position = match entry
                        .1
                        .iter()
                        .position(|(known, _, _)| *known == connection)
                    {
                        Some(position) => position,
                        None => {
                            entry.1.push((connection, Vec::new(), true));
                            entry.1.len() - 1
                        }
                    };
                    // A disabled target with no live session draws on no
                    // memory. `mj ec2 create` registers a Nitro host with a
                    // disabled bare sibling, which must not turn the pool
                    // back into physical memory less the host reserve.
                    let draws_memory = !self.config.disabled_targets.contains(target_id)
                        || self.state.sessions.values().any(|session| {
                            session.target_template_id == *target_id && session.state.is_active()
                        });
                    if draws_memory {
                        entry.1[position].2 &= nitro;
                    }
                    let paths = &mut entry.1[position].1;
                    push_unique(paths, ssh_storage_paths(template));
                    // Each live session's own directories: a project or
                    // clone can sit on a filesystem of its own.
                    push_unique(paths, self.session_storage_paths(target_id));
                }
                TargetTemplate::AwsEc2 { .. } => {
                    let mut probes = Vec::new();
                    let mut probe_error = None;
                    for session in self.state.sessions.values().filter(|session| {
                        session.target_template_id == *target_id
                            && session.state.is_active()
                            && session.target.is_some()
                    }) {
                        let result = backend_locator(
                            session.target.as_ref().expect("filtered target"),
                            session,
                            &self.config,
                        )
                        .and_then(|locator| {
                            targets::aws_allocated_capacity_command(&locator, &session.id)
                        });
                        match result {
                            Ok(command) => probes.push(command),
                            Err(error) => probe_error = Some(format!("{error:#}")),
                        }
                    }
                    targets.push(DeploymentCapacityTarget {
                        id: format!("aws:{target_id}"),
                        host: target_id.clone(),
                        target_ids: vec![target_id.clone()],
                        kind: DeploymentCapacityKind::AwsFleet,
                        local: false,
                        probes,
                        local_storage_paths: Vec::new(),
                        probe_error,
                    });
                }
            }
        }
        if !local_ids.is_empty() {
            // CPU and memory come from sysinfo; storage is measured over the
            // paths by `targets::measure_local_storage`.
            let mut paths = Vec::new();
            let mbx_paths = super::mbx::local_storage_paths();
            for target_id in &local_ids {
                if let Some(template) = self.config.targets.get(target_id) {
                    push_unique(&mut paths, local_storage_paths(template, &mbx_paths));
                }
                push_unique(&mut paths, self.session_storage_paths(target_id));
            }
            targets.push(DeploymentCapacityTarget {
                id: "local".into(),
                host: "local".into(),
                target_ids: local_ids,
                kind: DeploymentCapacityKind::Host,
                local: true,
                probes: Vec::new(),
                local_storage_paths: paths,
                probe_error: None,
            });
        }
        targets.extend(
            ssh_hosts
                .into_iter()
                .map(
                    |(host, (target_ids, connections))| DeploymentCapacityTarget {
                        id: format!("ssh:{host}"),
                        host,
                        target_ids,
                        kind: DeploymentCapacityKind::Host,
                        local: false,
                        probes: connections
                            .iter()
                            .map(|(ssh, paths, nitro)| {
                                if *nitro {
                                    targets::ssh_nitro_host_capacity_command(ssh, paths)
                                } else {
                                    targets::ssh_host_capacity_command(ssh, paths)
                                }
                            })
                            .collect(),
                        local_storage_paths: Vec::new(),
                        probe_error: None,
                    },
                ),
        );
        targets.sort_by(|left, right| left.id.cmp(&right.id));
        targets
    }

    /// The full check behind the Targets pane's Test action.
    pub fn test_target(&self, target_id: &str, executor: &impl CommandExecutor) -> Result<()> {
        verify_target(
            self.configured_target(target_id)?,
            executor,
            TargetCheck::BeforeLaunch,
        )
    }

    /// The check a session wizard runs before it offers a target: the same
    /// one a launch runs, worded for a launch that has not happened yet.
    pub fn check_target_readiness(
        &self,
        target_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        preflight_target(
            self.configured_target(target_id)?,
            executor,
            TargetCheck::BeforeLaunch,
        )
    }

    fn configured_target(&self, target_id: &str) -> Result<&TargetTemplate> {
        self.config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))
    }
}

/// When a target is checked, which decides how its refusal ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetCheck {
    /// Before anything is launched: the session wizard's target row, the
    /// Targets pane.
    BeforeLaunch,
    /// Inside a launch, resume, or move, whose failure the launch-failure
    /// dialog reports with its Retry launch button.
    Launch,
}

impl TargetCheck {
    /// The clause that points at the failure dialog's Retry launch. Before a
    /// launch there is no such dialog to point at (launch finding R6-3).
    const fn then_retry(self) -> &'static str {
        match self {
            Self::BeforeLaunch => "",
            Self::Launch => ", then Retry launch",
        }
    }
}

/// The directories Mjolnir writes to on an SSH host for one target, relative
/// to the SSH user's home where they live there: worker roots, the upload
/// staging and binary caches, and the workspace prefix or the container
/// engine's storage. Each is measured at its nearest existing ancestor.
pub(crate) fn ssh_storage_paths(template: &TargetTemplate) -> Vec<String> {
    use mj_core::targets::storage::{
        ContainerStorage, DEFAULT_BUILD_CACHE_DIRECTORY, REMOTE_CACHE_DIRECTORY,
        REMOTE_PROFILES_DIRECTORY, REMOTE_WORKERS_DIRECTORY, TEMPORARY_DIRECTORY,
    };
    let mut paths = vec![
        REMOTE_CACHE_DIRECTORY.to_owned(),
        DEFAULT_BUILD_CACHE_DIRECTORY.to_owned(),
        TEMPORARY_DIRECTORY.to_owned(),
    ];
    match template {
        TargetTemplate::SshBare {
            workspace_prefix, ..
        } => {
            paths.push(REMOTE_WORKERS_DIRECTORY.to_owned());
            paths.push(REMOTE_PROFILES_DIRECTORY.to_owned());
            paths.push(workspace_prefix.to_string_lossy().into_owned());
        }
        TargetTemplate::SshPodman { .. } => paths.push(ContainerStorage::Podman.path().to_owned()),
        TargetTemplate::SshDocker { .. } => paths.push(ContainerStorage::Docker.path().to_owned()),
        TargetTemplate::SshNitro { .. } => paths.push(ContainerStorage::Microvm.path().to_owned()),
        _ => {}
    }
    paths.extend(build_cache_directory(template));
    paths
}

/// A target's configured mbx cache directory, when it names one.
fn build_cache_directory(template: &TargetTemplate) -> Option<String> {
    template
        .container()?
        .build_cache
        .as_ref()?
        .directory
        .as_ref()
        .map(|directory| directory.to_string_lossy().into_owned())
}

/// The paths the local probe measures for one local target.
fn local_storage_paths(template: &TargetTemplate, mbx_paths: &[PathBuf]) -> Vec<String> {
    use mj_core::targets::storage::{ContainerStorage, TEMPORARY_DIRECTORY, local_home_path};
    let mut paths = vec![
        data_dir().to_string_lossy().into_owned(),
        TEMPORARY_DIRECTORY.to_owned(),
    ];
    paths.extend(
        mbx_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    match template {
        TargetTemplate::LocalPodman { .. } => {
            paths.push(local_home_path(ContainerStorage::Podman.path()));
        }
        TargetTemplate::LocalDocker { .. } => {
            paths.push(ContainerStorage::Docker.path().to_owned());
        }
        _ => {}
    }
    paths.extend(build_cache_directory(template));
    paths
}

fn push_unique(paths: &mut Vec<String>, more: impl IntoIterator<Item = String>) {
    for path in more {
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
}

/// Check a target before a launch, resume, or move uses it.
///
/// An SSH host's container runtime is verified by `mj setup`, `mj doctor`,
/// and the Targets pane's Test action; here it is assumed to still be as
/// they left it, and only the host's reachability is checked. That probe
/// joins an open shared connection, so it usually costs one SSH channel.
pub(super) fn preflight_target(
    template: &TargetTemplate,
    executor: &impl CommandExecutor,
    check: TargetCheck,
) -> Result<()> {
    match template {
        TargetTemplate::SshBare { ssh, .. }
        | TargetTemplate::SshPodman { ssh, .. }
        | TargetTemplate::SshDocker { ssh, .. }
        | TargetTemplate::SshAppleContainer { ssh, .. } => {
            verify_ssh_connectivity(&SshTarget::from(ssh), executor)
        }
        _ => verify_target(template, executor, check),
    }
}

/// Whether a local container engine can run sessions, as the launch
/// preflight and the dashboard's Targets pane judge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalEngineReadiness {
    Ready,
    /// The engine's command is not on this host.
    NotInstalled,
    /// The command is there, but the engine did not answer its check.
    NotReady,
}

/// Run the launch preflight's engine check for a local container target
/// kind (`local-podman`, `local-docker`, or `apple-container`).
///
/// Answers `None` for any other kind: their readiness is not a local engine's.
pub fn local_engine_readiness(
    kind: &str,
    executor: &impl CommandExecutor,
) -> Option<LocalEngineReadiness> {
    let result = match kind {
        "local-podman" => targets::verify_local_podman(executor).map(|_| ()),
        "local-docker" => targets::verify_local_docker(executor).map(|_| ()),
        // Observe only: opening the launch options must not start a service
        // the user may have stopped. Starting stays with daemon startup and
        // the launch preflight.
        "apple-container" => {
            targets::apple_container_runtime_status(executor).and_then(|status| match status {
                targets::AppleContainerStatus::Running => Ok(()),
                targets::AppleContainerStatus::Stopped { status, detail } => {
                    anyhow::bail!("container system status exited with status {status}: {detail}")
                }
            })
        }
        _ => return None,
    };
    Some(match result {
        Ok(()) => LocalEngineReadiness::Ready,
        Err(error) if is_missing_command(&error) => LocalEngineReadiness::NotInstalled,
        Err(_) => LocalEngineReadiness::NotReady,
    })
}

/// Whether a command failed because its program is not installed.
pub fn is_missing_command(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Check that a target's host and runtime can run sessions.
pub(super) fn verify_target(
    template: &TargetTemplate,
    executor: &impl CommandExecutor,
    check: TargetCheck,
) -> Result<()> {
    let then_retry = check.then_retry();
    match template {
        TargetTemplate::LocalPodman { .. } => targets::verify_local_podman(executor)
            .map(|preflight| {
                // A launch reports what it changes for these itself.
                if check == TargetCheck::BeforeLaunch {
                    for warning in preflight.warnings {
                        executor.notify_notice(&warning.notice());
                    }
                }
            })
            .map_err(|error| {
                anyhow::anyhow!(
                    "local Podman is not ready. Fix the problem below{then_retry}: {error:#}"
                )
            }),
        TargetTemplate::LocalDocker { .. } => targets::verify_local_docker(executor)
            .map(|_| ())
            .map_err(
                |error| match error.downcast_ref::<targets::DockerUnavailable>() {
                    // Leads with the sentence the launch options use, so a
                    // missing Docker reads "not installed" in the wizard's
                    // row too (launch finding R5-3).
                    Some(problem) => anyhow::anyhow!(
                        "{problem} {}",
                        match check {
                            TargetCheck::BeforeLaunch => problem.remedy(),
                            TargetCheck::Launch => problem.launch_remedy(),
                        }
                    ),
                    None => anyhow::anyhow!(
                        "local Docker is not ready. Start Docker or fix the problem below{then_retry}: {error:#}"
                    ),
                },
            ),
        TargetTemplate::SshPodman { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            targets::verify_ssh_podman(&ssh, executor)
                .map(|preflight| {
                    for warning in preflight.warnings {
                        executor.notify_notice(&warning.notice());
                    }
                })
                .map_err(|error| {
                    anyhow::anyhow!(
                        "remote Podman is not ready on {}. Fix the problem below{then_retry}: {error:#}",
                        ssh.destination
                    )
                })
        }
        TargetTemplate::SshDocker { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            targets::verify_ssh_docker(&ssh, executor)
                .map(|_| ())
                .map_err(|error| {
                    anyhow::anyhow!(
                        "remote Docker preflight failed for {}. Fix the problem below{then_retry}: {error:#}",
                        ssh.destination
                    )
                })
        }
        TargetTemplate::AppleContainer { .. } => {
            targets::ensure_apple_container_runtime(executor)
                .map(|_| ())
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Apple container is not ready{then_retry}. Fix the problem below: {error:#}"
                    )
                })
        }
        TargetTemplate::SshAppleContainer { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            targets::verify_ssh_apple_container(&ssh, executor)
                .map(|_| ())
                .map_err(|error| {
                    anyhow::anyhow!(
                        "remote Apple container preflight failed for {}. Fix the problem below{then_retry}: {error:#}",
                        ssh.destination
                    )
                })
        }
        TargetTemplate::SshNitro { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            // The engine is the worker binary itself, so it is installed
            // before the first command that needs it.
            super::worker_binary::ensure_microvm_engine(&ssh, executor)
                .and_then(|()| targets::verify_ssh_nitro(&ssh, executor))
                .map(|_| ())
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Nitro sandbox host preflight failed for {}. Fix the problem below{then_retry}: {error:#}",
                        ssh.destination
                    )
                })
        }
        TargetTemplate::SshBare { ssh, .. } => {
            verify_ssh_connectivity(&SshTarget::from(ssh), executor)
        }
        TargetTemplate::AwsEc2 { .. } => bail!(mj_core::config::LEGACY_EC2_TARGET_NOTICE),
        TargetTemplate::LocalBare => Ok(()),
    }
}

fn verify_ssh_connectivity(ssh: &SshTarget, executor: &impl CommandExecutor) -> Result<()> {
    let output = executor.execute(&targets::ssh_connectivity_probe(ssh))?;
    ensure!(
        output.status == 0,
        "SSH connectivity test failed for {} with status {}: {}",
        ssh.destination,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn backend_bundle(
    bundle: &ProjectBundle,
    executor: &impl CommandExecutor,
) -> Result<ProjectBundleSpec> {
    backend_bundle_with_sources(bundle, None, executor)
}

pub(super) fn backend_session_bundle(
    session: &SessionRecord,
    config: &mj_core::config::Config,
    executor: &impl CommandExecutor,
) -> Result<ProjectBundleSpec> {
    let bundle = session
        .project_bundle(config)
        .context("session bundle is missing")?;
    backend_bundle_with_sources(
        bundle,
        session
            .project
            .as_ref()
            .map(|project| &project.network_sources),
        executor,
    )
}

pub(super) fn apply_session_history_cutoff(
    session: &SessionRecord,
    config: &mj_core::config::Config,
    spec: &mut ProjectBundleSpec,
) -> Result<()> {
    if let Some(cutoff) = session_history_cutoff(session)? {
        let bundle = session
            .project_bundle(config)
            .context("session bundle is missing")?;
        let checkout = session
            .checkout
            .as_ref()
            .expect("cutoff comes from the checkout");
        let destination = bundle
            .repositories
            .iter()
            .find(|repository| repository.id == checkout.repository_id)
            .context("checkout repository is no longer in the configured bundle")?
            .destination
            .to_string_lossy()
            .into_owned();
        spec.repositories
            .iter_mut()
            .find(|repository| repository.destination == destination)
            .context("cutoff repository is missing from the workspace bundle")?
            .history_cutoff = Some(cutoff);
    }
    Ok(())
}

/// The history a session created with `no_later_history` starts with: its
/// exact checkout and the launch base, which must then be a commit ID.
pub(super) fn session_history_cutoff(
    session: &SessionRecord,
) -> Result<Option<targets::HistoryCutoff>> {
    let Some(checkout) = session
        .checkout
        .as_ref()
        .filter(|checkout| checkout.no_later_history)
    else {
        return Ok(None);
    };
    let cutoff = targets::HistoryCutoff {
        head: checkout.commit.clone(),
        base: session
            .launch_base
            .clone()
            .filter(|base| *base != checkout.commit),
    };
    cutoff.validate()?;
    Ok(Some(cutoff))
}

fn backend_bundle_with_sources(
    bundle: &ProjectBundle,
    sources: Option<&std::collections::BTreeMap<String, mj_core::remote_git::NetworkGitSource>>,
    executor: &impl CommandExecutor,
) -> Result<ProjectBundleSpec> {
    let primary = bundle.primary().context("bundle primary is missing")?;
    Ok(ProjectBundleSpec {
        primary: primary.destination.to_string_lossy().into_owned(),
        repositories: bundle
            .repositories
            .iter()
            .map(|repository| {
                let source = match sources.and_then(|sources| sources.get(&repository.id)) {
                    Some(source) => source.clone(),
                    None => mj_core::remote_git::resolve_repository(repository, executor)
                        .with_context(|| format!("repository {:?}", repository.id))?,
                };
                Ok(RepositorySpec {
                    url: Some(source.fetch_url),
                    push_urls: source.push_urls,
                    destination: repository.destination.to_string_lossy().into_owned(),
                    git_ref: None,
                    reference: None,
                    history_cutoff: None,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

/// Per-session credential policy applied while creating a container.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ContainerOverrides {
    pub no_github_credentials: bool,
}

impl ContainerOverrides {
    pub(super) fn for_session(session: &SessionRecord) -> Self {
        Self {
            no_github_credentials: session.no_github_credentials,
        }
    }
}

/// OCI platform of a container target, before its container exists.
pub(crate) fn session_image_platform(
    template: &TargetTemplate,
    executor: &impl CommandExecutor,
) -> Result<String> {
    let (host, container) = template
        .image_host()
        .context("image operation requires a container target")?;
    let architecture = if let Some(platform) = &container.platform {
        platform
            .split('/')
            .find_map(|part| targets::normalize_architecture(part).ok())
            .context("unsupported container platform")?
    } else if matches!(
        host,
        targets::ImageHost::AppleContainer | targets::ImageHost::SshAppleContainer(_)
    ) {
        "aarch64"
    } else {
        // The engine owns its native architecture, including Docker Desktop
        // on a Windows daemon host where uname is not available.
        let args = match host.engine() {
            "podman" => vec![
                "podman".into(),
                "info".into(),
                "--format".into(),
                "{{.Host.Arch}}".into(),
            ],
            "docker" => vec![
                "docker".into(),
                "info".into(),
                "--format".into(),
                "{{.Architecture}}".into(),
            ],
            _ => vec!["uname".into(), "-sm".into()], // Nitro hosts run Linux.
        };
        let output =
            super::execute_checked(executor, host.command(args, "detect image platform".into()))?;
        let output = std::str::from_utf8(&output.stdout)?;
        if matches!(host, targets::ImageHost::SshNitro(_)) {
            targets::TargetPlatform::parse(output)?.architecture
        } else {
            targets::normalize_architecture(output.trim())?
        }
    };
    Ok(format!(
        "linux/{}",
        match architecture {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => unreachable!(),
        }
    ))
}

/// Resolve the project assignment before preflight, creation, or recreation.
/// A target without project images does not require an additional host probe.
pub fn session_target(
    config: &Config,
    bundle_id: &str,
    target_id: &str,
    executor: &impl CommandExecutor,
) -> Result<TargetTemplate> {
    let template = config
        .targets
        .get(target_id)
        .context("session target configuration is missing")?;
    if template.container().is_none()
        || config
            .images
            .bundles
            .get(bundle_id)
            .is_none_or(BTreeMap::is_empty)
    {
        return Ok(template.clone());
    }
    let platform = session_image_platform(template, executor)?;
    config.session_target(Some(bundle_id), target_id, &platform)
}

pub(super) fn backend_target(
    template: &TargetTemplate,
    allocation: Option<&SessionResourceAllocation>,
    overrides: ContainerOverrides,
) -> Result<targets::TargetTemplate> {
    Ok(match template {
        TargetTemplate::LocalBare => targets::TargetTemplate::LocalBare,
        TargetTemplate::LocalPodman { container } => {
            let mut backend = backend_container(container, allocation, overrides);
            backend.workspace_storage = (&container.workspace_storage).into();
            targets::TargetTemplate::LocalPodman(backend)
        }
        TargetTemplate::LocalDocker { container } => targets::TargetTemplate::LocalDocker(
            backend_container(container, allocation, overrides),
        ),
        TargetTemplate::AppleContainer { container } => targets::TargetTemplate::AppleContainer(
            backend_container(container, allocation, overrides),
        ),
        TargetTemplate::SshAppleContainer { ssh, container } => {
            targets::TargetTemplate::SshAppleContainer {
                ssh: SshTarget::from(ssh),
                container: backend_container(container, allocation, overrides),
            }
        }
        TargetTemplate::AwsEc2 { .. } => bail!(mj_core::config::LEGACY_EC2_TARGET_NOTICE),
        TargetTemplate::SshBare {
            ssh,
            workspace_prefix,
            ..
        } => targets::TargetTemplate::SshBare {
            ssh: SshTarget::from(ssh),
            workspace_prefix: workspace_prefix.to_string_lossy().into_owned(),
        },
        TargetTemplate::SshPodman { ssh, container, .. } => {
            let mut backend = backend_container(container, allocation, overrides);
            backend.workspace_storage = (&container.workspace_storage).into();
            targets::TargetTemplate::SshPodman {
                ssh: SshTarget::from(ssh),
                container: backend,
            }
        }
        TargetTemplate::SshDocker { ssh, container, .. } => targets::TargetTemplate::SshDocker {
            ssh: SshTarget::from(ssh),
            container: backend_container(container, allocation, overrides),
        },
        TargetTemplate::SshNitro { ssh, container } => targets::TargetTemplate::SshNitro {
            ssh: SshTarget::from(ssh),
            container: backend_container_with(container, allocation, overrides, true),
        },
    })
}

pub(super) fn configure_github_token_environment(target: &mut targets::TargetTemplate) -> bool {
    let container = match target {
        targets::TargetTemplate::LocalPodman(container)
        | targets::TargetTemplate::LocalDocker(container)
        | targets::TargetTemplate::AppleContainer(container)
        | targets::TargetTemplate::SshPodman { container, .. }
        | targets::TargetTemplate::SshDocker { container, .. }
        | targets::TargetTemplate::SshAppleContainer { container, .. }
        | targets::TargetTemplate::SshNitro { container, .. } => container,
        targets::TargetTemplate::LocalBare | targets::TargetTemplate::SshBare { .. } => {
            return false;
        }
    };
    container
        .extra_run_args
        .extend(["--env".to_owned(), "GH_TOKEN".to_owned()]);
    true
}

pub(super) fn use_github_https_urls(bundle: &mut targets::ProjectBundleSpec) {
    for repository in &mut bundle.repositories {
        for source in repository
            .url
            .iter_mut()
            .chain(repository.push_urls.iter_mut())
        {
            if let Some(github) = crate::import::github_repository_from_origin(source) {
                *source = format!(
                    "https://github.com/{}/{}.git",
                    github.owner, github.repository
                );
            }
        }
    }
}

fn backend_container(
    container: &mj_core::config::ContainerTemplate,
    allocation: Option<&SessionResourceAllocation>,
    overrides: ContainerOverrides,
) -> ContainerTemplate {
    backend_container_with(container, allocation, overrides, false)
}

/// A container environment as the engine receives it: the `--env` arguments,
/// and the values those arguments leave out.
///
/// An entry whose value comes from a reference (`from_env`, `from_secret`) or
/// from the controller's own environment is a secret. When the engine runs
/// under `sudo`, which logs every command line, a secret is named by a
/// valueless `--env NAME` and its value travels in the ssh command's
/// environment instead (see `CommandPlan::provide_target_environment_secrets`).
/// One function builds both lists, so an argument and its value cannot
/// disagree. Everything else stays `--env NAME=value`.
pub(super) struct ContainerEnvironment {
    pub args: Vec<String>,
    pub secrets: Vec<(String, String)>,
}

pub(super) fn container_environment(
    container: &mj_core::config::ContainerTemplate,
    overrides: ContainerOverrides,
    secrets_out_of_band: bool,
) -> ContainerEnvironment {
    let mut environment = container.environment.resolved().clone();
    if overrides.no_github_credentials {
        mj_core::credentials::remove_github_authentication_environment(&mut environment);
    }
    let mut args = Vec::new();
    let mut secrets = Vec::new();
    for (key, value) in environment {
        if secrets_out_of_band && container.environment.is_secret_sourced(&key) {
            args.extend(["--env".to_owned(), key.clone()]);
            secrets.push((key, value));
        } else {
            args.extend(["--env".to_owned(), format!("{key}={value}")]);
        }
    }
    ContainerEnvironment { args, secrets }
}

fn backend_container_with(
    container: &mj_core::config::ContainerTemplate,
    allocation: Option<&SessionResourceAllocation>,
    overrides: ContainerOverrides,
    secrets_out_of_band: bool,
) -> ContainerTemplate {
    let mut extra_run_args = Vec::new();
    if let Some(platform) = &container.platform {
        extra_run_args.push(format!("--platform={platform}"));
    }
    let (cpus, memory) = match allocation {
        Some(SessionResourceAllocation::Container { cpus, memory_bytes }) => {
            (Some(cpus.to_string()), Some(memory_bytes.to_string()))
        }
        _ => (container.cpus.clone(), container.memory.clone()),
    };
    if let Some(cpus) = cpus {
        extra_run_args.push(format!("--cpus={cpus}"));
    }
    if let Some(memory) = memory {
        extra_run_args.push(format!("--memory={memory}"));
    }
    extra_run_args.extend(container_environment(container, overrides, secrets_out_of_band).args);
    ContainerTemplate {
        image: container.image.clone(),
        pull_policy: container.pull_policy,
        extra_run_args,
        workspace_storage: targets::PodmanWorkspaceStorage::ContainerLayer,
        build_cache: container.build_cache.clone(),
    }
}

pub fn validate_resource_allocation(
    target_kind: impl Into<ResourceAllocationKind>,
    allocation: Option<&SessionResourceAllocation>,
) -> Result<()> {
    if let Some(allocation) = allocation {
        allocation.validate()?;
    }
    match (target_kind.into(), allocation) {
        (_, None)
        | (ResourceAllocationKind::Container, Some(SessionResourceAllocation::Container { .. }))
        | (ResourceAllocationKind::AwsEc2, Some(SessionResourceAllocation::AwsEc2 { .. })) => {
            Ok(())
        }
        (ResourceAllocationKind::Fixed, Some(_)) => {
            bail!(mj_core::state::BARE_TARGET_FIXED_RESOURCES)
        }
        _ => bail!("resource allocation does not match the selected target kind"),
    }
}

/// How long a freshly launched EC2 instance may take to accept SSH.
const AWS_SSH_READY_TIMEOUT: Duration = Duration::from_secs(300);

const AWS_SSH_READY_RETRY_DELAY: Duration = Duration::from_secs(3);

/// Poll a remote host until it accepts SSH, or until the deadline passes.
///
/// `now` and `sleep` are injected so tests can drive the deadline without
/// waiting in real time.
fn wait_for_ssh_ready(
    executor: &impl CommandExecutor,
    probe: &CommandSpec,
    timeout: Duration,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    let started = now();
    loop {
        if executor.cancellation_requested() {
            bail!("cancelled while waiting for SSH on the new instance");
        }
        let failure = match executor.execute(probe) {
            Ok(output) if output.status == 0 => return Ok(()),
            Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_string(),
            Err(error) => error.to_string(),
        };
        if now().duration_since(started) >= timeout {
            bail!(
                "{} timed out after {}s: {}",
                probe.purpose,
                timeout.as_secs(),
                if failure.is_empty() {
                    "the SSH probe reported no error output"
                } else {
                    failure.as_str()
                }
            );
        }
        sleep(AWS_SSH_READY_RETRY_DELAY);
    }
}

/// Wait for a newly provisioned EC2 host using the same readiness policy as
/// session-owned instances. Call from supervised blocking work.
pub fn wait_for_ec2_ssh(executor: &impl CommandExecutor, ssh: &SshTarget) -> Result<()> {
    wait_for_ssh_ready(
        executor,
        &crate::targets::ssh_command(SshTrafficClass::Control, ssh, ["true"])
            .purpose("wait for EC2 SSH availability")
            .stage(ProvisionStage::Booting),
        AWS_SSH_READY_TIMEOUT,
        Instant::now,
        std::thread::sleep,
    )
}

/// Wait until a rebooting EC2 host answers SSH from a boot other than
/// `previous_boot_id`, so an answer from the old boot, just before it goes
/// down, cannot pass for the new one. Call from supervised blocking work.
pub fn wait_for_ec2_reboot(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    previous_boot_id: &str,
) -> Result<()> {
    wait_for_ssh_ready(
        executor,
        &crate::targets::ssh_command(
            SshTrafficClass::Control,
            ssh,
            [
                "sh",
                "-c",
                "test \"$(cat /proc/sys/kernel/random/boot_id)\" != \"$1\"",
                "sh",
                previous_boot_id,
            ],
        )
        .purpose("wait for the EC2 host to come back from its reboot")
        .stage(ProvisionStage::Booting),
        AWS_SSH_READY_TIMEOUT,
        Instant::now,
        std::thread::sleep,
    )
}

pub(super) fn locator_after_provision_named(
    canonical: &TargetTemplate,
    backend: &targets::TargetTemplate,
    session_id: &str,
    _first_output: Option<&CommandOutput>,
    _executor: &(impl CommandExecutor + Sync),
    name: &str,
) -> Result<TargetLocator> {
    let generated = name.to_owned();

    Ok(match canonical {
        TargetTemplate::LocalBare => TargetLocator::LocalBare {
            worker_root: data_dir().join("workers").join(session_id),
        },
        TargetTemplate::LocalPodman { .. } => {
            let targets::TargetTemplate::LocalPodman(container) = backend else {
                bail!("session locator/template mismatch")
            };
            TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: generated,
                workspace_storage: PodmanWorkspaceLocator::from(
                    targets::podman_workspace_locator_named(container, name)?,
                ),
            }
        }
        TargetTemplate::LocalDocker { .. } => TargetLocator::LocalDocker {
            borrowed_from: None,
            container_id: generated,
        },
        TargetTemplate::AppleContainer { .. } => TargetLocator::AppleContainer {
            borrowed_from: None,
            container_id: generated,
        },
        TargetTemplate::SshBare { ssh, .. } => TargetLocator::SshBare {
            host: ssh.host.clone(),
            workspace: PathBuf::from(targets::workspace_for(backend, session_id)?),
            worker_id: None,
        },
        TargetTemplate::SshPodman { ssh, .. } => {
            let targets::TargetTemplate::SshPodman { container, .. } = backend else {
                bail!("session locator/template mismatch")
            };
            TargetLocator::SshPodman {
                borrowed_from: None,
                host: ssh.host.clone(),
                container_id: generated,
                workspace_storage: PodmanWorkspaceLocator::from(
                    targets::podman_workspace_locator_named(container, name)?,
                ),
            }
        }
        TargetTemplate::SshDocker { ssh, .. } => TargetLocator::SshDocker {
            borrowed_from: None,
            host: ssh.host.clone(),
            container_id: generated,
        },
        TargetTemplate::SshAppleContainer { ssh, .. } => TargetLocator::SshAppleContainer {
            borrowed_from: None,
            host: ssh.host.clone(),
            container_id: generated,
        },
        TargetTemplate::SshNitro { ssh, .. } => TargetLocator::SshNitro {
            borrowed_from: None,
            host: ssh.host.clone(),
            container_id: generated,
        },
        TargetTemplate::AwsEc2 { .. } => bail!(mj_core::config::LEGACY_EC2_TARGET_NOTICE),
    })
}

#[cfg(test)]
mod tests;
