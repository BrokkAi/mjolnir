//! Projects, containers and the target templates a session runs on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{
    ExecutionPolicy, PermissionMode, is_github_source, validate_id, validate_relative_destination,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRepository {
    /// Stable name within the bundle, used by `primary_repo`.
    pub id: String,
    /// GitHub HTTPS or SSH URL (or `owner/repository` shorthand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// Controller-side repository whose default network remotes seed isolated sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// Safe relative path beneath the target's bundle root.
    pub destination: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBundle {
    /// Repository id used as the ACP session cwd.
    pub primary_repo: String,
    pub repositories: Vec<ProjectRepository>,
}

impl ProjectBundle {
    pub(super) fn validate(&self, bundle_id: &str) -> Result<()> {
        validate_id("bundle", bundle_id)?;
        if self.repositories.is_empty() {
            bail!("bundle {bundle_id:?} must contain at least one repository");
        }

        let mut ids = BTreeSet::new();
        let mut destinations = Vec::<PathBuf>::new();
        for repository in &self.repositories {
            validate_id("repository", &repository.id)
                .with_context(|| format!("bundle {bundle_id:?}"))?;
            if !ids.insert(repository.id.as_str()) {
                bail!(
                    "bundle {bundle_id:?} contains duplicate repository id {:?}",
                    repository.id
                );
            }
            if repository.github.is_some() == repository.local.is_some() {
                bail!(
                    "bundle {bundle_id:?} repository {:?} must declare exactly one of `github` or `local`",
                    repository.id,
                );
            }
            if repository
                .github
                .as_deref()
                .is_some_and(|source| !is_github_source(source))
            {
                bail!(
                    "bundle {bundle_id:?} repository {:?} is not a supported GitHub source",
                    repository.id,
                );
            }
            if repository
                .local
                .as_deref()
                .is_some_and(|path| !path.is_absolute())
            {
                bail!(
                    "bundle {bundle_id:?} repository {:?} local path must be absolute",
                    repository.id,
                );
            }
            if repository.git_ref.is_some() {
                bail!(
                    "bundle {bundle_id:?} repository {:?}: git_ref is no longer supported; remove it to start from the remote's default branch",
                    repository.id
                );
            }
            validate_relative_destination(&repository.destination).with_context(|| {
                format!(
                    "bundle {bundle_id:?} repository {:?} destination",
                    repository.id
                )
            })?;
            if let Some(existing) = destinations.iter().find(|existing| {
                repository.destination.starts_with(existing)
                    || existing.starts_with(&repository.destination)
            }) {
                bail!(
                    "bundle {bundle_id:?} contains overlapping destinations {} and {}",
                    existing.display(),
                    repository.destination.display()
                );
            }
            destinations.push(repository.destination.clone());
        }
        if !ids.contains(self.primary_repo.as_str()) {
            bail!(
                "bundle {bundle_id:?} primary repository {:?} does not exist",
                self.primary_repo
            );
        }
        Ok(())
    }

    pub fn primary(&self) -> Option<&ProjectRepository> {
        self.repositories
            .iter()
            .find(|repository| repository.id == self.primary_repo)
    }
}

impl ProjectRepository {
    pub fn source_label(&self) -> String {
        self.github
            .clone()
            .or_else(|| self.local.as_ref().map(|path| path.display().to_string()))
            .unwrap_or_else(|| "invalid repository source".into())
    }

    pub fn is_local(&self) -> bool {
        self.local.is_some()
    }
}

/// Per-target overrides for the mbx build cache. Every field is optional:
/// an unset field keeps the resolved default for that target's host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetBuildCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Cache directory on the target's own host, not on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    /// An mbx size string such as `100GiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<String>,
}

impl TargetBuildCache {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub(super) fn validate(&self, template_id: &str) -> Result<()> {
        if let Some(directory) = &self.directory
            && !directory.is_absolute()
        {
            bail!("target template {template_id:?} build cache directory must be absolute");
        }
        if let Some(max_size) = &self.max_size
            && parse_build_cache_size(max_size).is_none()
        {
            bail!(
                "target template {template_id:?} build cache size {max_size:?} is not a size such as 100GiB"
            );
        }
        Ok(())
    }
}

/// Whether a target carries no build cache overrides, so an unchanged target
/// is not rewritten with an empty section.
pub(super) fn is_default_target_build_cache(value: &Option<TargetBuildCache>) -> bool {
    value.as_ref().is_none_or(TargetBuildCache::is_default)
}

/// "No overrides" has one representation. A section with every field unset,
/// which both a hand-written file and the setup editor can produce, reads back
/// as no section at all.
pub(super) fn deserialize_target_build_cache<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<TargetBuildCache>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<TargetBuildCache>::deserialize(deserializer)?
        .filter(|build_cache| !build_cache.is_default()))
}

