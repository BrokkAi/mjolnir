//! Resuming a stopped session onto a profile and target.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::ContentBlock;
use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::checkpoint_transfer::restore_command;
use crate::session_manager::new_command_id;
use mj_checkpoint::archive::{
    CanonicalQueuedCommandKind, CanonicalSessionSnapshot, CheckpointRepositoryBundle, SystemGit,
    checkpoint_bundle_prerequisites, read_checkpoint_repository_bundles, verify_archive_streaming,
};
use mj_checkpoint::checkpoint::CheckpointRestoreSpec;
use mj_core::config::{Config, HarnessKind, ProjectRepository, mount_history_host};
use mj_core::state::{MaterializedSession, SessionRecord, SessionResourceAllocation, SessionState};
use mj_transcript::projection::materialized_session_from_canonical;

use crate::targets::{
    self, AdditionalMount, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec,
    ProcessExecutor, ProvisionStage, ProvisionStageGuard,
};
use mj_core::relay::RelayCommand;

use super::backend::{backend_locator, controller_github_token, validate_resource_allocation};
use super::checkpoint::upload_checkpoint_spec;
use super::provisioning::{
    ProvisioningFailureDisposition, StagedExecutor, execute_concurrent_lanes,
    install_attached_resources,
};
use super::readiness::{connect_started_worker, wait_for_native_session_in_stage};
use super::worker_binary::{bridge_readiness_stage, start_worker, worker_probe_diagnosis};
use super::worktree::{
    PrimaryCheckoutRequirement, ResumeConversion, ResumePlan, apply_raw_to_workspace,
    apply_workspace_to_raw, cleanup_managed_worktree, create_managed_worktree,
    managed_worktree_checkout_exists, plan_raw_to_workspace,
    preserve_retained_managed_worktree_branch, raw_checkout_divergence_notice,
    raw_checkout_position, raw_checkout_snapshot, raw_conversion_preview, restore_managed_worktree,
    resume_compatibility, retire_managed_worktree,
};
use super::{
    Controller, SessionResumeOptions, execute_checked, now, selected_host_container_size,
    target_profile_home,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRepositorySourceMismatch {
    pub session_id: String,
    pub bundle_id: String,
    pub repository_id: String,
    pub missing_commit: String,
    pub archived_origin: String,
    pub configured_origin: String,
}

pub use mj_core::state::ResumeRepositorySourceReceipt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeRepositorySourcePreflight {
    Ready(ResumeRepositorySourceReceipt),
    RepositoryMoved(ResumeRepositorySourceMismatch),
    /// The resume converts a local checkout into an isolated workspace. The
    /// receipt is already valid; the preview is what a person has to confirm
    /// before the checkout is snapshotted and the session moves off this
    /// machine.
    ConvertingRawCheckout {
        receipt: ResumeRepositorySourceReceipt,
        preview: Box<mj_core::state::RawConversionPreview>,
    },
}

struct ResumeRepositoryBundles {
    checkpoint_sha256: String,
    repositories: Vec<CheckpointRepositoryBundle>,
}

/// A small timing scope for the expensive resume phases. Target commands
/// already trace their own durations; this covers controller-side work and
/// lets an operator see where a slow resume spent its wall-clock budget.
struct ResumePhaseTimer<'a> {
    session_id: &'a str,
    phase: &'static str,
    started: Instant,
}

impl<'a> ResumePhaseTimer<'a> {
    fn new(session_id: &'a str, phase: &'static str) -> Self {
        Self {
            session_id,
            phase,
            started: Instant::now(),
        }
    }
}

impl Drop for ResumePhaseTimer<'_> {
    fn drop(&mut self) {
        tracing::debug!(
            session_id = self.session_id,
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis(),
            "resume phase completed"
        );
    }
}

impl Controller {
    /// Muse has one workspace root; native restore relocates its metadata.
    pub(super) fn validate_muse_resume_destination(
        &self,
        source: &SessionRecord,
        destination_harness: HarnessKind,
        _target_id: &str,
    ) -> Result<()> {
        if destination_harness != HarnessKind::Muse {
            return Ok(());
        }
        ensure!(
            source.project_directory.is_some()
                || self
                    .config
                    .bundles
                    .get(&source.bundle_id)
                    .is_none_or(|bundle| bundle.repositories.len() == 1),
            "Muse Code ACP supports one workspace root; use a single-repository bundle"
        );
        Ok(())
    }
    /// Prove that each configured repository source still supplies the commit
    /// boundary its checkpoint bundle expects, before provisioning anything,
    /// and describe a local checkout's conversion so a person can confirm it.
    pub fn preflight_resume_repository_sources(
        &self,
        session_id: &str,
        target_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        self.preflight_repository_sources(session_id, target_id, true, executor)
    }

    /// `describe_conversion` buys the conversion preview with a read of the
    /// checkout and a question to its remote. The resume itself only needs to
    /// know whether a configured source moved, and the person has already
    /// confirmed by then, so it asks for the cheap answer.
    fn preflight_repository_sources(
        &self,
        session_id: &str,
        target_id: &str,
        describe_conversion: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let checkpoint = session
            .checkpoint
            .as_ref()
            .context("session has no checkpoint")?;
        let plan = resume_compatibility(session, &self.config, target_id)
            .map_err(|reason| anyhow::anyhow!(reason))?;
        if session.project_directory.is_some() {
            debug_assert!(matches!(
                plan,
                ResumePlan::InPlace | ResumePlan::RawToWorkspace
            ));
            // A raw session resumes from its live checkout. Its synthetic
            // bundle is only a grouping identity and may no longer be in the
            // config; neither an in-place resume nor a raw-to-workspace
            // conversion restores repository contents from that bundle.
            let receipt = ResumeRepositorySourceReceipt {
                session_id: session_id.to_owned(),
                bundle_id: session.bundle_id.clone(),
                checkpoint_sha256: checkpoint.sha256.clone(),
                repositories: Vec::new(),
            };
            // A conversion reads the checkout and its remote so the person
            // sees what will travel. Planning failures (no network remote, an
            // unreachable remote, a dirty submodule) are this preflight's
            // error, which every surface already reports.
            if describe_conversion && plan == ResumePlan::RawToWorkspace {
                let preview = raw_conversion_preview_for(session, &self.config, executor)?;
                return Ok(ResumeRepositorySourcePreflight::ConvertingRawCheckout {
                    receipt,
                    preview: Box::new(preview),
                });
            }
            return Ok(ResumeRepositorySourcePreflight::Ready(receipt));
        }
        let repositories = read_checkpoint_repository_bundles(&checkpoint.archive_path)?;
        self.preflight_verified_repository_sources(
            session_id,
            ResumeRepositoryBundles {
                checkpoint_sha256: checkpoint.sha256.clone(),
                repositories,
            },
            None,
            plan != ResumePlan::WorkspaceToRaw,
            executor,
        )
    }

