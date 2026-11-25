//! Projects, containers and the target templates a session runs on.

use std::collections::BTreeSet;
use std::path::{Component, PathBuf};

use anyhow::{Context, Result, bail};
use bytesize::ByteSize;
use serde::{Deserialize, Serialize};

use super::{
    Environment, ExecutionPolicy, PermissionMode, validate_id, validate_relative_destination,
};

pub fn container_memory_limit(text: &str) -> Result<i64> {
    Ok(i64::try_from(parse_container_memory(text)?)?)
}

/// Read an engine memory limit in bytes, using binary units for Podman suffixes.
pub fn parse_container_memory(text: &str) -> Result<u64> {
    let text = text.trim();
    let value = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let (shift, unit) = match text[value.len()..].to_ascii_lowercase().as_str() {
        "" | "b" => (0, "B"),
        "k" | "kb" | "ki" | "kib" => (10, "KiB"),
        "m" | "mb" | "mi" | "mib" => (20, "MiB"),
        "g" | "gb" | "gi" | "gib" => (30, "GiB"),
        "t" | "tb" | "ti" | "tib" => (40, "TiB"),
        _ => bail!("invalid container memory limit {text:?}"),
    };
    if let Ok(amount) = value.trim().parse::<u64>() {
        return amount
            .checked_mul(1u64 << shift)
            .with_context(|| format!("container memory limit {text:?} is too large"));
    }
    // Keep support for fractional quantities in container upgrade settings.
    let bytes = format!("{value}{unit}")
        .parse::<ByteSize>()
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        bytes.as_u64() < u64::MAX,
        "container memory limit {text:?} is too large"
    );
    Ok(bytes.as_u64())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepository {
    /// Stable name within the bundle, used by `primary_repo`.
    pub id: String,
    /// Network Git URL (or GitHub `owner/repository` shorthand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// S3 build cache namespace. Defaults to the GitHub owner/repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbx_namespace: Option<String>,
    /// Controller-side repository whose default network remotes seed isolated sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// Safe relative path beneath the target's bundle root.
    pub destination: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
            if let Some(source) = repository.github.as_deref() {
                crate::remote_git::project_source(source, crate::remote_git::Shorthand::Allowed)
                    .map(drop)
                    .with_context(|| {
                        format!(
                            "invalid Git source for bundle {bundle_id:?} repository {:?}",
                            repository.id
                        )
                    })?;
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
        self.mbx_namespaces()
            .with_context(|| format!("bundle {bundle_id:?}"))?;
        Ok(())
    }

    pub fn primary(&self) -> Option<&ProjectRepository> {
        self.repositories
            .iter()
            .find(|repository| repository.id == self.primary_repo)
    }

    /// The session's default namespace followed by its other allowed namespaces.
    pub fn mbx_namespaces(&self) -> Result<Vec<String>> {
        let primary = self.primary();
        let mut seen = BTreeSet::new();
        let mut namespaces = Vec::new();
        for repository in primary.into_iter().chain(
            self.repositories
                .iter()
                .filter(|repo| repo.id != self.primary_repo),
        ) {
            let namespace = repository.mbx_namespace.clone().or_else(|| {
                let (owner, name) =
                    crate::remote_git::github_owner_repo(repository.github.as_deref()?)?;
                Some(format!("{owner}/{name}"))
            });
            if let Some(namespace) = namespace {
                if !namespace.split('/').all(|segment| {
                    !matches!(segment, "" | "." | "..")
                        && segment.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                        })
                }) {
                    bail!(
                        "repository {:?} has invalid mbx_namespace {namespace:?}",
                        repository.id
                    );
                }
                if seen.insert(namespace.clone()) {
                    namespaces.push(namespace);
                }
            }
        }
        Ok(namespaces)
    }
}

