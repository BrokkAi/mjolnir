//! Backend target, locator, and capacity conversion for provisioned sessions.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use hel::hel_config::{
    AwsAddressSource, HelConfig, PodmanWorkspaceStorage, ProjectBundle, TargetTemplate, data_dir,
};
use hel::hel_state::{
    PodmanWorkspaceLocator, SessionRecord, SessionResourceAllocation, TargetLocator,
    allocation_cpus,
};
use hel::hel_targets::{
    self, AwsTemplate, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate, ImageHost,
    ImageRefresh, ProjectBundleSpec, ProvisionStage, RepositorySpec, SshTarget,
};

use super::{Controller, backend_ssh, execute_checked, ssh_args_with_identity, ssh_command_spec};

impl Controller {
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
        let locator = session.target.as_ref().context("session has no target")?;
        let backend = backend_locator(locator, session, &self.config)?;
        hel_targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")
    }

    pub fn resource_probe(&self, session_id: &str) -> Result<hel_targets::SessionResourceProbe> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let locator = session.target.as_ref().context("session has no target")?;
        let backend = backend_locator(locator, session, &self.config)?;
        hel_targets::resource_probe(&backend, session_id)
    }

    pub fn deployment_capacity_targets(&self) -> Vec<hel_targets::DeploymentCapacityTarget> {
        use hel_targets::{DeploymentCapacityKind, DeploymentCapacityTarget};

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
                    let command = hel_targets::ssh_host_capacity_command(&backend_ssh(ssh));
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
                            hel_targets::aws_allocated_capacity_command(&locator, &session.id)
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
        TargetTemplate::LocalPodman { .. } => hel_targets::verify_local_podman(executor)
            .map(|_| ())
            .map_err(|error| {
                anyhow::anyhow!(
                    "local Podman preflight failed; run `mj doctor` for actionable prerequisites: {error:#}"
                )
            }),
        TargetTemplate::LocalDocker { .. } => hel_targets::verify_local_docker(executor)
            .map(|_| ())
            .map_err(|error| {
                anyhow::anyhow!(
                    "local Docker preflight failed; run `mj doctor` for actionable prerequisites: {error:#}"
                )
            }),
        TargetTemplate::SshPodman { ssh, .. } => {
            let ssh = backend_ssh(ssh);
            hel_targets::verify_ssh_podman(&ssh, executor)
                .map(|preflight| {
                    for warning in preflight.warnings {
                        executor.notify_notice(&warning.notice());
                    }
                })
                .map_err(|error| {
                    anyhow::anyhow!(
                        "remote Podman preflight failed for {}; run `mj doctor` for actionable prerequisites: {error:#}",
                        ssh.destination
                    )
                })
        }
        TargetTemplate::SshDocker { ssh, .. } => {
            let ssh = backend_ssh(ssh);
            hel_targets::verify_ssh_docker(&ssh, executor)
                .map(|_| ())
                .map_err(|error| {
                    anyhow::anyhow!(
                        "remote Docker preflight failed for {}; run `mj doctor` for actionable prerequisites: {error:#}",
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
                    "Apple container preflight failed; run `mj doctor` for actionable prerequisites: {error}"
                )
            })?;
            if output.status != 0 {
                bail!(
                    "Apple container preflight failed; run `mj doctor` for actionable prerequisites: container system status exited {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(())
        }
        TargetTemplate::SshBare { ssh, .. } => {
            let ssh = backend_ssh(ssh);
            let command = hel_targets::ssh_connectivity_probe(&ssh);
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

pub(super) fn backend_bundle(bundle: &ProjectBundle) -> Result<ProjectBundleSpec> {
    let primary = bundle.primary().context("bundle primary is missing")?;
    Ok(ProjectBundleSpec {
        primary: primary.destination.to_string_lossy().into_owned(),
        repositories: bundle
            .repositories
            .iter()
            .map(|repository| RepositorySpec {
                url: repository.github.as_deref().map(github_url),
                destination: repository.destination.to_string_lossy().into_owned(),
                git_ref: repository.git_ref.clone(),
                reference: None,
            })
            .collect(),
    })
}

fn github_url(source: &str) -> String {
    if source.contains("://") || source.starts_with("git@") {
        source.to_string()
    } else {
        format!("https://github.com/{}.git", source.trim_end_matches(".git"))
    }
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
) -> Result<hel_targets::TargetTemplate> {
    Ok(match template {
        TargetTemplate::LocalBare => hel_targets::TargetTemplate::LocalBare,
        TargetTemplate::LocalPodman { container } => {
            let mut backend = backend_container(container, allocation, overrides);
            backend.workspace_storage = backend_workspace_storage(&container.workspace_storage);
            hel_targets::TargetTemplate::LocalPodman(backend)
        }
        TargetTemplate::LocalDocker { container } => hel_targets::TargetTemplate::LocalDocker(
            backend_container(container, allocation, overrides),
        ),
        TargetTemplate::AppleContainer { container } => {
            hel_targets::TargetTemplate::AppleContainer(backend_container(
                container, allocation, overrides,
            ))
        }
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            launch_template,
            launch_template_version,
            ssh_user,
            identity_file,
            ssh_args,
            ..
        } => hel_targets::TargetTemplate::AwsEc2(AwsTemplate {
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
                ssh_args: ssh_args_with_identity(ssh_args, identity_file.as_deref()),
            },
        }),
        TargetTemplate::SshBare {
            ssh,
            workspace_prefix,
            ..
        } => hel_targets::TargetTemplate::SshBare {
            ssh: backend_ssh(ssh),
            workspace_prefix: workspace_prefix.to_string_lossy().into_owned(),
        },
        TargetTemplate::SshPodman { ssh, container, .. } => {
            let mut backend = backend_container(container, allocation, overrides);
            backend.workspace_storage = backend_workspace_storage(&container.workspace_storage);
            hel_targets::TargetTemplate::SshPodman {
                ssh: backend_ssh(ssh),
                container: backend,
            }
        }
        TargetTemplate::SshDocker { ssh, container, .. } => {
            hel_targets::TargetTemplate::SshDocker {
                ssh: backend_ssh(ssh),
                container: backend_container(container, allocation, overrides),
            }
        }
    })
}

/// Every container image a background refresh keeps current, once per
/// (host, image, platform).
///
/// Targets the host can already satisfy are left out: digest pins, versioned
/// tags, and the explicit `missing` and `never` policies. Apple's `container`
/// engine is left out too; it still refreshes its image during provisioning.
/// Several targets often share one image on one host, and that needs one pull.
pub fn image_refresh_plan(config: &HelConfig) -> Vec<ImageRefresh> {
    let mut plan: Vec<ImageRefresh> = Vec::new();
    for target in config.targets.values() {
        let (host, container) = match target {
            TargetTemplate::LocalPodman { container } => (ImageHost::LocalPodman, container),
            TargetTemplate::LocalDocker { container } => (ImageHost::LocalDocker, container),
            TargetTemplate::SshPodman { ssh, container } => {
                (ImageHost::SshPodman(backend_ssh(ssh)), container)
            }
            TargetTemplate::SshDocker { ssh, container } => {
                (ImageHost::SshDocker(backend_ssh(ssh)), container)
            }
            TargetTemplate::LocalBare
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::AwsEc2 { .. }
            | TargetTemplate::SshBare { .. } => continue,
        };
        // Commands are decided by the host, the image, and the platform alone,
        // so equal refreshes are exactly the duplicates worth collapsing.
        let Some(refresh) = hel_targets::image_refresh(
            host,
            &container.image,
            container.platform.as_deref(),
            container.pull_policy,
        ) else {
            continue;
        };
        if !plan.contains(&refresh) {
            plan.push(refresh);
        }
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

pub(super) fn configure_github_token_environment(target: &mut hel_targets::TargetTemplate) -> bool {
    let container = match target {
        hel_targets::TargetTemplate::LocalPodman(container)
        | hel_targets::TargetTemplate::LocalDocker(container)
        | hel_targets::TargetTemplate::AppleContainer(container)
        | hel_targets::TargetTemplate::SshPodman { container, .. }
        | hel_targets::TargetTemplate::SshDocker { container, .. } => container,
        hel_targets::TargetTemplate::LocalBare
        | hel_targets::TargetTemplate::AwsEc2(_)
        | hel_targets::TargetTemplate::SshBare { .. } => return false,
    };
    container
        .extra_run_args
        .extend(["--env".to_owned(), "GH_TOKEN".to_owned()]);
    true
}

pub(super) fn use_github_https_urls(bundle: &mut hel_targets::ProjectBundleSpec) {
    for repository in &mut bundle.repositories {
        let Some(source) = repository.url.as_deref() else {
            continue;
        };
        let Some(github) = crate::hel_setup::github_repository_from_origin(source) else {
            continue;
        };
        repository.url = Some(format!(
            "https://github.com/{}/{}.git",
            github.owner, github.repository
        ));
    }
}

fn backend_container(
    container: &hel::hel_config::ContainerTemplate,
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
        workspace_storage: hel_targets::PodmanWorkspaceStorage::ContainerLayer,
    }
}

fn backend_workspace_storage(
    storage: &PodmanWorkspaceStorage,
) -> hel_targets::PodmanWorkspaceStorage {
    match storage {
        PodmanWorkspaceStorage::PodmanVolume => hel_targets::PodmanWorkspaceStorage::PodmanVolume,
        PodmanWorkspaceStorage::HostHelper { root, helper } => {
            hel_targets::PodmanWorkspaceStorage::HostHelper {
                root: root.to_string_lossy().into_owned(),
                helper: helper.clone(),
            }
        }
        PodmanWorkspaceStorage::ContainerLayer => {
            hel_targets::PodmanWorkspaceStorage::ContainerLayer
        }
    }
}

fn backend_workspace_locator(
    storage: &PodmanWorkspaceLocator,
) -> hel_targets::PodmanWorkspaceLocator {
    match storage {
        PodmanWorkspaceLocator::ContainerLayer => {
            hel_targets::PodmanWorkspaceLocator::ContainerLayer
        }
        PodmanWorkspaceLocator::Volume { name } => {
            hel_targets::PodmanWorkspaceLocator::Volume { name: name.clone() }
        }
        PodmanWorkspaceLocator::HostPath {
            path,
            helper,
            resource,
        } => hel_targets::PodmanWorkspaceLocator::HostPath {
            path: path.to_string_lossy().into_owned(),
            helper: helper.clone(),
            resource: resource.clone(),
        },
    }
}

fn durable_workspace_locator(
    storage: hel_targets::PodmanWorkspaceLocator,
) -> PodmanWorkspaceLocator {
    match storage {
        hel_targets::PodmanWorkspaceLocator::ContainerLayer => {
            PodmanWorkspaceLocator::ContainerLayer
        }
        hel_targets::PodmanWorkspaceLocator::Volume { name } => {
            PodmanWorkspaceLocator::Volume { name }
        }
        hel_targets::PodmanWorkspaceLocator::HostPath {
            path,
            helper,
            resource,
        } => PodmanWorkspaceLocator::HostPath {
            path: PathBuf::from(path),
            helper,
            resource,
        },
    }
}

pub(super) fn validate_resource_allocation(
    template: &TargetTemplate,
    allocation: Option<&SessionResourceAllocation>,
) -> Result<()> {
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
    backend: &hel_targets::TargetTemplate,
    session_id: &str,
    first_output: Option<&CommandOutput>,
    executor: &(impl CommandExecutor + Sync),
) -> Result<TargetLocator> {
    let generated = hel_targets::resource_name(session_id)?;
    Ok(match canonical {
        TargetTemplate::LocalBare => TargetLocator::LocalBare {
            worker_root: data_dir().join("workers").join(session_id),
        },
        TargetTemplate::LocalPodman { .. } => {
            let hel_targets::TargetTemplate::LocalPodman(container) = backend else {
                bail!("session locator/template mismatch")
            };
            TargetLocator::LocalPodman {
                container_id: generated,
                workspace_storage: durable_workspace_locator(
                    hel_targets::podman_workspace_locator(container, session_id)?,
                ),
            }
        }
        TargetTemplate::LocalDocker { .. } => TargetLocator::LocalDocker {
            container_id: generated,
        },
        TargetTemplate::AppleContainer { .. } => TargetLocator::AppleContainer {
            container_id: generated,
        },
        TargetTemplate::SshBare { ssh, .. } => TargetLocator::SshBare {
            host: ssh.host.clone(),
            workspace: PathBuf::from(hel_targets::workspace_for(backend, session_id)?),
            worker_id: None,
        },
        TargetTemplate::SshPodman { ssh, .. } => {
            let hel_targets::TargetTemplate::SshPodman { container, .. } = backend else {
                bail!("session locator/template mismatch")
            };
            TargetLocator::SshPodman {
                host: ssh.host.clone(),
                container_id: generated,
                workspace_storage: durable_workspace_locator(
                    hel_targets::podman_workspace_locator(container, session_id)?,
                ),
            }
        }
        TargetTemplate::SshDocker { ssh, .. } => TargetLocator::SshDocker {
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
                ssh_args: ssh_args_with_identity(ssh_args, identity_file.as_deref()),
            };
            wait_for_ssh_ready(
                executor,
                &ssh_command_spec(&ssh, ["true"])
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

pub(super) fn backend_locator(
    locator: &TargetLocator,
    session: &SessionRecord,
    config: &HelConfig,
) -> Result<hel_targets::TargetLocator> {
    let template = config
        .targets
        .get(&session.target_template_id)
        .context("session target template is missing")?;
    Ok(match locator {
        TargetLocator::LocalBare { worker_root } => {
            let TargetTemplate::LocalBare = template else {
                bail!("session locator/template mismatch")
            };
            hel_targets::TargetLocator::LocalBare {
                worker_root: worker_root.to_string_lossy().into_owned(),
            }
        }
        TargetLocator::LocalPodman {
            container_id,
            workspace_storage,
        } => hel_targets::TargetLocator::LocalPodman {
            container_id: container_id.clone(),
            workspace_storage: backend_workspace_locator(workspace_storage),
        },
        TargetLocator::LocalDocker { container_id } => hel_targets::TargetLocator::LocalDocker {
            container_id: container_id.clone(),
        },
        TargetLocator::AppleContainer { container_id } => {
            hel_targets::TargetLocator::AppleContainer {
                container_id: container_id.clone(),
            }
        }
        TargetLocator::SshBare { workspace, .. } => {
            let TargetTemplate::SshBare { ssh, .. } = template else {
                bail!("session locator/template mismatch")
            };
            hel_targets::TargetLocator::SshBare {
                ssh: backend_ssh(ssh),
                workspace: workspace.to_string_lossy().into_owned(),
            }
        }
        TargetLocator::SshPodman {
            container_id,
            workspace_storage,
            ..
        } => {
            let TargetTemplate::SshPodman { ssh, .. } = template else {
                bail!("session locator/template mismatch")
            };
            hel_targets::TargetLocator::SshPodman {
                ssh: backend_ssh(ssh),
                container_id: container_id.clone(),
                workspace_storage: backend_workspace_locator(workspace_storage),
            }
        }
        TargetLocator::SshDocker { host, container_id } => {
            let TargetTemplate::SshDocker { ssh, .. } = template else {
                bail!("session locator/template mismatch")
            };
            ensure!(
                host == &ssh.host,
                "session locator/template SSH host mismatch"
            );
            hel_targets::TargetLocator::SshDocker {
                ssh: backend_ssh(ssh),
                container_id: container_id.clone(),
            }
        }
        TargetLocator::AwsEc2 {
            instance_id,
            address,
        } => {
            let TargetTemplate::AwsEc2 {
                aws_profile,
                region,
                ssh_user,
                identity_file,
                ssh_args,
                ..
            } = template
            else {
                bail!("session locator/template mismatch")
            };
            let address = address.as_deref().context("AWS locator has no address")?;
            hel_targets::TargetLocator::AwsEc2 {
                profile: aws_profile.clone().unwrap_or_else(|| "default".into()),
                region: region.clone(),
                instance_id: instance_id.clone(),
                ssh: SshTarget {
                    destination: format!("{ssh_user}@{address}"),
                    ssh_args: ssh_args_with_identity(ssh_args, identity_file.as_deref()),
                },
                workspace: format!(".local/share/hel/workspaces/{}", session.id),
            }
        }
    })
}

pub(super) fn absolute_target_path(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    path: &str,
) -> Result<String> {
    if path.starts_with('/') {
        return Ok(path.to_owned());
    }
    let output = execute_checked(
        executor,
        hel_targets::command_on_locator(
            locator,
            session_id,
            vec!["pwd".into()],
            "resolve target home directory",
        )?,
    )?;
    let directory = String::from_utf8(output.stdout).context("decode target working directory")?;
    let directory = directory.trim_end_matches(['\r', '\n', '/']);
    if directory.is_empty() || !directory.starts_with('/') {
        bail!("target returned an invalid working directory {directory:?}");
    }
    Ok(format!("{directory}/{path}"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::Result;

    use crate::hel_controller::Controller;
    use crate::hel_controller::provisioning::install_attached_resources;
    use hel::hel_config::{
        AwsAddressSource, ContainerTemplate as ConfigContainer, HelConfig, ProjectBundle,
        ProjectRepository, SshConnection, TargetTemplate,
    };
    use hel::hel_state::{HelState, SessionRecord, SessionState};
    use hel::hel_targets::{
        self, AdditionalMount, CommandExecutor, CommandOutput, CommandSpec, ContainerTemplate,
        SshTarget,
    };

    use super::*;

    /// A fake executor that fails the SSH probe a fixed number of times.
    struct SshProbeExecutor {
        failures_remaining: RefCell<u32>,
        attempts: RefCell<u32>,
        cancel_after: Option<u32>,
    }
    impl SshProbeExecutor {
        fn new(failures: u32) -> Self {
            Self {
                failures_remaining: RefCell::new(failures),
                attempts: RefCell::new(0),
                cancel_after: None,
            }
        }
    }
    impl CommandExecutor for SshProbeExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            *self.attempts.borrow_mut() += 1;
            let mut remaining = self.failures_remaining.borrow_mut();
            if *remaining == 0 {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            *remaining -= 1;
            Ok(CommandOutput {
                status: 255,
                stdout: Vec::new(),
                stderr: b"ssh: connect to host 10.0.0.1 port 22: Connection refused\n".to_vec(),
            })
        }

        fn cancellation_requested(&self) -> bool {
            self.cancel_after
                .is_some_and(|limit| *self.attempts.borrow() >= limit)
        }
    }
    fn ssh_probe_spec() -> CommandSpec {
        CommandSpec::new("ssh", ["host", "true"]).purpose("wait for EC2 SSH availability")
    }
    /// A virtual clock advanced only by the injected sleep hook.
    fn virtual_clock() -> (std::rc::Rc<std::cell::Cell<Instant>>, Instant) {
        let start = Instant::now();
        (std::rc::Rc::new(std::cell::Cell::new(start)), start)
    }
    #[test]
    fn ssh_readiness_wait_succeeds_after_failed_probes() {
        let executor = SshProbeExecutor::new(3);
        let (clock, _) = virtual_clock();
        let sleep_clock = clock.clone();
        wait_for_ssh_ready(
            &executor,
            &ssh_probe_spec(),
            Duration::from_secs(300),
            {
                let clock = clock.clone();
                move || clock.get()
            },
            move |delay| sleep_clock.set(sleep_clock.get() + delay),
        )
        .expect("the wait succeeds once SSH answers");
        assert_eq!(*executor.attempts.borrow(), 4);
    }
    #[test]
    fn ssh_readiness_wait_gives_up_at_the_deadline_and_reports_the_last_error() {
        let executor = SshProbeExecutor::new(u32::MAX);
        let (clock, _) = virtual_clock();
        let sleep_clock = clock.clone();
        let error = wait_for_ssh_ready(
            &executor,
            &ssh_probe_spec(),
            Duration::from_secs(30),
            {
                let clock = clock.clone();
                move || clock.get()
            },
            move |delay| sleep_clock.set(sleep_clock.get() + delay),
        )
        .expect_err("the wait stops at the deadline");
        let message = error.to_string();
        assert!(message.contains("timed out after 30s"), "{message}");
        assert!(message.contains("Connection refused"), "{message}");
    }
    #[test]
    fn ssh_readiness_wait_stops_when_cancellation_is_requested() {
        let mut executor = SshProbeExecutor::new(u32::MAX);
        executor.cancel_after = Some(2);
        let (clock, _) = virtual_clock();
        let sleep_clock = clock.clone();
        let error = wait_for_ssh_ready(
            &executor,
            &ssh_probe_spec(),
            Duration::from_secs(300),
            {
                let clock = clock.clone();
                move || clock.get()
            },
            move |delay| sleep_clock.set(sleep_clock.get() + delay),
        )
        .expect_err("the wait stops when cancelled");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert_eq!(*executor.attempts.borrow(), 2);
    }
    #[test]
    fn aws_resources_are_compressed_into_one_streamed_ssh_command() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
            streams: RefCell<Vec<Vec<u8>>>,
        }
        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }

            fn execute_with_stdin(
                &self,
                command: &CommandSpec,
                input: &mut (dyn std::io::Read + Send),
            ) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                let mut stream = Vec::new();
                input.read_to_end(&mut stream)?;
                self.streams.borrow_mut().push(stream);
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("many/files")).unwrap();
        std::fs::write(source.path().join("many/files/one"), b"one").unwrap();
        std::fs::write(source.path().join("many/files/two"), b"two").unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let record = SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
            title: "AWS resources".into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "aws".into(),
            resource_allocation: None,
            additional_mounts: vec![AdditionalMount {
                source: source.path().to_path_buf(),
                destination: "/home/ubuntu/mj-resources/data".into(),
                read_only: false,
            }],
            state: SessionState::Disconnected,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-12T00:00:00Z".into(),
            updated_at: "2026-08-12T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        let state = HelState {
            version: hel::hel_state::STATE_VERSION,
            sessions: BTreeMap::from([(session_id.into(), record)]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        let backend = hel_targets::TargetLocator::AwsEc2 {
            profile: "default".into(),
            region: "us-east-1".into(),
            instance_id: "i-1234567890abcdef0".into(),
            ssh: SshTarget {
                destination: "ubuntu@example.test".into(),
                ssh_args: Vec::new(),
            },
            workspace: format!(".local/share/hel/workspaces/{session_id}"),
        };
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
            streams: RefCell::new(Vec::new()),
        };

        install_attached_resources(
            &state,
            session_id,
            &backend,
            ".local/share/hel/workers/session",
            &executor,
        )
        .unwrap();

        let commands = executor.commands.borrow();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "ssh");
        assert!(
            commands[0]
                .args
                .iter()
                .any(|argument| argument.contains("install-resource"))
        );
        let streams = executor.streams.borrow();
        assert_eq!(streams.len(), 1);
        assert_eq!(&streams[0][..2], &[0x1f, 0x8b]);
    }
    #[test]
    fn canonical_bundle_maps_github_shorthand_and_primary_destination() {
        let bundle = ProjectBundle {
            primary_repo: "app".into(),
            repositories: vec![ProjectRepository {
                id: "app".into(),
                github: Some("example/app".into()),
                local: None,
                destination: PathBuf::from("services/app"),
                git_ref: Some("main".into()),
            }],
        };
        let backend = backend_bundle(&bundle).unwrap();
        assert_eq!(backend.primary, "services/app");
        assert_eq!(
            backend.repositories[0].url.as_deref(),
            Some("https://github.com/example/app.git")
        );
    }
    #[test]
    fn container_resources_and_environment_become_argv() {
        let template = TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "dev:1".into(),
                pull_policy: hel::hel_config::ImagePullPolicy::Never,
                platform: Some("linux/arm64".into()),
                cpus: Some("4".into()),
                memory: Some("8g".into()),
                environment: std::collections::BTreeMap::from([("A".into(), "b c".into())]),
                workspace_storage: Default::default(),
            },
        };
        let hel_targets::TargetTemplate::LocalPodman(container) =
            backend_target(&template, None, ContainerOverrides::default()).unwrap()
        else {
            unreachable!()
        };
        assert!(container.extra_run_args.contains(&"--cpus=4".into()));
        assert!(container.extra_run_args.contains(&"A=b c".into()));
        assert_eq!(
            container.pull_policy,
            hel::hel_config::ImagePullPolicy::Never
        );
    }
    #[test]
    fn session_size_overrides_beat_the_target_template_and_its_allocation() {
        let template = TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "dev:1".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: Some("4".into()),
                memory: Some("8g".into()),
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let mut session =
            crate::hel_controller::test_support::checkpoint_test_session("session-size");
        session.container_cpus = Some("2".into());
        session.container_memory = Some("3g".into());
        session.resource_allocation = Some(SessionResourceAllocation::Container {
            cpus: 16,
            memory_bytes: 64_000_000_000,
        });
        let hel_targets::TargetTemplate::LocalPodman(container) = backend_target(
            &template,
            session.resource_allocation.as_ref(),
            ContainerOverrides::for_session(&session),
        )
        .unwrap() else {
            unreachable!()
        };
        assert!(container.extra_run_args.contains(&"--cpus=2".into()));
        assert!(container.extra_run_args.contains(&"--memory=3g".into()));
        assert!(!container.extra_run_args.iter().any(|argument| {
            argument.starts_with("--cpus=4")
                || argument.starts_with("--cpus=16")
                || argument.starts_with("--memory=8g")
        }));
    }
    #[test]
    fn github_token_is_inherited_only_by_managed_containers() {
        let mut podman = hel_targets::TargetTemplate::LocalPodman(ContainerTemplate {
            image: "dev:1".into(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        });
        assert!(configure_github_token_environment(&mut podman));
        let hel_targets::TargetTemplate::LocalPodman(container) = podman else {
            unreachable!()
        };
        assert!(
            container
                .extra_run_args
                .windows(2)
                .any(|arguments| arguments == ["--env", "GH_TOKEN"])
        );
        assert!(
            !container
                .extra_run_args
                .iter()
                .any(|argument| argument.contains("github-token"))
        );

        let mut bare = hel_targets::TargetTemplate::LocalBare;
        assert!(!configure_github_token_environment(&mut bare));
        assert_eq!(bare, hel_targets::TargetTemplate::LocalBare);
        assert_eq!(usable_github_token("  token-value\n"), Some("token-value"));
        assert_eq!(usable_github_token("not a token"), None);

        let mut bundle = hel_targets::ProjectBundleSpec {
            primary: "app".into(),
            repositories: vec![hel_targets::RepositorySpec {
                url: Some("git@github.com:example/app.git".into()),
                destination: "app".into(),
                git_ref: None,
                reference: None,
            }],
        };
        use_github_https_urls(&mut bundle);
        assert_eq!(
            bundle.repositories[0].url.as_deref(),
            Some("https://github.com/example/app.git")
        );
    }
    fn container_target(
        image: &str,
        pull_policy: hel::hel_config::ImagePullPolicy,
    ) -> ConfigContainer {
        ConfigContainer {
            image: image.into(),
            pull_policy,
            platform: None,
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        }
    }

    #[test]
    fn the_image_refresh_plan_covers_every_image_a_launch_no_longer_pulls() {
        use hel::hel_config::ImagePullPolicy;

        let mut config = HelConfig::default();
        config.targets.insert(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto),
            },
        );
        // The same image on the same host, named by a second target.
        config.targets.insert(
            "podman-again".into(),
            TargetTemplate::LocalPodman {
                container: container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto),
            },
        );
        config.targets.insert(
            "ssh".into(),
            TargetTemplate::SshPodman {
                ssh: SshConnection {
                    host: "builder.example.test".into(),
                    user: Some("dev".into()),
                    identity_file: Some(PathBuf::from("/home/dev/.ssh/builder")),
                    extra_args: Vec::new(),
                },
                container: ConfigContainer {
                    platform: Some("linux/amd64".into()),
                    ..container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto)
                },
            },
        );
        config.targets.insert(
            "docker".into(),
            TargetTemplate::LocalDocker {
                container: container_target("ghcr.io/example/dev:1.2.3", ImagePullPolicy::Newer),
            },
        );
        config.targets.insert(
            "pinned".into(),
            TargetTemplate::LocalPodman {
                container: container_target(
                    "ghcr.io/example/dev@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    ImagePullPolicy::Auto,
                ),
            },
        );
        // Apple's engine still refreshes its image during provisioning.
        config.targets.insert(
            "apple".into(),
            TargetTemplate::AppleContainer {
                container: container_target("ghcr.io/example/dev:latest", ImagePullPolicy::Auto),
            },
        );

        let plan = image_refresh_plan(&config);
        assert_eq!(
            plan.len(),
            3,
            "expected one refresh per host and image: {plan:?}"
        );

        let local = plan
            .iter()
            .find(|refresh| refresh.host == ImageHost::LocalPodman)
            .expect("the auto latest target is refreshed");
        assert_eq!(local.pull.program, "podman");
        assert_eq!(local.pull.args, ["pull", "ghcr.io/example/dev:latest"]);
        assert_eq!(local.prune.args, ["image", "prune", "-f"]);
        assert_eq!(
            local.image_id.args,
            [
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                "ghcr.io/example/dev:latest"
            ]
        );

        let docker = plan
            .iter()
            .find(|refresh| refresh.host == ImageHost::LocalDocker)
            .expect("the explicit newer policy is refreshed too");
        assert_eq!(docker.pull.program, "docker");
        assert_eq!(docker.pull.args, ["pull", "ghcr.io/example/dev:1.2.3"]);
        assert_eq!(docker.prune.args, ["image", "prune", "-f"]);

        let ssh = plan
            .iter()
            .find(|refresh| matches!(refresh.host, ImageHost::SshPodman(_)))
            .expect("the SSH host is refreshed over its own connection");
        assert_eq!(ssh.pull.program, "ssh");
        // The identity file and destination come from the same builder
        // provisioning uses.
        assert!(ssh.pull.args.contains(&"/home/dev/.ssh/builder".to_owned()));
        assert!(
            ssh.pull
                .args
                .contains(&"dev@builder.example.test".to_owned())
        );
        assert_eq!(
            ssh.pull.args.last().map(String::as_str),
            Some("'podman' 'pull' '--platform=linux/amd64' 'ghcr.io/example/dev:latest'")
        );
        assert_eq!(
            ssh.prune.args.last().map(String::as_str),
            Some("'podman' 'image' 'prune' '-f'")
        );

        assert!(
            !plan.iter().any(|refresh| refresh.image.contains("sha256:")),
            "a digest-pinned image was refreshed: {plan:?}"
        );
    }

    #[test]
    fn ssh_docker_image_refresh_runs_docker_on_the_configured_host() {
        use hel::hel_config::ImagePullPolicy;

        let mut config = HelConfig::default();
        config.targets.insert(
            "docker".into(),
            TargetTemplate::SshDocker {
                ssh: SshConnection {
                    host: "builder.example.test".into(),
                    user: Some("dev".into()),
                    identity_file: None,
                    extra_args: Vec::new(),
                },
                container: ConfigContainer {
                    image: "ghcr.io/example/dev:latest".into(),
                    pull_policy: ImagePullPolicy::Auto,
                    platform: Some("linux/amd64".into()),
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        );

        let refresh = image_refresh_plan(&config).pop().expect("refresh plan");
        assert_eq!(
            refresh.host,
            ImageHost::SshDocker(backend_ssh(match config.targets.get("docker").unwrap() {
                TargetTemplate::SshDocker { ssh, .. } => ssh,
                _ => unreachable!(),
            }))
        );
        assert_eq!(refresh.pull.program, "ssh");
        assert_eq!(
            refresh.pull.args.last().map(String::as_str),
            Some("'docker' 'pull' '--platform=linux/amd64' 'ghcr.io/example/dev:latest'")
        );
        assert_eq!(
            refresh.prune.args.last().map(String::as_str),
            Some("'docker' 'image' 'prune' '-f'")
        );
    }

    #[test]
    fn aws_resource_options_follow_the_launch_template_family() {
        let mut config = HelConfig::default();
        config.targets.insert(
            "aws".into(),
            TargetTemplate::AwsEc2 {
                aws_profile: None,
                region: "us-east-1".into(),
                launch_template: "hel-runson".into(),
                launch_template_version: None,
                ssh_user: "ubuntu".into(),
                address_source: AwsAddressSource::PublicIp,
                identity_file: None,
                ssh_args: Vec::new(),
            },
        );
        let executor = PreflightExecutor {
            outputs: RefCell::new(vec![
                CommandOutput {
                    status: 0,
                    stdout: br#"{"LaunchTemplateVersions":[{"LaunchTemplateData":{"InstanceType":"m8i-flex.large"}}]}"#.to_vec(),
                    stderr: Vec::new(),
                },
                CommandOutput {
                    status: 0,
                    stdout: br#"{"InstanceTypes":[{"InstanceType":"m8i-flex.4xlarge","VCpuInfo":{"DefaultVCpus":16},"MemoryInfo":{"SizeInMiB":65536}},{"InstanceType":"m8i-flex.2xlarge","VCpuInfo":{"DefaultVCpus":8},"MemoryInfo":{"SizeInMiB":32768}}]}"#.to_vec(),
                    stderr: Vec::new(),
                },
            ]),
            notices: RefCell::new(vec![]),
        };
        let controller = Controller {
            config,
            state: HelState::default(),
        };

        let options = controller
            .resolve_aws_resource_options("aws", &executor)
            .unwrap();
        assert_eq!(
            options.iter().map(allocation_cpus).collect::<Vec<_>>(),
            [8, 16]
        );
    }
    #[test]
    fn deployment_capacity_groups_local_and_same_host_targets() {
        let container = || ConfigContainer {
            image: "dev:1".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        };
        let ssh = |host: &str| SshConnection {
            host: host.into(),
            user: Some("builder".into()),
            identity_file: None,
            extra_args: Vec::new(),
        };
        let config = HelConfig {
            version: hel::hel_config::CONFIG_VERSION,
            newer_config_version: None,
            phone: Default::default(),
            review: Default::default(),
            profiles: BTreeMap::new(),
            bundles: BTreeMap::new(),
            targets: BTreeMap::from([
                (
                    "apple".into(),
                    TargetTemplate::AppleContainer {
                        container: container(),
                    },
                ),
                (
                    "local".into(),
                    TargetTemplate::LocalPodman {
                        container: container(),
                    },
                ),
                (
                    "bare".into(),
                    TargetTemplate::SshBare {
                        ssh: ssh("builder"),
                        permissions: hel::hel_config::PermissionMode::Yolo,
                        workspace_prefix: ".local/share/hel/workspaces".into(),
                    },
                ),
                (
                    "remote-container".into(),
                    TargetTemplate::SshPodman {
                        ssh: ssh("builder"),
                        container: container(),
                    },
                ),
                (
                    "alias".into(),
                    TargetTemplate::SshBare {
                        ssh: ssh("builder-alias"),
                        permissions: hel::hel_config::PermissionMode::Yolo,
                        workspace_prefix: ".local/share/hel/workspaces".into(),
                    },
                ),
            ]),
        };
        let controller = Controller {
            config,
            state: HelState::default(),
        };

        let targets = controller.deployment_capacity_targets();

        assert_eq!(targets.len(), 3);
        let local = targets.iter().find(|target| target.id == "local").unwrap();
        assert_eq!(local.target_ids, ["apple", "local"]);
        let builder = targets
            .iter()
            .find(|target| target.id == "ssh:builder")
            .unwrap();
        assert_eq!(builder.target_ids, ["bare", "remote-container"]);
        assert_eq!(builder.probes.len(), 1);
        assert!(
            targets
                .iter()
                .any(|target| target.id == "ssh:builder-alias")
        );
    }
    struct PreflightExecutor {
        outputs: RefCell<Vec<CommandOutput>>,
        notices: RefCell<Vec<String>>,
    }
    impl CommandExecutor for PreflightExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            Ok(self.outputs.borrow_mut().remove(0))
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.borrow_mut().push(notice.to_owned());
        }
    }
    #[test]
    fn local_podman_preflight_failures_recommend_doctor() {
        let template = TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let executor = PreflightExecutor {
            outputs: RefCell::new(vec![CommandOutput {
                status: 0,
                stdout: b"podman version 3.4.7\n".to_vec(),
                stderr: vec![],
            }]),
            notices: RefCell::new(vec![]),
        };

        let error = preflight_target(&template, &executor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("mj doctor"));
        assert!(error.contains("Podman 4.0.0"));
    }
    #[test]
    fn ssh_podman_preflight_failures_name_the_destination_and_recommend_doctor() {
        let template = TargetTemplate::SshPodman {
            ssh: SshConnection {
                host: "example.test".into(),
                user: Some("dev".into()),
                identity_file: None,
                extra_args: vec![],
            },
            container: ConfigContainer {
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let executor = PreflightExecutor {
            outputs: RefCell::new(vec![CommandOutput {
                status: 0,
                stdout: b"podman version 3.4.7\n".to_vec(),
                stderr: vec![],
            }]),
            notices: RefCell::new(vec![]),
        };

        let error = preflight_target(&template, &executor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("mj doctor"));
        assert!(error.contains("dev@example.test"));
        assert!(error.contains("Podman 4.0.0"));
    }
    #[test]
    fn ssh_podman_preflight_notifies_when_remote_user_lingering_is_disabled() {
        let template = TargetTemplate::SshPodman {
            ssh: SshConnection {
                host: "example.test".into(),
                user: Some("dev".into()),
                identity_file: None,
                extra_args: vec![],
            },
            container: ConfigContainer {
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let executor = PreflightExecutor {
            outputs: RefCell::new(vec![
                CommandOutput {
                    status: 0,
                    stdout: b"podman version 5.4.2\n".to_vec(),
                    stderr: vec![],
                },
                CommandOutput {
                    status: 0,
                    stdout: b"true\n".to_vec(),
                    stderr: vec![],
                },
                CommandOutput {
                    status: 0,
                    stdout: b"0 1000 1\n1 100000 65536\n".to_vec(),
                    stderr: vec![],
                },
                CommandOutput {
                    status: 0,
                    stdout: b"no\n".to_vec(),
                    stderr: vec![],
                },
            ]),
            notices: RefCell::new(vec![]),
        };

        preflight_target(&template, &executor).unwrap();

        let notices = executor.notices.borrow();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("last SSH connection closes"));
        assert!(notices[0].contains("sudo loginctl enable-linger"));
    }
    #[test]
    fn apple_container_preflight_failures_recommend_doctor() {
        let template = TargetTemplate::AppleContainer {
            container: ConfigContainer {
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let executor = PreflightExecutor {
            outputs: RefCell::new(vec![CommandOutput {
                status: 1,
                stdout: vec![],
                stderr: b"daemon is not running".to_vec(),
            }]),
            notices: RefCell::new(vec![]),
        };

        let error = preflight_target(&template, &executor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("mj doctor"));
        assert!(error.contains("daemon is not running"));
    }
}