    fn preflight_verified_repository_sources(
        &self,
        session_id: &str,
        verified: ResumeRepositoryBundles,
        skip_repository_id: Option<&str>,
        use_archived_network_sources: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        ensure!(
            verified.repositories.iter().all(|repository| {
                !repository.metadata.origin.starts_with("mj-local:")
                    && !repository.metadata.origin.starts_with("ext::")
            }),
            "resuming legacy host-bridge sessions is not supported; start a new network-backed session"
        );
        // The immutable archive supplies network provenance. Local source
        // paths and later host configuration changes are irrelevant.
        if use_archived_network_sources
            && verified
                .repositories
                .iter()
                .all(|repository| repository.metadata.remote_workspace)
        {
            return Ok(ResumeRepositorySourcePreflight::Ready(
                ResumeRepositorySourceReceipt {
                    session_id: session_id.to_owned(),
                    bundle_id: session.bundle_id.clone(),
                    checkpoint_sha256: verified.checkpoint_sha256,
                    repositories: Vec::new(),
                },
            ));
        }
        if verified.repositories.is_empty() {
            return Ok(ResumeRepositorySourcePreflight::Ready(
                ResumeRepositorySourceReceipt {
                    session_id: session_id.to_owned(),
                    bundle_id: session.bundle_id.clone(),
                    checkpoint_sha256: verified.checkpoint_sha256,
                    repositories: Vec::new(),
                },
            ));
        }
        let bundle = self
            .config
            .bundles
            .get(&session.bundle_id)
            .with_context(|| format!("session bundle {:?} is missing", session.bundle_id))?;
        let configured = verified
            .repositories
            .iter()
            .map(|archived| {
                bundle
                    .repositories
                    .iter()
                    .find(|repository| repository.id == archived.metadata.id)
                    .cloned()
                    .with_context(|| {
                        format!(
                            "session bundle {:?} no longer contains repository {:?}",
                            session.bundle_id, archived.metadata.id
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let github_token = configured
            .iter()
            .any(|repository| repository.github.is_some())
            .then(controller_github_token)
            .flatten();
        let outcomes = verified
            .repositories
            .par_iter()
            .zip(configured.par_iter())
            .map(|(archived, configured)| {
                if skip_repository_id == Some(configured.id.as_str()) {
                    return Ok(None);
                }
                checkpoint_source_missing_commit(
                    configured,
                    archived,
                    executor,
                    github_token.as_deref(),
                )
                .map(|missing_commit| {
                    missing_commit.map(|missing_commit| ResumeRepositorySourceMismatch {
                        session_id: session_id.to_owned(),
                        bundle_id: session.bundle_id.clone(),
                        repository_id: configured.id.clone(),
                        missing_commit,
                        archived_origin: archived.metadata.origin.clone(),
                        configured_origin: configured.source_label(),
                    })
                })
            })
            .collect::<Vec<Result<Option<ResumeRepositorySourceMismatch>>>>();
        for outcome in outcomes {
            if let Some(mismatch) = outcome? {
                return Ok(ResumeRepositorySourcePreflight::RepositoryMoved(mismatch));
            }
        }
        Ok(ResumeRepositorySourcePreflight::Ready(
            ResumeRepositorySourceReceipt {
                session_id: session_id.to_owned(),
                bundle_id: session.bundle_id.clone(),
                checkpoint_sha256: verified.checkpoint_sha256,
                repositories: configured,
            },
        ))
    }

    fn repository_source_receipt_is_current(
        &self,
        session_id: &str,
        receipt: &ResumeRepositorySourceReceipt,
    ) -> bool {
        let Some(session) = self.state.sessions.get(session_id) else {
            return false;
        };
        if receipt.session_id != session_id
            || receipt.bundle_id != session.bundle_id
            || session
                .checkpoint
                .as_ref()
                .map(|checkpoint| &checkpoint.sha256)
                != Some(&receipt.checkpoint_sha256)
        {
            return false;
        }
        if receipt.repositories.is_empty() {
            return true;
        }
        let Some(bundle) = self.config.bundles.get(&session.bundle_id) else {
            return false;
        };
        receipt.repositories.iter().all(|expected| {
            bundle
                .repositories
                .iter()
                .any(|configured| configured == expected)
        })
    }

    /// Validate a replacement first, then atomically save it and check the
    /// remaining sources so multi-repository bundles can report the next moved
    /// repository without ever provisioning a partial target.
    pub fn replace_resume_repository_origin(
        &mut self,
        session_id: &str,
        repository_id: &str,
        replacement: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let bundle_id = session.bundle_id.clone();
        let checkpoint = session
            .checkpoint
            .as_ref()
            .context("session has no checkpoint")?;
        let replacement = replacement_repository_source(repository_id, replacement)?;
        let repositories = read_checkpoint_repository_bundles(&checkpoint.archive_path)?;
        let verified = ResumeRepositoryBundles {
            checkpoint_sha256: checkpoint.sha256.clone(),
            repositories,
        };
        let archived = verified
            .repositories
            .iter()
            .find(|repository| repository.metadata.id == repository_id)
            .with_context(|| format!("checkpoint does not contain repository {repository_id:?}"))?;
        if let Some(missing_commit) = checkpoint_source_missing_commit(
            &replacement,
            archived,
            executor,
            controller_github_token().as_deref(),
        )? {
            return Ok(ResumeRepositorySourcePreflight::RepositoryMoved(
                ResumeRepositorySourceMismatch {
                    session_id: session_id.to_owned(),
                    bundle_id,
                    repository_id: repository_id.to_owned(),
                    missing_commit,
                    archived_origin: archived.metadata.origin.clone(),
                    configured_origin: replacement.source_label(),
                },
            ));
        }
        let (config, ()) = Config::update(|config| {
            let bundle = config
                .bundles
                .get_mut(&bundle_id)
                .with_context(|| format!("session bundle {bundle_id:?} is missing"))?;
            let repository = bundle
                .repositories
                .iter_mut()
                .find(|repository| repository.id == repository_id)
                .with_context(|| {
                    format!(
                        "session bundle {:?} no longer contains repository {repository_id:?}",
                        bundle_id
                    )
                })?;
            repository.github = replacement.github.clone();
            repository.local = replacement.local.clone();
            Ok(())
        })?;
        self.config = config;
        self.preflight_verified_repository_sources(
            session_id,
            verified,
            Some(repository_id),
            false,
            executor,
        )
    }
}

/// Plan a local checkout's conversion into an isolated workspace and describe
/// it, without changing anything.
///
/// The resume preflight and the browser's resume card both need this answer
/// before a person confirms, and neither owns a [`Controller`] at that point,
/// so it takes the record and the configuration directly.
pub fn raw_conversion_preview_for(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
) -> Result<mj_core::state::RawConversionPreview> {
    let conversion = plan_raw_to_workspace(session, config, executor)?;
    raw_conversion_preview(session, &conversion, executor)
}

fn replacement_repository_source(id: &str, replacement: &str) -> Result<ProjectRepository> {
    let replacement = replacement.trim();
    ensure!(!replacement.is_empty(), "enter the repository's new origin");
    let expanded = mj_core::path_input::expand_local(Path::new(replacement))?;
    let path = expanded.as_path();
    let (github, local) = if path.is_absolute() {
        ensure!(
            path.is_dir(),
            "local repository {replacement:?} is not a directory"
        );
        (None, Some(mj_core::local_git::canonical_repository(path)?))
    } else {
        let github = crate::setup::github_repository_from_origin(replacement)
            .context("origin must be a GitHub repository or an absolute local repository path")?;
        (
            Some(format!("{}/{}", github.owner, github.repository)),
            None,
        )
    };
    Ok(ProjectRepository {
        id: id.to_owned(),
        github,
        local,
        destination: PathBuf::from(id),
        git_ref: None,
    })
}

fn checkpoint_source_missing_commit(
    configured: &ProjectRepository,
    archived: &CheckpointRepositoryBundle,
    executor: &impl CommandExecutor,
    github_token: Option<&str>,
) -> Result<Option<String>> {
    let staging = tempfile::tempdir().context("create repository source preflight")?;
    let repository = staging.path().join("repository.git");
    checked_preflight_git(
        executor,
        CommandSpec::new(
            "git",
            [
                "init".to_owned(),
                "--bare".to_owned(),
                "--quiet".to_owned(),
                repository.to_string_lossy().into_owned(),
            ],
        )
        .purpose("initialize repository source preflight"),
    )?;
    let missing = checkpoint_bundle_prerequisites(archived)?;
    if missing.is_empty() {
        let bundle = staging.path().join("checkpoint.bundle");
        std::fs::write(&bundle, &archived.committed_bundle)
            .context("write self-contained checkpoint bundle for source preflight")?;
        checked_preflight_git(
            executor,
            checkpoint_bundle_import_command(&repository, &bundle),
        )?;
        return Ok(None);
    }
    // The restore clone will obtain the reachable ancestry. This probe only
    // needs to establish that the source still serves each boundary object, so
    // stop at that object instead of downloading and walking its whole graph.
    for commit in missing {
        let output = fetch_source_commit(executor, &repository, configured, &commit, github_token)?;
        if output.status != 0 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if source_does_not_have_commit(&stderr) {
                return Ok(Some(commit));
            }
            bail!(
                "could not check configured source {:?}: {}",
                configured.source_label(),
                stderr.trim()
            );
        }
    }
    // Do not re-index the bundle against this deliberately shallow probe: the
    // shallow marker would make Git report artificial connectivity failures.
    // The real restore applies it to the full source clone.
    Ok(None)
}

fn checkpoint_bundle_import_command(repository: &Path, bundle: &Path) -> CommandSpec {
    let mut command = CommandSpec::new(
        "git",
        [
            "-C".to_owned(),
            repository.to_string_lossy().into_owned(),
            "fetch".to_owned(),
            "--no-tags".to_owned(),
            bundle.to_string_lossy().into_owned(),
            "HEAD".to_owned(),
        ],
    )
    .purpose("validate self-contained checkpoint bundle");
    command
        .env
        .insert("GIT_NO_LAZY_FETCH".to_owned(), "1".to_owned());
    command
        .env
        .insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    command
}

fn fetch_source_commit(
    executor: &impl CommandExecutor,
    repository: &Path,
    configured: &ProjectRepository,
    commit: &str,
    github_token: Option<&str>,
) -> Result<CommandOutput> {
    let mut arguments = Vec::new();
    let mut token_auth = false;
    let mut ssh_transport = false;
    let source = if let Some(local) = &configured.local {
        local.to_string_lossy().into_owned()
    } else {
        let source = configured
            .github
            .as_deref()
            .context("repository source is missing")?;
        let github = crate::setup::github_repository_from_origin(source)
            .context("configured repository is not a GitHub source")?;
        if github_token.is_some() {
            token_auth = true;
            arguments.extend([
                "-c".to_owned(),
                "credential.helper=".to_owned(),
                "-c".to_owned(),
                "credential.helper=!f() { if [ \"$1\" = get ]; then echo username=x-access-token; echo \"password=$GH_TOKEN\"; fi; }; f".to_owned(),
            ]);
            format!(
                "https://github.com/{}/{}.git",
                github.owner, github.repository
            )
        } else {
            ssh_transport = true;
            format!("git@github.com:{}/{}.git", github.owner, github.repository)
        }
    };
    arguments.extend([
        "-C".to_owned(),
        repository.to_string_lossy().into_owned(),
        "fetch".to_owned(),
        "--no-tags".to_owned(),
        "--depth=1".to_owned(),
        "--filter=blob:none".to_owned(),
        source,
        commit.to_owned(),
    ]);
    let mut command = CommandSpec::new("git", arguments).purpose("check checkpoint base commit");
    command
        .env
        .insert("GIT_NO_LAZY_FETCH".to_owned(), "1".to_owned());
    command
        .env
        .insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    if token_auth {
        let token = github_token.expect("token authentication requires a GitHub token");
        command.env.insert("GH_TOKEN".to_owned(), token.to_owned());
    }
    if ssh_transport {
        command.env.insert(
            "GIT_SSH_COMMAND".to_owned(),
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"
                .to_owned(),
        );
    }
    executor.execute(&command)
}

fn source_does_not_have_commit(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    [
        "not our ref",
        "couldn't find remote ref",
        "not a valid object name",
        "no such ref was fetched",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
}

fn checked_preflight_git(
    executor: &impl CommandExecutor,
    command: CommandSpec,
) -> Result<CommandOutput> {
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "{}: {}",
        command.purpose,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

impl Controller {
    /// Resume a stopped logical session on any configured profile and
    /// target. Cross-harness resume restores Git and canonical history, starts
    /// a fresh native session, and supplies the prior transcript as its first
    /// context turn.
    pub async fn resume_session_with_options(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_id: &str,
        additional_mounts: Option<Vec<AdditionalMount>>,
        resource_allocation: Option<SessionResourceAllocation>,
    ) -> Result<MaterializedSession> {
        self.resume_session_with_options_and_queue_disposition(
            session_id,
            profile_id,
            target_id,
            additional_mounts,
            resource_allocation,
            false,
        )
        .await
    }

    pub async fn resume_session_with_options_and_queue_disposition(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_id: &str,
        additional_mounts: Option<Vec<AdditionalMount>>,
        resource_allocation: Option<SessionResourceAllocation>,
        discard_queue: bool,
    ) -> Result<MaterializedSession> {
        self.resume_session_controlled(
            session_id,
            profile_id,
            target_id,
            SessionResumeOptions {
                additional_mounts,
                resource_allocation,
                discard_queue,
            },
            &ProcessExecutor,
        )
        .await
    }

    pub async fn resume_session_controlled(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_id: &str,
        options: SessionResumeOptions,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        self.resume_session_controlled_with_repository_preflight(
            session_id, profile_id, target_id, options, None, executor,
        )
        .await
    }

    pub async fn resume_session_controlled_with_repository_preflight(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_id: &str,
        options: SessionResumeOptions,
        repository_preflight: Option<ResumeRepositorySourceReceipt>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        let SessionResumeOptions {
            additional_mounts,
            resource_allocation,
            discard_queue,
        } = options;
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if !matches!(
            previous.state,
            SessionState::Stopped | SessionState::Lost | SessionState::Error
        ) {
            bail!("session {session_id} is not stopped, lost, or retryable");
        }
        let checkpoint = previous
            .checkpoint
            .as_ref()
            .context("session has no checkpoint")?;
        // A receipt for an isolated destination says nothing about a host
        // checkout. Explicit moves to raw execution must check that source.
        let moving_to_raw = previous.project_directory.is_none()
            && self
                .config
                .targets
                .get(target_id)
                .is_some_and(mj_core::config::is_bare_project_target);
        if moving_to_raw
            || !repository_preflight.as_ref().is_some_and(|receipt| {
                self.repository_source_receipt_is_current(session_id, receipt)
            })
        {
            let _phase = ResumePhaseTimer::new(session_id, "preflight repository sources");
            if let ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) =
                self.preflight_repository_sources(session_id, target_id, false, executor)?
            {
                bail!(
                    "checkpoint base commit {} is missing from configured source {:?} for repository {:?}; the repository may have moved (archived origin: {:?})",
                    mismatch.missing_commit,
                    mismatch.configured_origin,
                    mismatch.repository_id,
                    mismatch.archived_origin,
                );
            }
        }
        // Canonicalize before verification and keep this exact absolute path
        // for the restore. A LocalBare worker shares the controller's
        // filesystem, so it can consume the verified archive directly instead
        // of copying a second large file into its worker root.
        let archive_path = {
            let _phase = ResumePhaseTimer::new(session_id, "verify checkpoint archive");
            checkpoint.archive_path.canonicalize().with_context(|| {
                format!(
                    "resolve checkpoint archive {}",
                    checkpoint.archive_path.display()
                )
            })?
        };
        ensure!(
            archive_path.is_absolute() && archive_path.is_file(),
            "checkpoint archive path is not an absolute regular file: {}",
            archive_path.display()
        );
        // Take the snapshot out of the verified metadata and share it behind an
        // `Arc`: on a long session it is tens of megabytes, and resume reads it
        // from three places that used to hold private copies.
        let mj_checkpoint::archive::VerifiedArchiveMetadata {
            manifest: archive_manifest,
            canonical_session,
            archive_sha256,
        } = {
            let _phase = ResumePhaseTimer::new(session_id, "verify checkpoint archive contents");
            verify_archive_streaming(&archive_path)?
        };
        if archive_sha256 != checkpoint.sha256 || archive_manifest.session.id != session_id {
            bail!("persisted checkpoint verification failed");
        }
        ensure!(
            archive_manifest.repositories.iter().all(|repository| {
                !repository.metadata.origin.starts_with("mj-local:")
                    && !repository.metadata.origin.starts_with("ext::")
            }),
            "resuming legacy host-bridge sessions is not supported; start a new network-backed session"
        );
        let canonical_session = Arc::new(canonical_session);
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .with_context(|| format!("unknown profile {profile_id:?}"))?
            .clone();
        ensure!(profile.enabled, "profile {profile_id:?} is disabled");
        let target_template = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?
            .clone();
        // Decide the representation before the record changes, so an
        // incompatible target fails here instead of during provisioning.
        self.validate_muse_resume_destination(&previous, profile.kind, target_id)?;
        ensure!(
            profile.kind != HarnessKind::Muse || previous.additional_mounts.is_empty(),
            "Muse Code ACP supports one workspace root; attached directories are unsupported"
        );
        let plan = resume_compatibility(&previous, &self.config, target_id)
            .map_err(|reason| anyhow::anyhow!("{reason}"))?;
        // A converting resume writes its own archive below, from the host
        // checkout's own network remote, and provisioning reads that one. Every
        // other isolated resume clones what its stored archive already names.
        if !mj_core::config::is_bare_project_target(&target_template)
            && plan != ResumePlan::RawToWorkspace
        {
            super::network_git::bundle_from_manifest(&archive_manifest)?;
        }
        if plan == ResumePlan::InPlace
            && previous.managed_worktree.is_none()
            && let Some(project_directory) = &previous.project_directory
        {
            self.validate_project_directory(target_id, project_directory, executor)
                .context("raw project is unavailable for resume")?;
        }
        let conversion = match plan {
            ResumePlan::InPlace => None,
            ResumePlan::RawToWorkspace => Some(ResumeConversion::RawToWorkspace(
                plan_raw_to_workspace(&previous, &self.config, executor)
                    .context("prepare the raw checkout for its new target")?,
            )),
            ResumePlan::WorkspaceToRaw => Some(ResumeConversion::WorkspaceToRaw(
                self.plan_workspace_to_raw(&previous, target_id, executor)
                    .context("prepare a checkout for this session")?,
            )),
        };
        let resource_allocation =
            resource_allocation.or_else(|| previous.resource_allocation.clone());
        let additional_mounts =
            additional_mounts.unwrap_or_else(|| previous.additional_mounts.clone());
        validate_resource_allocation(&target_template, resource_allocation.as_ref())?;
        let selected_container_size =
            selected_host_container_size(&target_template, resource_allocation.as_ref());
        if !additional_mounts.is_empty() && mount_history_host(&target_template).is_none() {
            bail!("attached resources are unsupported for this target");
        }
        targets::validate_additional_mounts(&additional_mounts)?;
        let history_host = mount_history_host(&target_template);
        let history_mounts = additional_mounts.clone();
        if previous.state == SessionState::Error
            && let Some(locator) = &previous.target
        {
            let backend = backend_locator(locator, &previous, &self.config)?;
            targets::close_plan(&backend, session_id)?
                .execute(executor)
                .context("clean up target from failed resume")?;
        }
        let mut resume_notices = Vec::new();
        // The raw-to-workspace notice is written where the conversion snapshot
        // is taken, because it reports the branch the container arrives on.
        if let Some(conversion) = conversion
            .as_ref()
            .and_then(ResumeConversion::workspace_to_raw)
        {
            resume_notices.push(format!(
                "This session moved out of its {} target and into {}. Its branch {} is now {}.",
                previous.target_template_id,
                conversion.worktree.worktree_root.display(),
                archive_manifest
                    .repositories
                    .first()
                    .and_then(|repository| repository.metadata.branch.as_deref())
                    .unwrap_or("a detached head"),
                conversion.worktree.branch,
            ));
        }
        let managed_checkout_present = previous
            .managed_worktree
            .as_ref()
            .map(|worktree| managed_worktree_checkout_exists(executor, worktree))
            .transpose()?
            .unwrap_or(true);
        // A checkout Mjolnir did not retire remains the truth for a raw session.
        // A retired checkout is recreated from the branch and archive below.
        if managed_checkout_present && let Some(project_directory) = &previous.project_directory {
            match raw_checkout_position(&previous, &self.config, project_directory, executor) {
                Ok(live) => resume_notices.extend(raw_checkout_divergence_notice(
                    project_directory,
                    archive_manifest
                        .repositories
                        .first()
                        .map(|repository| &repository.metadata),
                    &live,
                )),
                // Informational only: a resume must not fail because Mjolnir could
                // not read where the checkout stands.
                Err(error) => tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not read the raw checkout position for a resume notice"
                ),
            }
        }
        // Resolving the worker binary is local and costs microseconds, while
        // the compaction below costs minutes and paid model requests. A resume
        // that could never install a worker fails here rather than after all
        // that work has been thrown away.
        super::worker_binary::preflight_worker_binary(&target_template)?;
        let same_harness = profile.kind == archive_manifest.session.harness_kind;
        let native_continuity =
            native_continuity_preserved(profile.kind, archive_manifest.session.harness_kind);
        let context_bytes = crate::handoff::profile_handoff_bytes(Some(&profile));
        // Cross-harness compaction is started alongside destination
        // provisioning below. Clone only the configuration it reads so the
        // controller can continue owning and mutating its session record.
        let utility_config = (!native_continuity).then(|| self.config.clone());
        let discard_queued_prompts = discard_queue || !same_harness;
        // When this controller archived the session, its durable projection is
        // already the archive's content. Reading one row decides that; a read
        // failure or any mismatch rebuilds as before.
        let stored_frontier = crate::database::materialized_event_frontier(session_id)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "could not read the stored projection frontier; rebuilding it from the archive"
                );
                None
            });
        let rebuild_projection = projection_rebuild_required(
            stored_frontier
                .as_ref()
                .map(|(ordinal, digest)| (*ordinal, digest.as_str())),
            canonical_session.event_frontier,
            &canonical_session.event_frontier_digest,
        );
        // Rebuilding the projection is a pure function of the archive and costs
        // seconds on a long session. Start it now so it runs while the target is
        // being provisioned; its result is awaited where it was consumed
        // before, and the writes it feeds have not moved.
        //
        // A resume that fails before the result is needed drops the handle.
        // `spawn_blocking` work cannot be cancelled, so the computation still
        // finishes on the blocking pool and its result is discarded; it owns
        // nothing but its own inputs, so nothing leaks beyond that CPU.
        let projection_build = rebuild_projection.then(|| {
            let canonical = Arc::clone(&canonical_session);
            let session_id = session_id.to_owned();
            tokio::task::spawn_blocking(move || {
                materialized_session_from_canonical(session_id, &canonical)
            })
        });
        let github_token = controller_github_token();

        // The configuration gains the bundle before the record points at it, so
        // no persisted session ever names a bundle that is not there.
        if let Some(conversion) = conversion
            .as_ref()
            .and_then(ResumeConversion::raw_to_workspace)
            && let Some(bundle) = &conversion.new_bundle
        {
            let (config, ()) = Config::update(|config| {
                if let Some(existing) = config.bundles.get(&conversion.bundle_id) {
                    ensure!(
                        existing == bundle,
                        "bundle {:?} was configured concurrently with a different definition; retry the resume",
                        conversion.bundle_id
                    );
                } else {
                    config
                        .bundles
                        .insert(conversion.bundle_id.clone(), bundle.clone());
                }
                Ok(())
            })
            .context("save the bundle for a converted raw session")?;
            self.config = config;
        }

        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.harness_kind = profile.kind;
        record.last_profile = profile_id.to_string();
        record.target_template_id = target_id.to_string();
        record.resource_allocation = resource_allocation;
        record.additional_mounts = additional_mounts;
        record.target = None;
        record.native_session_id =
            native_continuity.then(|| archive_manifest.session.native_session_id.clone());
        record.state = SessionState::Provisioning;
        record.updated_at = now();
        record.last_error = None;
        match &conversion {
            Some(ResumeConversion::RawToWorkspace(conversion)) => {
                apply_raw_to_workspace(record, conversion);
            }
            Some(ResumeConversion::WorkspaceToRaw(conversion)) => {
                apply_workspace_to_raw(record, conversion);
            }
            None => {}
        }
        let resumed_project_directory = record.project_directory.clone();
        if let Some(host) = history_host {
            self.state.remember_mount_sources(host, &history_mounts);
            crate::database::remember_mount_sources(host, &history_mounts)?;
        }
        // The session's prompt history is filed under its bundle, so a
        // conversion moves the history with it before the record is persisted.
        if let Some(conversion) = conversion
            .as_ref()
            .and_then(ResumeConversion::raw_to_workspace)
        {
            crate::database::rebind_session_bundle(session_id, &conversion.bundle_id)?;
        }
        // Resume rewrites the record it resumes, including the attached
        // directories and the harness session id, so it writes the whole row.
        if let Some((host, size)) = selected_container_size.as_ref() {
            crate::database::save_session_with_container_size(
                &self.state.sessions[session_id],
                host,
                *size,
            )?;
        } else {
            crate::database::save_session(&self.state.sessions[session_id])?;
        }
        if let Some((host, size)) = selected_container_size.as_ref() {
            self.state.remember_container_size(host, *size);
        }

        let mut recreated_managed_worktree = false;
        // Set once a conversion has written its archive, so the success path can
        // retire the archive it replaced and the failure path can remove it.
        let mut conversion_checkpoint_written: Option<mj_core::state::CheckpointMetadata> = None;
        let result = async {
            if let Some(worktree) = previous.managed_worktree.as_ref() {
                recreated_managed_worktree = restore_managed_worktree(executor, worktree)?;
                if recreated_managed_worktree && plan == ResumePlan::RawToWorkspace {
                    mj_checkpoint::checkpoint::restore_single_repository_onto_branch(
                        &archive_path,
                        &worktree.worktree_root,
                        &worktree.branch,
                        &SystemGit,
                    )
                    .context("restore the retired checkout before moving it into a target")?;
                }
            }
            // The record already names the worktree, so a failure here rolls
            // back through the same path that cleans up a new session's.
            if let Some(conversion) = conversion
                .as_ref()
                .and_then(ResumeConversion::workspace_to_raw)
            {
                if conversion.reuse_existing_branch {
                    let recovery_ref =
                        preserve_retained_managed_worktree_branch(executor, &conversion.worktree)?;
                    restore_managed_worktree(executor, &conversion.worktree)?;
                    resume_notices.push(format!(
                        "Before restoring this session's retained branch, Mjolnir preserved its tip at {recovery_ref}."
                    ));
                } else {
                    create_managed_worktree(
                        executor,
                        &conversion.worktree,
                        None,
                        PrimaryCheckoutRequirement::Any,
                    )?;
                }
                mj_checkpoint::checkpoint::restore_single_repository_onto_branch(
                    &archive_path,
                    &conversion.worktree.worktree_root,
                    &conversion.worktree.branch,
                    &SystemGit,
                )
                .context("restore this session's checkout")?;
            }
            // A local checkout becomes an isolated workspace by being
            // re-snapshotted into a new archive whose provenance is the
            // checkout's own network remote. Provisioning clones that remote,
            // and the restore below lays this snapshot over the fresh clone.
            if let Some(conversion) = conversion
                .as_ref()
                .and_then(ResumeConversion::raw_to_workspace)
            {
                let destination = PathBuf::from(
                    previous
                        .project_directory
                        .as_deref()
                        .context("a raw session has no project directory")?
                        .file_name()
                        .context("a raw project directory cannot be the filesystem root")?,
                );
                let snapshot = raw_checkout_snapshot(
                    &conversion.checkout,
                    &conversion.source,
                    &destination,
                    &SystemGit,
                )
                .context("snapshot the host checkout for its new target")?;
                resume_notices.push(conversion_notice(
                    target_id,
                    previous
                        .project_directory
                        .as_deref()
                        .unwrap_or(&conversion.checkout),
                    snapshot.metadata.branch.as_deref(),
                    conversion.retire.as_ref(),
                ));
                let archives = mj_core::config::sessions_dir();
                std::fs::create_dir_all(&archives).with_context(|| {
                    format!("create the checkpoint directory {}", archives.display())
                })?;
                // Named like every other managed archive, so an interrupted
                // conversion's file is reconciled away, with `converted`
                // marking where it came from.
                let output = archives.join(format!(
                    "{session_id}-converted-{}-{}.hel.zip",
                    previous
                        .checkpoint
                        .as_ref()
                        .map_or(0, |checkpoint| checkpoint.event_frontier),
                    new_command_id("archive")?
                ));
                let written = conversion_checkpoint(&archive_path, snapshot, &output)?;
                conversion_checkpoint_written = Some(written.clone());
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.checkpoint = Some(written);
                record.updated_at = now();
                // The whole row again: provisioning must read the converted
                // archive even if this process dies right here.
                if let Some((host, size)) = selected_container_size.as_ref() {
                    crate::database::save_session_with_container_size(
                        &self.state.sessions[session_id],
                        host,
                        *size,
                    )?;
                } else {
                    crate::database::save_session(&self.state.sessions[session_id])?;
                }
            }
            let utility_handoff = {
                let _provisioning = ResumePhaseTimer::new(session_id, "provision destination");
                if let Some(config) = utility_config.as_ref() {
                    Some(
                        provision_with_cross_harness_handoff(
                            self,
                            session_id,
                            executor,
                            github_token.as_deref(),
                            config,
                            &canonical_session,
                            context_bytes,
                        )
                        .context("prepare the cross-harness destination")?,
                    )
                } else {
                    self.provision_session_with_failure_disposition(
                        session_id,
                        executor,
                        github_token.as_deref(),
                        ProvisioningFailureDisposition::Preserve,
                    )
                    .await?;
                    None
                }
            };
            let (backend, worker_root) = self.worker_placement(session_id)?;
            let harness_home = target_profile_home(&backend, session_id, &profile);
            let workspace_root = if let Some(project_directory) = &resumed_project_directory {
                project_directory
                    .parent()
                    .context("bare project directory has no parent")?
                    .to_string_lossy()
                    .into_owned()
            } else {
                match &backend {
                    targets::TargetLocator::LocalPodman { .. }
                    | targets::TargetLocator::LocalDocker { .. }
                    | targets::TargetLocator::AppleContainer { .. }
                    | targets::TargetLocator::SshPodman { .. }
                    | targets::TargetLocator::SshDocker { .. } => "/workspace".to_string(),
                    targets::TargetLocator::AwsEc2 { workspace, .. }
                    | targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
                    targets::TargetLocator::LocalBare { worker_root } => worker_root.clone(),
                }
            };
            let target_path = |path: &str| match &backend {
                targets::TargetLocator::AwsEc2 { .. }
                | targets::TargetLocator::SshBare { .. }
                    if !path.starts_with('/') =>
                {
                    PathBuf::from(format!("~/{path}"))
                }
                _ => PathBuf::from(path),
            };
            let remote_archive = format!("{worker_root}/restore.hel.zip");
            let remote_spec = format!("{worker_root}/restore-spec.json");
            // A conversion restores the archive it just wrote, not the raw one
            // the session was stopped with.
            let restored_archive = conversion_checkpoint_written
                .as_ref()
                .map_or(archive_path.as_path(), |checkpoint| {
                    checkpoint.archive_path.as_path()
                });
            let restore = CheckpointRestoreSpec {
                archive_path: restore_archive_path(
                    &backend,
                    restored_archive,
                    &target_path(&remote_archive),
                ),
                workspace_root: target_path(&workspace_root),
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                // A local checkout converting into a workspace arrives as a
                // fresh clone of its own remote, and the conversion archive
                // carries the commits, dirty files, and branch that go over it.
                // An in-place managed checkout recreated from its retained
                // branch still needs the archive's dirty state.
                restore_repositories: (resumed_project_directory.is_none()
                    && conversion.is_none())
                    || plan == ResumePlan::RawToWorkspace
                    || (recreated_managed_worktree && plan == ResumePlan::InPlace),
                restore_native: native_continuity,
                // A move onto a checkout puts it somewhere the archive could
                // not have named, so the restored harness session is pointed at
                // the real working directory instead of the archived one. A
                // move into a target has no host directory left, and the
                // conversion archive already names the destination under
                // `/workspace`, so this stays empty there.
                primary_repository_root: conversion
                    .is_some()
                    .then(|| resumed_project_directory.clone())
                    .flatten()
                    .map(|directory| target_path(&directory.to_string_lossy())),
                discard_queued_prompts,
            };
            // A bare target keeps the closed session's worker root on the host.
            // Stop anything still writing there and clear the leftover relay
            // state, or the restore's seed loses to a stale snapshot whose
            // frontier no journal can support. This runs before the worker
            // binary is installed: a surviving daemon still holds the old one
            // open, and the install would land on a running executable.
            {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                if let Some(command) = targets::clear_relay_state_plan(&backend, session_id)? {
                    execute_checked(syncing, command)?;
                }
                // Both lanes below write into the worker root, so it exists first.
                execute_checked(
                    syncing,
                    targets::command_on_locator(
                        &backend,
                        session_id,
                        vec!["mkdir".into(), "-p".into(), worker_root.clone()],
                        "create the session worker root",
                    )?,
                )?;
            }
            let staging = tempfile::tempdir().context("create restore staging")?;
            let local_spec = staging.path().join("restore-spec.json");
            std::fs::write(&local_spec, serde_json::to_vec_pretty(&restore)?)?;
            // Two independent lanes into the target. The checkpoint transfer
            // needs nothing from the worker install, and the worker install
            // is independent of archive upload, so both run concurrently.
            let controller = &*self;
            let backend_ref = &backend;
            let worker_root_ref = worker_root.as_str();
            let local_spec_ref = local_spec.as_path();
            execute_concurrent_lanes(
                || {
                    let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                    controller.prepare_worker_files(
                        session_id,
                        backend_ref,
                        worker_root_ref,
                        syncing,
                    )?;
                    super::provisioning::install_inherited_git_settings(
                        syncing,
                        backend_ref,
                        session_id,
                    )?;
                    Ok(())
                },
                || {
                    let restoring = &StagedExecutor::new(executor, ProvisionStage::Restoring);
                    if should_upload_restore_archive(&backend) {
                        upload_checkpoint_spec(
                            restoring,
                            backend_ref,
                            session_id,
                            restored_archive,
                            &remote_archive,
                        )?;
                    }
                    upload_checkpoint_spec(
                        restoring,
                        backend_ref,
                        session_id,
                        local_spec_ref,
                        &remote_spec,
                    )
                },
            )?;
            {
                let restoring = &StagedExecutor::new(executor, ProvisionStage::Restoring);
                execute_checked(
                    restoring,
                    restore_command(&backend, session_id, &remote_spec)?,
                )?;
            }
            {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                install_attached_resources(
                    &self.state,
                    session_id,
                    &backend,
                    &worker_root,
                    syncing,
                )?;

            }
            match projection_build {
                Some(build) => {
                    let mut restored_projection = build
                        .await
                        .context("rebuild the restored projection")?
                        .context("rebuild the restored projection")?;
                    if discard_queued_prompts {
                        restored_projection.queued_prompts.clear();
                    }
                    crate::database::save_materialized_session(&restored_projection)?;
                }
                // The stored projection already is the archived one. Only the
                // queue can still need changing.
                None if discard_queued_prompts => {
                    crate::database::replace_materialized_queued_prompts(session_id, &[])?;
                }
                None => {}
            }
            let readiness_stage = bridge_readiness_stage(&profile);
            let spec = self.reconnect_command(session_id)?;
            let readiness = async {
                let mut relay = {
                    let _starting = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
                    start_worker(executor, &backend, &worker_root)?;
                    connect_started_worker(&spec, session_id, executor, &backend, &worker_root)
                        .await?
                };
                let native_session_id =
                    wait_for_native_session_in_stage(&mut relay, executor, readiness_stage).await?;
                Ok::<_, anyhow::Error>((relay, native_session_id))
            }
            .await;
            let (mut relay, native_session_id) = readiness
                .map_err(|error| worker_probe_diagnosis(executor, &backend, &worker_root, error))?;
            if native_continuity {
                if native_session_id != archive_manifest.session.native_session_id {
                    bail!(
                        "ACP loaded native session {native_session_id}, expected {}",
                        archive_manifest.session.native_session_id
                    );
                }
            } else {
                relay
                    .install_prompt_context(
                        utility_handoff
                            .clone()
                            .context("a resume into a fresh native session has no handoff")?,
                    )
                    .await?;
                if !discard_queue {
                    for prompt in &canonical_session.queued_prompts {
                        // A queued configuration change is replayed as itself;
                        // rebuilding it as a prompt would send `/model x` to
                        // the agent as text.
                        let command = match &prompt.kind {
                            CanonicalQueuedCommandKind::Prompt => RelayCommand::Prompt {
                                prompt: prompt
                                    .content
                                    .iter()
                                    .cloned()
                                    .map(serde_json::from_value)
                                    .collect::<serde_json::Result<Vec<ContentBlock>>>()?,
                            },
                            CanonicalQueuedCommandKind::SetConfig { key, value } => {
                                RelayCommand::SetConfig {
                                    key: key.clone(),
                                    value: value.clone(),
                                }
                            }
                        };
                        relay.submit(prompt.command_id.clone(), command).await?;
                    }
                }
            }
            // Last, and only once the resume has otherwise succeeded: a failure
            // before this point rolls the record back to a session whose
            // worktree still has to be there.
            if let Some(worktree) = conversion
                .as_ref()
                .and_then(ResumeConversion::raw_to_workspace)
                .and_then(|plan| plan.retire.as_ref())
                && let Err(error) = retire_managed_worktree(executor, worktree)
            {
                tracing::warn!(
                    session_id,
                    worktree = %worktree.worktree_root.display(),
                    error = format!("{error:#}"),
                    "could not retire the old managed worktree after resume"
                );
                resume_notices.push(worktree_cleanup_notice(&worktree.worktree_root, &error));
            }
            for notice in &resume_notices {
                let submitted = async {
                    let command_id = new_command_id("resume-notice")?;
                    relay
                        .submit(
                            command_id,
                            RelayCommand::RecordNotice {
                                text: notice.clone(),
                            },
                        )
                        .await
                }
                .await;
                // The conversation line is a courtesy. A relay that refuses it
                // has not damaged the resume, so report and carry on.
                if let Err(error) = submitted {
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "could not record a resume notice in the conversation"
                    );
                }
            }
            self.mark_worker_connected(session_id, Some(native_session_id))?;
            Ok::<_, anyhow::Error>(relay.sync().await?.materialized)
        }
        .await;
        match result {
            Ok(materialized) => {
                // The session now runs from the conversion archive, so the raw
                // one it replaced can go.
                if let Some(written) = &conversion_checkpoint_written {
                    super::checkpoint::prune_replaced_checkpoint(
                        previous.checkpoint.as_ref(),
                        written,
                    );
                }
                Ok(materialized)
            }
            Err(error) => {
                // The rollback puts back a record that names the previous
                // archive, so the conversion's archive is nothing but litter.
                if let Some(written) = &conversion_checkpoint_written
                    && let Err(remove_error) = std::fs::remove_file(&written.archive_path)
                    && remove_error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        session_id,
                        path = %written.archive_path.display(),
                        "could not remove the conversion checkpoint after resume failed: {remove_error}"
                    );
                }
                // Put back whatever this resume could have written to the
                // durable projection. Both branches restore archived content,
                // so they are correct whether or not the write had happened
                // when the resume failed.
                if rebuild_projection {
                    match materialized_session_from_canonical(session_id, &canonical_session) {
                        Ok(previous_projection) => {
                            if let Err(restore_error) =
                                crate::database::save_materialized_session(&previous_projection)
                            {
                                tracing::error!(
                                    session_id,
                                    error = format!("{restore_error:#}"),
                                    "could not restore the durable projection after resume failed"
                                );
                            }
                        }
                        Err(restore_error) => {
                            tracing::error!(
                                session_id,
                                error = format!("{restore_error:#}"),
                                "could not rebuild the durable projection after resume failed"
                            );
                        }
                    }
                } else if discard_queued_prompts
                    && let Err(restore_error) = crate::database::replace_materialized_queued_prompts(
                        session_id,
                        &mj_transcript::projection::materialized_queued_prompts_from_canonical(
                            &canonical_session.queued_prompts,
                        ),
                    )
                {
                    tracing::error!(
                        session_id,
                        error = format!("{restore_error:#}"),
                        "could not restore queued prompts after resume failed"
                    );
                }
                Err(self.rollback_failed_resume(
                    session_id,
                    &previous,
                    recreated_managed_worktree,
                    error,
                    executor,
                )?)
            }
        }
    }

    pub(super) fn rollback_failed_resume(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        recreated_managed_worktree: bool,
        error: anyhow::Error,
        _executor: &impl CommandExecutor,
    ) -> Result<anyhow::Error> {
        let current = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let cleanup = match current.target.as_ref() {
            Some(locator) => (|| -> Result<()> {
                let backend = backend_locator(locator, &current, &self.config)?;
                targets::close_plan(&backend, session_id)?
                    // Use a fresh executor: cancellation applies to the
                    // requested operation, not to its compensating cleanup.
                    .execute(&CancellableProcessExecutor::with_timeout(
                        Duration::from_secs(15),
                    ))
                    .map(|_| ())
            })(),
            None => Ok(()),
        };
        // A failed target teardown may leave its harness writing. Keep its
        // checkout intact until a later retry proves the process is stopped.
        let worktree_cleanup = if cleanup.is_err() {
            Ok(())
        } else {
            match (
                current.managed_worktree.as_ref(),
                previous.managed_worktree.as_ref(),
            ) {
                (_, Some(previous)) if recreated_managed_worktree => retire_managed_worktree(
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                    previous,
                ),
                (Some(current), Some(previous)) if current == previous => Ok(()),
                (Some(worktree), _) => cleanup_managed_worktree(
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                    worktree,
                ),
                (None, _) => Ok(()),
            }
        };
        let cleanup_error = [cleanup, worktree_cleanup]
            .into_iter()
            .filter_map(Result::err)
            .map(|cleanup_error| format!("{cleanup_error:#}"))
            .collect::<Vec<_>>()
            .join("; ");
        if !cleanup_error.is_empty() {
            tracing::warn!(
                session_id,
                error = %cleanup_error,
                "resume rollback cleanup reported failures"
            );
        }
        let original = format!("{error:#}");
        let record = self.state.sessions.get_mut(session_id).unwrap();
        let failure = apply_failed_resume_rollback(
            record,
            previous,
            &original,
            (!cleanup_error.is_empty()).then_some(cleanup_error),
        );
        // A conversion filed the session's prompt history under its new bundle.
        // The record went back, so the history goes back with it.
        if record.bundle_id != current.bundle_id {
            let bundle_id = record.bundle_id.clone();
            crate::database::rebind_session_bundle(session_id, &bundle_id)?;
        }
        // The rollback restores the record the resume replaced, attached
        // directories included, so it writes the whole row back.
        crate::database::save_session(&self.state.sessions[session_id])?;
        Ok(failure)
    }
}