/// Byte count of an mbx size string: digits and an optional unit, the same
/// spellings mbx's own `bytesize` parser accepts. `20GB` and `20GiB` are
/// different numbers, so the unit is kept exactly as written.
#[must_use]
pub fn parse_build_cache_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let digits = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    if digits == 0 {
        return None;
    }
    let count: u64 = value[..digits].parse().ok()?;
    let multiplier: u64 = match value[digits..].trim_start() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return None,
    };
    count.checked_mul(multiplier)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerTemplate {
    pub image: String,
    #[serde(default, skip_serializing_if = "ImagePullPolicy::is_auto")]
    pub pull_policy: ImagePullPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "PodmanWorkspaceStorage::is_default")]
    pub workspace_storage: PodmanWorkspaceStorage,
    /// Per-target mbx build cache overrides.
    #[serde(
        default,
        skip_serializing_if = "is_default_target_build_cache",
        deserialize_with = "deserialize_target_build_cache"
    )]
    pub build_cache: Option<TargetBuildCache>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PodmanWorkspaceStorage {
    #[default]
    PodmanVolume,
    HostHelper {
        root: PathBuf,
        helper: Vec<String>,
    },
    ContainerLayer,
}

impl PodmanWorkspaceStorage {
    fn is_default(&self) -> bool {
        matches!(self, Self::PodmanVolume)
    }

