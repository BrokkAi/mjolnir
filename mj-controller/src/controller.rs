//! Controller-side lifecycle transitions and canonical-to-backend conversion.

mod backend;
mod cache_host;
pub(crate) mod checkpoint;
mod git_cache;
mod lifecycle;
mod mbx;
pub mod move_session;
mod network_git;
mod new_session_preflight;
pub use new_session_preflight::{NewSessionPreflight, NewSessionRepository};
mod path_completion;
pub mod profile_config;
mod provisioning;
mod readiness;
pub(crate) use readiness::NATIVE_SESSION_STARTUP_TIMEOUT;
mod recovery_scan;
mod resume;
mod reviewer;
mod subagents;
#[cfg(test)]
pub(crate) mod test_support;
pub mod update;
mod worker_binary;
mod worker_restart;
mod worktree;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;

use mj_core::config::{
    Config, ProjectBundle, ProjectRepository, TargetTemplate, atomic_write, container_size_host,
    data_dir, is_bare_project_target, mount_history_host,
};

use crate::import::{
    RepositoryIdentity, bundle_matches, configured_bundle_for_local, configured_bundle_for_origin,
    setup_style_id,
};
use crate::setup::github_repository_from_origin;

const CONFIG_RENAME_JOURNAL: &str = "config-rename.json";

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigRenameKind {
    Profile,
    Target,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigRenameJournal {
    kind: ConfigRenameKind,
    old_id: String,
    new_id: String,
}
use mj_core::state::{
    HostContainerSize, SessionRecord, SessionResourceAllocation, SessionState, State,
    new_session_id, normalize_session_title,
};

use crate::targets::{
    self, AdditionalMount, CommandExecutor, CommandOutput, CommandSpec, SshTarget,
};

pub(crate) use backend::controller_github_token;
pub use backend::image_refresh_plan;
use backend::validate_resource_allocation;
pub use mbx::preview_build_cache;
use provisioning::apply_failed_new_session_rollback;
pub(crate) use worker_binary::refresh_target_worker_binary_if_stale;
pub(crate) use worktree::path_exists_on_managed_target;

pub use checkpoint::{
    CheckpointArtifact, CheckpointDeferred, IdleWorkspaceLease, SessionExportLayout,
    checkpoint_was_deferred, reconcile_managed_checkpoint_archives,
};
pub use lifecycle::{BranchDisposition, CheckoutDisposition, has_nothing_to_checkpoint};
pub use recovery_scan::{RecoveryCandidate, RecoveryScan};
pub use resume::{
    ResumeRepositorySourceMismatch, ResumeRepositorySourcePreflight, ResumeRepositorySourceReceipt,
    raw_conversion_preview_for,
};
pub use reviewer::reviewer_stager;
pub use subagents::RegisterSubagentRequest;
pub use worker_binary::{
    WorkerBinaryAvailability, native_worker_binary_prerequisite, pin_worker_binary_sources,
    worker_binary_prerequisite_for_arch,
};
pub use worker_restart::WorkerUpgradeOutcome;
pub use worktree::{ResumePlan, local_project_repository, resume_compatibility};

pub struct Controller {
    pub config: Config,
    pub state: State,
}

/// Machine-wide advisory lock for one controller data store. This prevents a
/// dashboard, server, or CLI lifecycle command from concurrently acting as a
/// second controller against the same SQLite state and relay sessions.
#[derive(Debug)]
pub struct ControllerStoreGuard {
    file: File,
}

impl ControllerStoreGuard {
    pub fn acquire() -> Result<Self> {
        let directory = data_dir();
        Self::acquire_at(&directory)
    }

    fn acquire_at(directory: &Path) -> Result<Self> {
        Self::try_acquire_at(directory)?.with_context(|| {
            format!(
                "another Mjolnir controller is already using {}; stop it before starting this command",
                directory.display()
            )
        })
    }

    /// Probe exclusivity without treating an owner that is still exiting as an error.
    pub fn try_acquire() -> Result<Option<Self>> {
        Self::try_acquire_at(&data_dir())
    }

    fn try_acquire_at(directory: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("create controller data directory {}", directory.display()))?;
        let path = directory.join("controller.lock");
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("open controller lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("lock controller store {}", directory.display()));
            }
        }
        Ok(Some(Self { file }))
    }

    /// Start the sole production SQLite writer after controller exclusivity
    /// has been established by this guard.
    pub fn start_database_writer(&self) -> Result<crate::database::DatabaseWriterOwner> {
        crate::database::start_database_writer()
    }
}

impl Drop for ControllerStoreGuard {
    fn drop(&mut self) {
        // Make release explicit. `File` also unlocks on close, but an explicit
        // unlock keeps same-process handoff deterministic across platforms.
        let _ = self.file.unlock();
    }
}

/// The durable result of creating a quick bundle. The returned config is the
/// same fresh config that was written, allowing a serving projection to publish
/// the new bundle before acknowledging the request that created it.
#[derive(Debug)]
pub struct QuickBundleCreation {
    pub config: Config,
    pub bundle_id: String,
}

/// Failure stages exposed to a viewer request without exposing the underlying
/// filesystem/configuration error. The detailed error remains available to
/// the caller for logs and terminal notices.
#[derive(Debug)]
pub enum QuickBundleFailure {
    InvalidSource(anyhow::Error),
    Persistence(anyhow::Error),
}