fn worktree_cleanup_notice(worktree_root: &Path, error: &anyhow::Error) -> String {
    format!(
        "Mjolnir could not remove the worktree at {}: {error:#}. Remove it with `git worktree remove --force {}`.",
        worktree_root.display(),
        worktree_root.display()
    )
}

pub(super) fn apply_failed_resume_rollback(
    current: &mut SessionRecord,
    previous: &SessionRecord,
    original_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    match cleanup_error {
        None => {
            *current = previous.clone();
            current.state = SessionState::Stopped;
            current.target = None;
            current.updated_at = now();
            current.last_error = Some(format!("resume failed: {original_error}"));
            anyhow::anyhow!(original_error.to_owned())
        }
        Some(cleanup_error) => {
            let failure = format!(
                "{original_error}; cleanup of the partial resume target failed: {cleanup_error}"
            );
            // Keep the exact partial target and checkout ownership until
            // cleanup succeeds; the harness may still be writing there.
            // A container conversion has no new managed host checkout, so
            // retain the original host checkout that it has not retired yet.
            if current.managed_worktree.is_none() {
                current
                    .project_directory
                    .clone_from(&previous.project_directory);
                current
                    .managed_worktree
                    .clone_from(&previous.managed_worktree);
                current.bundle_id.clone_from(&previous.bundle_id);
            }
            current.state = SessionState::Error;
            current.updated_at = now();
            current.last_error = Some(format!("resume failed: {failure}"));
            anyhow::anyhow!(failure)
        }
    }
}

