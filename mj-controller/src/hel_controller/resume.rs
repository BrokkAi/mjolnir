//! Resuming a stopped session onto a profile and target.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::ContentBlock;
use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::hel_session_manager::new_command_id;
use hel::hel_archive::{
    CanonicalQueuedCommandKind, CanonicalSessionSnapshot, CheckpointRepositoryBundle, SystemGit,
    checkpoint_bundle_prerequisites, read_checkpoint_repository_bundles, verify_archive_streaming,
};
use hel::hel_checkpoint::{CheckpointRestoreSpec, restore_command};
use hel::hel_config::{HelConfig, ProjectRepository, mount_history_host};
use hel::hel_projection::materialized_session_from_canonical;
use hel::hel_state::{MaterializedSession, SessionRecord, SessionResourceAllocation, SessionState};
use hel::hel_targets::{
    self, AdditionalMount, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec,
    ProcessExecutor, ProvisionStage, ProvisionStageGuard,
};
use hel::hel_worker::RelayCommand;

use super::backend::{backend_locator, controller_github_token, validate_resource_allocation};
use super::checkpoint::upload_checkpoint_spec;
use super::provisioning::{
    LocalBootstrap, ProvisioningFailureDisposition, StagedExecutor, execute_concurrent_lanes,
    install_attached_resources,
};
use super::readiness::{connect_started_worker, wait_for_native_session};
use super::worker_binary::{start_worker, worker_probe_diagnosis};
use super::worktree::{
    PrimaryCheckoutRequirement, ResumeConversion, ResumePlan, apply_raw_to_workspace,
    apply_workspace_to_raw, cleanup_managed_worktree, create_managed_worktree,
    managed_worktree_checkout_exists, plan_raw_to_workspace, raw_checkout_divergence_notice,
    raw_checkout_position, restore_managed_worktree, resume_compatibility, retire_managed_worktree,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRepositorySourceReceipt {
    session_id: String,
    bundle_id: String,
    checkpoint_sha256: String,
    repositories: Vec<ProjectRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeRepositorySourcePreflight {
    Ready(ResumeRepositorySourceReceipt),
    RepositoryMoved(ResumeRepositorySourceMismatch),
}

struct ResumeRepositoryBundles {
    checkpoint_sha256: String,
    repositories: Vec<CheckpointRepositoryBundle>,
}

impl Controller {
    /// Prove that each configured repository source still supplies the commit
    /// boundary its checkpoint bundle expects, before provisioning anything.
    pub fn preflight_resume_repository_sources(
        &self,
        session_id: &str,
        target_id: &str,
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
            return Ok(ResumeRepositorySourcePreflight::Ready(
                ResumeRepositorySourceReceipt {
                    session_id: session_id.to_owned(),
                    bundle_id: session.bundle_id.clone(),
                    checkpoint_sha256: checkpoint.sha256.clone(),
                    repositories: Vec::new(),
                },
            ));
        }
        let repositories = read_checkpoint_repository_bundles(&checkpoint.archive_path)?;
        self.preflight_verified_repository_sources(
            session_id,
            ResumeRepositoryBundles {
                checkpoint_sha256: checkpoint.sha256.clone(),
                repositories,
            },
            None,
            executor,
        )
    }

    fn preflight_verified_repository_sources(
        &self,
        session_id: &str,
        verified: ResumeRepositoryBundles,
        skip_repository_id: Option<&str>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
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
        let bundle = self
            .config
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
        repository.github = replacement.github;
        repository.local = replacement.local;
        self.config.save()?;
        self.preflight_verified_repository_sources(
            session_id,
            verified,
            Some(repository_id),
            executor,
        )
    }
}

fn replacement_repository_source(id: &str, replacement: &str) -> Result<ProjectRepository> {
    let replacement = replacement.trim();
    ensure!(!replacement.is_empty(), "enter the repository's new origin");
    let path = Path::new(replacement);
    let (github, local) = if path.is_absolute() {
        ensure!(
            path.is_dir(),
            "local repository {replacement:?} is not a directory"
        );
        (None, Some(hel::hel_local_git::canonical_repository(path)?))
    } else {
        let github = crate::hel_setup::github_repository_from_origin(replacement)
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
        let github = crate::hel_setup::github_repository_from_origin(source)
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
        if !repository_preflight
            .as_ref()
            .is_some_and(|receipt| self.repository_source_receipt_is_current(session_id, receipt))
            && let ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) =
                self.preflight_resume_repository_sources(session_id, target_id, executor)?
        {
            bail!(
                "checkpoint base commit {} is missing from configured source {:?} for repository {:?}; the repository may have moved (archived origin: {:?})",
                mismatch.missing_commit,
                mismatch.configured_origin,
                mismatch.repository_id,
                mismatch.archived_origin,
            );
        }
        // Take the snapshot out of the verified metadata and share it behind an
        // `Arc`: on a long session it is tens of megabytes, and resume reads it
        // from three places that used to hold private copies.
        let hel::hel_archive::VerifiedArchiveMetadata {
            manifest: archive_manifest,
            canonical_session,
            archive_sha256,
        } = verify_archive_streaming(&checkpoint.archive_path)?;
        if archive_sha256 != checkpoint.sha256 || archive_manifest.session.id != session_id {
            bail!("persisted checkpoint verification failed");
        }
        let canonical_session = Arc::new(canonical_session);
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .with_context(|| format!("unknown profile {profile_id:?}"))?
            .clone();
        let target_template = self
            .config
            .targets
            .get(target_id)
            .with_context(|| format!("unknown target template {target_id:?}"))?;
        // Decide the representation before the record changes, so an
        // incompatible target fails here instead of during provisioning.
        let plan = resume_compatibility(&previous, &self.config, target_id)
            .map_err(|reason| anyhow::anyhow!("{reason}"))?;
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
        validate_resource_allocation(target_template, resource_allocation.as_ref())?;
        let selected_container_size =
            selected_host_container_size(target_template, resource_allocation.as_ref());
        if !additional_mounts.is_empty() && mount_history_host(target_template).is_none() {
            bail!("attached resources are unsupported for this target");
        }
        hel_targets::validate_additional_mounts(&additional_mounts)?;
        let history_host = mount_history_host(target_template);
        let history_mounts = additional_mounts.clone();
        if previous.state == SessionState::Error
            && let Some(locator) = &previous.target
        {
            let backend = backend_locator(locator, &previous, &self.config)?;
            hel_targets::close_plan(&backend, session_id)?
                .execute(executor)
                .context("clean up target from failed resume")?;
        }
        let mut resume_notices = Vec::new();
        if let Some(conversion) = conversion
            .as_ref()
            .and_then(ResumeConversion::raw_to_workspace)
            && let Some(project_directory) = &previous.project_directory
        {
            resume_notices.push(match &conversion.retire {
                Some(worktree) => format!(
                    "This session moved out of {} and into the {target_id} target. Its branch {} stays in {}.",
                    project_directory.display(),
                    worktree.branch,
                    worktree.source_repository.display()
                ),
                None => format!(
                    "This session moved out of {} and into the {target_id} target.",
                    project_directory.display()
                ),
            });
        }
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
        let same_harness = profile.kind == archive_manifest.session.harness_kind;
        let context_bytes = profile
            .context_window_bytes
            .unwrap_or(crate::hel_compaction::DEFAULT_CONTEXT_BYTES);
        let utility_handoff = if same_harness {
            None
        } else {
            let _compacting = ProvisionStageGuard::new(executor, ProvisionStage::Compacting);
            Some(
                utility_handoff_while_cancellable(
                    &self.config,
                    &canonical_session,
                    context_bytes,
                    executor,
                )
                .await
                .context("compact the cross-harness handoff transcript")?,
            )
        };
        let discard_queued_prompts = discard_queue || !same_harness;
        // When this controller archived the session, its durable projection is
        // already the archive's content. Reading one row decides that; a read
        // failure or any mismatch rebuilds as before.
        let stored_frontier = hel::hel_database::materialized_event_frontier(session_id)
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
            self.config
                .bundles
                .insert(conversion.bundle_id.clone(), bundle.clone());
            self.config
                .save()
                .context("save the bundle for a converted raw session")?;
        }

        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.harness_kind = profile.kind;
        record.last_profile = profile_id.to_string();
        record.target_template_id = target_id.to_string();
        record.resource_allocation = resource_allocation;
        record.additional_mounts = additional_mounts;
        record.target = None;
        record.native_session_id =
            same_harness.then(|| archive_manifest.session.native_session_id.clone());
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
            hel::hel_database::remember_mount_sources(host, &history_mounts)?;
        }
        // The session's prompt history is filed under its bundle, so a
        // conversion moves the history with it before the record is persisted.
        if let Some(conversion) = conversion
            .as_ref()
            .and_then(ResumeConversion::raw_to_workspace)
        {
            hel::hel_database::rebind_session_bundle(session_id, &conversion.bundle_id)?;
        }
        // Resume rewrites the record it resumes, including the attached
        // directories and the harness session id, so it writes the whole row.
        if let Some((host, size)) = selected_container_size.as_ref() {
            hel::hel_database::save_session_with_container_size(
                &self.state.sessions[session_id],
                host,
                *size,
            )?;
        } else {
            hel::hel_database::save_session(&self.state.sessions[session_id])?;
        }
        if let Some((host, size)) = selected_container_size {
            self.state.remember_container_size(&host, size);
        }

        let mut recreated_managed_worktree = false;
        let result = async {
            if let Some(worktree) = previous.managed_worktree.as_ref() {
                recreated_managed_worktree = restore_managed_worktree(executor, worktree)?;
                if recreated_managed_worktree && plan == ResumePlan::RawToWorkspace {
                    hel::hel_checkpoint::restore_single_repository_onto_branch(
                        &checkpoint.archive_path,
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
                create_managed_worktree(
                    executor,
                    &conversion.worktree,
                    None,
                    PrimaryCheckoutRequirement::Any,
                )?;
                hel::hel_checkpoint::restore_single_repository_onto_branch(
                    &checkpoint.archive_path,
                    &conversion.worktree.worktree_root,
                    &conversion.worktree.branch,
                    &SystemGit,
                )
                .context("restore this session's checkout")?;
            }
            self.provision_session_with_failure_disposition(
                session_id,
                executor,
                github_token.as_deref(),
                ProvisioningFailureDisposition::Preserve,
            )
            .await?;
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
                    hel_targets::TargetLocator::LocalPodman { .. }
                    | hel_targets::TargetLocator::LocalDocker { .. }
                    | hel_targets::TargetLocator::AppleContainer { .. }
                    | hel_targets::TargetLocator::SshPodman { .. } => "/workspace".to_string(),
                    hel_targets::TargetLocator::AwsEc2 { workspace, .. }
                    | hel_targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
                    hel_targets::TargetLocator::LocalBare { worker_root } => worker_root.clone(),
                }
            };
            let target_path = |path: &str| match &backend {
                hel_targets::TargetLocator::AwsEc2 { .. }
                | hel_targets::TargetLocator::SshBare { .. }
                    if !path.starts_with('/') =>
                {
                    PathBuf::from(format!("~/{path}"))
                }
                _ => PathBuf::from(path),
            };
            let remote_archive = format!("{worker_root}/restore.hel.zip");
            let remote_spec = format!("{worker_root}/restore-spec.json");
            let restore = CheckpointRestoreSpec {
                archive_path: target_path(&remote_archive),
                workspace_root: target_path(&workspace_root),
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                // A converted session's repository arrives as a seed from its
                // own checkout. An in-place managed checkout recreated from
                // its retained branch still needs the archive's dirty state.
                restore_repositories: (resumed_project_directory.is_none() && conversion.is_none())
                    || (recreated_managed_worktree && plan == ResumePlan::InPlace),
                restore_native: same_harness,
                // A conversion puts the checkout somewhere the archive could
                // not have named, so the restored harness session is pointed at
                // the real working directory instead of the archived one.
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
                if let Some(command) = hel_targets::clear_relay_state_plan(&backend, session_id)? {
                    execute_checked(syncing, command)?;
                }
                // Both lanes below write into the worker root, so it exists first.
                execute_checked(
                    syncing,
                    hel_targets::command_on_locator(
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
            // and the local Git connection together are the longer of the two,
            // so overlapping them hides the smaller one entirely.
            //
            // The Git connection stays behind the worker install in its own
            // lane: the target fetches through `ext::<worker root>/hel worker
            // git-proxy`, so the binary has to be there before a fetch runs.
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
                    // The restore needs the fetched objects: a committed delta
                    // bundle cannot be applied without its prerequisites, and a
                    // bundle-free snapshot checks out a head commit only the
                    // proxy can supply. The archive carries this session's
                    // dirty state, so nothing is seeded.
                    controller.connect_local_repositories(
                        session_id,
                        backend_ref,
                        worker_root_ref,
                        syncing,
                        LocalBootstrap::Skip,
                    )
                },
                || {
                    let restoring = &StagedExecutor::new(executor, ProvisionStage::Restoring);
                    upload_checkpoint_spec(
                        restoring,
                        backend_ref,
                        session_id,
                        &checkpoint.archive_path,
                        &remote_archive,
                    )?;
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
                self.connect_local_repositories(
                    session_id,
                    &backend,
                    &worker_root,
                    syncing,
                    match conversion
                        .as_ref()
                        .and_then(ResumeConversion::raw_to_workspace)
                    {
                        Some(conversion) => LocalBootstrap::SeedFrom(conversion.checkout.clone()),
                        None => LocalBootstrap::Seed,
                    },
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
                    hel::hel_database::save_materialized_session(&restored_projection)?;
                }
                // The stored projection already is the archived one. Only the
                // queue can still need changing.
                None if discard_queued_prompts => {
                    hel::hel_database::replace_materialized_queued_prompts(session_id, &[])?;
                }
                None => {}
            }
            let executor = &StagedExecutor::new(executor, ProvisionStage::Starting);
            start_worker(executor, &backend, &worker_root)?;
            let spec = self.reconnect_command(session_id)?;
            let readiness = async {
                let mut relay =
                    connect_started_worker(&spec, session_id, executor, &backend, &worker_root)
                        .await?;
                let native_session_id = wait_for_native_session(&mut relay, executor).await?;
                Ok::<_, anyhow::Error>((relay, native_session_id))
            }
            .await;
            let (mut relay, native_session_id) = readiness
                .map_err(|error| worker_probe_diagnosis(executor, &backend, &worker_root, error))?;
            if same_harness {
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
                            .context("cross-harness resume has no utility-model handoff")?,
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
            Ok(materialized) => Ok(materialized),
            Err(error) => {
                // Put back whatever this resume could have written to the
                // durable projection. Both branches restore archived content,
                // so they are correct whether or not the write had happened
                // when the resume failed.
                if rebuild_projection {
                    match materialized_session_from_canonical(session_id, &canonical_session) {
                        Ok(previous_projection) => {
                            if let Err(restore_error) =
                                hel::hel_database::save_materialized_session(&previous_projection)
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
                    && let Err(restore_error) =
                        hel::hel_database::replace_materialized_queued_prompts(
                            session_id,
                            &hel::hel_projection::materialized_queued_prompts_from_canonical(
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

    fn rollback_failed_resume(
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
                hel_targets::close_plan(&backend, session_id)?
                    // Use a fresh executor: cancellation applies to the
                    // requested operation, not to its compensating cleanup.
                    .execute(&CancellableProcessExecutor::with_timeout(
                        Duration::from_secs(15),
                    ))
                    .map(|_| ())
            })(),
            None => Ok(()),
        };
        let worktree_cleanup = match (
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
            hel::hel_database::rebind_session_bundle(session_id, &bundle_id)?;
        }
        // The rollback restores the record the resume replaced, attached
        // directories included, so it writes the whole row back.
        hel::hel_database::save_session(&self.state.sessions[session_id])?;
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
            // The target locator stays so the leftover resource can still be
            // cleaned up, but the session's representation goes back: a resume
            // that converted the record never moved the checkout it names.
            current
                .project_directory
                .clone_from(&previous.project_directory);
            current
                .managed_worktree
                .clone_from(&previous.managed_worktree);
            current.bundle_id.clone_from(&previous.bundle_id);
            current.state = SessionState::Error;
            current.updated_at = now();
            current.last_error = Some(format!("resume failed: {failure}"));
            anyhow::anyhow!(failure)
        }
    }
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

/// Discover a utility model and compact the cross-harness handoff while still
/// watching for cancellation. Discovery and compaction can both make several
/// network requests, so a cancelled resume must not wait them out.
async fn utility_handoff_while_cancellable(
    config: &HelConfig,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    executor: &impl CommandExecutor,
) -> Result<String> {
    if executor.cancellation_requested() {
        bail!("operation cancelled while compacting the cross-harness handoff");
    }
    let cancel = tokio_util::sync::CancellationToken::new();
    let operation = async {
        let candidates = crate::hel_utility_llm::UtilityLlmRuntime::shared()
            .resolve(config, &cancel)
            .await?;
        let backend =
            crate::hel_utility_llm::UtilityCompactionBackend::new(candidates, cancel.clone());
        crate::hel_compaction::compact_snapshot(snapshot, context_bytes, &backend).await
    };
    tokio::pin!(operation);
    loop {
        tokio::select! {
            context = &mut operation => return context,
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

    use crate::hel_controller::test_support::{
        checkpoint_test_session, committed_repository, managed_worktree_session,
        resume_compatibility_config, write_checkpoint_gate_archive,
    };
    use crate::hel_controller::{Controller, SessionResumeOptions};
    use hel::hel_archive::{GitCommandRunner, verify_archive_streaming};
    use hel::hel_config::{
        ContainerTemplate as ConfigContainer, HarnessProfile, HelConfig, ProjectBundle,
        ProjectRepository, TargetTemplate,
    };
    use hel::hel_projection::materialized_session_from_canonical;
    use hel::hel_state::{HelState, SessionRecord, SessionState, TargetLocator};
    use hel::hel_targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};

    use super::*;

    const RESUME_ROLLBACK_TEST_CHILD: &str = "MJ_RESUME_ROLLBACK_TEST_CHILD";
    const RETIRED_WORKTREE_RESUME_TEST_CHILD: &str = "MJ_RETIRED_WORKTREE_RESUME_TEST_CHILD";

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
            config: HelConfig {
                targets: BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]),
                // The raw checkout is still usable even though its synthetic
                // grouping bundle has disappeared from the config.
                bundles: BTreeMap::new(),
                ..HelConfig::default()
            },
            state: HelState {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..HelState::default()
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
                    &hel::hel_archive::GitCommand {
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
        let snapshot = hel::hel_archive::collect_git_snapshot(
            &SystemGit,
            &source,
            &hel::hel_archive::GitCollectionSpec {
                id: "project".into(),
                relative_destination: "project".into(),
                history: hel::hel_archive::GitHistoryMode::SessionDelta,
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
            config: HelConfig {
                bundles: BTreeMap::from([(
                    session.bundle_id.clone(),
                    ProjectBundle {
                        primary_repo: "one".into(),
                        repositories: repositories.clone(),
                    },
                )]),
                ..HelConfig::default()
            },
            state: HelState {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..HelState::default()
            },
        };
        let verified = ResumeRepositoryBundles {
            checkpoint_sha256: checkpoint.sha256,
            repositories: repositories
                .iter()
                .map(|repository| CheckpointRepositoryBundle {
                    metadata: hel::hel_archive::RepositoryMetadata {
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
                controller
                    .preflight_verified_repository_sources(session_id, verified, None, &executor)
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
            metadata: hel::hel_archive::RepositoryMetadata {
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
        let mut config = HelConfig::default();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                kind: hel::hel_config::HarnessKind::Codex,
                home: profile_home,
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        let mut controller = Controller {
            config,
            state: HelState {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..HelState::default()
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
        let backend = hel_targets::TargetLocator::LocalPodman {
            container_id: "abcdef0123456789".into(),
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
    fn failed_resume_rolls_back_only_after_target_cleanup() {
        let previous = SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: "0123456789abcdef0123456789abcdef".into(),
            title: "imported session".into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
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

        let failure = apply_failed_resume_rollback(
            &mut cleanup_failed,
            &previous,
            "worker upload failed",
            Some("podman rm failed".into()),
        );

        assert_eq!(cleanup_failed.state, SessionState::Error);
        assert_eq!(cleanup_failed.last_profile, "codex-new");
        assert_eq!(cleanup_failed.target, Some(partial_target));
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
        let _writer = hel::hel_database::install_isolated_test_writer();

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
                    let durable = hel::hel_database::load_state().unwrap();
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
        let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);
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
        let mut config = HelConfig::default();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                kind: hel::hel_config::HarnessKind::Codex,
                home: profile_home,
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: Some("example/project".into()),
                    local: None,
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
                },
            },
        );
        let mut controller = Controller {
            config,
            state: HelState {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..HelState::default()
            },
        };
        hel::hel_database::save_state(&controller.state).unwrap();
        hel::hel_database::save_materialized_session(&expected_projection).unwrap();

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

        let durable = hel::hel_database::load_state().unwrap();
        let durable_session = durable.sessions.get(session_id).unwrap();
        assert_eq!(durable_session.state, SessionState::Stopped);
        assert_eq!(durable_session.checkpoint, previous.checkpoint);
        assert_eq!(
            durable_session.additional_mounts,
            previous.additional_mounts
        );
        assert_eq!(
            hel::hel_database::load_materialized_session(session_id).unwrap(),
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
        let _writer = hel::hel_database::install_isolated_test_writer();

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
                kind: hel::hel_config::HarnessKind::Codex,
                home: profile_home,
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        let mut controller = Controller {
            config,
            state: HelState {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..HelState::default()
            },
        };
        hel::hel_database::save_state(&controller.state).unwrap();

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
    const RAW_CONVERSION_TEST_CHILD: &str = "MJ_RAW_CONVERSION_TEST_CHILD";
    #[test]
    fn a_failed_raw_conversion_keeps_the_bundle_and_leaves_the_worktree_alone() {
        // MJ_DATA_DIR and MJ_CONFIG_DIR are process-global, so run the half
        // that writes them in an exact child test.
        if std::env::var_os(RAW_CONVERSION_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::a_failed_raw_conversion_keeps_the_bundle_and_leaves_the_worktree_alone",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(RAW_CONVERSION_TEST_CHILD, "1")
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
        let _writer = hel::hel_database::install_isolated_test_writer();

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
        std::fs::create_dir_all(hel::hel_config::config_dir()).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let checkpoint = write_checkpoint_gate_archive(&archive_directory, session_id, 7);

        let repository = committed_repository();
        let mut session = managed_worktree_session(repository.path(), session_id);
        session.checkpoint = Some(checkpoint);
        let worktree = session.managed_worktree.clone().unwrap();
        let previous = session.clone();

        let profile_home = data_directory.join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut config = resume_compatibility_config();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                kind: hel::hel_config::HarnessKind::Codex,
                home: profile_home,
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        let mut controller = Controller {
            config,
            state: HelState {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..HelState::default()
            },
        };
        hel::hel_database::save_state(&controller.state).unwrap();

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
        assert!(
            format!("{error:#}").contains("podman is temporarily unavailable"),
            "{error:#}"
        );
        assert!(!format!("{error:#}").contains("returned to stopped"));

        // The bundle stays: it was saved before the record referenced it, and a
        // retry reuses it instead of adding another.
        let (_, bundle) = controller
            .config
            .bundles
            .iter()
            .find(|(_, bundle)| bundle.repositories[0].local.as_deref() == Some(repository.path()))
            .expect("the conversion added a bundle for the checkout");
        assert_eq!(
            bundle.repositories[0].destination,
            PathBuf::from(session_id)
        );
        let saved = hel::hel_config::HelConfig::load().unwrap();
        assert_eq!(saved.bundles, controller.config.bundles);

        let retained = controller.state.sessions.get(session_id).unwrap();
        assert_eq!(retained.state, SessionState::Stopped);
        assert_eq!(retained.project_directory, previous.project_directory);
        assert_eq!(retained.managed_worktree, previous.managed_worktree);
        assert_eq!(retained.bundle_id, previous.bundle_id);
        assert!(worktree.worktree_root.is_dir(), "the checkout stays put");
    }
}