impl std::fmt::Display for QuickBundleFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSource(error) => write!(formatter, "invalid repository source: {error}"),
            Self::Persistence(error) => write!(formatter, "persist quick bundle: {error}"),
        }
    }
}

impl std::error::Error for QuickBundleFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSource(error) | Self::Persistence(error) => Some(error.root_cause()),
        }
    }
}

/// Create a quick bundle from a local repository or GitHub source and persist
/// it as one serialized fresh-config transaction. Identical sources reuse the
/// existing configured bundle, matching the terminal's behavior. The returned
/// config is the same fresh config that was written, allowing a serving
/// projection to publish the new bundle before acknowledging the request.
pub fn create_quick_bundle(
    source: &str,
) -> std::result::Result<QuickBundleCreation, QuickBundleFailure> {
    let (config, bundle_id) = Config::update(|config| {
        create_quick_bundle_in_config(config, source)
            .map_err(|error| anyhow::Error::new(QuickBundleFailure::InvalidSource(error)))
    })
    .map_err(|error| {
        error
            .downcast::<QuickBundleFailure>()
            .unwrap_or_else(QuickBundleFailure::Persistence)
    })?;
    Ok(QuickBundleCreation { config, bundle_id })
}

/// Add a quick bundle to an already-loaded config. The helper still performs
/// the local repository canonicalization/GitHub-source parsing, but callers
/// that persist a config should use [`create_quick_bundle`] so concurrent saves
/// cannot clobber one another.
pub fn create_quick_bundle_in_config(config: &mut Config, source: &str) -> Result<String> {
    let source = interpret_repository_source(source)?;
    let existing = match &source.kind {
        RepositorySourceKind::Local(root) => configured_bundle_for_local(config, root),
        RepositorySourceKind::Github(repository) => {
            configured_bundle_for_origin(config, repository)
        }
    };
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let repository_id = setup_style_id(&source.name);
    let mut bundle_id = repository_id.clone();
    for suffix in 2_u32.. {
        if !config.bundles.contains_key(&bundle_id) {
            break;
        }
        bundle_id = format!("{repository_id}-{suffix}");
    }
    config.bundles.insert(
        bundle_id.clone(),
        ProjectBundle {
            primary_repo: repository_id.clone(),
            repositories: vec![source.into_project_repository(repository_id.clone())],
        },
    );
    config.validate()?;
    Ok(bundle_id)
}

/// Create one bundle from one or more local repositories or GitHub sources.
///
/// All sources are interpreted and checked before the config transaction can
/// write anything. An existing bundle is reused only when its repository set
/// and primary repository exactly match the request; this keeps selecting one
/// repository from a larger bundle from silently changing the wizard's choice.
pub fn create_bundle_from_sources(
    sources: &[String],
) -> std::result::Result<QuickBundleCreation, QuickBundleFailure> {
    let (config, bundle_id) = Config::update(|config| {
        create_bundle_from_sources_in_config(config, sources)
            .map_err(|error| anyhow::Error::new(QuickBundleFailure::InvalidSource(error)))
    })
    .map_err(|error| {
        error
            .downcast::<QuickBundleFailure>()
            .unwrap_or_else(QuickBundleFailure::Persistence)
    })?;
    Ok(QuickBundleCreation { config, bundle_id })
}

/// Add a bundle for all `sources` to an already-loaded config. The source
/// interpretation is shared with the persisted [`create_bundle_from_sources`]
/// entry point and the legacy quick-bundle helper.
pub fn create_bundle_from_sources_in_config(
    config: &mut Config,
    sources: &[String],
) -> Result<String> {
    let sources = sources
        .iter()
        .map(|source| interpret_repository_source(source))
        .collect::<Result<Vec<_>>>()?;
    if sources.is_empty() {
        bail!("at least one repository source is required");
    }

    let mut identities = BTreeSet::new();
    for source in &sources {
        if !identities.insert(source.identity()) {
            bail!("duplicate repository source {:?}", source.display_name);
        }
    }

    if let Some(existing) = exact_configured_bundle(config, &sources) {
        return Ok(existing);
    }

    // Build and validate a candidate before replacing the caller's config, so
    // a later validation error cannot leave an in-memory partial mutation.
    let mut updated = config.clone();
    let mut used_repository_ids = BTreeSet::new();
    let mut repositories = Vec::with_capacity(sources.len());
    for source in sources {
        let base = setup_style_id(&source.name);
        let repository_id = unique_id(&base, |candidate| used_repository_ids.contains(candidate));
        used_repository_ids.insert(repository_id.clone());
        repositories.push(source.into_project_repository(repository_id));
    }
    let primary_repo = repositories
        .first()
        .map(|repository| repository.id.clone())
        .context("at least one repository source is required")?;
    let bundle_id = unique_id(&primary_repo, |candidate| {
        updated.bundles.contains_key(candidate)
    });
    updated.bundles.insert(
        bundle_id.clone(),
        ProjectBundle {
            primary_repo,
            repositories,
        },
    );
    updated.validate()?;
    *config = updated;
    Ok(bundle_id)
}