    pub(super) fn validate(&self, template_id: &str) -> Result<()> {
        let Self::HostHelper { root, helper } = self else {
            return Ok(());
        };
        if !root.is_absolute() {
            bail!("target template {template_id:?} workspace storage root must be absolute");
        }
        if helper.is_empty() || helper.iter().any(|argument| argument.is_empty()) {
            bail!(
                "target template {template_id:?} workspace storage helper must contain non-empty arguments"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImagePullPolicy {
    #[default]
    Auto,
    Always,
    Newer,
    Missing,
    Never,
}

impl ImagePullPolicy {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

impl ContainerTemplate {
    pub(super) fn validate(&self, template_id: &str) -> Result<()> {
        if self.image.trim().is_empty() {
            bail!("target template {template_id:?} has an empty container image");
        }
        validate_environment(template_id, &self.environment)?;
        if let Some(build_cache) = &self.build_cache {
            build_cache.validate(template_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AwsAddressSource {
    #[default]
    PublicDns,
    PublicIp,
    PrivateDns,
    PrivateIp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConnection {
    /// OpenSSH destination such as `builder.example.com` or an SSH config alias.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl SshConnection {
    pub(super) fn validate(&self, template_id: &str) -> Result<()> {
        if self.host.trim().is_empty() || self.host.chars().any(char::is_whitespace) {
            bail!("target template {template_id:?} has an invalid SSH host");
        }
        if self.user.as_deref().is_some_and(|user| {
            user.is_empty() || user.chars().any(|c| c.is_whitespace() || c == '@')
        }) {
            bail!("target template {template_id:?} has an invalid SSH user");
        }
        Ok(())
    }
}

pub(super) fn default_named_machine_prefix() -> PathBuf {
    PathBuf::from(".local/share/hel/workspaces")
}

/// Rejects a remote workspace directory that would escape the login home or
/// name the home itself. Shared by SSH machines and the targets that resolve
/// from them so both report the same rule.
pub(super) fn validate_workspace_prefix(
    id: &str,
    workspace_prefix: &std::path::Path,
) -> Result<()> {
    if workspace_prefix.as_os_str().is_empty()
        || workspace_prefix
            .components()
            .any(|part| part == Component::ParentDir)
        || matches!(workspace_prefix.to_str(), Some("/" | "." | "~" | "~/"))
    {
        bail!("{id:?} has an unsafe workspace prefix");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TargetTemplate {
    LocalBare,
    LocalPodman {
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    LocalDocker {
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    AppleContainer {
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    AwsEc2 {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aws_profile: Option<String>,
        region: String,
        launch_template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_template_version: Option<String>,
        ssh_user: String,
        #[serde(default)]
        address_source: AwsAddressSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ssh_args: Vec<String>,
    },
    SshBare {
        #[serde(flatten)]
        ssh: SshConnection,
        permissions: PermissionMode,
        #[serde(default = "default_named_machine_prefix")]
        workspace_prefix: PathBuf,
    },
    SshPodman {
        #[serde(flatten)]
        ssh: SshConnection,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    SshDocker {
        #[serde(flatten)]
        ssh: SshConnection,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
}

impl TargetTemplate {
    /// The `kind` spelling used in configuration and on the wire.
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::LocalBare => "local-bare",
            Self::LocalPodman { .. } => "local-podman",
            Self::LocalDocker { .. } => "local-docker",
            Self::AppleContainer { .. } => "apple-container",
            Self::AwsEc2 { .. } => "aws-ec2",
            Self::SshBare { .. } => "ssh-bare",
            Self::SshPodman { .. } => "ssh-podman",
            Self::SshDocker { .. } => "ssh-docker",
        }
    }

    /// A copy without the settings that only matter when a session is
    /// launched.
    ///
    /// Build cache settings are resolved once while a session is provisioned
    /// and stored on its record, so an active session does not depend on
    /// them. Comparing templates without them lets setup edit a machine's
    /// build cache while sessions are running.
    #[must_use]
    pub fn without_launch_only_settings(&self) -> Self {
        let mut stripped = self.clone();
        match &mut stripped {
            Self::LocalPodman { container }
            | Self::LocalDocker { container }
            | Self::AppleContainer { container }
            | Self::SshPodman { container, .. }
            | Self::SshDocker { container, .. } => container.build_cache = None,
            Self::LocalBare | Self::AwsEc2 { .. } | Self::SshBare { .. } => {}
        }
        stripped
    }

    pub const fn execution_policy(&self) -> ExecutionPolicy {
        match self {
            Self::LocalBare => ExecutionPolicy::ConfiguredApprovals,
            Self::SshBare { permissions, .. } => permissions.execution_policy(),
            _ => ExecutionPolicy::Unconstrained,
        }
    }

    pub const fn permission_mode(&self) -> Option<PermissionMode> {
        match self {
            Self::SshBare { permissions, .. } => Some(*permissions),
            _ => None,
        }
    }

    pub(super) fn validate(&self, id: &str) -> Result<()> {
        validate_id("target template", id)?;
        match self {
            Self::LocalBare => Ok(()),
            Self::LocalPodman { container } => {
                container.validate(id)?;
                container.workspace_storage.validate(id)
            }
            Self::LocalDocker { container } | Self::AppleContainer { container } => {
                container.validate(id)?;
                if !container.workspace_storage.is_default() {
                    bail!("target template {id:?} workspace storage is only supported by Podman");
                }
                Ok(())
            }
            Self::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                launch_template_version,
                ssh_user,
                ..
            } => {
                if region.trim().is_empty()
                    || launch_template.trim().is_empty()
                    || ssh_user.trim().is_empty()
                {
                    bail!(
                        "AWS target template {id:?} requires region, launch_template, and ssh_user"
                    );
                }
                if aws_profile.as_deref().is_some_and(str::is_empty)
                    || launch_template_version
                        .as_deref()
                        .is_some_and(str::is_empty)
                {
                    bail!("AWS target template {id:?} contains an empty optional value");
                }
                Ok(())
            }
            Self::SshBare {
                ssh,
                workspace_prefix,
                ..
            } => {
                ssh.validate(id)?;
                validate_workspace_prefix(&format!("target template {id}"), workspace_prefix)
            }
            Self::SshDocker { ssh, container } => {
                ssh.validate(id)?;
                container.validate(id)?;
                if !container.workspace_storage.is_default() {
                    bail!("target template {id:?} workspace storage is only supported by Podman");
                }
                Ok(())
            }
            Self::SshPodman { ssh, container, .. } => {
                ssh.validate(id)?;
                container.validate(id)?;
                container.workspace_storage.validate(id)
            }
        }
    }
}

/// Whether `template` hosts a raw project checkout directly on its machine,
/// with no managed workspace. Bare targets take a project directory instead
/// of a bundle.
pub fn is_bare_project_target(template: &TargetTemplate) -> bool {
    matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    )
}

/// Whether sessions on `template` run inside a container Hel creates, and so
/// work in a container workspace rather than a host or instance directory.
pub fn is_container_target(template: &TargetTemplate) -> bool {
    matches!(
        template,
        TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. }
    )
}

/// The host name that prompt-history mounts on `template` should be filed
/// under, or `None` if the target does not support attached mounts.
pub fn mount_history_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. }
        | TargetTemplate::AwsEc2 { .. } => Some("local"),
        TargetTemplate::SshPodman { ssh, .. } | TargetTemplate::SshDocker { ssh, .. } => {
            Some(&ssh.host)
        }
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => None,
    }
}

/// The host key whose project-directory history belongs to `template`.
/// Unlike mount history, raw bare targets keep a recent project checkout list
/// because their launch wizard selects a directory instead of an attachment.
pub fn project_history_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalBare => Some("local"),
        TargetTemplate::SshBare { ssh, .. } => Some(&ssh.host),
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. }
        | TargetTemplate::AwsEc2 { .. }
        | TargetTemplate::SshPodman { .. }
        | TargetTemplate::SshDocker { .. } => None,
    }
}

/// Stable physical-host key for reusable container CPU and memory defaults.
pub fn container_size_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. } => Some("local"),
        TargetTemplate::SshPodman { ssh, .. } | TargetTemplate::SshDocker { ssh, .. } => {
            Some(&ssh.host)
        }
        TargetTemplate::LocalBare
        | TargetTemplate::SshBare { .. }
        | TargetTemplate::AwsEc2 { .. } => None,
    }
}

pub(super) fn validate_environment(
    owner: &str,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    if environment
        .keys()
        .any(|key| key.trim().is_empty() || key.contains('='))
    {
        bail!("{owner:?} contains an invalid environment variable name");
    }
    Ok(())
}