impl ProjectRepository {
    pub(super) fn strip_http_userinfo(&mut self) {
        if let Some(source) = self.github.as_deref()
            && let Ok(std::borrow::Cow::Owned(clean)) =
                crate::remote_git::project_source(source, crate::remote_git::Shorthand::Allowed)
        {
            self.github = Some(clean);
            tracing::warn!("{}", crate::remote_git::HTTP_USERINFO_REMOVED);
        }
    }

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

/// Machine policy for the mbx build cache. Every field is optional:
/// an unset field keeps the resolved default for that host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "BuildCacheSettings")]
pub struct TargetBuildCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Cache directory on the target's own host, not on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    /// Native Go, Gradle, Turbo, Nx and Bazel caches on this machine. mbx
    /// continues to own its separate Rust/C/C++ storage and configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_directory: Option<PathBuf>,
    /// Shared storage budget, an mbx size string such as `100GiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_size: Option<String>,
    /// S3 cache read by the Nitro host on behalf of its guests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// S3 bucket region. Defaults to the Nitro host's region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_region: Option<String>,
    /// Machine-wide compiler admission settings shared by all builds on this host.
    #[serde(default, skip_serializing_if = "BuildCacheScheduler::is_default")]
    pub scheduler: BuildCacheScheduler,
}

/// Optional machine-wide mbx compiler scheduling overrides.
///
/// When a field is unset, mbx chooses its own default for the host or
/// container. `memory` is an admission budget used to weight concurrent
/// compilations, not an operating-system memory limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildCacheScheduler {
    /// Number of compiler permits shared across all mbx builds on the machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u64>,
    /// mbx size string such as `8GiB`, or `none` to disable memory weighting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
}

impl BuildCacheScheduler {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.cpus.is_none() && self.memory.is_none()
    }
}

/// Consume obsolete budgets without interpreting or migrating their values.
/// They are read whole rather than as `IgnoredAny`, which the file loader
/// counts as a key no setting reads.
#[derive(Default, Deserialize)]
#[serde(default)]
struct BuildCacheSettings {
    enabled: Option<bool>,
    directory: Option<PathBuf>,
    tools_directory: Option<PathBuf>,
    max_total_size: Option<String>,
    remote: Option<String>,
    remote_region: Option<String>,
    scheduler: BuildCacheScheduler,
    #[serde(rename = "max_size")]
    _legacy_max_size: Option<serde_json::Value>,
    #[serde(rename = "target_max_size")]
    _legacy_target_max_size: Option<serde_json::Value>,
}

impl From<BuildCacheSettings> for TargetBuildCache {
    fn from(settings: BuildCacheSettings) -> Self {
        Self {
            enabled: settings.enabled,
            directory: settings.directory,
            tools_directory: settings.tools_directory,
            max_total_size: settings.max_total_size,
            remote: settings.remote,
            remote_region: settings.remote_region,
            scheduler: settings.scheduler,
        }
    }
}

impl TargetBuildCache {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub(super) fn validate(&self, template_id: &str) -> Result<()> {
        if let Some(remote) = &self.remote {
            mbx_s3_location(remote)
                .with_context(|| format!("target template {template_id:?} build cache"))?;
        }
        if let Some(directory) = &self.tools_directory
            && !crate::target_path::is_absolute(directory)
        {
            bail!("target template {template_id:?} build cache tools_directory must be absolute");
        }
        if let Some(directory) = &self.directory
            && !crate::target_path::is_absolute(directory)
        {
            bail!("target template {template_id:?} build cache directory must be absolute");
        }
        if let Some(max_size) = &self.max_total_size
            && parse_build_cache_size(max_size).is_none()
        {
            bail!(
                "target template {template_id:?} build cache size {max_size:?} is not a size such as 100GiB"
            );
        }
        if let Some(cpus) = self.scheduler.cpus
            && (cpus == 0 || cpus > i64::MAX as u64)
        {
            bail!(
                "machine {template_id:?} build cache scheduler cpus must be a positive integer supported by mbx"
            );
        }
        if let Some(memory) = &self.scheduler.memory
            && !is_build_cache_scheduler_memory(memory)
        {
            bail!(
                "machine {template_id:?} build cache scheduler memory must be an mbx size such as 8GiB or \"none\""
            );
        }
        Ok(())
    }
}

/// Check the bucket name's length, alphabet and endpoints for an mbx S3 remote.
pub fn valid_mbx_s3_bucket(bucket: &str) -> bool {
    (3..=63).contains(&bucket.len())
        && bucket.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && bucket.as_bytes()[0].is_ascii_alphanumeric()
        && bucket.as_bytes()[bucket.len() - 1].is_ascii_alphanumeric()
}

