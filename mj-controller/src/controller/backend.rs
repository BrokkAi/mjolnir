//! Backend target, locator, and capacity conversion for provisioned sessions.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{AwsAddressSource, Config, ProjectBundle, TargetTemplate, data_dir};
use mj_core::state::{
    PodmanWorkspaceLocator, SessionRecord, SessionResourceAllocation, TargetLocator,
    allocation_cpus,
};

use crate::targets::{
    self, AwsTemplate, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate,
    ImageRefresh, ProjectBundleSpec, ProvisionStage, RepositorySpec, SshTarget,
};

use super::{Controller, execute_checked};

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
        let output = executor.execute(&targets::command_on_locator(
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
                // A plain folder opened through `mj go` has no branch. Say so
                // briefly rather than echoing multi-line git stderr, which wraps
                // the banner.
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
                &backend, session_id, command, purpose,
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
        let numstat = run(
            &["--no-optional-locks", "diff", "--numstat", "HEAD"],
            "count changed lines",
        )?
        .unwrap_or_default();
        let porcelain = run(
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ],
            "list changed files",
        )?
        .unwrap_or_default();
        Ok(mj_core::local_git::parse_git_status(
            cwd,
            &branch,
            ahead_behind.as_deref(),
            &numstat,
            &porcelain,
        ))
    }

    pub fn resolve_aws_resource_options(
        &self,
        target_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<Vec<SessionResourceAllocation>> {
        let TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            launch_template,
            launch_template_version,
            ..
        } = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?
        else {
            bail!("target {target_id:?} is not an AWS EC2 target");
        };
        let profile = aws_profile.as_deref().unwrap_or("default");
        let launch_key = if launch_template.starts_with("lt-") {
            "--launch-template-id"
        } else {
            "--launch-template-name"
        };
        let version = launch_template_version.as_deref().unwrap_or("$Default");
        let describe_template = CommandSpec::new(
            "aws",
            [
                "--profile",
                profile,
                "--region",
                region,
                "ec2",
                "describe-launch-template-versions",
                launch_key,
                launch_template,
                "--versions",
                version,
                "--output",
                "json",
            ],
        )
        .purpose("resolve EC2 launch template instance family");
        let output = executor.execute(&describe_template)?;
        if output.status != 0 {
            bail!(
                "{} failed with status {}: {}",
                describe_template.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("parse EC2 launch template response")?;
        let instance_type = response
            .pointer("/LaunchTemplateVersions/0/LaunchTemplateData/InstanceType")
            .and_then(serde_json::Value::as_str)
            .context("launch template does not specify a concrete instance type")?;
        let family = instance_type
            .rsplit_once('.')
            .map(|(family, _)| family)
            .context("launch template instance type has no size suffix")?;
        let filter = format!("Name=instance-type,Values={family}.*");
        let describe_types = CommandSpec::new(
            "aws",
            [
                "--profile",
                profile,
                "--region",
                region,
                "ec2",
                "describe-instance-types",
                "--filters",
                &filter,
                "--output",
                "json",
            ],
        )
        .purpose("discover EC2 instance sizes");
        let output = executor.execute(&describe_types)?;
        if output.status != 0 {
            bail!(
                "{} failed with status {}: {}",
                describe_types.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("parse EC2 instance type response")?;
        let mut options = response
            .get("InstanceTypes")
            .and_then(serde_json::Value::as_array)
            .context("EC2 instance type response omitted InstanceTypes")?
            .iter()
            .filter_map(|entry| {
                Some(SessionResourceAllocation::AwsEc2 {
                    instance_type: entry.get("InstanceType")?.as_str()?.to_owned(),
                    vcpus: entry.pointer("/VCpuInfo/DefaultVCpus")?.as_u64()?,
                    memory_bytes: entry
                        .pointer("/MemoryInfo/SizeInMiB")?
                        .as_u64()?
                        .checked_mul(1024 * 1024)?,
                })
            })
            .collect::<Vec<_>>();
        options.sort_by_key(allocation_cpus);
        if !options.iter().any(|option| allocation_cpus(option) == 8) {
            bail!("EC2 family {family:?} has no exact 8-vCPU baseline size");
        }
        Ok(options)
    }

    pub fn reconnect_command(&self, session_id: &str) -> Result<CommandSpec> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.validate_configuration(&self.config)?;
        let locator = session.target.as_ref().context("session has no target")?;
        let backend = backend_locator(locator, session, &self.config)?;
        targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")
    }

    /// Whether this session's harness home belongs to the session rather than
    /// to the user, resolved from the session record.
    ///
    /// The stored locator is not the backend locator the decision is made
    /// against, and only this module can build one, so callers elsewhere ask
    /// here instead of restating the conversion.
    pub(crate) fn session_owns_profile_home(&self, session_id: &str) -> Result<bool> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .with_context(|| format!("unknown profile {}", session.last_profile))?;
        let locator = session.target.as_ref().context("session has no target")?;
        let backend = backend_locator(locator, session, &self.config)?;
        Ok(crate::controller::session_owns_profile_home(
            &backend, session_id, profile,
        ))
    }

    pub fn resource_probe(&self, session_id: &str) -> Result<targets::SessionResourceProbe> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let locator = session.target.as_ref().context("session has no target")?;
        let backend = backend_locator(locator, session, &self.config)?;
        targets::resource_probe(&backend, session_id)
    }

    pub fn deployment_capacity_targets(&self) -> Vec<targets::DeploymentCapacityTarget> {
        use targets::{DeploymentCapacityKind, DeploymentCapacityTarget};

        let mut local_ids = Vec::new();
        let mut ssh_hosts: BTreeMap<String, (Vec<String>, Vec<CommandSpec>)> = BTreeMap::new();
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
                | TargetTemplate::SshDocker { ssh, .. } => {
                    let entry = ssh_hosts.entry(ssh.host.clone()).or_default();
                    entry.0.push(target_id.clone());
                    let command = targets::ssh_host_capacity_command(&SshTarget::from(ssh));
                    if !entry.1.contains(&command) {
                        entry.1.push(command);
                    }
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
                        probe_error,
                    });
                }
            }
        }
        if !local_ids.is_empty() {
            targets.push(DeploymentCapacityTarget {
                id: "local".into(),
                host: "local".into(),
                target_ids: local_ids,
                kind: DeploymentCapacityKind::Host,
                local: true,
                probes: Vec::new(),
                probe_error: None,
            });
        }
        targets.extend(ssh_hosts.into_iter().map(|(host, (target_ids, probes))| {
            DeploymentCapacityTarget {
                id: format!("ssh:{host}"),
                host,
                target_ids,
                kind: DeploymentCapacityKind::Host,
                local: false,
                probes,
                probe_error: None,
            }
        }));
        targets.sort_by(|left, right| left.id.cmp(&right.id));
        targets
    }

    pub fn test_target(&self, target_id: &str, executor: &impl CommandExecutor) -> Result<()> {
        let template = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?;
        preflight_target(template, executor)
    }
}

