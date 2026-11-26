//! Resuming a stopped session onto a profile and target.

use mj_core::targets::SshTrafficClass;
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::v1::schema::ContentBlock;
use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::checkpoint_transfer::restore_stdin_command;
use crate::handoff::NativePriorSource;
use crate::session_manager::new_command_id;
use mj_checkpoint::archive::{
    CanonicalQueuedCommandKind, CanonicalSessionSnapshot, CheckpointRepositoryBundle, SystemGit,
    checkpoint_bundle_prerequisites, manifest_uses_origin_delta,
    read_checkpoint_manifest_and_repository_bundles, read_checkpoint_repository_bundles,
    verify_archive_streaming,
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
use mj_core::remote_git::is_github_https_url;

use super::backend::{backend_locator, validate_resource_allocation};
use super::checkpoint::upload_checkpoint_spec;
use super::provisioning::{
    ProvisioningFailureDisposition, RepositorySeed, StagedExecutor, execute_concurrent_lanes,
    install_attached_resources,
};
use super::readiness::{
    connect_started_worker, wait_for_harness_prepared_in_stage, wait_for_native_session_in_stage,
};
use super::worker_binary::{
    bridge_readiness_stage, start_worker_durably_with_aws_profile, worker_probe_diagnosis,
};
use super::worktree::{
    PrimaryCheckoutRequirement, ResumeConversion, ResumePlan, apply_raw_to_workspace,
    apply_workspace_to_raw, capture_legacy_raw_checkout_snapshot, create_managed_worktree,
    ensure_managed_worktree_available, managed_worktree_checkout_exists, managed_worktree_target,
    plan_raw_to_workspace_for_session, plan_raw_to_workspace_from_origin_delta,
    preserve_retained_managed_worktree_branch, raw_checkout_divergence_notice,
    raw_checkout_position_with_checkout, raw_conversion_preview_with_checkout,
    restore_legacy_worktree_as_managed_clone, restore_managed_worktree,
    resume_compatibility_with_checkout, upgrade_legacy_worktree_to_clone,
};
use super::{
    Controller, SessionResumeOptions, execute_checked, now, selected_host_container_size,
    target_profile_home,
};
use crate::github_app::{GithubAppTokenProvider, github_repositories};

pub use mj_core::state::{
    ResumeRepositorySourceMismatch, ResumeRepositorySourcePreflight, ResumeRepositorySourceReceipt,
};

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
    /// Muse has one workspace root, and its native restore relocates its
    /// metadata.
    pub(super) fn validate_muse_resume_destination(
        &self,
        source: &SessionRecord,
        destination_harness: HarnessKind,
        _target_id: &str,
    ) -> Result<()> {
        if destination_harness != HarnessKind::Muse {
            return Ok(());
        }
        let checkout = self.state.checkout(&source.id)?;
        ensure!(
            checkout.project_directory().is_some()
                || source
                    .project_bundle(&self.config)
                    .is_none_or(|bundle| bundle.repositories.len() == 1),
            "{} ACP supports one workspace root; use a single-repository bundle",
            destination_harness.display_name()
        );
        Ok(())
    }
    /// Prove that each configured repository source still supplies the commit
    /// boundary its checkpoint bundle expects, before provisioning anything,
    /// and describe a local checkout's conversion so a person can confirm it.
    pub async fn preflight_resume_repository_sources(
        &self,
        session_id: &str,
        target_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        if let Some(receipt) = self.repair_preflight_receipt(session_id, target_id, executor)? {
            return Ok(ResumeRepositorySourcePreflight::Ready(receipt));
        }
        let github_token = self
            .github_token_for_repository_preflight(session_id)
            .await?;
        self.preflight_repository_sources(
            session_id,
            target_id,
            true,
            github_token.as_deref(),
            executor,
        )
    }

    /// Preflight when the caller already resolved the session's token and must
    /// keep the remaining repository checks synchronous.
    pub fn preflight_resume_repository_sources_with_token(
        &self,
        session_id: &str,
        target_id: &str,
        github_token: Option<&str>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        self.preflight_repository_sources(session_id, target_id, true, github_token, executor)
    }

    /// Retained placements can repair without repository credentials or an
    /// archive, but a raw conversion still needs its preview and confirmation.
    fn repair_preflight_receipt(
        &self,
        session_id: &str,
        target_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<Option<ResumeRepositorySourceReceipt>> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session missing")?;
        if session.state != SessionState::Error || session.target.is_none() {
            return Ok(None);
        }
        let checkout = self.state.checkout(session_id)?;
        let plan = resume_compatibility_with_checkout(session, &checkout, &self.config, target_id)
            .map_err(|reason| anyhow::anyhow!(reason))?;
        if plan == ResumePlan::RawToWorkspace {
            return Ok(None);
        }
        // Advisory only: the lifecycle owner reclassifies under its permit.
        // Unknown evidence is not permission to demand archive credentials.
        if matches!(
            super::environment::resume_checkout_available(self, session_id, executor),
            Ok(false)
        ) {
            return Ok(None);
        }
        Ok(Some(ResumeRepositorySourceReceipt {
            session_id: session_id.to_owned(),
            bundle_id: session.bundle_id.clone(),
            checkpoint_sha256: String::new(),
            repositories: Vec::new(),
        }))
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
        github_token: Option<&str>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if describe_conversion
            && let Some(receipt) = self.repair_preflight_receipt(session_id, target_id, executor)?
        {
            return Ok(ResumeRepositorySourcePreflight::Ready(receipt));
        }
        let checkpoint = session
            .checkpoint
            .as_ref()
            .context("session has no checkpoint")?;

        let checkout = self.state.checkout(session_id)?;
        let plan = resume_compatibility_with_checkout(session, &checkout, &self.config, target_id)
            .map_err(|reason| anyhow::anyhow!(reason))?;
        if checkout.project_directory().is_some() {
            debug_assert!(matches!(
                plan,
                ResumePlan::InPlace | ResumePlan::RawToWorkspace
            ));
            // A raw session resumes from its live checkout. Its accepted
            // snapshot can outlive its configuration entry, so raw preflight
            // does not require the bundle to remain configured.
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
                let preview =
                    raw_conversion_preview_for(session, &checkout, &self.config, executor)?;
                return Ok(ResumeRepositorySourcePreflight::ConvertingRawCheckout {
                    receipt,
                    preview: Box::new(preview),
                });
            }
            return Ok(ResumeRepositorySourcePreflight::Ready(receipt));
        }
        let (manifest, repositories) =
            read_checkpoint_manifest_and_repository_bundles(&checkpoint.archive_path)?;
        // The destination checks the delta's prerequisites after cloning the
        // archived origin, which may only be reachable from that target.
        for repository in &manifest.repositories {
            if let Some(origin) = repository
                .metadata
                .origin_delta
                .as_ref()
                .and_then(|delta| delta.origin_url.as_deref())
            {
                mj_core::remote_git::validate_network_url(origin)?;
            }
        }
        self.preflight_verified_repository_sources(
            session_id,
            ResumeRepositoryBundles {
                checkpoint_sha256: checkpoint.sha256.clone(),
                repositories,
            },
            None,
            plan != ResumePlan::WorkspaceToRaw,
            github_token,
            executor,
        )
    }

    fn preflight_verified_repository_sources(
        &self,
        session_id: &str,
        verified: ResumeRepositoryBundles,
        skip_repository_id: Option<&str>,
        use_archived_network_sources: bool,
        github_token: Option<&str>,
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
        let bundle = session
            .project_bundle(&self.config)
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
        let outcomes = verified
            .repositories
            .par_iter()
            .zip(configured.par_iter())
            .map(move |(archived, configured)| {
                if skip_repository_id == Some(configured.id.as_str()) {
                    return Ok(None);
                }
                checkpoint_source_missing_commit(
                    configured,
                    session
                        .project
                        .as_ref()
                        .and_then(|project| project.network_sources.get(&configured.id)),
                    archived,
                    executor,
                    github_token,
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
        let Some(bundle) = session.project_bundle(&self.config) else {
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
    pub async fn replace_resume_repository_origin(
        &mut self,
        session_id: &str,
        repository_id: &str,
        replacement: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        match self
            .prepare_resume_repository_origin(session_id, repository_id, replacement, executor)
            .await?
        {
            ResumeOriginPreparation::Mismatch(mismatch) => {
                Ok(ResumeRepositorySourcePreflight::RepositoryMoved(mismatch))
            }
            ResumeOriginPreparation::Verified(repair) => {
                repair.save(self)?.check_remaining(self, executor)
            }
        }
    }

    pub async fn prepare_resume_repository_origin(
        &self,
        session_id: &str,
        repository_id: &str,
        replacement: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeOriginPreparation> {
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
        let replacement_network = session
            .project
            .as_ref()
            .map(|_| mj_core::remote_git::resolve_repository(&replacement, executor))
            .transpose()?;
        let previous = session.clone();
        let mut project = session.project.clone();
        if let Some(project) = &mut project {
            let project = std::sync::Arc::make_mut(project);
            let repository = project
                .bundle
                .repositories
                .iter_mut()
                .find(|repository| repository.id == repository_id)
                .context("accepted project no longer contains the repaired repository")?;
            repository.github = replacement.github.clone();
            repository.local = replacement.local.clone();
            project.network_sources.insert(
                repository_id.to_owned(),
                replacement_network
                    .clone()
                    .expect("accepted project source resolved above"),
            );
        }
        let github_token = if self.config.github.session_token_path.is_some() {
            self.github_token_for_session(session_id).await?
        } else if let Some(app) = self.config.github.app.as_ref() {
            if let Some(project) = &project {
                let bundle = project.bundle.clone();
                let network_sources = project.network_sources.clone();
                let repositories = mj_core::runtime::spawn_blocking(move || {
                    github_repositories(&bundle, Some(&network_sources), &ProcessExecutor)
                })
                .await
                .context("replacement GitHub repository source task failed")??;
                let provider = GithubAppTokenProvider::shared(app)?;
                let scope = provider
                    .token_for_owner_repo_pairs(&bundle_id, &repositories)
                    .await
                    .map_err(super::GithubBundleSelectionError::into_anyhow)?;
                match scope {
                    Some(scope) => Some(
                        provider
                            .token_for_installation(
                                scope.installation_id,
                                &scope.repositories,
                                app.session_permissions.as_ref(),
                            )
                            .await?,
                    ),
                    None => None,
                }
            } else {
                None
            }
        } else {
            self.github_token_for_session(session_id).await?
        };
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
            replacement_network.as_ref(),
            archived,
            executor,
            github_token.as_deref(),
        )? {
            return Ok(ResumeOriginPreparation::Mismatch(
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
        Ok(ResumeOriginPreparation::Verified(Box::new(
            VerifiedResumeOrigin {
                previous,
                repository_id: repository_id.to_owned(),
                replacement,
                project,
                verified,
                github_token,
            },
        )))
    }
}

pub enum ResumeOriginPreparation {
    Mismatch(ResumeRepositorySourceMismatch),
    Verified(Box<VerifiedResumeOrigin>),
}

pub struct VerifiedResumeOrigin {
    previous: SessionRecord,
    repository_id: String,
    replacement: ProjectRepository,
    project: Option<Arc<mj_core::repository::ProjectBundleSnapshot>>,
    verified: ResumeRepositoryBundles,
    github_token: Option<String>,
}

impl VerifiedResumeOrigin {
    pub fn save(self, controller: &mut Controller) -> Result<SavedResumeOrigin> {
        let VerifiedResumeOrigin {
            previous,
            repository_id,
            replacement,
            project,
            verified,
            github_token,
        } = self;
        let session_id = previous.id.as_str();
        let bundle_id = &previous.bundle_id;
        let repository_id = repository_id.as_str();
        // A merged project can use different repository IDs. Its destination
        // and identity identify the authoring entry; the session keeps its IDs.
        let saved = crate::database::saved_project(bundle_id)?;
        let config_repository_id = saved
            .as_ref()
            .and_then(|(_, saved)| {
                let accepted = previous.project.as_ref()?;
                let original = accepted
                    .bundle
                    .repositories
                    .iter()
                    .find(|repository| repository.id == repository_id)?;
                saved
                    .bundle
                    .repositories
                    .iter()
                    .find(|repository| {
                        repository.destination == original.destination
                            && saved.identities.get(&repository.id)
                                == accepted.identities.get(repository_id)
                    })
                    .map(|repository| repository.id.clone())
            })
            .unwrap_or_else(|| repository_id.to_owned());
        let config_bundle_id = saved
            .as_ref()
            .map_or(bundle_id.as_str(), |(id, _)| id.as_str());
        let (config, ()) = Config::update(|config| {
            if project.is_some() && !config.bundles.contains_key(config_bundle_id) {
                return Ok(());
            }
            let bundle = config
                .bundles
                .get_mut(config_bundle_id)
                .with_context(|| format!("session bundle {bundle_id:?} is missing"))?;
            let repository = bundle
                .repositories
                .iter_mut()
                .find(|repository| repository.id == config_repository_id)
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
        controller.config = config;
        if let Some(project) = project {
            controller
                .state
                .sessions
                .get_mut(session_id)
                .expect("session checked above")
                .project = Some(project);
            controller.persist_session_transition_or_restore(
                session_id,
                &previous,
                "save repaired project source",
            )?;
        }
        Ok(SavedResumeOrigin {
            session_id: previous.id,
            repository_id: repository_id.to_owned(),
            verified,
            github_token,
        })
    }
}

pub struct SavedResumeOrigin {
    session_id: String,
    repository_id: String,
    verified: ResumeRepositoryBundles,
    github_token: Option<String>,
}

impl SavedResumeOrigin {
    pub fn check_remaining(
        self,
        controller: &Controller,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<ResumeRepositorySourcePreflight> {
        controller.preflight_verified_repository_sources(
            &self.session_id,
            self.verified,
            Some(&self.repository_id),
            false,
            self.github_token.as_deref(),
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
/// How a restore prepares the worker root it is about to install into.
pub(super) enum WorkerRootReset {
    /// Today's behaviour for a freshly provisioned target: clear leftover relay
    /// state on a bare target, then make sure the worker root exists.
    FreshTarget,
    /// The environment is being kept and only the harness replaced. Stop the
    /// live daemon and clear relay state. A compatible native Move keeps its
    /// executable and native home; a full replacement removes them.
    InPlace {
        /// The per-session profile root the replaced harness ran from.
        previous_profile_root: String,
        /// The compatible native conversation and executable already exist here.
        retain_native: bool,
    },
}

/// Direct target-to-target Move transfer context passed into
/// `restore_into_target` so the Move archive and `.mj/keep/<session-id>/`
/// stream straight from source target to destination target without staging on
/// daemon disk.
pub(super) struct MoveDirectTransfer<'a> {
    pub operation: &'a mut mj_core::state::MoveOperation,
    pub source_backend: targets::TargetLocator,
    pub source_worker_root: String,
    pub source_keep_dir: Option<PathBuf>,
    pub dest_keep_dir: Option<PathBuf>,
    pub already_transferred: bool,
}

/// Everything the restore tail of a resume needs, once the destination target
/// is ready and the session record already describes the destination.
///
/// The head of a resume differs a great deal between a fresh environment and an
/// in-place harness swap; from here on the two are the same work.
pub(super) struct RestoreIntoTarget<'a> {
    /// The destination profile, which decides the harness home inside the target.
    pub profile: &'a mj_core::config::HarnessProfile,
    pub archive: &'a VerifiedResumeArchive,
    /// The archive the target actually restores: the conversion's own archive
    /// when a resume wrote one, otherwise the verified checkpoint.
    pub restored_archive: &'a Path,
    pub resumed_project_directory: Option<PathBuf>,
    pub resumed_container_workspace: Option<PathBuf>,
    pub restore_repositories: bool,
    pub native_continuity: bool,
    pub discard_queued_prompts: bool,
    /// Whether the archived queue is resubmitted to a fresh native session.
    pub replay_queue: bool,
    /// `Import` for Resume, `Suppress` for cross-target Move (which admits or
    /// discards queued prompts and mail after `DestinationOwns` commits).
    pub queue_policy_override: QueuePolicy,
    /// Direct source-to-destination transfer for cross-target Move.
    pub move_transfer: Option<MoveDirectTransfer<'a>>,
    /// The compacted handoff a cross-harness restore installs as its first
    /// prompt context. Required whenever `native_continuity` is false.
    pub utility_handoff: Option<String>,
    pub projection_build: Option<tokio::task::JoinHandle<Result<MaterializedSession>>>,
    /// Conversation lines to record once the destination answers.
    pub resume_notices: Vec<String>,
    /// Refresh the retained-child roster immediately before prompt context and
    /// the conversation notice are installed.
    pub include_in_place_subagents: bool,
    pub install_attached_resources: bool,
    pub worker_root_reset: WorkerRootReset,
    /// A managed checkout to retire once, and only once, the restore succeeded.
    pub retire_after_ready: Option<&'a crate::database::EnvironmentRecord>,
}

/// Who dispatches the archived queue and the archived worker's pending mail.
///
/// The restored worker may own both at once only when nothing must happen
/// first. A Move destination admits them after Move commits ownership
/// (`admit_move_queue`): until then a failed Move hands the session back to
/// its source, which still holds the same queue and mail. A fresh native
/// session admits them after its handoff context is installed, which a turn
/// started by a waking message would refuse.
/// Controls whether shared restore imports the archived queue into the
/// destination relay (`Import` for Resume) or leaves the destination relay's
/// queue empty (`Suppress` for Move, which admits or discards the queue in its
/// own post-commit admission step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePolicy {
    Import,
    Suppress,
}

fn restore_queue_policy(
    move_queue: Option<mj_core::state::ResumeQueueDisposition>,
    native_continuity: bool,
    discard_queued_prompts: bool,
    replay_queue: bool,
) -> mj_checkpoint::checkpoint::QueueRestorePolicy {
    use mj_checkpoint::checkpoint::QueueRestorePolicy;
    use mj_core::state::ResumeQueueDisposition;
    match move_queue {
        Some(ResumeQueueDisposition::Start) => QueueRestorePolicy::Defer,
        Some(ResumeQueueDisposition::Discard) => QueueRestorePolicy::DiscardDeferringMail,
        None if !native_continuity && replay_queue => QueueRestorePolicy::Defer,
        None if !native_continuity => QueueRestorePolicy::DiscardDeferringMail,
        None if discard_queued_prompts => QueueRestorePolicy::Discard,
        None => QueueRestorePolicy::Restore,
    }
}

/// The archived worker's pending mail, as the commands that admit it to a
/// destination whose restore deferred it. Each keeps the command ID the daemon
/// uses for that event, and the worker acknowledges a key it already holds or
/// delivered without taking it again, so resubmitting after a lost answer or
/// an interrupted admission cannot deliver twice.
pub(super) fn deferred_mail_commands(
    canonical: &mj_core::archive::CanonicalSessionSnapshot,
) -> Vec<(String, RelayCommand)> {
    deferred_mail_commands_from_mailbox(canonical.mailbox.as_ref())
}

pub(super) fn deferred_mail_commands_from_mailbox(
    mailbox: Option<&mj_core::archive::CanonicalMailbox>,
) -> Vec<(String, RelayCommand)> {
    mailbox
        .into_iter()
        .flat_map(|mailbox| &mailbox.pending_events)
        .map(|event| {
            (
                crate::mailbox_outbox::mailbox_command_id(&event.key),
                RelayCommand::DeliverMailboxEvent {
                    event: event.clone(),
                },
            )
        })
        .collect()
}

impl Controller {
    /// Prepare the destination worker, seed its relay, start the harness, and
    /// report the projection the destination answers with.
    pub(super) async fn restore_into_target(
        &mut self,
        session_id: &str,
        restore: RestoreIntoTarget<'_>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        crate::worker_lifecycle::run(session_id, "restore into target", executor, async {
            let RestoreIntoTarget {
                profile,
                archive,
                restored_archive,
                resumed_project_directory,
                resumed_container_workspace,
                restore_repositories,
                native_continuity,
                discard_queued_prompts,
                replay_queue,
                queue_policy_override,
                mut move_transfer,
                utility_handoff,
                projection_build,
                mut resume_notices,
                include_in_place_subagents,
                install_attached_resources: should_install_attached_resources,
                worker_root_reset,
                retire_after_ready,
            } = restore;
            let canonical_session = &archive.canonical_session;
            let retain_native = matches!(
                &worker_root_reset,
                WorkerRootReset::InPlace {
                    retain_native: true,
                    ..
                }
            );
            // Sub-agents the suspend stopped: the model hears about them on its
            // first prompt, and the person in a conversation line. The list is
            // kept until the relay has the note, so a resume that fails tells
            // the next one.
            let stopped_subagents = crate::database::load_stopped_subagents(session_id)
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "could not read the sub-agents this session's suspend stopped"
                    );
                    Vec::new()
                });
            let stopped_subagents_context =
                mj_core::subagent::stopped_subagents_prompt_context(&stopped_subagents);
            let reopen_move_subagents =
                matches!(&worker_root_reset, WorkerRootReset::InPlace { .. })
                    && super::move_session::worker_subagent_queue_enabled(
                        &self.state.sessions[session_id]
                            .subagents
                            .clone()
                            .unwrap_or_default(),
                        profile.kind,
                        self.state
                            .subagents
                            .get(session_id)
                            .map(|child| child.handback_tool),
                        true,
                    );
            resume_notices.extend(mj_core::subagent::stopped_subagents_notice(
                &stopped_subagents,
            ));
            let mut prompt_contexts = Vec::new();
            prompt_contexts.extend(stopped_subagents_context);
            let (backend, worker_root) = self.worker_placement(session_id)?;
            let harness_home = target_profile_home(&backend, session_id, profile);
            let workspace_root =
                if let Some(worktree) = self.state.sessions[session_id].managed_worktree.as_ref() {
                    // Use the managed clone's parent rather than `project_directory.parent()`:
                    // a remotely seeded `--no-checkout` clone has not checked out any
                    // subdirectory yet when `ensure_room_for` and `restore_checkpoint`
                    // validate `workspace_root`.
                    worktree
                        .worktree_root
                        .parent()
                        .context("managed worktree has no parent")?
                        .to_string_lossy()
                        .into_owned()
                } else if let Some(project_directory) = &resumed_project_directory {
                    project_directory
                        .parent()
                        .context("bare project directory has no parent")?
                        .to_string_lossy()
                        .into_owned()
                } else {
                    super::network_git::workspace_root(
                        &backend,
                        resumed_container_workspace.as_deref(),
                        session_id,
                    )
                };
            let target_path = |path: &str| match &backend {
                targets::TargetLocator::AwsEc2 { .. } | targets::TargetLocator::SshBare { .. }
                    if !path.starts_with('/') =>
                {
                    PathBuf::from(format!("~/{path}"))
                }
                _ => PathBuf::from(path),
            };
            let remote_archive = format!("{worker_root}/restore.hel.zip");
            let move_destination =
                crate::database::load_move_operation(session_id)?.filter(|operation| {
                    matches!(
                        operation.phase,
                        mj_core::state::MovePhase::Capturing
                            | mj_core::state::MovePhase::Transferring
                            | mj_core::state::MovePhase::Restoring
                            | mj_core::state::MovePhase::DestinationStarting
                    ) && !operation.queue_admission_started
                });
            let suppress_move_queue = match queue_policy_override {
                QueuePolicy::Suppress => Some(
                    move_transfer
                        .as_ref()
                        .map(|t| t.operation.queue)
                        .or_else(|| move_destination.as_ref().map(|op| op.queue))
                        .unwrap_or(mj_core::state::ResumeQueueDisposition::Discard),
                ),
                QueuePolicy::Import => move_destination.as_ref().map(|operation| operation.queue),
            };
            let queue_policy = restore_queue_policy(
                suppress_move_queue,
                native_continuity,
                discard_queued_prompts,
                replay_queue,
            );
            let report_root = resumed_project_directory.as_ref().map_or_else(
                || PathBuf::from(&workspace_root).join(mj_core::subagent::REPORT_ROOT_DIR),
                |project| project.join(mj_core::subagent::PROJECT_REPORT_ROOT_DIR),
            );
            let restore = CheckpointRestoreSpec {
                report_root: Some(target_path(&report_root.to_string_lossy())),
                archive_path: if move_transfer.is_some() {
                    target_path(&remote_archive)
                } else {
                    restore_archive_path(&backend, restored_archive, &target_path(&remote_archive))
                },
                workspace_root: target_path(&workspace_root),
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                // A local checkout converting into a workspace arrives as a
                // fresh clone of its own remote, and the conversion archive
                // carries the commits, dirty files, and branch that go over it.
                // An in-place managed checkout recreated from its retained
                // branch still needs the archive's dirty state.
                restore_repositories,
                restore_native: native_continuity,
                // A session with a project directory launches its harness there,
                // spelled exactly as recorded (the worker launch configuration's
                // `cwd`), so the restored harness session is keyed by that same
                // text. Rebuilding it from the archive's repository layout loses a
                // trailing separator, which Grok Build keys by, and cannot name
                // the checkout a move put the session on. A session in a target
                // workspace launches in its accepted project's primary
                // repository under the workspace. The archive need not name
                // that folder: a Move from a host checkout records the
                // checkout's own folder. A session with no accepted project
                // keeps the archive's layout, as before.
                primary_repository_root: match &resumed_project_directory {
                    Some(directory) => Some(target_path(&directory.to_string_lossy())),
                    None => self.state.sessions[session_id]
                        .project
                        .as_ref()
                        .and_then(|project| project.bundle.primary())
                        .map(|primary| target_path(&workspace_root).join(&primary.destination)),
                },
                queue_policy,
            };
            // Prepare the worker root before the worker binary is installed:
            // a surviving daemon still holds the old binary open, and the
            // install would land on a running executable.
            {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                match &worker_root_reset {
                    // A bare target keeps the closed session's worker root on
                    // the host. Stop anything still writing there and clear the
                    // leftover relay state, or the restore's seed loses to a
                    // stale snapshot whose frontier no journal can support.
                    WorkerRootReset::FreshTarget => {
                        if let Some(command) =
                            targets::clear_relay_state_plan(&backend, session_id)?
                        {
                            command.execute(syncing, &crate::controller::ControlWorker)?;
                        }
                        // Both lanes below write into the worker root, so it
                        // exists first.
                        execute_checked(
                            syncing,
                            targets::command_on_locator(
                                SshTrafficClass::Control,
                                &backend,
                                session_id,
                                vec!["mkdir".into(), "-p".into(), worker_root.clone()],
                                "create the session worker root",
                            )?,
                        )?;
                    }
                    // Stop the owning process before changing its files. A
                    // native continuation needs only a fresh relay; a new
                    // harness also replaces its executable and native home.
                    WorkerRootReset::InPlace {
                        previous_profile_root,
                        retain_native,
                    } => {
                        let reset = if *retain_native {
                            targets::retained_native_worker_reset_plan(&backend, session_id)?
                        } else {
                            targets::in_place_worker_reset_plan(
                                &backend,
                                session_id,
                                previous_profile_root,
                            )?
                        };
                        reset.execute(syncing, &crate::controller::ControlWorker)?;
                    }
                }
            }
            {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                self.prepare_subagent_report_dir(session_id, &backend, syncing)?;
            }
            let staged_aws_profile = if retain_native {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                let staged = self.prepare_worker_files_for(
                    session_id,
                    &backend,
                    &worker_root,
                    syncing,
                    super::worker_binary::WorkerFilePreparation::RefreshProfile,
                    super::worker_binary::LaunchHistory::Restored(canonical_session),
                )?;
                let seed = mj_checkpoint::checkpoint::relay_seed_for_restore(
                    canonical_session,
                    true,
                    queue_policy,
                )?;
                let staging = tempfile::tempdir().context("stage retained native relay seed")?;
                let local_seed = staging
                    .path()
                    .join(mj_core::relay::RESTORED_RELAY_SEED_FILE);
                mj_core::config::atomic_write(&local_seed, &serde_json::to_vec(&seed)?)?;
                let remote_seed = mj_core::target_path::text(
                    &mj_core::relay::restored_relay_seed_path(Path::new(&worker_root)),
                );
                upload_checkpoint_spec(syncing, &backend, session_id, &local_seed, &remote_seed)?;
                staged.aws_profile
            } else {
                let controller = &*self;
                let backend_ref = &backend;
                let worker_root_ref = worker_root.as_str();
                let staged_aws_profile = if let Some(transfer) = move_transfer.as_mut() {
                    // Direct cross-target Move: install destination worker
                    // binary first so `receive-move-stream` is available, then
                    // stream archive + `.mj/keep/` directly from source target
                    // to destination target without touching daemon disk.
                    let staged = {
                        let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                        let staged = controller.prepare_worker_files(
                            session_id,
                            backend_ref,
                            worker_root_ref,
                            syncing,
                            super::worker_binary::LaunchHistory::Restored(canonical_session),
                        )?;
                        super::provisioning::install_inherited_git_settings(
                            syncing,
                            backend_ref,
                            session_id,
                        )?;
                        staged
                    };
                    if !transfer.already_transferred {
                        transfer.operation.phase = mj_core::state::MovePhase::Transferring;
                        transfer.operation.updated_at = now();
                        crate::database::save_move_operation(transfer.operation)?;
                        let restoring = &StagedExecutor::new(executor, ProvisionStage::Restoring);
                        let computed_dest_keep = match transfer.dest_keep_dir.clone() {
                            Some(path) => Some(path),
                            None => {
                                let repo_root =
                                    resumed_project_directory.clone().unwrap_or_else(|| {
                                        self.state.sessions[session_id]
                                            .project_bundle(&self.config)
                                            .and_then(|bundle| bundle.primary())
                                            .map(|primary| {
                                                PathBuf::from(&workspace_root)
                                                    .join(&primary.destination)
                                            })
                                            .unwrap_or_else(|| PathBuf::from(&workspace_root))
                                    });
                                Some(resolve_move_keep_dir(
                                    restoring,
                                    backend_ref,
                                    session_id,
                                    &repo_root,
                                )?)
                            }
                        };
                        super::move_session::transfer_move_snapshot_to_destination(
                            transfer.operation,
                            &transfer.source_backend,
                            &transfer.source_worker_root,
                            backend_ref,
                            worker_root_ref,
                            Path::new(&remote_archive),
                            transfer.source_keep_dir.as_deref(),
                            computed_dest_keep.as_deref(),
                            restoring,
                        )?;
                        transfer.already_transferred = true;
                    }
                    transfer.operation.phase = mj_core::state::MovePhase::Restoring;
                    transfer.operation.updated_at = now();
                    crate::database::save_move_operation(transfer.operation)?;
                    staged
                } else {
                    // Two independent lanes into the target. The checkpoint transfer
                    // needs nothing from the worker install, and the worker install
                    // is independent of archive upload, so both run concurrently.
                    // The archive lands in the worker root and unpacks into the
                    // workspace and the harness home; each filesystem needs room.
                    crate::target_storage::ensure_room_for(
                        &backend,
                        [
                            worker_root.as_str(),
                            workspace_root.as_str(),
                            harness_home.as_str(),
                        ],
                        || {
                            Ok(std::fs::metadata(restored_archive)
                                .with_context(|| format!("measure {}", restored_archive.display()))?
                                .len())
                        },
                        "restore the checkpoint",
                    )?;
                    let (staged, ()) = execute_concurrent_lanes(
                        || {
                            let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                            let staged = controller.prepare_worker_files(
                                session_id,
                                backend_ref,
                                worker_root_ref,
                                syncing,
                                super::worker_binary::LaunchHistory::Restored(canonical_session),
                            )?;
                            super::provisioning::install_inherited_git_settings(
                                syncing,
                                backend_ref,
                                session_id,
                            )?;
                            Ok(staged)
                        },
                        || {
                            let restoring =
                                &StagedExecutor::new(executor, ProvisionStage::Restoring);
                            if should_upload_restore_archive(&backend) {
                                let reused =
                                    mj_checkpoint::checkpoint::checkpoint_sha256(restored_archive)
                                        .ok()
                                        .and_then(|sha256| {
                                            super::checkpoint::try_stage_host_snapshot_cache(
                                                restoring,
                                                backend_ref,
                                                session_id,
                                                &sha256,
                                                &remote_archive,
                                            )
                                            .ok()
                                        })
                                        .unwrap_or(false);
                                if !reused {
                                    upload_checkpoint_spec(
                                        restoring,
                                        backend_ref,
                                        session_id,
                                        restored_archive,
                                        &remote_archive,
                                    )?;
                                }
                            }
                            Ok(())
                        },
                    )?;
                    staged
                };
                {
                    let restoring = &StagedExecutor::new(executor, ProvisionStage::Restoring);
                    // The spec streams over stdin, so no upload of it
                    // precedes the restore. Both branches above install this
                    // build's worker first; an older one would read `-` as a
                    // file name.
                    let command = restore_stdin_command(&backend, session_id)?;
                    let output = super::checkpoint::send_spec_streamed(
                        restoring,
                        &command,
                        &restore,
                        "restore target checkpoint",
                    )?;
                    let output = targets::checked_command_output(&command, output)?;
                    if move_transfer.is_some() {
                        // Remove the transferred Move archive from the
                        // destination worker root once restore has unpacked it.
                        if let Ok(rm_cmd) = targets::command_on_locator(
                            SshTrafficClass::Control,
                            &backend,
                            session_id,
                            vec![
                                "rm".into(),
                                "-f".into(),
                                "--".into(),
                                remote_archive.clone(),
                            ],
                            "remove consumed Move restore archive",
                        ) {
                            let _ = executor.execute_cleanup(&rm_cmd);
                        }
                    } else {
                        super::checkpoint::load_snapshot_pack_into_clone_cache_best_effort(
                            &backend,
                            restored_archive,
                            restoring,
                        );
                    }
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let trimmed = stdout.trim();
                    if !trimmed.is_empty() {
                        #[derive(serde::Deserialize)]
                        struct RestoreCommandOutput {
                            #[serde(default)]
                            notices: Vec<String>,
                        }
                        if let Ok(parsed) = serde_json::from_str::<RestoreCommandOutput>(trimmed) {
                            resume_notices.extend(parsed.notices);
                        }
                    }
                }
                if let Some(transfer) = move_transfer.as_mut() {
                    transfer.operation.phase = mj_core::state::MovePhase::DestinationStarting;
                    transfer.operation.updated_at = now();
                    crate::database::save_move_operation(transfer.operation)?;
                }
                if should_install_attached_resources {
                    let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                    install_attached_resources(
                        &self.state,
                        session_id,
                        &backend,
                        &worker_root,
                        syncing,
                    )?;
                }
                staged_aws_profile
            };
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
            // Restore imports only the archived delta; its origin configuration
            // is removed and the cutoff checked before any worker can run.
            self.verify_history_cutoff_workspace(session_id, &backend, executor)?;
            let readiness_stage = bridge_readiness_stage(profile);
            let spec = self.reconnect_command(session_id)?;
            let readiness = async {
                let mut relay = {
                    let _starting = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
                    start_worker_durably_with_aws_profile(
                        &crate::worker_lifecycle::require(session_id)?,
                        self.state.sessions[session_id]
                            .target
                            .as_ref()
                            .context("worker start has no durable target")?,
                        executor,
                        &backend,
                        &worker_root,
                        &self.config.github,
                        Some(staged_aws_profile.as_deref()),
                    )?;
                    connect_started_worker(&spec, session_id, executor, &backend, &worker_root)
                        .await?
                };
                if reopen_move_subagents {
                    // The worker builds its sub-agent queue while it prepares the
                    // harness and refuses admission changes until then. Reopen
                    // before the native session exists, so a prompt the restored
                    // relay starts on its own cannot find admission closed.
                    wait_for_harness_prepared_in_stage(
                        &mut relay,
                        executor,
                        readiness_stage.clone(),
                        profile.kind,
                    )
                    .await?;
                    relay
                        .set_subagent_admission(true)
                        .await
                        .context("reopen sub-agent requests on the replacement worker")?;
                }
                if include_in_place_subagents {
                    let session_id = session_id.to_owned();
                    let (context, _) = mj_core::runtime::spawn_blocking(move || {
                        load_in_place_subagent_prompt_data(&session_id)
                    })
                    .await
                    .context("join the retained sub-agent roster read")??;
                    prompt_contexts.extend(context);
                }
                // Installed before the harness is ready, so a queued prompt the
                // restored relay starts on its own cannot claim the hidden
                // context first. The relay hands it to one prompt only.
                if !prompt_contexts.is_empty() {
                    relay
                        .install_prompt_context(prompt_contexts.join("\n\n"))
                        .await
                        .context("tell the resumed session about its sub-agents")?;
                }
                let native_session_id = wait_for_native_session_in_stage(
                    &mut relay,
                    executor,
                    readiness_stage,
                    profile.kind,
                )
                .await?;
                // Keep the boot owner until Running is durable. The restored
                // relay can already accept queued work before publication.
                Ok::<_, anyhow::Error>((relay, native_session_id))
            }
            .await;
            let (mut relay, native_session_id) = readiness
                .map_err(|error| worker_probe_diagnosis(executor, &backend, &worker_root, error))?;
            if native_continuity {
                // With nothing to load, the harness opened a new session and
                // any identity it reports is the session's from now on.
                if let Some(archived) = archive.native_session_to_load(native_continuity)
                    && !restored_native_session_accepted(
                        archived,
                        &native_session_id,
                        relay
                            .operational()
                            .replaced_unused_native_session_id
                            .as_deref(),
                    )
                {
                    bail!("ACP loaded native session {native_session_id}, expected {archived}");
                }
            } else {
                let handoff = utility_handoff
                    .clone()
                    .context("a resume into a fresh native session has no handoff")?;
                relay.install_prompt_context(handoff).await?;
                if replay_queue {
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
                // Mail waited for the handoff too, and joins the queue behind
                // what was already queued. A Move admits it with its queue once
                // it owns the destination.
                if queue_policy_override == QueuePolicy::Import && move_destination.is_none() {
                    for (command_id, command) in deferred_mail_commands(canonical_session) {
                        relay.submit(command_id, command).await?;
                    }
                }
            }
            // Last, and only once the resume has otherwise succeeded: a failure
            // before this point rolls the record back to a session whose
            // worktree still has to be there.
            if let Some(environment) = retire_after_ready
                && let Some(worktree) = &environment.placement.managed_worktree
                && let Err(error) = super::environment::retire_owned_environment(
                    environment.clone(),
                    &self.config,
                    executor,
                    super::BranchDisposition::Keep,
                    super::CheckoutDisposition::Remove,
                )
            {
                tracing::warn!(
                    session_id,
                    worktree = %worktree.worktree_root.display(),
                    error = format!("{error:#}"),
                    "could not retire the old managed worktree after resume"
                );
                resume_notices.push(worktree_cleanup_notice(&worktree.worktree_root, &error));
            }
            if include_in_place_subagents {
                let session_id = session_id.to_owned();
                let (_, notice) = mj_core::runtime::spawn_blocking(move || {
                    load_in_place_subagent_prompt_data(&session_id)
                })
                .await
                .context("join the retained sub-agent notice roster read")??;
                resume_notices.extend(notice);
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
            let owner = crate::worker_lifecycle::require(session_id)?;
            crate::database::finish_worker_restart(session_id, owner.operation_id())?;
            let materialized = relay.sync().await?.materialized;
            if !stopped_subagents.is_empty() {
                let delivered = stopped_subagents
                    .iter()
                    .map(|child| child.child_session_id.clone())
                    .collect::<Vec<_>>();
                // The relay owns the note now. Failing to forget the list only
                // means a later resume tells the model again.
                if let Err(error) = crate::database::clear_stopped_subagents(session_id, &delivered)
                {
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "could not clear the stopped sub-agents after telling the resumed session"
                    );
                }
            }
            Ok(materialized)
        })
        .await
    }
}

/// One checkpoint archive that has been read end to end and proved to be the
/// one the session record names.
///
/// The canonical snapshot is shared behind an `Arc`: on a long session it is
/// tens of megabytes, and a resume reads it from several places that would
/// otherwise hold private copies.
pub(super) struct VerifiedResumeArchive {
    /// The archive's absolute, canonical path. A LocalBare worker shares the
    /// controller's filesystem, so it can consume this file directly instead of
    /// receiving a second large copy in its worker root.
    pub archive_path: PathBuf,
    pub manifest: mj_checkpoint::archive::ArchiveManifest,
    pub canonical_session: Arc<CanonicalSessionSnapshot>,
}

impl VerifiedResumeArchive {
    /// The native session a restore from this archive asks its harness to
    /// load: the archived one, when the restore continues the same harness and
    /// the archive holds that session's conversation.
    ///
    /// The archive's inventory decides, not its transcript. A session that
    /// never received a prompt can have no native files at all, and Codex
    /// cannot load a thread it never wrote, so the restored harness opens a
    /// new native session instead (#1618).
    pub(super) fn native_session_to_load(&self, native_continuity: bool) -> Option<&str> {
        (native_continuity
            && mj_checkpoint::checkpoint::archive_holds_native_conversation(&self.manifest))
        .then_some(self.manifest.session.native_session_id.as_str())
    }
}

fn load_in_place_subagent_prompt_data(
    session_id: &str,
) -> Result<(Option<String>, Option<String>)> {
    let state = crate::database::load_state()
        .context("load current retained sub-agents for the in-place resume")?;
    let children = super::move_session::move_children(&state, session_id);
    Ok((
        mj_core::subagent::in_place_subagents_prompt_context(&children),
        mj_core::subagent::in_place_subagents_notice(&children),
    ))
}

/// Read and verify the archive a stopped session will be restored from.
///
/// Canonicalizing first keeps one exact absolute path for the restore. The
/// digest and session id must both match the record, and a legacy host-bridge
/// archive is refused here rather than part way through a restore.
pub(super) fn verify_resume_checkpoint(
    session_id: &str,
    checkpoint: &mj_core::state::CheckpointMetadata,
) -> Result<VerifiedResumeArchive> {
    verify_resume_checkpoint_unless(session_id, checkpoint, None)
}

/// [`verify_resume_checkpoint`] that stops reading the archive once `cancel`
/// returns true, for work that runs for a client that can leave.
pub(super) fn verify_resume_checkpoint_unless(
    session_id: &str,
    checkpoint: &mj_core::state::CheckpointMetadata,
    cancel: Option<&mj_checkpoint::archive::ArchivePurgeCancel>,
) -> Result<VerifiedResumeArchive> {
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
    let mj_checkpoint::archive::VerifiedArchiveMetadata {
        manifest,
        canonical_session,
        archive_sha256,
    } = {
        let _phase = ResumePhaseTimer::new(session_id, "verify checkpoint archive contents");
        match cancel {
            Some(cancel) => {
                mj_checkpoint::archive::verify_archive_streaming_unless(&archive_path, cancel)?
            }
            None => verify_archive_streaming(&archive_path)?,
        }
    };
    if archive_sha256 != checkpoint.sha256 || manifest.session.id != session_id {
        bail!("persisted checkpoint verification failed");
    }
    ensure!(
        manifest.repositories.iter().all(|repository| {
            !repository.metadata.origin.starts_with("mj-local:")
                && !repository.metadata.origin.starts_with("ext::")
        }),
        "resuming legacy host-bridge sessions is not supported; start a new network-backed session"
    );
    Ok(VerifiedResumeArchive {
        archive_path,
        manifest,
        canonical_session: Arc::new(canonical_session),
    })
}

pub fn raw_conversion_preview_for(
    session: &SessionRecord,
    checkout: &mj_core::state::Checkout<'_>,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
) -> Result<mj_core::state::RawConversionPreview> {
    let conversion = plan_raw_to_workspace_for_session(session, checkout, config, executor)?;
    raw_conversion_preview_with_checkout(session, &conversion, executor)
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
        let source = mj_core::remote_git::project_source(replacement, mj_core::remote_git::Shorthand::Allowed)
            .context("origin must be a Git URL, GitHub owner/repository, or an absolute local repository path")?;
        if matches!(source, std::borrow::Cow::Owned(_)) {
            tracing::warn!("{}", mj_core::remote_git::HTTP_USERINFO_REMOVED);
        }
        (Some(source.into_owned()), None)
    };
    Ok(ProjectRepository {
        id: id.to_owned(),
        github,
        local,
        destination: PathBuf::from(id),
        git_ref: None,
        mbx_namespace: None,
    })
}

fn checkpoint_source_missing_commit(
    configured: &ProjectRepository,
    accepted_source: Option<&mj_core::remote_git::NetworkGitSource>,
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
        let output = fetch_source_commit(
            executor,
            &repository,
            configured,
            accepted_source,
            &commit,
            github_token,
        )?;
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
    accepted_source: Option<&mj_core::remote_git::NetworkGitSource>,
    commit: &str,
    github_token: Option<&str>,
) -> Result<CommandOutput> {
    let mut arguments = Vec::new();
    let mut token_auth = false;
    let source = if let Some(source) = accepted_source {
        source.fetch_url.clone()
    } else if let Some(local) = &configured.local {
        local.to_string_lossy().into_owned()
    } else {
        let source = configured
            .github
            .as_deref()
            .context("repository source is missing")?;
        let expanded = mj_core::remote_git::expand_github_source(source)?;
        if let Some(github) = mj_core::remote_git::parse_github_remote(
            source,
            mj_core::remote_git::Shorthand::Allowed,
        )
        .filter(|github| github.is_default_endpoint())
        {
            if github_token.is_some() {
                token_auth = true;
                arguments.extend(mj_core::remote_git::github_token_credential_config());
                format!(
                    "https://github.com/{}/{}.git",
                    github.owner, github.repository
                )
            } else if github.spelling == mj_core::remote_git::GithubSpelling::Shorthand {
                format!("git@github.com:{}/{}.git", github.owner, github.repository)
            } else {
                expanded
            }
        } else {
            expanded
        }
    };
    if accepted_source.is_some() && github_token.is_some() && is_github_https_url(&source) {
        token_auth = true;
        arguments.extend(mj_core::remote_git::github_token_credential_config());
    }
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
    mj_core::local_git::unattended_git_environment(&mut command);
    if token_auth {
        let token = github_token.expect("token authentication requires a GitHub token");
        command.env.insert("GH_TOKEN".to_owned(), token.to_owned());
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
        self.resume_session_controlled_with_repository_preflight_and_admission(
            session_id,
            profile_id,
            target_id,
            options,
            repository_preflight,
            executor,
            || {},
        )
        .await
    }

    // Keep the established resume arguments together with its admission hook.
    #[allow(clippy::too_many_arguments)]
    pub async fn resume_session_controlled_with_repository_preflight_and_admission(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_id: &str,
        options: SessionResumeOptions,
        repository_preflight: Option<ResumeRepositorySourceReceipt>,
        executor: &(impl CommandExecutor + Sync),
        on_accepted: impl FnOnce() + Send,
    ) -> Result<MaterializedSession> {
        self.resume_session_with_origin(
            session_id,
            profile_id,
            target_id,
            options,
            repository_preflight,
            None,
            executor,
            on_accepted,
        )
        .await
    }

    pub(super) async fn resume_session_for_cross_target_move(
        &mut self,
        operation: &mut mj_core::state::MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        let session_id = operation.selection.session_id.clone();
        let profile_id = operation
            .selection
            .profile_id
            .clone()
            .context("move profile is unresolved")?;
        let target_id = operation
            .selection
            .target_template_id
            .clone()
            .context("move target is unresolved")?;
        self.resume_session_with_origin(
            &session_id,
            &profile_id,
            &target_id,
            SessionResumeOptions {
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: true,
            },
            None,
            Some(operation),
            executor,
            || {},
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn resume_session_with_origin(
        &mut self,
        session_id: &str,
        profile_id: &str,
        target_id: &str,
        options: SessionResumeOptions,
        repository_preflight: Option<ResumeRepositorySourceReceipt>,
        mut move_operation: Option<&mut mj_core::state::MoveOperation>,
        executor: &(impl CommandExecutor + Sync),
        on_accepted: impl FnOnce() + Send,
    ) -> Result<MaterializedSession> {
        crate::worker_lifecycle::run(session_id, "resume session with origin", executor, async {
            crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
        let transferring_workspace = move_operation.is_some();
        if let Some(operation) = crate::database::load_move_operation(session_id)? {
            ensure!(
                transferring_workspace || !operation.holds_source_environment(),
                "a profile switch retains this environment; retry Move instead of recreating it with Resume"
            );
        }
        let SessionResumeOptions {
            additional_mounts,
            resource_allocation,
            discard_queue,
        } = options;
        let mut previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let previous_checkout = self.state.checkout(session_id)?;
        let previous_project_directory = previous_checkout
            .project_directory()
            .map(|path| path.to_path_buf());
        let mut previous_managed_worktree = match &previous_checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => Some((*worktree).clone()),
            _ => None,
        };
        drop(previous_checkout);
        if !(matches!(
            previous.state,
            SessionState::Stopped | SessionState::Lost | SessionState::Error
        ) || (transferring_workspace && previous.state == SessionState::Closing))
        {
            bail!("session {session_id} is not stopped, lost, or retryable");
        }
        if !transferring_workspace && previous.state == SessionState::Error && previous.target.is_some() {
            use super::environment::{ResumeEnvironment, classify_resume_environment};
            let condition = classify_resume_environment(self, session_id, executor);
            if !matches!(&condition, ResumeEnvironment::Absent | ResumeEnvironment::Unknown(_)) {
                ensure!(profile_id == previous.last_profile && target_id == previous.target_template_id,
                    "Resume repairs the existing environment; resume with its current profile and target, then use Move to change them");
                if additional_mounts.as_ref().is_some_and(|mounts| mounts != &previous.additional_mounts)
                    || resource_allocation.as_ref().is_some_and(|allocation| Some(allocation) != previous.resource_allocation.as_ref()) {
                    executor.notify_notice("Resume keeps the existing container resources and attached directories. Use Move after repair to change them.");
                }
            }
            match condition {
                condition @ (ResumeEnvironment::LiveWorker | ResumeEnvironment::WorkerMissing) => {
                    on_accepted();
                    return self.repair_existing_environment(session_id, if matches!(condition, ResumeEnvironment::LiveWorker) { RetainedWorker::Live } else { RetainedWorker::Missing }, discard_queue, executor).await;
                }
                ResumeEnvironment::ParentDataMissing => {
                    on_accepted();
                    return self.recover_missing_parent_data(session_id, discard_queue, executor).await;
                }
                ResumeEnvironment::RetainedContainerMissing => {
                    on_accepted();
                    self.recover_removed_resume_container(session_id, discard_queue, executor).await?;
                    return crate::database::load_materialized_session(session_id)?.context("restored parent projection is missing");
                }
                ResumeEnvironment::CheckoutMissing => {
                    on_accepted();
                    return self.recover_missing_checkout(session_id, discard_queue, executor).await;
                }
                ResumeEnvironment::Unknown(error) => {
                    bail!("Could not establish the existing environment for session {session_id}: {error:#}. Everything was kept. Retry Resume when the target is reachable.");
                }
                ResumeEnvironment::Absent => {
                    if previous.checkpoint.is_none() {
                        let record = self.state.sessions.get_mut(session_id).context("session disappeared")?;
                        // Presentation loses the absent target; the environment
                        // ledger retains all physical ownership for explicit Destroy.
                        record.target = None;
                        record.state = SessionState::Lost;
                        record.updated_at = now();
                        let reason = format!("The target is confirmed absent and this session has no checkpoint. Run `mj destroy --session {session_id}` to clean its remaining resources, or restore another recovery archive.");
                        record.last_error = Some(reason.clone());
                        crate::database::save_lifecycle_session(record)?;
                        bail!("{reason}");
                    }
                },
            }
        }
        // For a direct cross-target Move whose archive remains on the source
        // target, capture source placement & keep directory BEFORE the session
        // record is updated for the destination.
        let direct_move_source = if let Some(operation) = move_operation.as_deref()
            && operation.source_snapshot.is_some()
        {
            let source_locator = previous
                .target
                .as_deref()
                .or(operation.source_target.as_ref())
                .context("cross-target Move source target is missing")?;
            let source_backend = backend_locator(source_locator, &previous, &self.config)?;
            let source_worker_root = targets::worker_root(&source_backend, session_id)?;
            let source_keep_dir = Some(self.move_source_keep_dir(
                &previous,
                &source_backend,
                executor,
            )?);
            Some((source_backend, source_worker_root, source_keep_dir))
        } else {
            None
        };
        let verified_archive = if let Some(snapshot) = move_operation
            .as_deref()
            .and_then(|op| op.source_snapshot.as_deref())
        {
            let manifest = snapshot
                .manifest
                .clone()
                .context("cross-target Move snapshot is missing its archive manifest")?;
            let materialized = crate::database::load_materialized_session(session_id)?
                .context("cross-target Move session projection is missing")?;
            let mut canonical =
                mj_transcript::projection::canonical_session_from_materialized_owned(materialized)?;
            canonical.queued_prompts = snapshot.queued_prompts.clone();
            canonical.mailbox = snapshot.mailbox.clone();
            VerifiedResumeArchive {
                archive_path: snapshot.archive_path.clone(),
                manifest,
                canonical_session: Arc::new(canonical),
            }
        } else {
            let checkpoint = move_operation
                .as_deref()
                .and_then(|op| op.checkpoint.as_ref())
                .or(previous.checkpoint.as_deref())
                .context("session has no checkpoint")?;
            // A receipt for an isolated destination says nothing about a host
            // checkout. Explicit moves to raw execution must check that source.
            let moving_to_raw = previous_project_directory.is_none()
                && self
                    .config
                    .targets
                    .get(target_id)
                    .is_some_and(mj_core::config::is_bare_project_target);
            if !transferring_workspace
                && (moving_to_raw
                    || !repository_preflight.as_ref().is_some_and(|receipt| {
                        self.repository_source_receipt_is_current(session_id, receipt)
                    }))
            {
                let _phase = ResumePhaseTimer::new(session_id, "preflight repository sources");
                if let ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) =
                    self.preflight_repository_sources(
                        session_id,
                        target_id,
                        false,
                        self.github_token_for_repository_preflight(session_id)
                            .await?
                            .as_deref(),
                        executor,
                    )?
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
            verify_resume_checkpoint(session_id, checkpoint)?
        };
        let archive_path = verified_archive.archive_path.clone();
        let archive_manifest = &verified_archive.manifest;
        let canonical_session = Arc::clone(&verified_archive.canonical_session);
        let profile = self
            .config
            .named_profile(profile_id)?
            .clone();
        let target_template = self.config.ensure_target_enabled(target_id)?.clone();
        // Decide the representation before the record changes, so an
        // incompatible target fails here instead of during provisioning.
        self.validate_muse_resume_destination(&previous, profile.kind, target_id)?;
        let checkout = self.state.checkout(session_id)?;
        let plan = resume_compatibility_with_checkout(
            &previous,
            &checkout,
            &self.config,
            target_id,
        )
            .map_err(|reason| anyhow::anyhow!("{reason}"))?;
        let is_attached_checkout =
            matches!(checkout.effective(), mj_core::state::Checkout::Attached { .. });
        let is_history_cutoff_origin_delta =
            super::network_git::manifest_has_history_cutoff_origin_delta(archive_manifest)
                && super::backend::session_history_cutoff(&previous)?.is_some();
        let history_only_archive = archive_manifest
            .repositories
            .iter()
            .any(|repository| repository.metadata.review_refs_metadata_only)
            && archive_manifest.repositories.iter().all(|repository| {
                repository.metadata.review_refs_metadata_only || repository.metadata.plain_directory
            });
        let origin_delta = manifest_uses_origin_delta(archive_manifest);
        if origin_delta && plan != ResumePlan::InPlace {
            let originless = archive_manifest.repositories.iter().any(|repository| {
                repository
                    .metadata
                    .origin_delta
                    .as_ref()
                    .is_some_and(|delta| !delta.history_cutoff && delta.origin_url.is_none())
            });
            if originless {
                return Err(mj_core::refusal::Refusal::precondition(
                    "This raw session has no Git origin remote and can only be resumed on its original target.",
                )
                .into());
            }
            if is_history_cutoff_origin_delta && plan == ResumePlan::WorkspaceToRaw {
                return Err(mj_core::refusal::Refusal::precondition(
                    "A history-cutoff session cannot be resumed into a bare checkout.",
                )
                .into());
            }
            if plan == ResumePlan::WorkspaceToRaw
                && !matches!(target_template, mj_core::config::TargetTemplate::LocalBare)
            {
                return Err(mj_core::refusal::Refusal::precondition(
                    "Resuming an OriginDelta checkpoint onto an SSH bare target is not supported yet; resume on a local bare or container target.",
                )
                .into());
            }
        }
        if !origin_delta
            && !history_only_archive
            && !transferring_workspace
            && plan == ResumePlan::RawToWorkspace
            && let Some(worktree) = previous_managed_worktree.as_ref()
        {
            if managed_worktree_checkout_exists(executor, worktree)? {
                return Err(mj_core::refusal::Refusal::precondition(
                    "This older session still has a local managed checkout on disk; resume it on its original target first, then suspend or move it.",
                )
                .into());
            }
            let token = self
                .github_token_for_repository_preflight(session_id)
                .await?;
            ensure_legacy_worktree_cross_target_base_on_origin(
                &previous,
                worktree,
                &archive_path,
                archive_manifest,
                executor,
                token.as_deref(),
            )?;
        }
        // A worktree that stays put is reached with the machine's current ssh
        // options; only its location had to match (J-21).
        if plan == ResumePlan::InPlace
            && previous_managed_worktree.is_some()
            && let Some(worktree) = previous.managed_worktree.as_mut()
            && let Ok(current) = managed_worktree_target(&target_template)
        {
            worktree.target = current;
            let refreshed_checkout = self.state.checkout_for_record(session_id, &previous)?;
            previous_managed_worktree = refreshed_checkout.effective().managed_worktree().cloned();
        }
        let mut upgraded_legacy_worktree: Option<mj_core::state::ManagedWorktree> = None;
        if !origin_delta
            && !transferring_workspace
            && plan == ResumePlan::InPlace
            && !archive_manifest.repositories.is_empty()
            && let Some(worktree) = previous_managed_worktree.as_ref()
            && worktree.kind == mj_core::state::ManagedCheckoutKind::Worktree
            && matches!(
                &worktree.target,
                mj_core::state::ManagedWorktreeTarget::Local
            )
            && !managed_worktree_checkout_exists(executor, worktree)?
        {
            let upgraded_clone = upgrade_legacy_worktree_to_clone(worktree, session_id);
            ensure_managed_worktree_available(executor, &upgraded_clone)?;
            upgraded_legacy_worktree = Some(upgraded_clone);
        }
        // A converting resume writes its own archive below, from the accepted
        // repository's selected network source, and provisioning reads that
        // one. Every other isolated resume clones what its stored archive names.
        if !mj_core::config::is_bare_project_target(&target_template)
            && plan != ResumePlan::RawToWorkspace
            && !is_history_cutoff_origin_delta
        {
            super::network_git::bundle_from_manifest_for_session(
                archive_manifest,
                Some(&previous),
                Some(&self.config),
            )?;
        }
        if plan == ResumePlan::InPlace
            && matches!(checkout.effective(), mj_core::state::Checkout::Attached { .. })
            && let Some(project_directory) = checkout.project_directory()
        {
            self.validate_existing_project_directory(&previous, target_id, &project_directory, executor)
                .context("raw project is unavailable for resume")?;
        }
        let conversion = match plan {
            ResumePlan::InPlace => None,
            ResumePlan::RawToWorkspace => Some(ResumeConversion::RawToWorkspace(Box::new(
                if origin_delta {
                    plan_raw_to_workspace_from_origin_delta(
                        &previous,
                        archive_manifest,
                        &self.config,
                        executor,
                    )
                } else {
                    plan_raw_to_workspace_for_session(&previous, &checkout, &self.config, executor)
                }
                .context("prepare the raw checkout for its new target")?,
            ))),
            ResumePlan::WorkspaceToRaw => Some(ResumeConversion::WorkspaceToRaw(
                self.plan_workspace_to_raw(&previous, target_id, origin_delta, executor)
                    .context("prepare a checkout for this session")?,
            )),
        };
        let requests = if move_operation.is_none() {
            SessionResumeOptions { additional_mounts: additional_mounts.clone(), resource_allocation: resource_allocation.clone(), discard_queue }
        } else { SessionResumeOptions { additional_mounts: None, resource_allocation: None, discard_queue } };
        let resource_allocation =
            resource_allocation.or_else(|| previous.resource_allocation.clone());
        let additional_mounts =
            additional_mounts.unwrap_or_else(|| previous.additional_mounts.clone());
        ensure!(
            profile.kind != HarnessKind::Muse || additional_mounts.is_empty(),
            "{} ACP supports one workspace root; attached directories are unsupported",
            profile.kind.display_name()
        );
        validate_resource_allocation(&target_template, resource_allocation.as_ref())?;
        let selected_container_size =
            selected_host_container_size(&target_template, resource_allocation.as_ref());
        if !additional_mounts.is_empty() && mount_history_host(&target_template).is_none() {
            bail!("attached resources are unsupported for this target");
        }
        targets::validate_additional_mounts(&additional_mounts)?;
        let history_host = mount_history_host(&target_template);
        let history_mounts = additional_mounts.clone();
        let mut resume_notices = Vec::new();
        if history_only_archive && !is_attached_checkout {
            resume_notices.push("Resumed from a history-only checkpoint into a fresh workspace checkout; prior workspace Git changes were not saved.".to_owned());
        }
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
        let mut source_environment = crate::database::session_environment(session_id)?;
        if previous_managed_worktree.is_some() {
            let mut physical_source = previous.clone();
            physical_source.managed_worktree = previous_managed_worktree.clone().map(Box::new);
            source_environment = Some(crate::database::reserve_owned_checkout(&physical_source)?);
        }
        let managed_checkout_present = previous_managed_worktree
            .as_ref()
            .map(|worktree| managed_worktree_checkout_exists(executor, worktree))
            .transpose()?
            .unwrap_or(true);
        // A checkout Mjolnir did not retire remains the truth for a raw session.
        // A retired checkout is recreated from the branch and archive below.
        if managed_checkout_present
            && !(origin_delta && plan == ResumePlan::RawToWorkspace)
            && let Some(project_directory) = previous_project_directory.as_deref()
        {
            let checkout = self.state.checkout(session_id)?;
            match raw_checkout_position_with_checkout(
                &previous,
                &checkout,
                &self.config,
                project_directory,
                executor,
            ) {
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
        // Resolve the worker before compaction costs minutes and paid model
        // requests, including probing the platform of an existing SSH host. A resume
        // that could never install a worker fails here rather than after all
        // that work has been thrown away.
        let worker_target = super::backend::session_target(&self.config, &previous.bundle_id, target_id, executor)?;
        super::worker_binary::preflight_worker_binary(&worker_target, executor)?;
        let native_continuity = self.config.native_continuity(
            archive_manifest.session.harness_kind,
            &archive_manifest.session.profile_id,
            profile_id,
        )?;
        let context_bytes = crate::handoff::profile_handoff_bytes(Some(&profile));
        // Cross-harness compaction is started alongside destination
        // provisioning below. Clone only the configuration it reads so the
        // controller can continue owning and mutating its session record.
        let utility_config = (!native_continuity).then(|| self.config.clone());
        let native_prior = if utility_config.is_some() {
            if let Some((source_backend, _, _)) = &direct_move_source {
                native_prior_from_target_while_cancellable(
                    session_id,
                    source_backend,
                    archive_manifest.session.harness_kind,
                    Some(archive_manifest.session.native_session_id.as_str()),
                    &verified_archive.canonical_session,
                    executor,
                )
                .await?
            } else {
                native_prior_while_cancellable(session_id, &verified_archive, executor).await?
            }
        } else {
            None
        };
        let discard_queued_prompts = discard_queue || !native_continuity;
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
            mj_core::runtime::spawn_blocking(move || {
                materialized_session_from_canonical(session_id, &canonical)
            })
        });
        let github_token = self.github_token_for_session(session_id).await?;

        // A session that leaves a non-container target for a container one is
        // built a container it never had, so it starts using the per-session
        // workspace path here if it predates them. A session that already ran
        // in a container keeps its path: the harness's recorded working
        // directory has to survive the resume.
        let moving_into_first_container = self
            .config
            .targets
            .get(target_id)
            .is_some_and(mj_core::config::is_container_target)
            && !self
                .config
                .targets
                .get(&previous.target_template_id)
                .is_some_and(mj_core::config::is_container_target);
        let upgraded_origin_delta_clone = if origin_delta && plan == ResumePlan::InPlace {
            if let Some(worktree) = previous_managed_worktree.as_ref().filter(|worktree| {
                worktree.kind == mj_core::state::ManagedCheckoutKind::Worktree
                    && matches!(worktree.target, mj_core::state::ManagedWorktreeTarget::Local)
            }) {
                let upgraded = upgrade_legacy_worktree_to_clone(worktree, session_id);
                ensure_managed_worktree_available(executor, &upgraded)?;
                Some(upgraded)
            } else {
                None
            }
        } else {
            None
        };
        let record = self.state.sessions.get_mut(session_id).unwrap();
        if record.container_workspace.is_none() && moving_into_first_container {
            record.container_workspace = Some(targets::new_container_workspace(session_id)?);
        }
        record.harness_kind = profile.kind;
        record.last_profile = profile_id.to_string();
        record.target_template_id = target_id.to_string();
        record.target_runtime = Some(
            self.config
                .targets
                .get(target_id)
                .context("resume target disappeared")?
                .into(),
        );
        record.machine_id = self.config.template_machine_id(target_id);
        record.resource_allocation = resource_allocation;
        record.additional_mounts = additional_mounts;
        record.target = None;
        record.native_session_id = verified_archive
            .native_session_to_load(native_continuity)
            .map(str::to_owned);
        record.state = SessionState::Provisioning;
        record.updated_at = now();
        record.last_error = None;
        match &conversion {
            Some(ResumeConversion::RawToWorkspace(conversion)) => {
                apply_raw_to_workspace(record, conversion)?;
            }
            Some(ResumeConversion::WorkspaceToRaw(conversion)) => {
                apply_workspace_to_raw(record, conversion);
            }
            None => {
                if let Some(upgraded) = upgraded_origin_delta_clone
                    .as_ref()
                    .or(upgraded_legacy_worktree.as_ref())
                {
                    // The clone's path derives from its descriptor.
                    record.managed_worktree = Some(Box::new(upgraded.clone()));
                    record.project_directory = None;
                } else if let (Some(worktree), Some(refreshed)) = (
                    record.managed_worktree.as_mut(),
                    previous_managed_worktree.as_ref(),
                ) {
                    worktree.target = refreshed.target.clone();
                }
            }
        }
        let resumed_project_directory = record
            .checkout()
            .project_directory()
            .map(|path| path.to_path_buf());
        let resumed_container_workspace = record.container_workspace.clone();
        if let Some(host) = history_host {
            self.state.remember_mount_sources(host, &history_mounts);
            crate::database::remember_mount_sources(host, &history_mounts)?;
        }
        // Resume changes target resources, while titles and drafts remain
        // owned by their independent writers throughout provisioning.
        crate::database::accept_resumed_session(
            &self.state.sessions[session_id],
            selected_container_size.as_ref().map(|(host, size)| (host.as_str(), *size)),
            &requests,
        )?;
        on_accepted();
        if let Some((host, size)) = selected_container_size.as_ref() {
            self.state.remember_container_size(host, *size);
        }

        let mut recreated_managed_worktree = false;
        // Ephemeral temp directory holding an origin-delta archive for legacy
        // raw-to-workspace conversion (deleted when this resume finishes; never
        // replaces `record.checkpoint`).
        let mut ephemeral_conversion_archive: Option<(tempfile::TempDir, PathBuf)> = None;
        let mut direct_already_transferred = false;
        let mut remote_origin_delta_clone_restore = false;
        let result = async {
            if origin_delta {
                if is_history_cutoff_origin_delta || plan == ResumePlan::RawToWorkspace {
                    // Provisioning clones from the origin/bundle; restore_into_target
                    // then applies the origin-delta snapshot into the workspace.
                } else if plan == ResumePlan::WorkspaceToRaw {
                    // Handled in the WorkspaceToRaw conversion block below.
                } else if let Some(upgraded) = upgraded_origin_delta_clone.as_ref() {
                    match super::worktree::restore_managed_clone_origin_delta(
                        executor, upgraded, &archive_path, archive_manifest,
                    )? {
                        super::worktree::ManagedCloneRestoreOutcome::RestoredLocally(notices) => {
                            resume_notices.extend(notices);
                        }
                        super::worktree::ManagedCloneRestoreOutcome::SeededForRemoteRestore => {
                            remote_origin_delta_clone_restore = true;
                        }
                    }
                } else if let Some(worktree) = previous_managed_worktree.as_ref() {
                    match super::worktree::restore_managed_clone_origin_delta(
                        executor, worktree, &archive_path, archive_manifest,
                    )? {
                        super::worktree::ManagedCloneRestoreOutcome::RestoredLocally(notices) => {
                            resume_notices.extend(notices);
                        }
                        super::worktree::ManagedCloneRestoreOutcome::SeededForRemoteRestore => {
                            remote_origin_delta_clone_restore = true;
                        }
                    }
                    recreated_managed_worktree = true;
                } else if is_attached_checkout {
                    // Raw checkout resuming in place: the existing checkout is authoritative.
                } else {
                    // ManagedWorkspace resuming in place: provisioning clones from origin;
                    // restore_into_target applies the origin-delta snapshot.
                }
            } else if let Some(upgraded_clone) = upgraded_legacy_worktree.as_ref() {
                if history_only_archive {
                    if let Some(fresh_head) =
                        super::worktree::restore_managed_clone_fresh_head(executor, upgraded_clone)?
                    {
                        self.record_fresh_managed_clone_head(session_id, fresh_head)?;
                        recreated_managed_worktree = true;
                    }
                } else {
                    resume_notices.extend(restore_legacy_worktree_as_managed_clone(
                        executor,
                        upgraded_clone,
                        &archive_path,
                        archive_manifest,
                        None,
                    )?);
                }
            } else if let Some(worktree) = previous_managed_worktree.as_ref() {
                if !transferring_workspace
                    && plan == ResumePlan::RawToWorkspace
                    && !archive_manifest.repositories.is_empty()
                {
                    // Legacy managed checkout (Worktree or Clone) already
                    // retired: base commit was verified on origin before
                    // acceptance; provisioning clones origin and
                    // restore_into_target applies the original legacy archive
                    // directly (preserving archived untracked files & stashes).
                } else {
                    recreated_managed_worktree = if history_only_archive
                        && worktree.kind == mj_core::state::ManagedCheckoutKind::Clone
                    {
                        if let Some(fresh_head) =
                            super::worktree::restore_managed_clone_fresh_head(executor, worktree)?
                        {
                            self.record_fresh_managed_clone_head(session_id, fresh_head)?;
                            true
                        } else {
                            false
                        }
                    } else {
                        restore_managed_worktree(executor, worktree)?
                    };
                    if !transferring_workspace
                        && recreated_managed_worktree
                        && plan == ResumePlan::RawToWorkspace
                    {
                        if worktree.kind == mj_core::state::ManagedCheckoutKind::Clone {
                            resume_notices.extend(
                                mj_checkpoint::checkpoint::restore_single_repository_into_checkout(
                                    &archive_path,
                                    &worktree.worktree_root,
                                    &SystemGit,
                                )?,
                            );
                        } else {
                            resume_notices.extend(
                                mj_checkpoint::checkpoint::restore_single_repository_onto_branch(
                                    &archive_path,
                                    &worktree.worktree_root,
                                    &worktree.branch,
                                    &SystemGit,
                                )?,
                            );
                        }
                    }
                }
            }
            // The record already names the worktree, so a failure here rolls
            // back through the same path that cleans up a new session's.
            if let Some(conversion) = conversion
                .as_ref()
                .and_then(ResumeConversion::workspace_to_raw)
            {
                if origin_delta {
                    let mut received_local_keep = false;
                    let local_restore_archive = if matches!(
                        &conversion.worktree.target,
                        mj_core::state::ManagedWorktreeTarget::Local
                    ) && let (
                        Some(operation),
                        Some((source_backend, source_worker_root, source_keep_dir)),
                    ) = (move_operation.as_deref_mut(), direct_move_source.as_ref())
                    {
                        // Moving into LocalBare: stream the archive + `.mj/keep/`
                        // straight into the local destination worker root first so
                        // `restore_managed_clone_origin_delta` reads it locally.
                        let local_worker_root =
                            mj_core::config::DataLayout::current().worker_root(session_id);
                        self.reserve_provisioning_target(
                            session_id,
                            mj_core::state::TargetLocator::LocalBare {
                                worker_root: local_worker_root.clone(),
                            },
                        )?;
                        let (local_dest_locator, local_worker_root_str) =
                            self.worker_placement(session_id)?;
                        // This bootstrap makes receive-move-stream available. The
                        // final staging pass owns the profile published at cutover.
                        self.prepare_worker_files(
                            session_id,
                            &local_dest_locator,
                            &local_worker_root_str,
                            executor,
                            super::worker_binary::LaunchHistory::Restored(&canonical_session),
                        )?;
                        let local_archive = local_worker_root.join("restore.hel.zip");
                        // The clone path must stay absent until Git restore
                        // publishes it. Stage keep on the same filesystem.
                        let staged_keep =
                            super::worktree::managed_clone_move_keep_stage(&conversion.worktree);
                        operation.phase = mj_core::state::MovePhase::Transferring;
                        operation.updated_at = now();
                        crate::database::save_move_operation(operation)?;
                        let receipt = super::move_session::transfer_move_snapshot_to_destination(
                            operation,
                            source_backend,
                            source_worker_root,
                            &local_dest_locator,
                            &local_worker_root_str,
                            &local_archive,
                            source_keep_dir.as_deref(),
                            Some(&staged_keep),
                            executor,
                        )?;
                        received_local_keep = receipt.keep_bytes > 0;
                        direct_already_transferred = true;
                        local_archive
                    } else {
                        archive_path.clone()
                    };
                    match super::worktree::restore_managed_clone_origin_delta(
                        executor,
                        &conversion.worktree,
                        &local_restore_archive,
                        archive_manifest,
                    )? {
                        super::worktree::ManagedCloneRestoreOutcome::RestoredLocally(notices) => {
                            resume_notices.extend(notices);
                        }
                        super::worktree::ManagedCloneRestoreOutcome::SeededForRemoteRestore => {
                            remote_origin_delta_clone_restore = true;
                        }
                    }
                    if received_local_keep {
                        super::worktree::publish_managed_clone_move_keep(
                            &conversion.worktree,
                            session_id,
                        )?;
                    }
                } else {
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
                    if transferring_workspace {
                        // Move installs its separately verified Git/file transfer below.
                    } else if conversion.worktree.kind == mj_core::state::ManagedCheckoutKind::Clone {
                        resume_notices.extend(
                            mj_checkpoint::checkpoint::restore_single_repository_into_checkout(
                                &archive_path,
                                &conversion.worktree.worktree_root,
                                &SystemGit,
                            )?,
                        );
                    } else {
                        resume_notices.extend(
                            mj_checkpoint::checkpoint::restore_single_repository_onto_branch(
                                &archive_path,
                                &conversion.worktree.worktree_root,
                                &conversion.worktree.branch,
                                &SystemGit,
                            )?,
                        );
                    }
                }
            }
            // A legacy local raw checkout becomes an isolated workspace by
            // capturing an origin-delta snapshot directly from the untouched
            // raw checkout, without replacing `record.checkpoint`.
            if let Some(conversion) = conversion
                .as_ref()
                .and_then(ResumeConversion::raw_to_workspace)
                && !transferring_workspace
                && !origin_delta
                && previous_managed_worktree.is_none()
            {
                ensure!(
                    conversion.retire.is_none(),
                    "cannot run raw-checkout conversion when retiring a managed checkout"
                );
                let captured = capture_legacy_raw_checkout_snapshot(
                    &conversion.checkout,
                    &conversion.repository_id,
                    &conversion.source,
                    &conversion.destination,
                    &SystemGit,
                )
                .context("snapshot the host checkout for its new target")?;
                resume_notices.push(conversion_notice(
                    target_id,
                    previous_project_directory
                        .as_deref()
                        .unwrap_or(&conversion.checkout),
                    captured.repository.metadata.branch.as_deref(),
                    conversion.retire.as_ref(),
                ));
                if conversion.retire.is_none() && !captured.untracked_paths.is_empty() {
                    let list = captured
                        .untracked_paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    resume_notices.push(format!(
                        "Untracked files were not copied and remain in {}: {list}",
                        conversion.checkout.display()
                    ));
                }
                let temp_dir = tempfile::tempdir()
                    .context("create temporary directory for legacy raw conversion")?;
                let temp_archive = temp_dir.path().join("conversion.hel.zip");
                let ephemeral_meta =
                    conversion_checkpoint(&archive_path, captured.repository, &temp_archive)?;
                self.state.sessions.get_mut(session_id).unwrap().checkpoint =
                    Some(Box::new(ephemeral_meta));
                ephemeral_conversion_archive = Some((temp_dir, temp_archive));
            }
            let utility_handoff = {
                let _provisioning = ResumePhaseTimer::new(session_id, "provision destination");
                let move_manifest = transferring_workspace.then_some(archive_manifest);
                if let Some(config) = utility_config.as_ref() {
                    Some(
                        provision_with_cross_harness_handoff(
                            self,
                            session_id,
                            executor,
                            github_token.as_deref(),
                            config,
                            &canonical_session,
                            native_prior.as_ref(),
                            context_bytes,
                            move_manifest,
                        )
                        .context("prepare the cross-harness destination")?,
                    )
                } else {
                    self.provision_session_with_failure_disposition(
                        session_id,
                        executor,
                        github_token.as_deref(),
                        ProvisioningFailureDisposition::Preserve,
                        RepositorySeed::Restore(move_manifest),
                    )
                    .await?;
                    None
                }
            };
            if ephemeral_conversion_archive.is_some() {
                // Provisioning has read the ephemeral conversion metadata;
                // restore the durable checkpoint on the in-memory record
                // before restore_into_target or any later persistence runs.
                self.state.sessions.get_mut(session_id).unwrap().checkpoint =
                    previous.checkpoint.clone();
            }
            let restore_repositories = if origin_delta {
                is_history_cutoff_origin_delta
                    || plan == ResumePlan::RawToWorkspace
                    || remote_origin_delta_clone_restore
                    || (plan == ResumePlan::InPlace
                        && resumed_project_directory.is_none()
                        && previous_managed_worktree.is_none()
                        && !is_attached_checkout)
            } else {
                upgraded_legacy_worktree.is_none()
                    && ((resumed_project_directory.is_none() && conversion.is_none())
                        || plan == ResumePlan::RawToWorkspace
                        || (recreated_managed_worktree && plan == ResumePlan::InPlace))
            };
            // A legacy raw-to-workspace conversion restores the ephemeral
            // origin-delta archive captured from the untouched raw checkout.
            let restored_archive = ephemeral_conversion_archive
                .as_ref()
                .map_or(archive_path.as_path(), |(_, path)| path.as_path());
            let move_transfer = if let (
                Some(operation),
                Some((source_backend, source_worker_root, source_keep_dir)),
            ) = (move_operation.as_deref_mut(), direct_move_source)
            {
                let dest_keep_dir = conversion
                    .as_ref()
                    .and_then(ResumeConversion::workspace_to_raw)
                    .map(|conv| {
                        mj_checkpoint::checkpoint::session_keep_dir(
                            &conv.worktree.worktree_root,
                            session_id,
                        )
                    })
                    .transpose()?;
                Some(MoveDirectTransfer {
                    operation,
                    source_backend,
                    source_worker_root,
                    source_keep_dir,
                    dest_keep_dir,
                    already_transferred: direct_already_transferred,
                })
            } else {
                None
            };
            self.restore_into_target(
                session_id,
                RestoreIntoTarget {
                    profile: &profile,
                    archive: &verified_archive,
                    restored_archive,
                    resumed_project_directory,
                    resumed_container_workspace,
                    restore_repositories,
                    native_continuity,
                    discard_queued_prompts,
                    replay_queue: !discard_queue,
                    queue_policy_override: if transferring_workspace {
                        QueuePolicy::Suppress
                    } else {
                        QueuePolicy::Import
                    },
                    move_transfer,
                    utility_handoff,
                    projection_build,
                    resume_notices,
                    include_in_place_subagents: false,
                    install_attached_resources: true,
                    worker_root_reset: WorkerRootReset::FreshTarget,
                    retire_after_ready: conversion
                        .as_ref()
                        .filter(|_| !transferring_workspace)
                        .and_then(ResumeConversion::raw_to_workspace)
                        .and_then(|plan| plan.retire.as_ref())
                        .and(source_environment.as_ref()),
                },
                executor,
            )
            .await
        }
        .await;
        if ephemeral_conversion_archive.is_some()
            && let Some(record) = self.state.sessions.get_mut(session_id)
        {
            // Restore the durable checkpoint on every exit path (including a
            // provisioning error before the post-provision restore above) so
            // failure handlers never persist the deleted temp archive path.
            record.checkpoint = previous.checkpoint.clone();
        }
        drop(ephemeral_conversion_archive);
        match result {
            Ok(materialized) => {
                let latest = crate::database::load_session_record(session_id)?.context("resumed session disappeared")?;
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.checkpoint = latest.checkpoint;
                record.resource_allocation = latest.resource_allocation;
                record.additional_mounts = latest.additional_mounts;
                Ok(materialized)
            }
            Err(error) => {
                if move_operation.is_none() && self.resume_runtime_has_boot_owner(session_id)? {
                    return Err(self.keep_started_resume_after_failure(session_id, error)?);
                }
                // Put back whatever this resume wrote to the durable
                // projection, including the failed worker's own lines.
                restore_projection_after_failed_resume(
                    session_id,
                    &canonical_session,
                    discard_queued_prompts,
                );
                if let Some(operation) = move_operation {
                    super::move_session::recovery::cleanup_local_move_key_files(
                        &operation.granted_keys,
                    );
                }
                Err(self.rollback_failed_resume(
                    session_id,
                    &previous,
                    source_environment.as_ref(),
                    recreated_managed_worktree,
                    error,
                    executor,
                )?)
            }
        }

        }).await
    }

    fn resume_runtime_has_boot_owner(&self, session_id: &str) -> Result<bool> {
        let record = self
            .state
            .sessions
            .get(session_id)
            .context("resume session disappeared")?;
        Ok(record.state == SessionState::Running
            || crate::database::load_worker_restart(session_id)?
                .is_some_and(|intent| record.target.as_deref() == Some(&intent.target)))
    }

    pub(in crate::controller) fn record_fresh_managed_clone_head(
        &mut self,
        session_id: &str,
        fresh_head: String,
    ) -> Result<()> {
        let record = self
            .state
            .sessions
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let worktree = record
            .managed_worktree
            .as_mut()
            .context("fresh managed clone has no worktree record")?;
        ensure!(
            worktree.kind == mj_core::state::ManagedCheckoutKind::Clone,
            "fresh managed clone head belongs to a non-clone checkout"
        );
        worktree.base_commit = Some(fresh_head);
        // accept_resumed_session persisted the old checkout before the clone
        // was recreated; publish the new baseline in the same durable record.
        crate::database::save_resumed_session(record, None)
    }

    fn keep_started_resume_after_failure(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
    ) -> Result<anyhow::Error> {
        // The durable boot owner is admitted before launch. An unacknowledged
        // start can already be serving work, so its files are no longer a
        // disposable archive reconstruction, even before Running is published.
        let detail = format!(
            "Resume failed after parent startup: {}. The current environment and history were kept; retry Resume to reconnect it.",
            super::worker_binary::failure_line(&error)
        );
        let record = self
            .state
            .sessions
            .get_mut(session_id)
            .context("resume session disappeared")?;
        record.state = SessionState::Error;
        record.updated_at = now();
        record.last_error = Some(detail.clone());
        crate::database::save_lifecycle_session(record)?;
        Ok(error.context(detail))
    }

    pub fn rollback_failed_resume(
        &mut self,
        session_id: &str,
        previous: &SessionRecord,
        source_environment: Option<&crate::database::EnvironmentRecord>,
        recreated_managed_worktree: bool,
        error: anyhow::Error,
        _executor: &impl CommandExecutor,
    ) -> Result<anyhow::Error> {
        if self.resume_runtime_has_boot_owner(session_id)? {
            return self.keep_started_resume_after_failure(session_id, error);
        }
        let current = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        // An environment that still has its original runtime is not owned by
        // this attempt. A new generation or a previously empty placement is.
        let cleanup = match current.target.as_ref() {
            Some(_)
                if source_environment.is_none_or(|source| {
                    source.placement.target.is_none()
                        || source.phase == crate::database::EnvironmentPhase::Reclaimed
                }) || current.target != previous.target
                    || crate::database::session_environment(session_id)?
                        .as_ref()
                        .map(|e| &e.id)
                        != source_environment.map(|e| &e.id) =>
            {
                super::environment::remove_runtime_preserving_workspace(
                    &current,
                    &self.config,
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                )
            }
            _ => Ok(()),
        };
        // A failed target teardown may leave its harness writing. Keep its
        // checkout intact until a later retry proves the process is stopped.
        let worktree_cleanup = if cleanup.is_err() {
            Ok(())
        } else {
            let current_worktree = self.state.checkout(session_id)?.managed_worktree().cloned();
            let previous_checkout = self.state.checkout_for_record(session_id, previous)?;
            let previous_worktree = previous_checkout.managed_worktree().cloned();
            let cleanup_checkout = match (current_worktree.as_ref(), previous_worktree.as_ref()) {
                (_, Some(previous)) if recreated_managed_worktree => {
                    Some((previous, super::BranchDisposition::Keep))
                }
                (Some(current), Some(previous)) if current == previous => None,
                (Some(current), _) => Some((current, super::BranchDisposition::Delete)),
                (None, _) => None,
            };
            if let Some((worktree, branch)) = cleanup_checkout {
                let owners = crate::database::checkout_environments(worktree)?
                    .into_iter()
                    .filter(|environment| {
                        environment.owner_session_id == session_id
                            && environment.phase != crate::database::EnvironmentPhase::Reclaimed
                    })
                    .collect::<Vec<_>>();
                ensure!(
                    owners.len() == 1,
                    "resume rollback lost its exact checkout owner"
                );
                super::environment::retire_owned_environment(
                    owners.into_iter().next().unwrap(),
                    &self.config,
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                    branch,
                    super::CheckoutDisposition::Remove,
                )
                .map(|_| ())
            } else {
                Ok(())
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
        // The session's error is one line; a worker's diagnostic dump goes to
        // the log (R8-2).
        let original = super::worker_binary::failure_line(&error);
        let detail = format!("{error:#}");
        if detail != original {
            tracing::warn!(session_id, error = %detail, "resume failed");
        }
        let current_owns_managed_checkout = self
            .state
            .checkout(session_id)?
            .managed_worktree()
            .is_some();
        let rollback_clean = cleanup_error.is_empty();
        let record = self.state.sessions.get_mut(session_id).unwrap();
        let failure = apply_failed_resume_rollback_with_ownership(
            record,
            previous,
            current_owns_managed_checkout,
            &original,
            (!cleanup_error.is_empty()).then_some(cleanup_error),
        );
        // Rollback restores only resources owned by this attempt, preserving
        // client edits committed while provisioning was in flight.
        if rollback_clean && let Some(source_environment) = source_environment {
            crate::database::restore_resumed_session(
                &self.state.sessions[session_id],
                &source_environment.id,
            )?;
        } else {
            crate::database::save_resumed_session(&self.state.sessions[session_id], None)?;
        }
        let latest = crate::database::load_session_record(session_id)?
            .context("resumed session disappeared")?;
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.resource_allocation = latest.resource_allocation;
        record.additional_mounts = latest.additional_mounts;
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

#[cfg(test)]
pub(super) fn apply_failed_resume_rollback(
    current: &mut SessionRecord,
    previous: &SessionRecord,
    original_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    let current_owns_managed_checkout = current.checkout().managed_worktree().is_some();
    apply_failed_resume_rollback_with_ownership(
        current,
        previous,
        current_owns_managed_checkout,
        original_error,
        cleanup_error,
    )
}

fn apply_failed_resume_rollback_with_ownership(
    current: &mut SessionRecord,
    previous: &SessionRecord,
    current_owns_managed_checkout: bool,
    original_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    match cleanup_error {
        None => {
            *current = previous.clone();
            if previous.state == SessionState::Closing && previous.target.is_some() {
                // Cross-target Move retained the source environment across
                // destination provisioning: preserve `Closing` + source target
                // so the Move failure handler can restart the source agent in
                // place.
                current.state = SessionState::Closing;
            } else {
                current.state = SessionState::Stopped;
                current.target = None;
            }
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
            if !current_owns_managed_checkout {
                // Restore the persisted fields as a snapshot. The State-level
                // checkout derivation decides whether this record owns them.
                current
                    .project_directory
                    .clone_from(&previous.project_directory);
                current
                    .managed_worktree
                    .clone_from(&previous.managed_worktree);
                current.bundle_id.clone_from(&previous.bundle_id);
                current.project.clone_from(&previous.project);
            }
            current.state = SessionState::Error;
            current.updated_at = now();
            current.last_error = Some(format!("resume failed: {failure}"));
            anyhow::anyhow!(failure)
        }
    }
}

/// Verify that a legacy linked-worktree archive's base commit / bundle
/// prerequisites are available on its network origin before accepting a
/// cross-target (`RawToWorkspace`) resume. If the starting commit exists only
/// in `source_repository` on the original target, refuse with the ExecPlan's
/// exact guidance.
pub(super) fn ensure_legacy_worktree_cross_target_base_on_origin(
    previous: &SessionRecord,
    worktree: &mj_core::state::ManagedWorktree,
    archive_path: &Path,
    archive_manifest: &mj_checkpoint::archive::ArchiveManifest,
    executor: &impl CommandExecutor,
    github_token: Option<&str>,
) -> Result<()> {
    let refusal = || -> anyhow::Error {
        mj_core::refusal::Refusal::precondition(format!(
            "this older session's starting commit exists only in {} on {}; resume it there, push, then move it",
            worktree.source_repository.display(),
            previous.target_template_id
        ))
        .into()
    };
    let [repository] = archive_manifest.repositories.as_slice() else {
        return Err(refusal());
    };
    let origin = repository.metadata.origin.trim();
    if origin.is_empty() || mj_core::remote_git::validate_network_url(origin).is_err() {
        return Err(refusal());
    }
    let bundles = mj_checkpoint::archive::read_checkpoint_repository_bundles(archive_path)?;
    let Some(archived) = bundles
        .into_iter()
        .find(|bundle| bundle.metadata.id == repository.metadata.id)
    else {
        return Err(refusal());
    };
    let configured = ProjectRepository {
        id: repository.metadata.id.clone(),
        github: None,
        local: None,
        git_ref: None,
        destination: repository.metadata.relative_destination.clone(),
        mbx_namespace: None,
    };
    let accepted_source = mj_core::remote_git::NetworkGitSource {
        fetch_url: origin.to_owned(),
        push_urls: repository.metadata.push_urls.clone(),
    };
    if checkpoint_source_missing_commit(
        &configured,
        Some(&accepted_source),
        &archived,
        executor,
        github_token,
    )?
    .is_some()
    {
        return Err(refusal());
    }
    Ok(())
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
pub(super) fn conversion_checkpoint(
    previous_archive: &Path,
    snapshot: mj_checkpoint::archive::RepositorySnapshot,
    output: &Path,
) -> Result<mj_core::state::CheckpointMetadata> {
    let previous = mj_checkpoint::archive::verify_archive_streaming(previous_archive)
        .with_context(|| format!("read checkpoint archive {}", previous_archive.display()))?;
    // Native harness state and relay attachments travel byte for byte: a
    // conversion replaces repository content and nothing else.
    let parent = output
        .parent()
        .context("conversion checkpoint has no parent")?;
    std::fs::create_dir_all(parent)?;
    let spool = tempfile::Builder::new()
        .prefix("conversion-native-")
        .tempdir_in(parent)?;
    let mut payload_reader =
        mj_checkpoint::archive::CheckpointPayloadReader::open(previous_archive)?;
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
        .enumerate()
        .map(|(index, (relative_path, descriptor))| {
            let path = spool.path().join(index.to_string());
            let mut file = std::fs::File::create(&path)?;
            payload_reader.copy_payload(descriptor, &mut file)?;
            Ok(mj_checkpoint::archive::NativeArtifact {
                relative_path: relative_path.clone(),
                data: mj_checkpoint::archive::NativeArtifactData::File(path),
                mode: descriptor.mode,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let canonical_session = previous.canonical_session;
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

/// Whether the native session a restored worker opened may stand in for the
/// archived one.
///
/// The archived session is the one the conversation lives in, so a different
/// one is refused: resuming in it would silently drop that history. The one
/// exception is a worker that says it replaced exactly the archived session
/// because the harness had no record of it and this session never used it.
/// Claude Code and Codex write nothing for a native session until its first
/// prompt, so a session suspended before one (or right after `/clear`) can
/// only ever come back this way (R7-5).
fn restored_native_session_accepted(
    archived: &str,
    opened: &str,
    replaced_unused: Option<&str>,
) -> bool {
    opened == archived || replaced_unused == Some(archived)
}

/// Put the durable projection back to the archived one after a failed resume
/// or in-place swap.
///
/// The attempt can have changed it two ways: a rebuild from the archive, and
/// the events of the worker it started, which the controller projects while it
/// waits for the harness. Either leaves the stored frontier somewhere other
/// than the archive's, so the frontier decides, and the failed attempt leaves
/// no lines in the transcript for the next one to add to. A projection still
/// at the archived frontier only needs its queue back when the attempt
/// discarded it.
fn restore_projection_after_failed_resume(
    session_id: &str,
    canonical_session: &mj_checkpoint::archive::CanonicalSessionSnapshot,
    discard_queued_prompts: bool,
) {
    let stored_frontier = crate::database::materialized_event_frontier(session_id)
        .unwrap_or_else(|error| {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not read the stored projection frontier after a failed resume; rebuilding it from the archive"
            );
            None
        });
    if projection_rebuild_required(
        stored_frontier
            .as_ref()
            .map(|(ordinal, digest)| (*ordinal, digest.as_str())),
        canonical_session.event_frontier,
        &canonical_session.event_frontier_digest,
    ) {
        match materialized_session_from_canonical(session_id, canonical_session) {
            Ok(previous_projection) => {
                if let Err(restore_error) =
                    crate::database::save_materialized_session(&previous_projection)
                {
                    tracing::error!(
                        session_id,
                        error = format!("{restore_error:#}"),
                        "could not restore the durable projection after a failed resume"
                    );
                }
            }
            Err(restore_error) => tracing::error!(
                session_id,
                error = format!("{restore_error:#}"),
                "could not rebuild the durable projection after a failed resume"
            ),
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
            "could not restore queued prompts after a failed resume"
        );
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
    fn execute_to_file(
        &self,
        command: &CommandSpec,
        output: &std::fs::File,
    ) -> Result<CommandOutput> {
        if self.cancellation.is_cancelled() {
            bail!("operation cancelled while provisioning destination");
        }
        self.inner.execute_to_file(command, output)
    }

    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        self.inner.pause_deadline()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation.is_cancelled() || self.inner.cancellation_requested_without_deadline()
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if self.cancellation.is_cancelled() {
            bail!("operation cancelled while provisioning destination");
        }
        self.inner.execute(command)
    }

    fn cancellation_requested(&self) -> bool {
        self.cancellation.is_cancelled() || self.inner.cancellation_requested()
    }

    fn shutdown_interrupted(&self) -> bool {
        self.inner.shutdown_interrupted()
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

    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if self.cancellation.is_cancelled() {
            bail!("operation cancelled while provisioning destination");
        }
        self.inner.execute_without_deadline(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        if self.cancellation.is_cancelled() {
            bail!("operation cancelled while provisioning destination");
        }
        self.inner
            .execute_with_stdin_without_deadline(command, input)
    }
}

#[allow(clippy::too_many_arguments)]
fn provision_with_cross_harness_handoff(
    controller: &mut Controller,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    github_token: Option<&str>,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    prior: Option<&crate::compaction::PriorCompaction>,
    context_bytes: usize,
    move_manifest: Option<&mj_checkpoint::archive::ArchiveManifest>,
) -> Result<String> {
    let (_provision, handoff) = execute_joined_cross_harness_work(
        "cross-harness provisioning",
        move |cancellation| {
            let provision_executor = CrossHarnessProvisionExecutor {
                inner: executor,
                cancellation,
            };
            block_on_cross_harness_lane(
                "cross-harness provisioning",
                controller.provision_session_with_failure_disposition(
                    session_id,
                    &provision_executor,
                    github_token,
                    ProvisioningFailureDisposition::Preserve,
                    RepositorySeed::Restore(move_manifest),
                ),
            )
        },
        "cross-harness handoff",
        move |cancellation| {
            block_on_cross_harness_lane(
                "cross-harness handoff",
                utility_handoff_while_cancellable(
                    session_id,
                    config,
                    snapshot,
                    prior,
                    context_bytes,
                    executor,
                    cancellation,
                ),
            )
        },
    )?;
    ensure!(
        !executor.cancellation_requested(),
        "operation cancelled while provisioning destination"
    );
    Ok(handoff)
}

/// Each joined OS thread must own the Tokio reactor used by provisioning and
/// handoff I/O. A plain futures executor cannot drive Tokio sockets or timers.
fn block_on_cross_harness_lane<T>(
    name: &str,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| format!("create {name} runtime"))?;
    runtime.block_on(work)
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
    let owner = crate::worker_lifecycle::capture();
    std::thread::scope(|scope| {
        let first_cancel = cancellation.clone();
        let first_owner = owner.clone();
        let mut first_handle = Some(scope.spawn(move || match first_owner {
            Some(owner) => owner.scope_blocking(|| first(first_cancel)),
            None => first(first_cancel),
        }));
        let second_cancel = cancellation.clone();
        let mut second_handle = Some(scope.spawn(move || match owner {
            Some(owner) => owner.scope_blocking(|| second(second_cancel)),
            None => second(second_cancel),
        }));
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

async fn native_prior_while_cancellable(
    session_id: &str,
    archive: &VerifiedResumeArchive,
    executor: &impl CommandExecutor,
) -> Result<Option<crate::compaction::PriorCompaction>> {
    native_prior_source_while_cancellable(
        session_id,
        NativePriorSource::Archive {
            archive_path: archive.archive_path.clone(),
            manifest: archive.manifest.clone(),
        },
        &archive.canonical_session,
        executor,
    )
    .await
}

/// POSIX scripts that print a source session's native transcript, taking the
/// harness home as `$1` and the native session ID as `$2`. They follow no
/// symlink and print nothing when the transcript is absent.
const CLAUDE_TRANSCRIPT_READ_SCRIPT: &str = r#"if [ -d "$1/projects" ] && [ ! -L "$1/projects" ]; then for d in "$1"/projects/*; do [ -d "$d" ] && [ ! -L "$d" ] && [ "${d##*/}" != subagents ] && f="$d/$2.jsonl" && [ -f "$f" ] && [ ! -L "$f" ] && exec cat -- "$f"; done; fi; exit 0"#;
const ANTIGRAVITY_TRANSCRIPT_READ_SCRIPT: &str = r#"d="$1"; for part in antigravity-cli brain "$2" .system_generated logs; do d="$d/$part"; [ -d "$d" ] && [ ! -L "$d" ] || exit 0; done; f="$d/transcript_full.jsonl"; [ -f "$f" ] && [ ! -L "$f" ] && exec cat -- "$f"; exit 0"#;

pub(in crate::controller) async fn native_prior_from_target_while_cancellable(
    session_id: &str,
    source_backend: &targets::TargetLocator,
    source_harness_kind: HarnessKind,
    native_session_id: Option<&str>,
    snapshot: &Arc<CanonicalSessionSnapshot>,
    executor: &impl CommandExecutor,
) -> Result<Option<crate::compaction::PriorCompaction>> {
    let script = match source_harness_kind {
        HarnessKind::Claude => CLAUDE_TRANSCRIPT_READ_SCRIPT,
        HarnessKind::Antigravity => ANTIGRAVITY_TRANSCRIPT_READ_SCRIPT,
        _ => return Ok(None),
    };
    let Some(id) = native_session_id.filter(|id| {
        !id.is_empty()
            && !id.contains('/')
            && !id.contains('\\')
            && !id.contains("..")
            && !id.chars().any(char::is_control)
    }) else {
        return Ok(None);
    };
    let harness_home =
        targets::target_profile_home_for_kind(source_backend, session_id, source_harness_kind);
    if matches!(source_backend, targets::TargetLocator::LocalBare { .. }) {
        return native_prior_source_while_cancellable(
            session_id,
            NativePriorSource::Home {
                harness_kind: source_harness_kind,
                harness_home: PathBuf::from(harness_home),
                native_session_id: id.to_owned(),
            },
            snapshot,
            executor,
        )
        .await;
    }

    ensure!(
        !executor.cancellation_requested(),
        "operation cancelled while reading native compaction"
    );
    let mut spool = mj_checkpoint::archive::create_fetch_spool()?;
    let command = targets::locator_command(
        SshTrafficClass::Bulk,
        source_backend,
        vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            "mj-read-native-transcript".into(),
            harness_home,
            id.to_owned(),
        ],
    )
    .purpose("read source native transcript for compaction");
    let output = match executor.execute_to_file(&command, &spool) {
        Ok(output) => output,
        Err(error) if executor.cancellation_requested() => {
            return Err(error.context("operation cancelled while reading native compaction"));
        }
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not read source native transcript; using canonical history"
            );
            return Ok(None);
        }
    };
    ensure!(
        !executor.cancellation_requested(),
        "operation cancelled while reading native compaction"
    );
    if output.status != 0 {
        tracing::warn!(
            session_id,
            status = output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "could not read source native transcript; using canonical history"
        );
        return Ok(None);
    }
    spool.rewind()?;
    if spool.metadata()?.len() == 0 {
        return Ok(None);
    }
    native_prior_source_while_cancellable(
        session_id,
        NativePriorSource::File {
            harness_kind: source_harness_kind,
            file: spool,
        },
        snapshot,
        executor,
    )
    .await
}

/// Cooperatively stop native parsing on cancellation and join its blocking
/// task before releasing the source and lifecycle ownership.
async fn native_prior_source_while_cancellable(
    session_id: &str,
    source: NativePriorSource,
    snapshot: &Arc<CanonicalSessionSnapshot>,
    executor: &impl CommandExecutor,
) -> Result<Option<crate::compaction::PriorCompaction>> {
    ensure!(
        !executor.cancellation_requested(),
        "operation cancelled while reading native compaction"
    );
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let operation = crate::handoff::native_prior(session_id, source, snapshot, cancel.clone());
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => return result,
            _ = tokio::time::sleep(super::readiness::CANCELLATION_POLL_INTERVAL) => {
                if executor.cancellation_requested() {
                    cancel.cancel();
                    return match operation.await {
                        Ok(_) => Err(anyhow::anyhow!("operation cancelled while reading native compaction")),
                        Err(error) => Err(error.context("operation cancelled while reading native compaction")),
                    };
                }
            }
        }
    }
}

/// Discover a utility model and compact the cross-harness handoff while still
/// watching for cancellation. Discovery and compaction can both make several
/// network requests, so a cancelled resume must not wait them out.
pub(in crate::controller) async fn utility_handoff_while_cancellable(
    session_id: &str,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    prior: Option<&crate::compaction::PriorCompaction>,
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
    let operation = crate::handoff::build_handoff_context_with_prior(
        session_id,
        config,
        snapshot,
        context_bytes,
        prior,
        &cancel,
    );
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

/// Resolve a Move source or destination `.mj/keep/<session-id>` directory using
/// the same Git top-level interpretation worker startup uses for `MJ_KEEP_DIR`,
/// so sessions started inside a repository subdirectory or nested repository
/// transfer the exact scratch directory their worker created.
impl Controller {
    pub(super) fn move_source_keep_dir(
        &self,
        source: &SessionRecord,
        backend: &targets::TargetLocator,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<PathBuf> {
        let checkout = self.state.checkout(&source.id)?;
        let cwd = if let Some(path) = checkout.project_directory() {
            path.to_path_buf()
        } else if let Some(worktree) = checkout.managed_worktree() {
            worktree.worktree_root.clone()
        } else {
            let workspace = PathBuf::from(super::network_git::workspace_root(
                backend,
                source.container_workspace.as_deref(),
                &source.id,
            ));
            source
                .project_bundle(&self.config)
                .and_then(|bundle| bundle.primary())
                .map(|primary| workspace.join(&primary.destination))
                .unwrap_or(workspace)
        };
        mj_core::runtime::off_async_worker(|| {
            resolve_move_keep_dir(executor, backend, &source.id, &cwd)
        })
    }
}

pub(super) fn resolve_move_keep_dir(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    session_id: &str,
    cwd: &Path,
) -> Result<PathBuf> {
    let command = targets::command_on_locator(
        SshTrafficClass::Control,
        backend,
        session_id,
        vec![
            "git".into(),
            "-C".into(),
            cwd.to_string_lossy().into_owned(),
            "rev-parse".into(),
            "--show-prefix".into(),
            "--show-toplevel".into(),
        ],
        "resolve checkout top level for session keep directory",
    )?;
    let output = executor.execute(&command)?;
    let (top_level, _) = mj_core::session_keep::interpret_checkout_top_level_as_given(
        cwd,
        Some(output.status),
        &output.stdout,
        &output.stderr,
    )?;
    mj_checkpoint::checkpoint::session_keep_dir(&top_level, session_id)
}

mod in_place;
mod repair;
pub use in_place::InPlaceRestartError;
pub(super) use repair::{RetainedWorker, release_settled_in_place_hold};

#[cfg(test)]
mod tests;