/// What the conversation is told when a local session moves into a target.
fn conversion_notice(
    target_id: &str,
    checkout: &Path,
    branch: Option<&str>,
    retire: Option<&mj_core::state::ManagedWorktree>,
) -> String {
    let branch = branch.unwrap_or("a detached head");
    match retire {
        Some(worktree) => format!(
            "This session moved out of {} and into the {target_id} target, where its checkout is on {branch}. Its branch {} stays in {}.",
            checkout.display(),
            worktree.branch,
            worktree.source_repository.display()
        ),
        None => format!(
            "This session moved out of {} and into the {target_id} target, where its checkout is on {branch}. The checkout on this machine stays where it is.",
            checkout.display()
        ),
    }
}

/// The archive a converting resume provisions and restores from: the previous
/// archive's session, conversation, and native state, with the host checkout's
/// snapshot as its only repository.
///
/// The snapshot carries network provenance, so this archive is what lets the
/// destination clone a real remote and later checkpoint like any other
/// isolated session.
fn conversion_checkpoint(
    previous_archive: &Path,
    snapshot: mj_checkpoint::archive::RepositorySnapshot,
    output: &Path,
) -> Result<mj_core::state::CheckpointMetadata> {
    let previous = mj_checkpoint::archive::read_archive_verified(previous_archive)
        .with_context(|| format!("read checkpoint archive {}", previous_archive.display()))?;
    // Native harness state and relay attachments travel byte for byte: a
    // conversion replaces repository content and nothing else.
    let native_artifacts = previous
        .manifest
        .payloads
        .iter()
        .filter_map(|descriptor| match &descriptor.role {
            mj_checkpoint::archive::PayloadRole::NativeArtifact { relative_path } => {
                Some((relative_path, descriptor))
            }
            _ => None,
        })
        .map(|(relative_path, descriptor)| {
            Ok(mj_checkpoint::archive::NativeArtifact {
                relative_path: relative_path.clone(),
                data: previous.payload(descriptor)?.to_vec(),
                mode: descriptor.mode,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let canonical_session = previous.canonical_session()?;
    let event_frontier = canonical_session.event_frontier;
    let written = mj_checkpoint::archive::write_archive_atomic(
        output,
        &mj_checkpoint::archive::ArchiveInput {
            session: previous.manifest.session.clone(),
            // Provenance for a person reading the archive; a restore reads
            // nothing from it, so the target it was captured on stands.
            target: previous.manifest.target.clone(),
            bundle: mj_checkpoint::archive::BundleManifest {
                id: previous.manifest.bundle.id.clone(),
                // A restore finds the primary repository by this id, and with
                // it the working directory the native transcript is rewritten
                // to, so it has to name the snapshot.
                primary_repository: snapshot.metadata.id.clone(),
            },
            canonical_session,
            native_artifacts,
            repositories: vec![snapshot],
        },
    )
    .with_context(|| format!("write the conversion archive {}", output.display()))?;
    Ok(mj_core::state::CheckpointMetadata {
        archive_path: output.to_path_buf(),
        sha256: written.archive_sha256,
        created_at: now(),
        event_frontier,
    })
}

/// Whether a resume has to rebuild the durable projection from its archive.
///
/// The projection is a deterministic fold of the relay event chain, so a stored
/// projection standing at the archive's frontier *and* carrying the archive's
/// frontier digest already holds the archived content: same chain, same
/// ordinal, same result. Anything else - no stored row, a different ordinal, a
/// different digest, or a frontier that could not be read - rebuilds.
fn projection_rebuild_required(
    stored: Option<(u64, &str)>,
    archive_frontier: u64,
    archive_frontier_digest: &str,
) -> bool {
    stored != Some((archive_frontier, archive_frontier_digest))
}

fn restore_archive_path(
    backend: &targets::TargetLocator,
    verified_archive: &Path,
    remote_archive: &Path,
) -> PathBuf {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        verified_archive.to_path_buf()
    } else {
        remote_archive.to_path_buf()
    }
}

fn should_upload_restore_archive(backend: &targets::TargetLocator) -> bool {
    !matches!(backend, targets::TargetLocator::LocalBare { .. })
}

/// Provisioning currently performs its target plan synchronously inside an
/// async function. Run the network-bound cross-harness handoff on a joined
/// side runtime so it can make progress during that plan without borrowing
/// the mutable controller or leaving work behind on failure.
struct CrossHarnessProvisionExecutor<'a, E: CommandExecutor + ?Sized> {
    inner: &'a E,
    cancellation: CancellationToken,
}

impl<E: CommandExecutor + ?Sized> CommandExecutor for CrossHarnessProvisionExecutor<'_, E> {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if self.cancellation.is_cancelled() {
            bail!("operation cancelled while provisioning destination");
        }
        self.inner.execute(command)
    }

    fn cancellation_requested(&self) -> bool {
        self.cancellation.is_cancelled() || self.inner.cancellation_requested()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        self.inner.stage_started(stage);
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        self.inner.stage_finished(stage);
    }

    fn notify_notice(&self, notice: &str) {
        self.inner.notify_notice(notice);
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        if self.cancellation.is_cancelled() {
            bail!("operation cancelled while provisioning destination");
        }
        self.inner.execute_with_stdin(command, input)
    }
}