#[derive(Debug, Clone)]
enum RepositorySourceKind {
    Github(crate::setup::GithubRepository),
    Local(PathBuf),
}

#[derive(Debug, Clone)]
struct InterpretedRepositorySource {
    display_name: String,
    name: String,
    kind: RepositorySourceKind,
}

impl InterpretedRepositorySource {
    fn identity(&self) -> RepositoryIdentity {
        match &self.kind {
            RepositorySourceKind::Github(repository) => RepositoryIdentity::Github(
                repository.owner.to_ascii_lowercase(),
                repository.repository.to_ascii_lowercase(),
            ),
            RepositorySourceKind::Local(root) => RepositoryIdentity::Local(root.clone()),
        }
    }

    fn into_project_repository(self, id: String) -> ProjectRepository {
        let (github, local) = match self.kind {
            RepositorySourceKind::Github(repository) => (
                Some(format!("{}/{}", repository.owner, repository.repository)),
                None,
            ),
            RepositorySourceKind::Local(root) => (None, Some(root)),
        };
        ProjectRepository {
            id: id.clone(),
            github,
            local,
            destination: PathBuf::from(id),
            git_ref: None,
        }
    }
}

/// Interpret a source once, including local Git canonicalization and GitHub
/// parsing, so all creation paths use exactly the same source semantics.
fn interpret_repository_source(source: &str) -> Result<InterpretedRepositorySource> {
    let source = source.trim();
    if source.is_empty() {
        bail!("repository source cannot be empty");
    }
    let expanded = mj_core::path_input::expand_local(Path::new(source))?;
    let candidate = expanded.as_path();
    if candidate.exists() {
        let root = mj_core::local_git::canonical_repository(candidate)?;
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .context("local repository has no usable directory name")?
            .to_owned();
        return Ok(InterpretedRepositorySource {
            display_name: source.to_owned(),
            name,
            kind: RepositorySourceKind::Local(root),
        });
    }
    if candidate.is_absolute() || source.starts_with('.') || source.starts_with('~') {
        bail!("local repository path {source:?} does not exist");
    }
    let repository = github_repository_from_origin(source).context(format!(
        "{source:?} is not a GitHub owner/repository or URL"
    ))?;
    Ok(InterpretedRepositorySource {
        display_name: source.to_owned(),
        name: repository.repository.clone(),
        kind: RepositorySourceKind::Github(repository),
    })
}

fn exact_configured_bundle(
    config: &Config,
    requested: &[InterpretedRepositorySource],
) -> Option<String> {
    let requested_identities = requested
        .iter()
        .map(InterpretedRepositorySource::identity)
        .collect::<BTreeSet<_>>();
    let primary = requested.first()?.identity();
    config.bundles.iter().find_map(|(id, bundle)| {
        if bundle.repositories.len() != requested.len()
            || bundle
                .repositories
                .iter()
                .any(|repository| repository.git_ref.is_some())
        {
            return None;
        }
        bundle_matches(bundle, &requested_identities, &primary).then(|| id.clone())
    })
}