/// Split an mbx S3 cache location, allowing the bucket root as its prefix.
pub fn mbx_s3_location(remote: &str) -> Result<(&str, &str)> {
    let location = remote.strip_prefix("s3://").unwrap_or_default();
    let (bucket, prefix) = location.split_once('/').unwrap_or((location, ""));
    if !valid_mbx_s3_bucket(bucket) {
        bail!("build cache remote {remote:?} must be s3://<bucket>/<prefix>");
    }
    let prefix = prefix.trim_matches('/');
    if prefix
        .split('/')
        .any(|segment| matches!(segment, "." | ".."))
    {
        bail!("build cache remote {remote:?} contains an S3 prefix with a dot segment");
    }
    // The IAM policy and the host proxy must read the prefix the same way;
    // a percent escape would mean one literal key to IAM and another to S3.
    if prefix.contains('%') {
        bail!("build cache remote {remote:?} must spell its S3 prefix without percent escapes");
    }
    Ok((bucket, prefix))
}

fn is_build_cache_scheduler_memory(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("none") || value.parse::<ByteSize>().is_ok()
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

/// An mbx size string as a whole number of gigabytes, rounded to the nearest
/// one. The setup editor measures the cache in GB, so a value written in any
/// other unit still has a number to show.
#[must_use]
pub fn build_cache_size_gigabytes(value: &str) -> Option<u64> {
    const GIGABYTE: u64 = 1_000_000_000;
    let bytes = parse_build_cache_size(value)?;
    Some(bytes.saturating_add(GIGABYTE / 2) / GIGABYTE)
}

/// The size string stored for a whole number of gigabytes.
#[must_use]
pub fn build_cache_size_from_gigabytes(gigabytes: u64) -> String {
    format!("{gigabytes}GB")
}

/// The image a container target runs when its table names none: the
/// reference image CI publishes, which first-run setup also writes.
fn default_container_image() -> String {
    super::DEFAULT_SESSION_IMAGE.to_owned()
}

/// Whether a container image is exactly this build's default. A template that
/// keeps the default writes no `image` line, so an upgraded install follows
/// the image built for its own version.
fn is_default_image(image: &String) -> bool {
    image == super::DEFAULT_SESSION_IMAGE
}

/// Every key a container runtime's table may carry beside `kind` and
/// `machine`. A target table flattens [`ContainerTemplate`] into a tagged
/// enum, where serde cannot refuse unknown keys, so the loader compares the
/// table against this list to name the keys it ignores (R4-6).
pub(super) const CONTAINER_TEMPLATE_KEYS: [&str; 9] = [
    "image",
    "pull_policy",
    "platform",
    "cpus",
    "memory",
    "memory_hard_limit",
    "environment",
    "workspace_storage",
    "build_cache",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerTemplate {
    #[serde(
        default = "default_container_image",
        skip_serializing_if = "is_default_image"
    )]
    pub image: String,
    #[serde(default, skip_serializing_if = "ImagePullPolicy::is_auto")]
    pub pull_policy: ImagePullPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_hard_limit: Option<bool>,
    /// Environment inside the container. An entry may name a secret instead
    /// of holding it; see [`super::Environment`].
    #[serde(default, skip_serializing_if = "Environment::is_empty")]
    pub environment: Environment,
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

impl ContainerTemplate {
    pub fn default_host_container_size(&self) -> Option<crate::state::HostContainerSize> {
        let cpus = self.cpus.as_deref()?.trim().parse().ok()?;
        let memory_bytes = parse_container_memory(self.memory.as_deref()?).ok()?;
        (cpus > 0 && memory_bytes > 0)
            .then_some(crate::state::HostContainerSize { cpus, memory_bytes })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
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
        if !crate::target_path::is_absolute(root) {
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

/// Where a new SSH machine keeps bare-runtime workspaces, relative to the
/// login home. Setup writes it into the file for every machine it adds.
pub const DEFAULT_WORKSPACE_PREFIX: &str = ".local/share/mjolnir/workspaces";

/// The directory used before the product was renamed. A machine whose file
/// does not name a directory keeps this one, because its existing
/// workspaces are there.
pub const LEGACY_WORKSPACE_PREFIX: &str = ".local/share/hel/workspaces";

/// The directory of a machine the file does not give one.
pub(super) fn default_named_machine_prefix() -> PathBuf {
    PathBuf::from(LEGACY_WORKSPACE_PREFIX)
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
    SshAppleContainer {
        #[serde(flatten)]
        ssh: SshConnection,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    /// An AWS Nitro Sandbox: one Firecracker microVM per session on an EC2
    /// host reached over SSH, driven by `mj-worker microvm` as root.
    SshNitro {
        #[serde(flatten)]
        ssh: SshConnection,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
}

impl TargetTemplate {
    /// One availability rule for runtime kinds retained only to read history.
    pub fn launch_refusal(&self) -> Option<&'static str> {
        matches!(self, Self::AwsEc2 { .. }).then_some(super::LEGACY_EC2_TARGET_NOTICE)
    }

    /// Whether a session can start on this target: every environment
    /// reference its container names resolved. Like
    /// [`HarnessProfile::ensure_ready`](super::HarnessProfile::ensure_ready),
    /// this is not part of configuration validation, so a missing secret
    /// stops only this target's sessions.
    pub fn ensure_ready(&self, id: &str) -> Result<()> {
        let Some(container) = self.container() else {
            return Ok(());
        };
        container.environment.ensure_resolved().map_err(|error| {
            crate::refusal::Refusal::precondition(format!("target {id:?}: {error:#}")).into()
        })
    }

    /// The container this runtime starts, for the container runtimes.
    pub fn container(&self) -> Option<&ContainerTemplate> {
        match self {
            Self::LocalPodman { container }
            | Self::LocalDocker { container }
            | Self::AppleContainer { container }
            | Self::SshPodman { container, .. }
            | Self::SshDocker { container, .. }
            | Self::SshAppleContainer { container, .. }
            | Self::SshNitro { container, .. } => Some(container),
            _ => None,
        }
    }

    /// The container this runtime starts, for editing during a load.
    pub(super) fn container_mut(&mut self) -> Option<&mut ContainerTemplate> {
        match self {
            Self::LocalPodman { container }
            | Self::LocalDocker { container }
            | Self::AppleContainer { container }
            | Self::SshPodman { container, .. }
            | Self::SshDocker { container, .. }
            | Self::SshAppleContainer { container, .. }
            | Self::SshNitro { container, .. } => Some(container),
            _ => None,
        }
    }

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
            Self::SshAppleContainer { .. } => "ssh-apple-container",
            Self::SshNitro { .. } => "ssh-nitro",
        }
    }

    /// A copy without the settings that only matter when a session is
    /// launched.
    ///
    /// Build cache settings are captured at provisioning. RAM admission
    /// policy is consulted for future launches. Both may be edited while a
    /// session is running.
    #[must_use]
    pub fn without_launch_only_settings(&self) -> Self {
        let mut stripped = self.clone();
        if let Some(container) = stripped.container_mut() {
            container.build_cache = None;
            container.memory_hard_limit = None;
        }
        stripped
    }

    pub const fn execution_policy(&self) -> ExecutionPolicy {
        match self.permission_mode() {
            Some(mode) => mode.execution_policy(),
            None => ExecutionPolicy::Unconstrained,
        }
    }

    /// The approval behavior of a target that runs the harness directly on a
    /// machine, or `None` for a target whose container or instance boundary
    /// replaces approvals. Localhost has no setting for it: the harness
    /// always keeps its configured approvals there.
    pub const fn permission_mode(&self) -> Option<PermissionMode> {
        match self {
            Self::LocalBare => Some(PermissionMode::Guardian),
            Self::SshBare { permissions, .. } => Some(*permissions),
            _ => None,
        }
    }

    /// Whether the harness runs without approvals on this target: the
    /// execution policy a new session records and its worker launch
    /// enforces. Containers and SSH bare targets with
    /// `permissions = "yolo"` qualify.
    pub const fn is_yolo(&self) -> bool {
        matches!(self.execution_policy(), ExecutionPolicy::Unconstrained)
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
            // A remote Apple container runtime accepts exactly what the local
            // one does: the engine is the same, only its host differs.
            Self::SshAppleContainer { ssh, container } => {
                ssh.validate(id)?;
                container.validate(id)?;
                if !container.workspace_storage.is_default() {
                    bail!("target template {id:?} workspace storage is only supported by Podman");
                }
                Ok(())
            }
            // A Nitro sandbox keeps its workspace on a named volume the
            // engine creates; no other storage shape exists.
            Self::SshNitro { ssh, container } => {
                ssh.validate(id)?;
                container.validate(id)?;
                if !container.workspace_storage.is_default() {
                    bail!(
                        "target template {id:?} workspace storage is only supported by Podman; \
                         a Nitro sandbox always uses a volume"
                    );
                }
                if container.memory_hard_limit == Some(false) {
                    bail!(
                        "target template {id:?} cannot disable memory_hard_limit; \
                         Nitro sandboxes use a fixed memory pool reserved at boot"
                    );
                }
                Ok(())
            }
        }
    }
}

impl From<&TargetTemplate> for crate::state::ResourceAllocationKind {
    fn from(template: &TargetTemplate) -> Self {
        match template {
            TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. }
            | TargetTemplate::SshAppleContainer { .. }
            | TargetTemplate::SshNitro { .. } => Self::Container,
            TargetTemplate::AwsEc2 { .. } => Self::AwsEc2,
            TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => Self::Fixed,
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
            | TargetTemplate::SshAppleContainer { .. }
            | TargetTemplate::SshNitro { .. }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryHardLimitReason {
    AppleContainer,
    NitroPool,
    TargetSetting,
}

impl MemoryHardLimitReason {
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::AppleContainer => "Apple containers do not give freed memory back to macOS",
            Self::NitroPool => {
                "Nitro sandboxes take their memory from the memory pool the host reserved at boot"
            }
            Self::TargetSetting => "this target is configured to enforce RAM limits",
        }
    }
}

/// Why a target refuses to overcommit host RAM, if it does.
pub fn container_memory_hard_limit_reason(
    template: &TargetTemplate,
) -> Option<MemoryHardLimitReason> {
    match template {
        TargetTemplate::SshNitro { .. } => Some(MemoryHardLimitReason::NitroPool),
        TargetTemplate::AppleContainer { container }
        | TargetTemplate::SshAppleContainer { container, .. } => (container.memory_hard_limit
            != Some(false))
        .then_some(MemoryHardLimitReason::AppleContainer),
        TargetTemplate::LocalPodman { container }
        | TargetTemplate::SshPodman { container, .. }
        | TargetTemplate::LocalDocker { container }
        | TargetTemplate::SshDocker { container, .. } => (container.memory_hard_limit
            == Some(true))
        .then_some(MemoryHardLimitReason::TargetSetting),
        TargetTemplate::LocalBare
        | TargetTemplate::SshBare { .. }
        | TargetTemplate::AwsEc2 { .. } => None,
    }
}

pub fn container_memory_is_hard_limit(template: &TargetTemplate) -> bool {
    container_memory_hard_limit_reason(template).is_some()
}

/// The host name that prompt-history mounts on `template` should be filed
/// under, or `None` if the target does not support attached mounts.
pub fn mount_history_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. }
        | TargetTemplate::AwsEc2 { .. } => Some("local"),
        TargetTemplate::SshPodman { ssh, .. }
        | TargetTemplate::SshDocker { ssh, .. }
        | TargetTemplate::SshAppleContainer { ssh, .. }
        | TargetTemplate::SshNitro { ssh, .. } => Some(&ssh.host),
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => None,
    }
}