fn provision_with_cross_harness_handoff(
    controller: &mut Controller,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    github_token: Option<&str>,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
) -> Result<String> {
    let (_provision, handoff) = execute_joined_cross_harness_work(
        "cross-harness provisioning",
        move |cancellation| {
            let provision_executor = CrossHarnessProvisionExecutor {
                inner: executor,
                cancellation,
            };
            futures::executor::block_on(controller.provision_session_with_failure_disposition(
                session_id,
                &provision_executor,
                github_token,
                ProvisioningFailureDisposition::Preserve,
            ))
        },
        "cross-harness handoff",
        move |cancellation| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("create cross-harness handoff runtime")?;
            runtime.block_on(utility_handoff_while_cancellable(
                session_id,
                config,
                snapshot,
                context_bytes,
                executor,
                cancellation,
            ))
        },
    )?;
    ensure!(
        !executor.cancellation_requested(),
        "operation cancelled while provisioning destination"
    );
    Ok(handoff)
}

/// Run the two independent cross-harness lanes together, cancelling and
/// joining the peer as soon as either lane fails. The lane order is stable so
/// diagnostics do not depend on which worker happened to finish first.
fn execute_joined_cross_harness_work<A: Send, B: Send>(
    first_name: &'static str,
    first: impl FnOnce(CancellationToken) -> Result<A> + Send,
    second_name: &'static str,
    second: impl FnOnce(CancellationToken) -> Result<B> + Send,
) -> Result<(A, B)> {
    let cancellation = CancellationToken::new();
    std::thread::scope(|scope| {
        let first_cancel = cancellation.clone();
        let mut first_handle = Some(scope.spawn(move || first(first_cancel)));
        let second_cancel = cancellation.clone();
        let mut second_handle = Some(scope.spawn(move || second(second_cancel)));
        let mut first_result = None;
        let mut second_result = None;

        while first_result.is_none() || second_result.is_none() {
            if first_result.is_none()
                && first_handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_finished())
            {
                let handle = first_handle.take().expect("first lane handle present");
                first_result = Some(match handle.join() {
                    Ok(result) => result,
                    Err(panic) => {
                        cancellation.cancel();
                        Err(anyhow::anyhow!(
                            "{first_name} thread panicked: {}",
                            targets::command_thread_panic_message(panic.as_ref())
                        ))
                    }
                });
                if first_result.as_ref().is_some_and(Result::is_err) {
                    cancellation.cancel();
                }
            }
            if second_result.is_none()
                && second_handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_finished())
            {
                let handle = second_handle.take().expect("second lane handle present");
                second_result = Some(match handle.join() {
                    Ok(result) => result,
                    Err(panic) => {
                        cancellation.cancel();
                        Err(anyhow::anyhow!(
                            "{second_name} thread panicked: {}",
                            targets::command_thread_panic_message(panic.as_ref())
                        ))
                    }
                });
                if second_result.as_ref().is_some_and(Result::is_err) {
                    cancellation.cancel();
                }
            }
            if first_result.is_none() || second_result.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        match (
            first_result.expect("first lane result received after joined handle"),
            second_result.expect("second lane result received after joined handle"),
        ) {
            (Err(first), Err(second)) => {
                Err(first.context(format!("{second_name} lane also failed: {second:#}")))
            }
            (Err(error), Ok(_)) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(first), Ok(second)) => Ok((first, second)),
        }
    })
}

/// Whether the restored native session can carry the conversation into the
/// resumed session.
///
fn native_continuity_preserved(profile_kind: HarnessKind, archived_kind: HarnessKind) -> bool {
    profile_kind == archived_kind
}