fn unique_id(base: &str, mut is_used: impl FnMut(&str) -> bool) -> String {
    if !is_used(base) {
        return base.to_owned();
    }
    for suffix in 2_u32.. {
        let suffix = format!("-{suffix}");
        let prefix_len = 64usize.saturating_sub(suffix.len());
        let prefix = base.chars().take(prefix_len).collect::<String>();
        let candidate = format!("{prefix}{suffix}");
        if !is_used(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 repository/bundle id suffixes exhausted")
}

pub struct SessionLaunchOptions {
    pub create_managed_worktree: Option<bool>,
    pub launch_base: Option<String>,
    pub mjolnir_subagents: Option<bool>,
    pub initial_prompt: Option<String>,
    pub workspace_id: String,
    pub additional_mounts: Vec<AdditionalMount>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub project_directory: Option<PathBuf>,
    pub session_title_override: Option<String>,
}

pub struct SessionResumeOptions {
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub discard_queue: bool,
}

fn selected_host_container_size(
    template: &TargetTemplate,
    allocation: Option<&SessionResourceAllocation>,
) -> Option<(String, HostContainerSize)> {
    let host = container_size_host(template)?;
    let SessionResourceAllocation::Container { cpus, memory_bytes } = allocation? else {
        return None;
    };
    Some((
        host.to_owned(),
        HostContainerSize {
            cpus: *cpus,
            memory_bytes: *memory_bytes,
        },
    ))
}

impl Controller {
    pub fn load() -> Result<Self> {
        let config = Config::load()?;
        let state = crate::database::load_state()?;
        // Missing session dependencies must not lock users out of the tools
        // needed to repair them. Operations validate the session they act on.
        state.validate()?;
        for session in state.sessions.values() {
            if let Some(issue) = session.configuration_issue(&config) {
                tracing::warn!(session_id = %session.id, "{issue}");
            }
        }
        Ok(Self { config, state })
    }

    pub fn reload(&mut self) -> Result<()> {
        *self = Self::load()?;
        Ok(())
    }

    fn persist_session_state(&self, session_id: &str) -> Result<()> {
        match self.state.sessions.get(session_id) {
            Some(session) => crate::database::save_lifecycle_session(session),
            None => crate::database::delete_session(session_id),
        }
    }

    fn persist_session_transition_or_restore(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        context: &'static str,
    ) -> Result<()> {
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            previous,
            context,
            &crate::database::save_lifecycle_session,
        )
    }

    fn restore_prior_session_after_persistence_failure(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        primary: anyhow::Error,
    ) -> anyhow::Error {
        restore_session_after_persistence_failure(
            &mut self.state,
            session_id,
            previous,
            primary,
            crate::database::save_lifecycle_session,
        )
    }

    /// Resolve an entered path on its owning host. Call only from background work.
    pub fn resolve_input_path(
        &self,
        target_id: &str,
        path: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<PathBuf> {
        let target = self
            .config
            .targets
            .get(target_id)
            .context("Unknown path target")?;
        resolve_target_input_path(target, path, executor)
    }

    /// Verify a mount source on the host where Mjolnir will consume it, and report
    /// the filesystem reason it must be attached read-only, if there is one.
    ///
    /// The probe runs in the same round trip as the existence check so the
    /// editor learns both answers without a second wait. A probe that cannot
    /// answer reports no reason: provisioning decides that authoritatively.
    pub fn validate_mount_source(
        &self,
        target_id: &str,
        source: &Path,
        executor: &impl CommandExecutor,
    ) -> Result<Option<String>> {
        let target = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?;
        let exists = match target {
            TargetTemplate::LocalPodman { .. }
            | TargetTemplate::LocalDocker { .. }
            | TargetTemplate::AppleContainer { .. }
            | TargetTemplate::AwsEc2 { .. } => std::fs::metadata(source)
                .map(|metadata| metadata.is_dir())
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(false)
                    } else {
                        Err(error)
                    }
                })
                .with_context(|| format!("inspect resource source {}", source.display()))?,
            TargetTemplate::SshPodman { ssh, .. } | TargetTemplate::SshDocker { ssh, .. } => {
                targets::ssh_directory_exists(&SshTarget::from(ssh), source, executor)?
            }
            TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => {
                bail!("resource attachments are unsupported for bare targets")
            }
        };
        ensure!(
            exists,
            "source path {} does not exist or is not a directory",
            source.display()
        );
        Ok(self.forced_read_only_reason(target, source, executor))
    }

    /// The `filesystem (reason)` label for a source the runtime cannot overlay.
    fn forced_read_only_reason(
        &self,
        target: &TargetTemplate,
        source: &Path,
        executor: &impl CommandExecutor,
    ) -> Option<String> {
        let ssh = match target {
            TargetTemplate::LocalPodman { .. } | TargetTemplate::LocalDocker { .. } => None,
            TargetTemplate::SshPodman { ssh, .. } | TargetTemplate::SshDocker { ssh, .. } => {
                Some(SshTarget::from(ssh))
            }
            // Apple Container already mounts read-only, and EC2 copies instead
            // of mounting, so neither has an overlay to lose.
            _ => return None,
        };
        let filesystem = targets::probe_filesystem_types(
            ssh.as_ref(),
            std::slice::from_ref(&source.to_path_buf()),
            executor,
        )
        .map_err(|error| {
            tracing::debug!(
                source = %source.display(),
                error = format!("{error:#}"),
                "could not probe the filesystem under a mount source"
            );
        })
        .ok()?
        .pop()?;
        let reason = targets::overlay_unsupported_filesystem(&filesystem)?;
        Some(format!("{filesystem} ({reason})"))
    }

    fn fail_new_session_with_cleanup(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
        executor: &impl CommandExecutor,
    ) -> Result<anyhow::Error> {
        let original = provisioning::note_new_session_launch_failure(session_id, &error);
        let cleanup_error = self
            .cleanup_new_session_worktree_after_failure(session_id, executor)
            .err()
            .map(|cleanup_error| format!("{cleanup_error:#}"));
        if let Some(cleanup_error) = &cleanup_error {
            tracing::warn!(
                session_id,
                error = %cleanup_error,
                "new-session worktree rollback reported a cleanup failure"
            );
        }
        let failure = apply_failed_new_session_rollback(
            &mut self.state,
            session_id,
            &original,
            cleanup_error,
        );
        self.persist_session_state(session_id)?;
        Ok(failure)
    }

    pub fn register_session_with_resources(
        &mut self,
        profile_id: &str,
        bundle_id: &str,
        target_id: &str,
        title: impl Into<String>,
        options: SessionLaunchOptions,
    ) -> Result<String> {
        let SessionLaunchOptions {
            create_managed_worktree,
            launch_base,
            mjolnir_subagents,
            initial_prompt,
            workspace_id,
            additional_mounts,
            resource_allocation,
            project_directory,
            session_title_override,
        } = options;
        let launch_base = match launch_base {
            Some(base) => {
                let base = base.trim();
                if base.is_empty() {
                    bail!("launch base must not be empty");
                }
                Some(base.to_owned())
            }
            None => None,
        };
        if launch_base.is_some() && create_managed_worktree == Some(false) {
            bail!("a launch base requires a managed worktree or a bundle session");
        }
        let session_title_override = match session_title_override {
            Some(title) => {
                Some(normalize_session_title(&title).context("session name cannot be empty")?)
            }
            None => None,
        };
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .with_context(|| format!("unknown profile {profile_id:?}"))?;
        ensure!(profile.enabled, "profile {profile_id:?} is disabled");
        let template = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?;
        if create_managed_worktree == Some(true) && !is_bare_project_target(template) {
            bail!("managed worktree creation requires a bare Git project");
        }
        if project_directory.is_some() != is_bare_project_target(template) {
            bail!("raw project directories require a bare target, and bare targets require one");
        }
        if let Some(path) = &project_directory
            && (!path.is_absolute()
                || path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir))
        {
            bail!("bare project directory must be an absolute safe path");
        }
        let bundle = project_directory
            .is_none()
            .then(|| self.config.bundles.get(bundle_id))
            .flatten();
        if project_directory.is_none() && bundle.is_none() {
            bail!("unknown bundle {bundle_id:?}");
        }
        if profile.kind == mj_core::config::HarnessKind::Muse
            && (!additional_mounts.is_empty()
                || bundle.is_some_and(|bundle| bundle.repositories.len() > 1))
        {
            bail!(
                "{} ACP supports one workspace root; use a single-repository bundle without attached directories",
                profile.kind.display_name()
            );
        }
        if let Some(bundle) = bundle {
            for repository in &bundle.repositories {
                mj_core::remote_git::resolve_repository(
                    repository,
                    &targets::CancellableProcessExecutor::with_timeout(
                        std::time::Duration::from_secs(15),
                    ),
                )
                .with_context(|| format!("repository {:?}", repository.id))?;
            }
        }
        validate_resource_allocation(template, resource_allocation.as_ref())?;
        let selected_container_size =
            selected_host_container_size(template, resource_allocation.as_ref());
        if !additional_mounts.is_empty() && mount_history_host(template).is_none() {
            bail!("attached resources are unsupported for this target");
        }
        targets::validate_additional_mounts(&additional_mounts)?;
        let id = new_session_id()?;
        let now = now();
        let record = SessionRecord {
            build_cache: None,
            create_managed_worktree,
            launch_base,
            mjolnir_subagents,
            archived: false,
            container_cpus: None,
            container_memory: None,
            // Recorded for every new session, container-backed or not, so a
            // later move into a container already knows the path its checkout
            // will occupy. Only sessions that predate per-session container
            // workspaces leave it unset.
            container_workspace: Some(targets::new_container_workspace(&id)?),
            id: id.clone(),
            workspace_id,
            title: title.into(),
            harness_kind: profile.kind,
            last_profile: profile_id.to_string(),
            bundle_id: bundle_id.to_string(),
            project_directory,
            managed_worktree: None,
            target_template_id: target_id.to_string(),
            resource_allocation,
            additional_mounts: additional_mounts.clone(),
            state: SessionState::Provisioning,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override,
            created_at: now.clone(),
            updated_at: now,
            viewed_through_event_ordinal: 0,
            draft_input: initial_prompt.unwrap_or_default(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        // Creation authors the whole record, so it writes the whole row. The
        // record reaches memory only once it is durable: a session this process
        // alone knows about is one the database can never resume or clean up.
        if let Some((host, size)) = selected_container_size.as_ref() {
            crate::database::save_session_with_container_size(&record, host, *size)?;
        } else {
            crate::database::save_session(&record)?;
        }
        self.state.sessions.insert(id.clone(), record);
        if let Some((host, size)) = selected_container_size {
            self.state.remember_container_size(&host, size);
        }
        if let Some(host) = mount_history_host(template) {
            // Mount history only seeds the attach dialog's suggestions. The
            // session row is already committed, so a failed suggestion write is
            // reported rather than turned into a failed registration.
            match crate::database::remember_mount_sources(host, &additional_mounts) {
                Ok(()) => self.state.remember_mount_sources(host, &additional_mounts),
                Err(error) => tracing::warn!(
                    session_id = id,
                    error = format!("{error:#}"),
                    "could not remember the attached resource directories for later suggestions"
                ),
            }
        }
        Ok(id)
    }

    pub fn rename_session(&mut self, session_id: &str, title: &str) -> Result<String> {
        let title = normalize_session_title(title).context("session name cannot be empty")?;
        ensure!(
            self.state.sessions.contains_key(session_id),
            "unknown session {session_id}"
        );
        let updated_at = now();
        crate::database::set_session_title_override(session_id, &title, &updated_at)?;
        let record = self
            .state
            .sessions
            .get_mut(session_id)
            .expect("session was checked before updating its title");
        record.session_title_override = Some(title.clone());
        record.updated_at = updated_at;
        Ok(title)
    }

    pub fn rename_profile_id(&mut self, old_id: &str, new_id: &str) -> Result<()> {
        mj_core::config::validate_id("profile", new_id)?;
        if old_id == new_id {
            ensure!(
                self.config.profiles.contains_key(old_id),
                "unknown profile {old_id:?}"
            );
            return Ok(());
        }
        let journal = ConfigRenameJournal {
            kind: ConfigRenameKind::Profile,
            old_id: old_id.to_owned(),
            new_id: new_id.to_owned(),
        };
        write_config_rename_journal(&journal)?;
        let (config, ()) = match Config::update(|config| {
            ensure!(
                config.profiles.contains_key(old_id),
                "unknown profile {old_id:?}"
            );
            ensure!(
                !config.profiles.contains_key(new_id),
                "profile {new_id:?} already exists"
            );
            let profile = config
                .profiles
                .remove(old_id)
                .expect("profile was checked in the transaction");
            config.profiles.insert(new_id.to_owned(), profile);
            Ok(())
        }) {
            Ok(result) => result,
            Err(error) => {
                remove_config_rename_journal()
                    .context("remove profile rename journal after config save failed")?;
                return Err(error).context("save renamed profile configuration");
            }
        };
        self.config = config;
        mj_core::test_hooks::reach_test_hook("config_replacement_before_reference_migration")?;
        if let Err(error) = crate::database::rename_profile_references(old_id, new_id) {
            let restore = Config::update(|config| {
                let profile = config
                    .profiles
                    .remove(new_id)
                    .with_context(|| format!("renamed profile {new_id:?} is missing"))?;
                ensure!(
                    !config.profiles.contains_key(old_id),
                    "cannot restore profile rename: both {old_id:?} and {new_id:?} exist"
                );
                config.profiles.insert(old_id.to_owned(), profile);
                Ok(())
            });
            let restored = match restore {
                Ok((config, ())) => config,
                Err(restore_error) => {
                    return Err(error).context(format!(
                        "rename profile references; additionally failed to restore config: {restore_error:#}"
                    ));
                }
            };
            self.config = restored;
            if let Err(restore_error) = remove_config_rename_journal() {
                return Err(error).context(format!(
                    "rename profile references; additionally failed to remove rename journal: {restore_error:#}"
                ));
            }
            return Err(error).context("rename profile references");
        }
        for session in self.state.sessions.values_mut() {
            if session.last_profile == old_id {
                session.last_profile = new_id.to_owned();
            }
        }
        remove_config_rename_journal()?;
        Ok(())
    }

    pub fn rename_target_id(&mut self, old_id: &str, new_id: &str) -> Result<()> {
        mj_core::config::validate_id("target template", new_id)?;
        if old_id == new_id {
            ensure!(
                self.config.targets.contains_key(old_id),
                "unknown target {old_id:?}"
            );
            return Ok(());
        }
        let journal = ConfigRenameJournal {
            kind: ConfigRenameKind::Target,
            old_id: old_id.to_owned(),
            new_id: new_id.to_owned(),
        };
        write_config_rename_journal(&journal)?;
        let (config, ()) = match Config::update(|config| {
            ensure!(
                config.targets.contains_key(old_id),
                "unknown target {old_id:?}"
            );
            ensure!(
                !config.targets.contains_key(new_id),
                "target {new_id:?} already exists"
            );
            let target = config
                .targets
                .remove(old_id)
                .expect("target was checked in the transaction");
            config.targets.insert(new_id.to_owned(), target);
            Ok(())
        }) {
            Ok(result) => result,
            Err(error) => {
                remove_config_rename_journal()
                    .context("remove target rename journal after config save failed")?;
                return Err(error).context("save renamed target configuration");
            }
        };
        self.config = config;
        mj_core::test_hooks::reach_test_hook("config_replacement_before_reference_migration")?;
        if let Err(error) = crate::database::rename_target_references(old_id, new_id) {
            let restore = Config::update(|config| {
                let target = config
                    .targets
                    .remove(new_id)
                    .with_context(|| format!("renamed target {new_id:?} is missing"))?;
                ensure!(
                    !config.targets.contains_key(old_id),
                    "cannot restore target rename: both {old_id:?} and {new_id:?} exist"
                );
                config.targets.insert(old_id.to_owned(), target);
                Ok(())
            });
            let restored = match restore {
                Ok((config, ())) => config,
                Err(restore_error) => {
                    return Err(error).context(format!(
                        "rename target references; additionally failed to restore config: {restore_error:#}"
                    ));
                }
            };
            self.config = restored;
            if let Err(restore_error) = remove_config_rename_journal() {
                return Err(error).context(format!(
                    "rename target references; additionally failed to remove rename journal: {restore_error:#}"
                ));
            }
            return Err(error).context("rename target references");
        }
        for session in self.state.sessions.values_mut() {
            if session.target_template_id == old_id {
                session.target_template_id = new_id.to_owned();
            }
        }
        remove_config_rename_journal()?;
        Ok(())
    }

    /// Finish a profile/target id rename interrupted between the atomic config
    /// replacement and SQLite transaction. Each step is idempotent, so a
    /// second crash leaves the same intent available for the next startup.
    pub fn recover_config_id_rename() -> Result<bool> {
        let path = config_rename_journal_path();
        let body = match fs::read(&path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context(format!("read {}", path.display())),
        };
        let journal: ConfigRenameJournal =
            serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))?;
        match journal.kind {
            ConfigRenameKind::Profile => {
                Config::update(|config| {
                    finish_config_map_rename(
                        &mut config.profiles,
                        &journal.old_id,
                        &journal.new_id,
                        "profile",
                    )?;
                    Ok(())
                })?;
                crate::database::rename_profile_references(&journal.old_id, &journal.new_id)?;
            }
            ConfigRenameKind::Target => {
                // The file's own entries, not `Config::update`'s view: that one
                // adds the standard local targets, so after a built-in id such
                // as `localhost` was renamed away, its default would reappear
                // beside the new id and read as a rename that cannot finish.
                Config::update_to(&mj_core::config::config_path(), |config| {
                    finish_config_map_rename(
                        &mut config.targets,
                        &journal.old_id,
                        &journal.new_id,
                        "target",
                    )?;
                    Ok(())
                })?;
                crate::database::rename_target_references(&journal.old_id, &journal.new_id)?;
            }
        }
        remove_config_rename_journal()?;
        Ok(true)
    }

    /// Record the per-session container size overrides and attached
    /// directories. Nothing is applied to a running container: the values are
    /// read the next time the session's container is created.
    pub fn update_session_container_settings(
        &mut self,
        session_id: &str,
        cpus: Option<String>,
        memory: Option<String>,
        additional_mounts: Vec<targets::AdditionalMount>,
        mount_history: Vec<std::path::PathBuf>,
    ) -> Result<()> {
        ensure!(
            self.state.sessions.contains_key(session_id),
            "unknown session {session_id}"
        );
        let cpus = cpus.filter(|value| !value.trim().is_empty());
        let memory = memory.filter(|value| !value.trim().is_empty());
        let updated_at = now();
        crate::database::set_session_container_settings(
            session_id,
            cpus.as_deref(),
            memory.as_deref(),
            &additional_mounts,
            &updated_at,
        )?;
        if let Some(host) = self
            .config
            .targets
            .get(
                &self.state.sessions[session_id]
                    .target_template_id
                    .to_owned(),
            )
            .and_then(mj_core::config::mount_history_host)
        {
            let host = host.to_owned();
            // The dialog owns the suggestion list, so forgetting a directory
            // there has to survive the mounts being remembered right after.
            crate::database::replace_mount_history(&host, &mount_history)?;
            crate::database::remember_mount_sources(&host, &additional_mounts)?;
            self.state.mount_history.insert(host.clone(), mount_history);
            self.state.remember_mount_sources(&host, &additional_mounts);
        }
        let record = self
            .state
            .sessions
            .get_mut(session_id)
            .expect("session was checked before updating its container settings");
        record.container_cpus = cpus;
        record.container_memory = memory;
        record.additional_mounts = additional_mounts;
        record.updated_at = updated_at;
        Ok(())
    }
}