/// Whether a launch surface should offer attached host directories for
/// `template`. A Nitro sandbox is a VM that cannot attach one, but a request
/// that carries attachments (an API call, a resumed session) is still
/// accepted: the launch drops them and says so, instead of refusing.
pub fn offers_attached_directories(template: &TargetTemplate) -> bool {
    mount_history_host(template).is_some() && !matches!(template, TargetTemplate::SshNitro { .. })
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
        | TargetTemplate::SshDocker { .. }
        | TargetTemplate::SshAppleContainer { .. }
        | TargetTemplate::SshNitro { .. } => None,
    }
}

/// Stable physical-host key for reusable container CPU and memory defaults.
pub fn container_size_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. } => Some("local"),
        TargetTemplate::SshPodman { ssh, .. }
        | TargetTemplate::SshDocker { ssh, .. }
        | TargetTemplate::SshAppleContainer { ssh, .. }
        | TargetTemplate::SshNitro { ssh, .. } => Some(&ssh.host),
        TargetTemplate::LocalBare
        | TargetTemplate::SshBare { .. }
        | TargetTemplate::AwsEc2 { .. } => None,
    }
}

pub(super) fn validate_environment(owner: &str, environment: &Environment) -> Result<()> {
    // The entries as written, so one whose reference did not resolve is
    // checked too.
    if environment
        .sources()
        .keys()
        .any(|key| key.trim().is_empty() || key.contains('='))
    {
        bail!("{owner:?} contains an invalid environment variable name");
    }
    Ok(())
}