/// Discover a utility model and compact the cross-harness handoff while still
/// watching for cancellation. Discovery and compaction can both make several
/// network requests, so a cancelled resume must not wait them out.
async fn utility_handoff_while_cancellable(
    session_id: &str,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    executor: &impl CommandExecutor,
    cancellation: CancellationToken,
) -> Result<String> {
    let _phase = ResumePhaseTimer::new(session_id, "cross-harness handoff");
    if executor.cancellation_requested() {
        bail!("operation cancelled while compacting the cross-harness handoff");
    }
    let _compacting = ProvisionStageGuard::new(executor, ProvisionStage::Compacting);
    let cancel = cancellation.child_token();
    let operation =
        crate::handoff::build_handoff_context(session_id, config, snapshot, context_bytes, &cancel);
    tokio::pin!(operation);
    loop {
        tokio::select! {
            context = &mut operation => return context,
            _ = cancellation.cancelled() => {
                cancel.cancel();
                bail!("operation cancelled while compacting the cross-harness handoff");
            }
            _ = tokio::time::sleep(super::readiness::CANCELLATION_POLL_INTERVAL) => {
                if executor.cancellation_requested() {
                    cancel.cancel();
                    bail!("operation cancelled while compacting the cross-harness handoff");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Barrier, Mutex};

    use anyhow::Result;

    use crate::controller::test_support::{
        FIXTURE_FETCH_URL, FixtureRemoteExecutor, checkout_with_network_remote,
        checkpoint_test_session, committed_repository, managed_worktree_session,
        network_remote_for, raw_session_on, resume_compatibility_config,
        write_checkpoint_archive_with_native_state, write_checkpoint_gate_archive,
    };
    use crate::controller::{Controller, SessionResumeOptions};
    use mj_checkpoint::archive::{GitCommandRunner, verify_archive_streaming};
    use mj_core::config::{
        Config, ContainerTemplate as ConfigContainer, HarnessProfile, ProjectBundle,
        ProjectRepository, TargetTemplate,
    };
    use mj_core::state::{SessionRecord, SessionState, State, TargetLocator};
    use mj_transcript::projection::materialized_session_from_canonical;

    use crate::targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};

    use super::*;

    /// A person choosing a container for a local session has to see what the
    /// move does before it happens, and a person resuming the same session in
    /// place must not be asked anything.
    #[test]
    fn a_local_checkout_resuming_into_a_container_preflights_its_conversion() {
        let (checkout, _remote_parent, remote) = checkout_with_network_remote();
        std::fs::write(checkout.path().join("untracked.txt"), "u".repeat(2048)).unwrap();
        let mut session = raw_session_on("local-bare", &checkout.path().to_string_lossy());
        session.checkpoint = Some(mj_core::state::CheckpointMetadata {
            archive_path: checkout.path().join("unused.hel.zip"),
            sha256: "a".repeat(64),
            created_at: "2026-09-14T00:00:00Z".into(),
            event_frontier: 3,
        });
        let session_id = session.id.clone();
        let controller = Controller {
            config: resume_compatibility_config(),
            state: State {
                sessions: BTreeMap::from([(session_id.clone(), session)]),
                ..State::default()
            },
        };
        let executor = FixtureRemoteExecutor { remote };

        let converting = controller
            .preflight_resume_repository_sources(&session_id, "podman", &executor)
            .unwrap();
        let ResumeRepositorySourcePreflight::ConvertingRawCheckout { receipt, preview } =
            converting
        else {
            panic!("a container destination converts the checkout, got {converting:?}");
        };
        assert_eq!(receipt.session_id, session_id);
        assert_eq!(preview.fetch_url, FIXTURE_FETCH_URL);
        assert_eq!(preview.untracked_files, 1);
        assert!(preview.host_checkout_retained);

        assert!(
            matches!(
                controller
                    .preflight_resume_repository_sources(&session_id, "local-bare", &executor)
                    .unwrap(),
                ResumeRepositorySourcePreflight::Ready(_)
            ),
            "resuming in place asks nothing"
        );
    }

    const RESUME_ROLLBACK_TEST_CHILD: &str = "MJ_RESUME_ROLLBACK_TEST_CHILD";
    const RETIRED_WORKTREE_RESUME_TEST_CHILD: &str = "MJ_RETIRED_WORKTREE_RESUME_TEST_CHILD";
    const WORKER_PREFLIGHT_TEST_CHILD: &str = "MJ_WORKER_PREFLIGHT_TEST_CHILD";

    #[test]
    fn muse_resume_allows_workspace_relocation_before_provisioning() {
        let mut config = resume_compatibility_config();
        config
            .targets
            .insert("other-container".into(), config.targets["podman"].clone());
        let controller = Controller {
            config,
            state: State::default(),
        };
        let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
        session.harness_kind = HarnessKind::Muse;
        assert!(
            controller
                .validate_muse_resume_destination(&session, HarnessKind::Muse, "podman")
                .is_ok()
        );
        assert!(
            controller
                .validate_muse_resume_destination(&session, HarnessKind::Muse, "other-container")
                .is_ok()
        );
        controller
            .validate_muse_resume_destination(&session, HarnessKind::Muse, "ssh-bare")
            .unwrap();
        assert!(
            controller
                .validate_muse_resume_destination(&session, HarnessKind::Codex, "ssh-bare")
                .is_ok()
        );
        assert_eq!(session.state, SessionState::Running);
    }

    /// Compaction costs minutes and paid model requests; resolving the worker
    /// binary is local and costs microseconds. A cross-harness resume that
    /// cannot produce a worker must say so before it compacts anything.
    #[test]
    fn a_resume_preflights_the_worker_binary_before_compacting() {
        // MJ_WORKER_BINARY, MJ_DATA_DIR, and MJ_CONFIG_DIR are process-global,
        // so run the half that sets them in an exact child test.
        if std::env::var_os(WORKER_PREFLIGHT_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::a_resume_preflights_the_worker_binary_before_compacting",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(WORKER_PREFLIGHT_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path().join("data"))
                .env("MJ_CONFIG_DIR", directory.path().join("config"))
                // Names a worker binary that is not there, which is how a
                // machine without an installed worker fails the same lookup.
                .env("MJ_WORKER_BINARY", directory.path().join("absent-worker"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated worker preflight test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::database::install_isolated_test_writer();

        let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
        let archive_directory = data_directory.join("archives");
        std::fs::create_dir_all(&archive_directory).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);
        let repository = committed_repository();
        let mut session = managed_worktree_session(repository.path(), session_id);
        session.checkpoint = Some(checkpoint);

        let profile_home = data_directory.join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut config = resume_compatibility_config();
        // The archive was written by Codex, so resuming onto Claude is a
        // cross-harness resume and would compact the transcript.
        config.profiles.insert(
            "claude".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Claude,
                home: profile_home,
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        let mut controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        crate::database::save_state(&controller.state).unwrap();

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.resume_session_controlled(
                session_id,
                "claude",
                "local-bare",
                SessionResumeOptions {
                    additional_mounts: None,
                    resource_allocation: None,
                    discard_queue: false,
                },
                &ProcessExecutor,
            ))
            .unwrap_err();

        let detail = format!("{error:#}");
        assert!(
            detail.contains("preflight the worker binary before resuming"),
            "{detail}"
        );
        assert!(detail.contains("absent-worker"), "{detail}");
        assert!(
            !detail.contains("compact the cross-harness handoff transcript"),
            "compaction must not run for a resume that cannot install a worker: {detail}"
        );
        assert_eq!(
            controller.state.sessions[session_id].state,
            SessionState::Stopped
        );
    }

    #[test]
    fn network_resume_ignores_host_history_but_an_explicit_raw_move_checks_it() {
        let directory = tempfile::tempdir().unwrap();
        let repository = committed_repository();
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut session = checkpoint_test_session(session_id);
        session.state = SessionState::Stopped;
        session.checkpoint = Some(
            super::super::test_support::write_network_checkpoint_archive(
                directory.path(),
                session_id,
                0,
            ),
        );
        let mut config = resume_compatibility_config();
        config.bundles.insert(
            "project".into(),
            super::super::test_support::local_bundle(repository.path()),
        );
        let controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        assert!(matches!(
            controller
                .preflight_resume_repository_sources(session_id, "podman", &ProcessExecutor,)
                .unwrap(),
            ResumeRepositorySourcePreflight::Ready(_)
        ));
        let result = controller
            .preflight_resume_repository_sources(session_id, "local-bare", &ProcessExecutor)
            .unwrap();
        let ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) = result else {
            panic!("moving into a host checkout must detect its missing archive base");
        };
        assert_eq!(mismatch.missing_commit, "a".repeat(40));
        assert!(!repository.path().join(".mj/worktrees").exists());
    }

    #[test]
    fn raw_in_place_preflight_does_not_require_its_synthetic_bundle() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("raw in-place preflight ran {}", command.purpose);
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut session = checkpoint_test_session(session_id);
        session.checkpoint = Some(write_checkpoint_gate_archive(
            directory.path(),
            session_id,
            3,
        ));
        session.bundle_id = "remote-project-a66373eef659f856".into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some("/mnt/optane/bifrost-fird".into());
        let controller = Controller {
            config: Config {
                targets: BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]),
                // The raw checkout is still usable even though its synthetic
                // grouping bundle has disappeared from the config.
                bundles: BTreeMap::new(),
                ..Config::default()
            },
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };

        let preflight = controller
            .preflight_resume_repository_sources(session_id, "localhost", &UnusedExecutor)
            .unwrap();
        let ResumeRepositorySourcePreflight::Ready(receipt) = preflight else {
            panic!("raw in-place resume unexpectedly needs a repository replacement");
        };
        assert!(controller.repository_source_receipt_is_current(session_id, &receipt));
    }

    #[test]
    fn repository_preflight_distinguishes_the_original_source_from_a_reused_name() {
        fn git(repository: &Path, arguments: &[&str]) {
            let output = SystemGit
                .run(
                    repository,
                    &mj_checkpoint::archive::GitCommand {
                        arguments: arguments.iter().map(std::ffi::OsString::from).collect(),
                        stdin: Vec::new(),
                        env: Vec::new(),
                    },
                )
                .unwrap();
            assert_eq!(
                output.status,
                0,
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("original");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "main"]);
        git(&origin, &["config", "user.name", "Hel Test"]);
        git(&origin, &["config", "user.email", "hel@example.test"]);
        git(&origin, &["commit", "--allow-empty", "-qm", "base"]);
        let source = directory.path().join("source");
        git(
            directory.path(),
            &["clone", "-q", origin.to_str().unwrap(), "source"],
        );
        git(&source, &["config", "user.name", "Hel Test"]);
        git(&source, &["config", "user.email", "hel@example.test"]);
        git(&source, &["commit", "--allow-empty", "-qm", "session"]);
        let snapshot = mj_checkpoint::archive::collect_git_snapshot(
            &SystemGit,
            &source,
            &mj_checkpoint::archive::GitCollectionSpec {
                id: "project".into(),
                relative_destination: "project".into(),
                history: mj_checkpoint::archive::GitHistoryMode::SessionDelta,
                origin_override: None,
            },
        )
        .unwrap();
        let configured = ProjectRepository {
            id: "project".into(),
            github: None,
            local: Some(origin.clone()),
            destination: "project".into(),
            git_ref: None,
        };
        assert_eq!(
            checkpoint_source_missing_commit(
                &configured,
                &CheckpointRepositoryBundle {
                    metadata: snapshot.metadata.clone(),
                    committed_bundle: snapshot.committed_bundle.clone(),
                },
                &ProcessExecutor,
                None,
            )
            .unwrap(),
            None
        );

        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&replacement).unwrap();
        git(&replacement, &["init", "-q", "-b", "main"]);
        git(&replacement, &["config", "user.name", "Hel Test"]);
        git(&replacement, &["config", "user.email", "hel@example.test"]);
        git(
            &replacement,
            &["commit", "--allow-empty", "-qm", "different history"],
        );
        let configured = ProjectRepository {
            local: Some(replacement),
            ..configured
        };
        assert!(
            checkpoint_source_missing_commit(
                &configured,
                &CheckpointRepositoryBundle {
                    metadata: snapshot.metadata,
                    committed_bundle: snapshot.committed_bundle,
                },
                &ProcessExecutor,
                None,
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn repository_preflight_checks_independent_sources_concurrently_and_receipts_are_scoped() {
        struct ConcurrentSourceExecutor {
            source_checks: Barrier,
        }

        impl CommandExecutor for ConcurrentSourceExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                if command.purpose == "check checkpoint base commit" {
                    self.source_checks.wait();
                }
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 3);
        let repositories = ["one", "two"]
            .map(|id| ProjectRepository {
                id: id.into(),
                github: None,
                local: Some(PathBuf::from(format!("/origin/{id}"))),
                destination: id.into(),
                git_ref: None,
            })
            .to_vec();
        let mut session = checkpoint_test_session(session_id);
        session.checkpoint = Some(checkpoint.clone());
        let mut controller = Controller {
            config: Config {
                bundles: BTreeMap::from([(
                    session.bundle_id.clone(),
                    ProjectBundle {
                        primary_repo: "one".into(),
                        repositories: repositories.clone(),
                    },
                )]),
                ..Config::default()
            },
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        let verified = ResumeRepositoryBundles {
            checkpoint_sha256: checkpoint.sha256,
            repositories: repositories
                .iter()
                .map(|repository| CheckpointRepositoryBundle {
                    metadata: mj_checkpoint::archive::RepositoryMetadata {
                        push_urls: Vec::new(),
                        remote_workspace: false,
                        id: repository.id.clone(),
                        relative_destination: repository.destination.clone(),
                        origin: repository.source_label(),
                        base_commit: String::new(),
                        head_commit: if repository.id == "one" {
                            "a".repeat(40)
                        } else {
                            "b".repeat(40)
                        },
                        branch: Some("main".into()),
                    },
                    committed_bundle: Vec::new(),
                })
                .collect(),
        };
        let executor = ConcurrentSourceExecutor {
            source_checks: Barrier::new(2),
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let preflight = pool
            .install(|| {
                controller.preflight_verified_repository_sources(
                    session_id, verified, None, false, &executor,
                )
            })
            .unwrap();
        let ResumeRepositorySourcePreflight::Ready(receipt) = preflight else {
            panic!("expected repository source receipt");
        };
        assert!(controller.repository_source_receipt_is_current(session_id, &receipt));

        controller
            .config
            .bundles
            .values_mut()
            .next()
            .unwrap()
            .repositories[0]
            .local = Some(PathBuf::from("/different-origin"));
        assert!(!controller.repository_source_receipt_is_current(session_id, &receipt));
    }

    #[test]
    fn repository_preflight_checks_declared_boundary_without_importing_delta_bundle() {
        struct RecordingExecutor {
            commands: Mutex<Vec<CommandSpec>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.lock().unwrap().push(command.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let prerequisite = "a".repeat(40);
        let head = "b".repeat(40);
        let archived = CheckpointRepositoryBundle {
            metadata: mj_checkpoint::archive::RepositoryMetadata {
                push_urls: Vec::new(),
                remote_workspace: false,
                id: "project".into(),
                relative_destination: "project".into(),
                origin: "https://github.com/archived/should-not-be-contacted.git".into(),
                base_commit: prerequisite.clone(),
                head_commit: head.clone(),
                branch: Some("main".into()),
            },
            committed_bundle: format!(
                "# v2 git bundle\n-{prerequisite} base\n{head} HEAD\n\nPACKnot-read"
            )
            .into_bytes(),
        };
        let configured = ProjectRepository {
            id: "project".into(),
            github: Some("configured/project".into()),
            local: None,
            destination: "project".into(),
            git_ref: None,
        };
        let executor = RecordingExecutor {
            commands: Mutex::new(Vec::new()),
        };

        assert_eq!(
            checkpoint_source_missing_commit(
                &configured,
                &archived,
                &executor,
                Some("secret-token")
            )
            .unwrap(),
            None
        );

        let commands = executor.commands.into_inner().unwrap();
        assert_eq!(commands.len(), 2, "commands: {commands:?}");
        assert_eq!(
            commands
                .iter()
                .map(|command| command.purpose.as_str())
                .collect::<Vec<_>>(),
            [
                "initialize repository source preflight",
                "check checkpoint base commit"
            ]
        );
        let source_check = &commands[1];
        assert!(
            source_check
                .args
                .iter()
                .any(|argument| argument == "credential.helper=")
        );
        assert_eq!(
            source_check
                .env
                .get("GIT_NO_LAZY_FETCH")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            source_check
                .env
                .get("GIT_TERMINAL_PROMPT")
                .map(String::as_str),
            Some("0")
        );
        assert_eq!(
            source_check.args.last().map(String::as_str),
            Some(prerequisite.as_str())
        );
        assert!(
            !source_check
                .args
                .iter()
                .any(|argument| argument.contains("archived"))
        );
    }

    #[test]
    fn self_contained_bundle_validation_cannot_lazy_fetch_or_prompt() {
        let command = checkpoint_bundle_import_command(
            Path::new("/tmp/repository.git"),
            Path::new("/tmp/checkpoint.bundle"),
        );
        assert_eq!(
            command.env.get("GIT_NO_LAZY_FETCH").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            command.env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn lost_bundle_sessions_reach_resume_compatibility_before_the_record_changes() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("resume ran {} before rejecting the target", command.program);
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 3);
        let mut session = checkpoint_test_session(session_id);
        session.state = SessionState::Lost;
        session.checkpoint = Some(checkpoint);
        let previous = session.clone();
        let profile_home = directory.path().join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut config = Config::default();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: profile_home,
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let mut controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.resume_session_controlled(
                session_id,
                "codex",
                "localhost",
                SessionResumeOptions {
                    additional_mounts: None,
                    resource_allocation: None,
                    discard_queue: false,
                },
                &UnusedExecutor,
            ))
            .unwrap_err();

        let detail = format!("{error:#}");
        assert!(detail.contains("created from a project bundle"), "{detail}");
        assert!(
            detail.contains("resume it on a container, SSH, or EC2 target"),
            "{detail}"
        );
        assert_eq!(controller.state.sessions[session_id], previous);
    }
    /// Records what it ran and blocks every command on a barrier sized to
    /// both lanes, so a run only finishes if the second lane started before
    /// the first one's command returned.
    struct BarrierExecutor {
        seen: Mutex<Vec<String>>,
        barrier: Barrier,
    }

    impl CommandExecutor for BarrierExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.seen.lock().unwrap().push(command.purpose.clone());
            self.barrier.wait();
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn lane_command(purpose: &str) -> CommandSpec {
        CommandSpec::new("hel", ["worker"]).purpose(purpose)
    }

    /// Launch progress must not claim "Start" while the target is still
    /// receiving the worker binary, the checkpoint archive and the restore.
    /// Everything before the daemon launch reports as Sync; the launch itself
    /// names its own stage, so a Sync-labelled executor cannot relabel it.
    #[test]
    fn start_begins_at_the_worker_launch_not_at_the_transfers_before_it() {
        struct RecordingExecutor {
            commands: RefCell<Vec<CommandSpec>>,
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
        }

        let session_id = "0123456789abcdef0123456789abcdef";
        let worker_root = format!("/var/lib/hel/workers/{session_id}");
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        let syncing = StagedExecutor::new(&executor, ProvisionStage::Syncing);
        let backend = targets::TargetLocator::LocalPodman {
            container_id: "abcdef0123456789".into(),
            workspace_storage: Default::default(),
        };

        upload_checkpoint_spec(
            &syncing,
            &backend,
            session_id,
            Path::new("/archives/session.hel.zip"),
            &format!("{worker_root}/restore.hel.zip"),
        )
        .unwrap();
        execute_checked(
            &syncing,
            restore_command(
                &backend,
                session_id,
                &format!("{worker_root}/restore-spec.json"),
            )
            .unwrap(),
        )
        .unwrap();
        // Deliberately run the launch through the Sync-labelled executor: it
        // must still report Start.
        start_worker(&syncing, &backend, &worker_root).unwrap();

        let stages = executor
            .commands
            .borrow()
            .iter()
            .map(|command| (command.purpose.clone(), command.stage))
            .collect::<Vec<_>>();
        assert_eq!(
            stages,
            vec![
                (
                    "upload checkpoint specification".to_owned(),
                    Some(ProvisionStage::Syncing)
                ),
                (
                    "restore target checkpoint".to_owned(),
                    Some(ProvisionStage::Syncing)
                ),
                (
                    "start detached Mjolnir worker".to_owned(),
                    Some(ProvisionStage::Starting)
                ),
            ]
        );
    }
    #[test]
    fn independent_target_lanes_run_at_the_same_time() {
        let executor = BarrierExecutor {
            seen: Mutex::new(Vec::new()),
            barrier: Barrier::new(2),
        };

        execute_concurrent_lanes(
            || execute_checked(&executor, lane_command("install the worker")).map(|_| ()),
            || execute_checked(&executor, lane_command("upload the checkpoint")).map(|_| ()),
        )
        .unwrap();

        let mut seen = executor.seen.into_inner().unwrap();
        seen.sort();
        assert_eq!(seen, ["install the worker", "upload the checkpoint"]);
    }
    #[test]
    fn a_lane_failure_is_reported_in_lane_order_and_never_abandons_the_other_lane() {
        let reached = Mutex::new(Vec::new());

        // The first lane fails slowly and the second immediately, so a
        // completion-order report could only pick the second.
        let error = execute_concurrent_lanes(
            || -> Result<()> {
                std::thread::sleep(Duration::from_millis(50));
                bail!("worker install failed")
            },
            || -> Result<()> {
                reached.lock().unwrap().push("second");
                bail!("checkpoint upload failed")
            },
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "worker install failed");
        assert_eq!(
            *reached.lock().unwrap(),
            ["second"],
            "a failing first lane must not cut the second one short"
        );

        let error = execute_concurrent_lanes(
            || Ok(()),
            || -> Result<()> { bail!("checkpoint upload failed") },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "checkpoint upload failed");
    }

    #[test]
    fn cross_harness_lanes_prove_overlap_with_handshake_channels() {
        let (provision_started_tx, provision_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (handoff_started_tx, handoff_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (provision_seen_handoff_tx, provision_seen_handoff_rx) =
            std::sync::mpsc::sync_channel::<()>(1);
        let (handoff_seen_provision_tx, handoff_seen_provision_rx) =
            std::sync::mpsc::sync_channel::<()>(1);

        execute_joined_cross_harness_work(
            "provision",
            move |_cancellation| -> Result<()> {
                provision_started_tx
                    .send(())
                    .map_err(|error| anyhow::anyhow!("signal provisioning start: {error}"))?;
                handoff_started_rx
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|error| anyhow::anyhow!("wait for handoff start: {error}"))?;
                provision_seen_handoff_tx
                    .send(())
                    .map_err(|error| anyhow::anyhow!("signal provisioning overlap: {error}"))?;
                Ok(())
            },
            "handoff",
            move |_cancellation| -> Result<()> {
                handoff_started_tx
                    .send(())
                    .map_err(|error| anyhow::anyhow!("signal handoff start: {error}"))?;
                provision_started_rx
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|error| anyhow::anyhow!("wait for provisioning start: {error}"))?;
                handoff_seen_provision_tx
                    .send(())
                    .map_err(|error| anyhow::anyhow!("signal handoff overlap: {error}"))?;
                Ok(())
            },
        )
        .unwrap();

        assert!(provision_seen_handoff_rx.recv().is_ok());
        assert!(handoff_seen_provision_rx.recv().is_ok());
    }

    #[test]
    fn cross_harness_lane_failure_cancels_and_joins_the_peer() {
        let (handoff_started_tx, handoff_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (handoff_joined_tx, handoff_joined_rx) = std::sync::mpsc::sync_channel::<()>(1);

        let error = execute_joined_cross_harness_work(
            "provision",
            move |_cancellation| -> Result<()> {
                handoff_started_rx
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|error| anyhow::anyhow!("wait for handoff start: {error}"))?;
                bail!("provisioning failed after handoff started");
            },
            "handoff",
            move |cancellation| -> Result<()> {
                handoff_started_tx
                    .send(())
                    .map_err(|error| anyhow::anyhow!("signal handoff start: {error}"))?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        anyhow::anyhow!("create cancellation test runtime: {error}")
                    })?;
                runtime
                    .block_on(async {
                        tokio::time::timeout(Duration::from_secs(2), cancellation.cancelled()).await
                    })
                    .map_err(|error| anyhow::anyhow!("peer was not cancelled: {error}"))?;
                handoff_joined_tx
                    .send(())
                    .map_err(|error| anyhow::anyhow!("signal handoff join: {error}"))?;
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "provisioning failed after handoff started"
        );
        assert!(handoff_joined_rx.recv().is_ok());
    }

    #[test]
    fn a_projection_standing_at_the_archived_frontier_is_reused() {
        let digest = "a".repeat(64);
        let other = "b".repeat(64);

        assert!(!projection_rebuild_required(
            Some((82_000, &digest)),
            82_000,
            &digest
        ));

        for stored in [
            // Same ordinal, different event chain.
            Some((82_000, other.as_str())),
            // Behind the archive, and ahead of it.
            Some((81_999, digest.as_str())),
            Some((82_001, digest.as_str())),
            // No projection stored, or none that could be read.
            None,
        ] {
            assert!(
                projection_rebuild_required(stored, 82_000, &digest),
                "{stored:?} must not be mistaken for the archived projection"
            );
        }
    }

    #[test]
    fn local_bare_restore_reuses_verified_absolute_archive_without_upload() {
        let archive = Path::new("/var/lib/hel/archives/session.hel.zip");
        let remote = Path::new("/var/lib/hel/workers/session/restore.hel.zip");
        let local = targets::TargetLocator::LocalBare {
            worker_root: "/var/lib/hel/workers/session".into(),
        };
        let container = targets::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
        };

        assert_eq!(restore_archive_path(&local, archive, remote), archive);
        assert!(!should_upload_restore_archive(&local));
        assert_eq!(restore_archive_path(&container, archive, remote), remote);
        assert!(should_upload_restore_archive(&container));
    }

    #[test]
    fn cross_harness_provision_cancellation_stops_the_next_command() {
        struct RecordingExecutor {
            commands: Mutex<Vec<String>>,
        }

        impl CommandExecutor for RecordingExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.lock().unwrap().push(command.purpose.clone());
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let inner = RecordingExecutor {
            commands: Mutex::new(Vec::new()),
        };
        let cancellation = CancellationToken::new();
        let provision = CrossHarnessProvisionExecutor {
            inner: &inner,
            cancellation: cancellation.clone(),
        };
        let command = CommandSpec::new("hel", ["worker"]).purpose("provision target");
        provision.execute(&command).unwrap();
        cancellation.cancel();

        let error = provision.execute(&command).unwrap_err();
        assert!(error.to_string().contains("cancelled while provisioning"));
        assert_eq!(
            inner.commands.lock().unwrap().as_slice(),
            ["provision target"]
        );
    }

    #[test]
    fn failed_resume_rolls_back_only_after_target_cleanup() {
        let previous = SessionRecord {
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: "0123456789abcdef0123456789abcdef".into(),
            title: "imported session".into(),
            harness_kind: mj_core::config::HarnessKind::Codex,
            last_profile: "codex-old".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman-old".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Stopped,
            target: None,
            native_session_id: Some("native-session".into()),
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
        let partial_target = TargetLocator::LocalPodman {
            container_id: "partial-container".into(),
            workspace_storage: Default::default(),
        };
        let mut cleaned = previous.clone();
        cleaned.state = SessionState::Error;
        cleaned.last_profile = "codex-new".into();
        cleaned.target = Some(partial_target.clone());

        let failure =
            apply_failed_resume_rollback(&mut cleaned, &previous, "worker upload failed", None);

        assert_eq!(cleaned.state, SessionState::Stopped);
        assert_eq!(cleaned.last_profile, "codex-old");
        assert_eq!(cleaned.target, None);
        assert_eq!(failure.to_string(), "worker upload failed");
        assert_eq!(
            cleaned.last_error.as_deref(),
            Some("resume failed: worker upload failed")
        );

        let mut cleanup_failed = previous.clone();
        cleanup_failed.state = SessionState::Error;
        cleanup_failed.last_profile = "codex-new".into();
        cleanup_failed.target = Some(partial_target.clone());
        let partial_checkout = crate::controller::test_support::managed_raw_session(
            mj_core::state::ManagedWorktreeTarget::Local,
        );
        cleanup_failed.project_directory = partial_checkout.project_directory.clone();
        cleanup_failed.managed_worktree = partial_checkout.managed_worktree.clone();

        let failure = apply_failed_resume_rollback(
            &mut cleanup_failed,
            &previous,
            "worker upload failed",
            Some("podman rm failed".into()),
        );

        assert_eq!(cleanup_failed.state, SessionState::Error);
        assert_eq!(cleanup_failed.last_profile, "codex-new");
        assert_eq!(cleanup_failed.target, Some(partial_target));
        assert_eq!(
            cleanup_failed.project_directory,
            partial_checkout.project_directory
        );
        assert_eq!(
            cleanup_failed.managed_worktree,
            partial_checkout.managed_worktree
        );
        assert!(failure.to_string().contains("cleanup"));
    }
    #[test]
    fn failed_worktree_cleanup_notice_names_mjolnir_and_the_recovery_command() {
        let notice = worktree_cleanup_notice(
            Path::new("/workspace/project"),
            &anyhow::anyhow!("permission denied"),
        );

        assert!(
            notice.starts_with(
                "Mjolnir could not remove the worktree at /workspace/project: permission denied."
            ),
            "{notice}"
        );
        assert!(
            notice.contains("`git worktree remove --force /workspace/project`"),
            "{notice}"
        );
        assert!(!notice.contains("Hel"), "{notice}");
    }
    #[test]
    fn failed_resume_provisioning_preserves_checkpoint_and_projection_lineage() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in an
        // exact child test instead of racing unrelated tests in this process.
        if std::env::var_os(RESUME_ROLLBACK_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::failed_resume_provisioning_preserves_checkpoint_and_projection_lineage",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(RESUME_ROLLBACK_TEST_CHILD, "1")
                // A remote target needs a portable worker; any existing file
                // satisfies the preflight so the test reaches provisioning.
                .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
                .env("MJ_DATA_DIR", directory.path())
                .env("GH_TOKEN", "test-token")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated resume rollback test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::database::install_isolated_test_writer();

        /// Provisioning runs after the resumed record is persisted, so the
        /// durable mounts read here are the ones resume just committed.
        #[derive(Default)]
        struct FailingPreflightExecutor {
            mounts_during_provisioning: Mutex<Option<Vec<AdditionalMount>>>,
        }

        impl CommandExecutor for FailingPreflightExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                // Provisioning probes the mount source's filesystem before it
                // builds the run arguments; a local disk keeps the overlay.
                if command.program == "stat" {
                    return Ok(CommandOutput {
                        status: 0,
                        stdout: b"ext4\n".to_vec(),
                        stderr: Vec::new(),
                    });
                }
                assert_eq!(command.program, "podman");
                let mut observed = self.mounts_during_provisioning.lock().unwrap();
                if observed.is_none() {
                    let durable = crate::database::load_state().unwrap();
                    *observed = Some(
                        durable.sessions["0123456789abcdef0123456789abcdef"]
                            .additional_mounts
                            .clone(),
                    );
                }
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"podman is temporarily unavailable".to_vec(),
                })
            }
        }

        let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
        let archive_directory = data_directory.join("archives");
        std::fs::create_dir_all(&archive_directory).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = super::super::test_support::write_network_checkpoint_archive(
            &archive_directory,
            session_id,
            7,
        );
        let archive = verify_archive_streaming(&checkpoint.archive_path).unwrap();
        let expected_projection =
            materialized_session_from_canonical(session_id, &archive.canonical_session).unwrap();

        let mut session = checkpoint_test_session(session_id);
        session.state = SessionState::Stopped;
        session.checkpoint = Some(checkpoint.clone());
        session.additional_mounts = vec![AdditionalMount {
            source: PathBuf::from("/host/old"),
            destination: PathBuf::from("/mnt/old"),
            read_only: false,
        }];
        let previous = session.clone();
        let resumed_mounts = vec![AdditionalMount {
            source: PathBuf::from("/host/new"),
            destination: PathBuf::from("/mnt/new"),
            read_only: false,
        }];
        let profile_home = data_directory.join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut config = Config::default();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: profile_home,
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: None,
                    local: Some(data_directory.join("host-clone-that-no-longer-exists")),
                    destination: "project".into(),
                    git_ref: None,
                }],
            },
        );
        config.targets.insert(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ConfigContainer {
                    image: "example.invalid/hel-test:latest".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        );
        let mut controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        crate::database::save_state(&controller.state).unwrap();
        crate::database::save_materialized_session(&expected_projection).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let executor = FailingPreflightExecutor::default();
        let error = runtime
            .block_on(controller.resume_session_controlled(
                session_id,
                "codex",
                "podman",
                SessionResumeOptions {
                    additional_mounts: Some(resumed_mounts.clone()),
                    resource_allocation: None,
                    discard_queue: false,
                },
                &executor,
            ))
            .unwrap_err();
        let detail = format!("{error:#}");
        assert!(
            detail.contains("podman is temporarily unavailable"),
            "{detail}"
        );
        assert!(!detail.contains("returned to stopped"), "{detail}");
        assert!(!detail.contains("unknown session"), "{detail}");
        assert_eq!(
            executor.mounts_during_provisioning.into_inner().unwrap(),
            Some(resumed_mounts)
        );

        let retained = controller.state.sessions.get(session_id).unwrap();
        assert_eq!(retained.state, SessionState::Stopped);
        assert_eq!(retained.checkpoint, previous.checkpoint);
        assert_eq!(retained.managed_worktree, previous.managed_worktree);
        assert!(checkpoint.archive_path.is_file());

        let durable = crate::database::load_state().unwrap();
        let durable_session = durable.sessions.get(session_id).unwrap();
        assert_eq!(durable_session.state, SessionState::Stopped);
        assert_eq!(durable_session.checkpoint, previous.checkpoint);
        assert_eq!(
            durable_session.additional_mounts,
            previous.additional_mounts
        );
        assert_eq!(
            crate::database::load_materialized_session(session_id).unwrap(),
            Some(expected_projection)
        );
    }
    #[test]
    fn failed_resume_retires_a_checkout_it_recreated() {
        if std::env::var_os(RETIRED_WORKTREE_RESUME_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::failed_resume_retires_a_checkout_it_recreated",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(RETIRED_WORKTREE_RESUME_TEST_CHILD, "1")
                // A remote target needs a portable worker; any existing file
                // satisfies the preflight so the test reaches provisioning.
                .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
                .env("MJ_DATA_DIR", directory.path().join("data"))
                .env("MJ_CONFIG_DIR", directory.path().join("config"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated retired-worktree resume test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::database::install_isolated_test_writer();

        struct FailAfterWorktreeRestore;

        impl CommandExecutor for FailAfterWorktreeRestore {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                if matches!(command.program.as_str(), "git" | "mkdir") {
                    return ProcessExecutor.execute(command);
                }
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"stop after recreating the checkout".to_vec(),
                })
            }
        }

        let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
        let archive_directory = data_directory.join("archives");
        std::fs::create_dir_all(&archive_directory).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);
        let repository = committed_repository();
        let mut session = managed_worktree_session(repository.path(), session_id);
        session.checkpoint = Some(checkpoint);
        let worktree = session.managed_worktree.clone().unwrap();
        retire_managed_worktree(&ProcessExecutor, &worktree).unwrap();
        assert!(!worktree.worktree_root.exists());

        let profile_home = data_directory.join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut config = resume_compatibility_config();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: profile_home,
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        let mut controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        crate::database::save_state(&controller.state).unwrap();

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.resume_session_controlled(
                session_id,
                "codex",
                "local-bare",
                SessionResumeOptions {
                    additional_mounts: None,
                    resource_allocation: None,
                    discard_queue: false,
                },
                &FailAfterWorktreeRestore,
            ))
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("stop after recreating the checkout"),
            "{error:#}"
        );
        assert!(!worktree.worktree_root.exists());
        assert_eq!(
            controller.state.sessions[session_id].state,
            SessionState::Stopped
        );
        let branch = Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args([
                "show-ref",
                "--verify",
                &format!("refs/heads/{}", worktree.branch),
            ])
            .status()
            .unwrap();
        assert!(branch.success(), "resume rollback must retain the branch");
    }
    #[test]
    fn a_conversion_archive_carries_the_checkouts_remote_and_the_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let previous = write_checkpoint_archive_with_native_state(directory.path(), session_id, 7);
        let (checkout, _remote_parent, _remote) = checkout_with_network_remote();
        let source =
            mj_core::remote_git::resolve_local_repository(checkout.path(), &ProcessExecutor)
                .unwrap();
        let dirname = PathBuf::from(checkout.path().file_name().unwrap());
        let snapshot =
            raw_checkout_snapshot(checkout.path(), &source, &dirname, &SystemGit).unwrap();

        let output = directory.path().join("converted.hel.zip");
        let converted = conversion_checkpoint(&previous.archive_path, snapshot, &output).unwrap();

        // The archive provisioning will read: a real network clone of the
        // checkout's own remote, landing where the archive says.
        let verified = mj_checkpoint::archive::read_archive_verified(&output).unwrap();
        assert_eq!(converted.archive_path, output);
        assert_eq!(converted.sha256, verified.archive_sha256);
        assert_eq!(converted.event_frontier, previous.event_frontier);
        let bundle =
            crate::controller::network_git::bundle_from_manifest(&verified.manifest).unwrap();
        assert_eq!(bundle.primary, dirname.to_string_lossy());
        assert_eq!(bundle.repositories.len(), 1);
        assert_eq!(
            bundle.repositories[0].url.as_deref(),
            Some(FIXTURE_FETCH_URL)
        );
        assert_eq!(bundle.repositories[0].push_urls, [FIXTURE_FETCH_URL]);
        assert_eq!(
            bundle.repositories[0].destination,
            dirname.to_string_lossy()
        );

        // Everything the conversation is made of comes across untouched.
        let original =
            mj_checkpoint::archive::read_archive_verified(&previous.archive_path).unwrap();
        assert_eq!(
            verified.canonical_session().unwrap(),
            original.canonical_session().unwrap()
        );
        assert_eq!(verified.manifest.session, original.manifest.session);
        assert_eq!(native_state(&original), native_state(&verified));
        assert!(
            !native_state(&verified).is_empty(),
            "the fixture has native state"
        );
    }

    /// Every native payload of an archive as (path, mode, bytes).
    fn native_state(
        archive: &mj_checkpoint::archive::VerifiedArchive,
    ) -> Vec<(PathBuf, u32, Vec<u8>)> {
        archive
            .manifest
            .payloads
            .iter()
            .filter_map(|descriptor| match &descriptor.role {
                mj_checkpoint::archive::PayloadRole::NativeArtifact { relative_path } => Some((
                    relative_path.clone(),
                    descriptor.mode,
                    archive.payload(descriptor).unwrap().to_vec(),
                )),
                _ => None,
            })
            .collect()
    }
    const RAW_CONVERSION_TEST_CHILD: &str = "MJ_RAW_CONVERSION_TEST_CHILD";
    #[test]
    fn a_failed_raw_conversion_keeps_the_checkout_and_its_previous_checkpoint() {
        // MJ_DATA_DIR and MJ_CONFIG_DIR are process-global, so run the half
        // that writes them in an exact child test.
        if std::env::var_os(RAW_CONVERSION_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::a_failed_raw_conversion_keeps_the_checkout_and_its_previous_checkpoint",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(RAW_CONVERSION_TEST_CHILD, "1")
                // A remote target needs a portable worker; any existing file
                // satisfies the preflight so the test reaches provisioning.
                .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
                .env("MJ_DATA_DIR", directory.path().join("data"))
                .env("MJ_CONFIG_DIR", directory.path().join("config"))
                .env("GH_TOKEN", "test-token")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated raw conversion test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::database::install_isolated_test_writer();

        /// Real Git, no container runtime. Provisioning fails at preflight,
        /// after the conversion has already reshaped the record.
        struct GitWithoutPodmanExecutor;

        impl CommandExecutor for GitWithoutPodmanExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                if command.program == "git" {
                    return ProcessExecutor.execute(command);
                }
                Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"podman is temporarily unavailable".to_vec(),
                })
            }
        }

        let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
        let archive_directory = data_directory.join("archives");
        std::fs::create_dir_all(&archive_directory).unwrap();
        std::fs::create_dir_all(mj_core::config::config_dir()).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);

        let repository = committed_repository();
        // An isolated workspace is a clone of a network remote, so the
        // checkout that converts has to have one, with its base pushed.
        let (_remote_parent, _remote) = network_remote_for(repository.path());
        let mut session = managed_worktree_session(repository.path(), session_id);
        session.checkpoint = Some(checkpoint.clone());
        let worktree = session.managed_worktree.clone().unwrap();
        let previous = session.clone();

        let profile_home = data_directory.join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut config = resume_compatibility_config();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: profile_home,
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        // Production controllers read this configuration from disk; bundle
        // updates now deliberately reload it under the transaction lock.
        config.save().unwrap();
        let original_config = config.clone();
        let mut controller = Controller {
            config,
            state: State {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..State::default()
            },
        };
        crate::database::save_state(&controller.state).unwrap();

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.resume_session_controlled(
                session_id,
                "codex",
                "podman",
                SessionResumeOptions {
                    additional_mounts: None,
                    resource_allocation: None,
                    discard_queue: false,
                },
                &GitWithoutPodmanExecutor,
            ))
            .unwrap_err();
        // The conversion ran: it wrote its archive and reshaped the record,
        // and then the destination could not be provisioned.
        assert!(
            format!("{error:#}").contains("podman is temporarily unavailable"),
            "{error:#}"
        );
        assert!(!format!("{error:#}").contains("returned to stopped"));

        // A conversion installs a bundle for the checkout it converts, and
        // reuses it on a retry. Nothing else about the configuration moves.
        let mut expected_config = original_config.clone();
        let (bundle_id, bundle) = mj_core::config::Config::load()
            .unwrap()
            .bundles
            .into_iter()
            .next()
            .expect("the conversion installed a bundle for the checkout");
        expected_config.bundles.insert(bundle_id, bundle);
        assert_eq!(
            controller.config,
            expected_config.clone().with_local_targets()
        );
        assert_eq!(
            mj_core::config::Config::load_from(&mj_core::config::config_path()).unwrap(),
            expected_config
        );

        let retained = controller.state.sessions.get(session_id).unwrap();
        assert_eq!(retained.state, SessionState::Stopped);
        assert_eq!(retained.checkpoint, Some(checkpoint.clone()));
        assert_eq!(retained.project_directory, previous.project_directory);
        assert_eq!(retained.managed_worktree, previous.managed_worktree);
        assert_eq!(retained.bundle_id, previous.bundle_id);
        assert!(worktree.worktree_root.is_dir(), "the checkout stays put");
        assert!(
            checkpoint.archive_path.is_file(),
            "the previous archive is what the rolled-back record names"
        );
        let durable = crate::database::load_state().unwrap();
        assert_eq!(durable.sessions[session_id].checkpoint, Some(checkpoint));
        // The conversion archive is litter once the resume has failed, and the
        // directory it was written in proves the conversion really ran.
        let sessions = mj_core::config::sessions_dir();
        assert!(sessions.is_dir(), "the conversion wrote an archive");
        let leftover: Vec<_> = std::fs::read_dir(&sessions)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name())
                    .filter(|name| name.to_string_lossy().ends_with(".hel.zip"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "{leftover:?} in {}",
            sessions.display()
        );
    }

    /// Resuming into a different harness cannot reload the archived native
    /// session, so the transcript is handed over as the first context instead.
    #[test]
    fn only_the_same_harness_keeps_native_continuity_on_resume() {
        use mj_core::config::HarnessKind;

        assert!(super::native_continuity_preserved(
            HarnessKind::Codex,
            HarnessKind::Codex
        ));
        assert!(super::native_continuity_preserved(
            HarnessKind::Claude,
            HarnessKind::Claude
        ));
        assert!(!super::native_continuity_preserved(
            HarnessKind::Claude,
            HarnessKind::Codex
        ));
    }
}