fn config_rename_journal_path() -> PathBuf {
    data_dir().join(CONFIG_RENAME_JOURNAL)
}

fn write_config_rename_journal(journal: &ConfigRenameJournal) -> Result<()> {
    let path = config_rename_journal_path();
    let body = serde_json::to_vec(journal).context("serialize config rename journal")?;
    atomic_write(&path, &body).with_context(|| format!("write {}", path.display()))
}

fn remove_config_rename_journal() -> Result<()> {
    let path = config_rename_journal_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn finish_config_map_rename<T>(
    entries: &mut BTreeMap<String, T>,
    old_id: &str,
    new_id: &str,
    kind: &str,
) -> Result<()> {
    if let Some(entry) = entries.remove(old_id) {
        ensure!(
            !entries.contains_key(new_id),
            "cannot recover {kind} rename: both {old_id:?} and {new_id:?} exist"
        );
        entries.insert(new_id.to_owned(), entry);
    } else {
        ensure!(
            entries.contains_key(new_id),
            "cannot recover {kind} rename: neither {old_id:?} nor {new_id:?} exists"
        );
    }
    Ok(())
}

/// Whether this profile must run from a private staged copy of its home even on
/// a local bare target, where a session would otherwise use the profile home
/// directly.
///
/// A Codex profile with a custom model provider qualifies: Mjolnir generates the
/// provider's model catalog for each launch and points the staged `config.toml`
/// at it, and it must never write either into the user's own profile home.
pub(crate) fn requires_private_profile_home(profile: &mj_core::config::HarnessProfile) -> bool {
    profile.codex_provider().ok().flatten().is_some()
}

/// Whether this session's harness home belongs to the session rather than to
/// the profile.
///
/// Only such a session has anywhere to stage files into. Staging when this is
/// false would copy the daemon's staged profile straight over the user's own
/// harness configuration, and nothing would ever remove it again: teardown
/// removes what [`removable_profile_root`] names, which is nothing here.
pub(crate) fn session_owns_profile_home(
    locator: &targets::TargetLocator,
    session_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> bool {
    removable_profile_root(locator, session_id, profile).is_some()
}

/// Whether a Claude session on a local bare target runs from a private staged
/// home. It can only do so where `CLAUDE_CONFIG_DIR` is what points Claude at
/// that home; on macOS the variable scopes nothing, so a private copy would be
/// a home Claude never reads. See
/// [`HarnessKind::scopes_home_with_environment`](mj_core::config::HarnessKind::scopes_home_with_environment).
fn claude_takes_a_private_home(
    profile: &mj_core::config::HarnessProfile,
    locator: &targets::TargetLocator,
) -> bool {
    profile.kind == mj_core::config::HarnessKind::Claude
        && profile
            .kind
            .scopes_home_with_environment(locator.harness_host())
}

/// Where this session's harness reads and writes its profile inside the target.
///
/// Every case but one is the per-session root `removable_profile_root` names; a
/// profile that runs straight out of the user's own home has no per-session root
/// and uses that home. Muse keeps its state in a `muse` subdirectory of the
/// root, because its ACP adapter owns the directory it is given.
#[cfg(test)]
pub(crate) fn target_profile_home_for_test(
    locator: &targets::TargetLocator,
    session_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> String {
    target_profile_home(locator, session_id, profile)
}

fn target_profile_home(
    locator: &targets::TargetLocator,
    session_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> String {
    let root = removable_profile_root(locator, session_id, profile)
        .unwrap_or_else(|| profile.home.to_string_lossy().into_owned());
    if profile.kind == mj_core::config::HarnessKind::Muse {
        PathBuf::from(root)
            .join("muse")
            .to_string_lossy()
            .into_owned()
    } else {
        root
    }
}

/// The per-session profile directory an in-place harness replacement may delete,
/// or `None` when the session runs straight out of the user's own profile home.
///
/// This is the root that [`target_profile_home`] derives its answer from, not
/// that answer itself: a Muse session's home is a `muse` subdirectory of a
/// per-session root, and the whole root is what belongs to the session.
pub(super) fn removable_profile_root(
    locator: &targets::TargetLocator,
    session_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> Option<String> {
    match locator {
        targets::TargetLocator::LocalBare { worker_root } => {
            if profile.kind == mj_core::config::HarnessKind::Muse {
                Some(
                    mj_core::config::data_dir()
                        .join("profiles")
                        .join(session_id)
                        .to_string_lossy()
                        .into_owned(),
                )
            } else if claude_takes_a_private_home(profile, locator)
                || requires_private_profile_home(profile)
            {
                Some(
                    Path::new(worker_root)
                        .join("profile")
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                // The session reads and writes the user's own profile home.
                // Nothing here belongs to the session, so nothing is removed.
                None
            }
        }
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => {
            Some(format!("/var/lib/hel/profiles/{session_id}"))
        }
        targets::TargetLocator::AwsEc2 { .. } | targets::TargetLocator::SshBare { .. } => {
            Some(format!(".local/share/hel/profiles/{session_id}"))
        }
    }
}

/// Resolve the login home on the machine that owns an editable path.
pub fn resolve_target_input_path(
    target: &TargetTemplate,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    if !mj_core::path_input::needs_home(path)? {
        return Ok(path.to_path_buf());
    }
    let host = cache_host::CacheHost::for_path_target(target)?;
    mj_core::path_input::expand_home(path, Some(&host.home(executor)?))
}

/// Resolve the login home on a configured machine, for the Settings screen's
/// path fields. A machine, not a runtime, is what owns a home directory.
pub fn resolve_machine_input_path(
    machine: &mj_core::config::Machine,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    if !mj_core::path_input::needs_home(path)? {
        return Ok(path.to_path_buf());
    }
    // An EC2 instance does not exist until a session starts, so the only home
    // this screen can resolve is this machine's.
    let host = cache_host::CacheHost::for_path_machine(machine)?;
    mj_core::path_input::expand_home(path, Some(&host.home(executor)?))
}

fn execute_checked(executor: &impl CommandExecutor, command: CommandSpec) -> Result<CommandOutput> {
    let output = executor.execute(&command)?;
    if output.status != 0 {
        let detail = command_error_detail(&output.stderr);
        if detail.is_empty() {
            bail!("{} failed with status {}", command.purpose, output.status);
        }
        bail!("{detail}");
    }
    Ok(output)
}

fn command_error_detail(stderr: &[u8]) -> String {
    let reported = String::from_utf8_lossy(stderr);
    let reported = reported.trim();
    let detail = reported
        .rsplit_once("\nCaused by:\n")
        .map_or(reported, |(_, causes)| causes);
    let detail = detail.strip_prefix("Error: ").unwrap_or(detail);
    detail
        .lines()
        .map(|line| line.strip_prefix("    ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn restore_session_after_persistence_failure(
    state: &mut State,
    session_id: &str,
    previous: &SessionRecord,
    primary: anyhow::Error,
    persist: impl FnOnce(&SessionRecord) -> Result<()>,
) -> anyhow::Error {
    state
        .sessions
        .insert(session_id.to_owned(), previous.clone());
    let restored = state
        .sessions
        .get(session_id)
        .expect("restored session record disappeared");
    match persist(restored) {
        Ok(()) => primary,
        Err(error) => primary.context(format!(
            "restored prior session state in memory, but failed to persist the rollback: {error:#}"
        )),
    }
}

fn persist_session_record_transition_or_restore(
    state: &mut State,
    session_id: &str,
    previous: &SessionRecord,
    context: &'static str,
    persist: &impl Fn(&SessionRecord) -> Result<()>,
) -> Result<()> {
    let result = persist(
        state
            .sessions
            .get(session_id)
            .expect("checkpoint session disappeared before persistence"),
    );
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(restore_session_after_persistence_failure(
            state,
            session_id,
            previous,
            error.context(context),
            persist,
        )),
    }
}

pub fn config_only_controller(config: Config) -> Controller {
    Controller {
        config,
        state: State::default(),
    }
}

#[cfg(test)]
mod tests;