pub(super) fn preflight_target(
    template: &TargetTemplate,
    executor: &impl CommandExecutor,
) -> Result<()> {
    match template {
        TargetTemplate::LocalPodman { .. } => targets::verify_local_podman(executor)
            .map(|_| ())
            .map_err(|error| {
                anyhow::anyhow!(
                    "local Podman is not ready. Fix the problem below, then Retry launch: {error:#}"
                )
            }),
        TargetTemplate::LocalDocker { .. } => targets::verify_local_docker(executor)
            .map(|_| ())
            .map_err(|error| {
                anyhow::anyhow!(
                    "local Docker is not ready. Start Docker or fix the problem below, then Retry launch: {error:#}"
                )
            }),
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
                        "remote Podman is not ready on {}. Fix the problem below, then Retry launch: {error:#}",
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
                        "remote Docker preflight failed for {}. Fix the problem below, then Retry launch: {error:#}",
                        ssh.destination
                    )
                })
        }
        TargetTemplate::AppleContainer { .. } => {
            let command = CommandSpec::new("container", ["system", "status"])
                .purpose("preflight Apple container runtime")
                .stage(ProvisionStage::Provisioning);
            let output = executor.execute(&command).map_err(|error| {
                anyhow::anyhow!(
                    "Apple container is not ready. Fix the problem below, then Retry launch: {error}"
                )
            })?;
            if output.status != 0 {
                bail!(
                    "Apple container is not ready. Start the runtime with `container system start`, then Retry launch: container system status exited {}: {}",
                    output.status,
                    [
                        String::from_utf8_lossy(&output.stdout).trim(),
                        String::from_utf8_lossy(&output.stderr).trim(),
                    ]
                    .into_iter()
                    .filter(|message| !message.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
                );
            }
            Ok(())
        }
        TargetTemplate::SshBare { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            let command = targets::ssh_connectivity_probe(&ssh);
            let output = executor.execute(&command)?;
            ensure!(
                output.status == 0,
                "SSH connectivity test failed for {} with status {}: {}",
                ssh.destination,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        }
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            launch_template,
            launch_template_version,
            ..
        } => {
            let mut identity_args = vec!["sts".into(), "get-caller-identity".into()];
            if let Some(profile) = aws_profile {
                identity_args.extend(["--profile".into(), profile.clone()]);
            }
            let identity = CommandSpec::new("aws", identity_args)
                .purpose("verify AWS credentials")
                .stage(ProvisionStage::Provisioning);
            let output = executor.execute(&identity)?;
            ensure!(
                output.status == 0,
                "AWS credential test failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );

            let mut launch_args = vec![
                "ec2".into(),
                "describe-launch-template-versions".into(),
                "--region".into(),
                region.clone(),
                "--launch-template-name".into(),
                launch_template.clone(),
                "--versions".into(),
                launch_template_version
                    .clone()
                    .unwrap_or_else(|| "$Default".into()),
            ];
            if let Some(profile) = aws_profile {
                launch_args.extend(["--profile".into(), profile.clone()]);
            }
            let launch = CommandSpec::new("aws", launch_args)
                .purpose("verify AWS launch template")
                .stage(ProvisionStage::Provisioning);
            let output = executor.execute(&launch)?;
            ensure!(
                output.status == 0,
                "AWS launch-template test failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        }
        TargetTemplate::LocalBare => Ok(()),
    }
}

pub(super) fn backend_bundle(
    bundle: &ProjectBundle,
    executor: &impl CommandExecutor,
) -> Result<ProjectBundleSpec> {
    let primary = bundle.primary().context("bundle primary is missing")?;
    Ok(ProjectBundleSpec {
        primary: primary.destination.to_string_lossy().into_owned(),
        repositories: bundle
            .repositories
            .iter()
            .map(|repository| {
                let source = mj_core::remote_git::resolve_repository(repository, executor)
                    .with_context(|| format!("repository {:?}", repository.id))?;
                Ok(RepositorySpec {
                    url: Some(source.fetch_url),
                    push_urls: source.push_urls,
                    destination: repository.destination.to_string_lossy().into_owned(),
                    git_ref: None,
                    reference: None,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

/// Per-session container size overrides. They win over both the target
/// template's values and any recorded resource allocation, and they are read
/// only while a container is being created.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ContainerOverrides<'a> {
    pub cpus: Option<&'a str>,
    pub memory: Option<&'a str>,
}

impl<'a> ContainerOverrides<'a> {
    pub(super) fn for_session(session: &'a SessionRecord) -> Self {
        Self {
            cpus: session.container_cpus.as_deref(),
            memory: session.container_memory.as_deref(),
        }
    }
}

pub(super) fn backend_target(
    template: &TargetTemplate,
    allocation: Option<&SessionResourceAllocation>,
    overrides: ContainerOverrides<'_>,
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
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            launch_template,
            launch_template_version,
            ssh_user,
            identity_file,
            ssh_args,
            ..
        } => targets::TargetTemplate::AwsEc2(AwsTemplate {
            profile: aws_profile.clone().unwrap_or_else(|| "default".into()),
            region: region.clone(),
            launch_template: launch_template.clone(),
            launch_template_version: launch_template_version.clone(),
            instance_type: match allocation {
                Some(SessionResourceAllocation::AwsEc2 { instance_type, .. }) => {
                    Some(instance_type.clone())
                }
                _ => None,
            },
            // The address is filled after describe-instances.
            ssh: SshTarget {
                destination: format!("{ssh_user}@pending.invalid"),
                ssh_args: targets::ssh_args_with_identity(ssh_args, identity_file.as_deref()),
            },
        }),
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
    })
}

/// Every container image the daemon downloads in the background, once per
/// (host, image, platform).
///
/// Every container target is covered, including Apple's `container` engine.
/// A `never` policy is the one opt-out. The rest differ only in when they
/// download: `always` and `newer` pull on every refresh, while the others
/// pull only when the host has no copy of the image.
///
/// Several targets often share one image on one host, and that needs one
/// download. When two such targets disagree about when to pull, the merged
/// entry takes the more eager of the two.
pub fn image_refresh_plan(config: &Config) -> Vec<ImageRefresh> {
    let mut plan: Vec<ImageRefresh> = Vec::new();
    for target in config.targets.values() {
        let Some((host, container)) = target.image_host() else {
            continue;
        };
        let Some(refresh) = targets::image_refresh(
            host,
            &container.image,
            container.platform.as_deref(),
            container.pull_policy,
        ) else {
            continue;
        };
        // Commands are decided by the host, the image, and the platform alone,
        // so those three identify the duplicates worth collapsing.
        if let Some(existing) = plan.iter_mut().find(|entry| {
            entry.host == refresh.host
                && entry.image == refresh.image
                && entry.platform == refresh.platform
        }) {
            existing.when = existing.when.max(refresh.when);
            continue;
        }
        plan.push(refresh);
    }
    plan
}

pub(crate) fn controller_github_token() -> Option<String> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(name)
            && let Some(token) = usable_github_token(&token)
        {
            return Some(token.to_owned());
        }
    }
    let output = match Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(%error, "could not query the GitHub CLI for a token");
            return None;
        }
    };
    if !output.status.success() {
        tracing::debug!(status = ?output.status, "GitHub CLI did not return an authenticated token");
        return None;
    }
    let token = match std::str::from_utf8(&output.stdout) {
        Ok(token) => token,
        Err(error) => {
            tracing::debug!(%error, "GitHub CLI returned a non-UTF-8 token");
            return None;
        }
    };
    let Some(token) = usable_github_token(token) else {
        tracing::debug!("GitHub CLI returned an empty or invalid token");
        return None;
    };
    Some(token.to_owned())
}

fn usable_github_token(token: &str) -> Option<&str> {
    let token = token.trim();
    (!token.is_empty() && !token.chars().any(char::is_whitespace)).then_some(token)
}

pub(super) fn configure_github_token_environment(target: &mut targets::TargetTemplate) -> bool {
    let container = match target {
        targets::TargetTemplate::LocalPodman(container)
        | targets::TargetTemplate::LocalDocker(container)
        | targets::TargetTemplate::AppleContainer(container)
        | targets::TargetTemplate::SshPodman { container, .. }
        | targets::TargetTemplate::SshDocker { container, .. } => container,
        targets::TargetTemplate::LocalBare
        | targets::TargetTemplate::AwsEc2(_)
        | targets::TargetTemplate::SshBare { .. } => return false,
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
            if let Some(github) = crate::setup::github_repository_from_origin(source) {
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
    overrides: ContainerOverrides<'_>,
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
    // The session's own overrides are the last word on size.
    let cpus = overrides.cpus.map(str::to_owned).or(cpus);
    let memory = overrides.memory.map(str::to_owned).or(memory);
    if let Some(cpus) = cpus {
        extra_run_args.push(format!("--cpus={cpus}"));
    }
    if let Some(memory) = memory {
        extra_run_args.push(format!("--memory={memory}"));
    }
    for (key, value) in &container.environment {
        extra_run_args.extend(["--env".to_string(), format!("{key}={value}")]);
    }
    ContainerTemplate {
        image: container.image.clone(),
        pull_policy: container.pull_policy,
        extra_run_args,
        workspace_storage: targets::PodmanWorkspaceStorage::ContainerLayer,
        build_cache: container.build_cache.clone(),
    }
}

pub(super) fn validate_resource_allocation(
    template: &TargetTemplate,
    allocation: Option<&SessionResourceAllocation>,
) -> Result<()> {
    if let Some(allocation) = allocation {
        allocation.validate()?;
    }
    match (template, allocation) {
        (_, None)
        | (
            TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. },
            Some(SessionResourceAllocation::Container { .. }),
        )
        | (TargetTemplate::AwsEc2 { .. }, Some(SessionResourceAllocation::AwsEc2 { .. })) => Ok(()),
        (TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }, Some(_)) => {
            bail!("bare targets have fixed host resources")
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

pub(super) fn locator_after_provision(
    canonical: &TargetTemplate,
    backend: &targets::TargetTemplate,
    session_id: &str,
    first_output: Option<&CommandOutput>,
    executor: &(impl CommandExecutor + Sync),
) -> Result<TargetLocator> {
    let generated = targets::resource_name(session_id)?;
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
                workspace_storage: PodmanWorkspaceLocator::from(targets::podman_workspace_locator(
                    container, session_id,
                )?),
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
                workspace_storage: PodmanWorkspaceLocator::from(targets::podman_workspace_locator(
                    container, session_id,
                )?),
            }
        }
        TargetTemplate::SshDocker { ssh, .. } => TargetLocator::SshDocker {
            borrowed_from: None,
            host: ssh.host.clone(),
            container_id: generated,
        },
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            ssh_user,
            address_source,
            identity_file,
            ssh_args,
            ..
        } => {
            let output = first_output.context("AWS launch produced no output")?;
            let json: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("parse aws ec2 run-instances response")?;
            let instance_id = json
                .pointer("/Instances/0/InstanceId")
                .and_then(serde_json::Value::as_str)
                .context("AWS response omitted instance ID")?
                .to_string();
            let profile = aws_profile.clone().unwrap_or_else(|| "default".into());
            execute_checked(
                executor,
                CommandSpec::new(
                    "aws",
                    [
                        "--profile".into(),
                        profile.clone(),
                        "--region".into(),
                        region.clone(),
                        "ec2".into(),
                        "wait".into(),
                        "instance-running".into(),
                        "--instance-ids".into(),
                        instance_id.clone(),
                    ],
                )
                .purpose("wait for EC2 session instance to run")
                .stage(ProvisionStage::Booting),
            )?;
            let field = match address_source {
                AwsAddressSource::PublicDns => "PublicDnsName",
                AwsAddressSource::PublicIp => "PublicIpAddress",
                AwsAddressSource::PrivateDns => "PrivateDnsName",
                AwsAddressSource::PrivateIp => "PrivateIpAddress",
            };
            let address = execute_checked(
                executor,
                CommandSpec::new(
                    "aws",
                    [
                        "--profile".into(),
                        profile.clone(),
                        "--region".into(),
                        region.clone(),
                        "ec2".into(),
                        "describe-instances".into(),
                        "--instance-ids".into(),
                        instance_id.clone(),
                        "--query".into(),
                        format!("Reservations[0].Instances[0].{field}"),
                        "--output".into(),
                        "text".into(),
                    ],
                )
                .purpose("resolve EC2 session address")
                .stage(ProvisionStage::Booting),
            )?;
            let address = String::from_utf8(address.stdout)
                .context("AWS address was not UTF-8")?
                .trim()
                .to_string();
            if address.is_empty() || address == "None" {
                bail!("AWS instance {instance_id} has no configured address");
            }
            let ssh = SshTarget {
                destination: format!("{ssh_user}@{address}"),
                ssh_args: targets::ssh_args_with_identity(ssh_args, identity_file.as_deref()),
            };
            wait_for_ssh_ready(
                executor,
                &crate::targets::ssh_command(&ssh, ["true"])
                    .purpose("wait for EC2 SSH availability")
                    .stage(ProvisionStage::Booting),
                AWS_SSH_READY_TIMEOUT,
                Instant::now,
                std::thread::sleep,
            )?;
            TargetLocator::AwsEc2 {
                instance_id,
                address: Some(address),
            }
        }
    })
}

/// The execution-plan locator for a session's stored target.
///
/// The mapping itself lives in `mj_core::targets`; this only pairs the stored
/// locator with the target template the session was created against.
pub(super) fn backend_locator(
    locator: &TargetLocator,
    session: &SessionRecord,
    config: &Config,
) -> Result<targets::TargetLocator> {
    let runtime = if session.target_runtime.is_some() || targets::locator_needs_connection(locator)
    {
        Some(session.target_runtime_settings(config)?)
    } else {
        None
    };
    Ok(targets::TargetLocator::try_from(targets::RecordedTarget {
        locator,
        runtime: runtime.as_deref(),
        session_id: &session.id,
    })?)
}

#[cfg(test)]
mod tests;
