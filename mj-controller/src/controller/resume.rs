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

use super::backend::{backend_locator, validate_resource_allocation};
use super::checkpoint::upload_checkpoint_spec;
use super::github_app::{GithubAppTokenProvider, github_repositories};
use super::provisioning::{
    ProvisioningFailureDisposition, StagedExecutor, execute_concurrent_lanes,
    install_attached_resources,
};
use super::readiness::{
    connect_started_worker, wait_for_harness_prepared_in_stage, wait_for_native_session_in_stage,
};
use super::worker_binary::{bridge_readiness_stage, start_worker_durably, worker_probe_diagnosis};
use super::worktree::{
    PrimaryCheckoutRequirement, ResumeConversion, ResumePlan, apply_raw_to_workspace,
    apply_workspace_to_raw, cleanup_managed_worktree, create_managed_worktree,
    managed_worktree_checkout_exists, managed_worktree_target, plan_raw_to_workspace_for_session,
    preserve_retained_managed_worktree_branch, raw_checkout_divergence_notice,
    raw_checkout_position_with_checkout, raw_checkout_snapshot,
    raw_conversion_preview_with_checkout, restore_managed_worktree,
    resume_compatibility_with_checkout, retire_managed_worktree,
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
        let checkout = self.state.checkout(&source.id)?;
        ensure!(
            checkout.project_directory().is_some()
                || source
                    .project_bundle(&self.config)
                    .is_none_or(|bundle| bundle.repositories.len() == 1),
            "Muse Code ACP supports one workspace root; use a single-repository bundle"
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
        let repositories = read_checkpoint_repository_bundles(&checkpoint.archive_path)?;
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
        self.replace_resume_repository_origin_with_token(
            session_id,
            repository_id,
            replacement,
            executor,
        )
        .await
    }

    async fn replace_resume_repository_origin_with_token(
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
        let replacement_network = session
            .project
            .as_ref()
            .map(|_| mj_core::remote_git::resolve_repository(&replacement, executor))
            .transpose()?;
        let previous = session.clone();
        let mut project = session.project.clone();
        if let Some(project) = &mut project {
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
        let github_token = if let Some(app) = self.config.github.app.as_ref() {
            if let Some(project) = &project {
                let bundle = project.bundle.clone();
                let network_sources = project.network_sources.clone();
                let repositories = tokio::task::spawn_blocking(move || {
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
        // A merged project can use different repository IDs. Its destination
        // and identity identify the authoring entry; the session keeps its IDs.
        let saved = crate::database::saved_project(&bundle_id)?;
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
        self.config = config;
        if let Some(project) = project {
            self.state
                .sessions
                .get_mut(session_id)
                .expect("session checked above")
                .project = Some(project);
            self.persist_session_transition_or_restore(
                session_id,
                &previous,
                "save repaired project source",
            )?;
        }
        self.preflight_verified_repository_sources(
            session_id,
            verified,
            Some(repository_id),
            false,
            github_token.as_deref(),
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
    /// live daemon, clear relay state, unlink the installed worker files, and
    /// remove the previous per-session profile home. Runs on every locator.
    InPlace {
        /// The per-session profile root the replaced harness ran from.
        previous_profile_root: String,
    },
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
    pub retire_after_ready: Option<&'a mj_core::state::ManagedWorktree>,
}

impl Controller {
    /// Install the worker, restore the archive into the target, start the
    /// harness, and report the projection the destination answers with.
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
                utility_handoff,
                projection_build,
                mut resume_notices,
                include_in_place_subagents,
                install_attached_resources: should_install_attached_resources,
                worker_root_reset,
                retire_after_ready,
            } = restore;
            let archive_manifest = &archive.manifest;
            let canonical_session = &archive.canonical_session;
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
                        self.state.subagents.get(session_id),
                        self.config.agent_mailboxes_enabled(),
                    );
            resume_notices.extend(mj_core::subagent::stopped_subagents_notice(
                &stopped_subagents,
            ));
            let mut prompt_contexts = Vec::new();
            prompt_contexts.extend(stopped_subagents_context);
            let (backend, worker_root) = self.worker_placement(session_id)?;
            let harness_home = target_profile_home(&backend, session_id, profile);
            let workspace_root = if let Some(project_directory) = &resumed_project_directory {
                project_directory
                    .parent()
                    .context("bare project directory has no parent")?
                    .to_string_lossy()
                    .into_owned()
            } else {
                super::network_git::workspace_root(&backend, resumed_container_workspace.as_deref())
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
            let remote_spec = format!("{worker_root}/restore-spec.json");
            use mj_checkpoint::checkpoint::QueueRestorePolicy;
            let move_admission =
                crate::database::load_move_operation(session_id)?.is_some_and(|operation| {
                    operation.phase == mj_core::state::MovePhase::ResumingDestination
                        && !operation.queue_admission_started
                        && operation.queue == mj_core::state::ResumeQueueDisposition::Start
                });
            let queue_policy = if move_admission || (!native_continuity && replay_queue) {
                QueueRestorePolicy::Defer
            } else if discard_queued_prompts {
                QueueRestorePolicy::Discard
            } else {
                QueueRestorePolicy::Restore
            };
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
                restore_repositories,
                restore_native: native_continuity,
                // A session with a project directory launches its harness there,
                // spelled exactly as recorded (the worker launch configuration's
                // `cwd`), so the restored harness session is keyed by that same
                // text. Rebuilding it from the archive's repository layout loses a
                // trailing separator, which Grok Build keys by, and cannot name
                // the checkout a move put the session on. A session in a target
                // workspace has no project directory, and the archive's layout
                // names its directory under `/workspace`.
                primary_repository_root: resumed_project_directory
                    .as_ref()
                    .map(|directory| target_path(&directory.to_string_lossy())),
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
                            execute_checked(syncing, command)?;
                        }
                        // Both lanes below write into the worker root, so it
                        // exists first.
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
                    // The environment survives this restore, so the old
                    // harness has to be taken out of it: its daemon, its relay
                    // state, the installed worker files, and its profile home.
                    // The same command recreates the worker root.
                    WorkerRootReset::InPlace {
                        previous_profile_root,
                    } => {
                        execute_checked(
                            syncing,
                            targets::in_place_worker_reset_plan(
                                &backend,
                                session_id,
                                previous_profile_root,
                            )?,
                        )?;
                    }
                }
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
            let readiness_stage = bridge_readiness_stage(profile);
            let spec = self.reconnect_command(session_id)?;
            let readiness = async {
                let mut relay = {
                    let _starting = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
                    start_worker_durably(
                        &crate::worker_lifecycle::require(session_id)?,
                        self.state.sessions[session_id]
                            .target
                            .as_ref()
                            .context("worker start has no durable target")?,
                        executor,
                        &backend,
                        &worker_root,
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
                    let (context, _) = tokio::task::spawn_blocking(move || {
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
                let owner = crate::worker_lifecycle::require(session_id)?;
                crate::database::finish_worker_restart(session_id, owner.operation_id())?;
                Ok::<_, anyhow::Error>((relay, native_session_id))
            }
            .await;
            let (mut relay, native_session_id) = readiness
                .map_err(|error| worker_probe_diagnosis(executor, &backend, &worker_root, error))?;
            if native_continuity {
                if !restored_native_session_accepted(
                    &archive_manifest.session.native_session_id,
                    &native_session_id,
                    relay
                        .operational()
                        .replaced_unused_native_session_id
                        .as_deref(),
                ) {
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
            }
            // Last, and only once the resume has otherwise succeeded: a failure
            // before this point rolls the record back to a session whose
            // worktree still has to be there.
            if let Some(worktree) = retire_after_ready
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
            if include_in_place_subagents {
                let session_id = session_id.to_owned();
                let (_, notice) = tokio::task::spawn_blocking(move || {
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
        verify_archive_streaming(&archive_path)?
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
    let mut ssh_transport = false;
    let source = if let Some(source) = accepted_source {
        source.fetch_url.clone()
    } else if let Some(local) = &configured.local {
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
    if accepted_source.is_some() {
        if source.starts_with("https://github.com/") && github_token.is_some() {
            token_auth = true;
            arguments.extend([
                "-c".to_owned(), "credential.helper=".to_owned(), "-c".to_owned(),
                "credential.helper=!f() { if [ \"$1\" = get ]; then echo username=x-access-token; echo \"password=$GH_TOKEN\"; fi; }; f".to_owned(),
            ]);
        }
        ssh_transport = source.starts_with("git@") || source.starts_with("ssh://");
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
        self.resume_session_with_origin(
            session_id,
            profile_id,
            target_id,
            options,
            repository_preflight,
            None,
            executor,
        )
        .await
    }

    pub(in crate::controller) async fn resume_session_for_move(
        &mut self,
        operation: &mj_core::state::MoveOperation,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        self.resume_session_with_origin(
            &operation.selection.session_id,
            operation
                .selection
                .profile_id
                .as_deref()
                .context("Move profile missing")?,
            operation
                .selection
                .target_template_id
                .as_deref()
                .context("Move target missing")?,
            SessionResumeOptions {
                additional_mounts: operation.selection.additional_mounts.clone(),
                resource_allocation: operation.selection.resource_allocation.clone(),
                discard_queue: true,
            },
            None,
            Some(operation),
            executor,
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
        move_operation: Option<&mj_core::state::MoveOperation>,
        executor: &(impl CommandExecutor + Sync),
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
        let checkpoint = move_operation
            .and_then(|op| op.handoff.as_ref())
            .or(previous.checkpoint.as_ref())
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
        let verified_archive = verify_resume_checkpoint(session_id, checkpoint)?;
        let archive_path = verified_archive.archive_path.clone();
        let archive_manifest = &verified_archive.manifest;
        let canonical_session = Arc::clone(&verified_archive.canonical_session);
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
        let checkout = self.state.checkout(session_id)?;
        let plan = resume_compatibility_with_checkout(
            &previous,
            &checkout,
            &self.config,
            target_id,
        )
            .map_err(|reason| anyhow::anyhow!("{reason}"))?;
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
        // A converting resume writes its own archive below, from the accepted
        // repository's selected network source, and provisioning reads that
        // one. Every other isolated resume clones what its stored archive names.
        if !mj_core::config::is_bare_project_target(&target_template)
            && plan != ResumePlan::RawToWorkspace
            && !transferring_workspace
        {
            super::network_git::bundle_from_manifest(archive_manifest)?;
        }
        if plan == ResumePlan::InPlace
            && matches!(checkout.effective(), mj_core::state::Checkout::Attached { .. })
            && let Some(project_directory) = checkout.project_directory()
        {
            self.validate_project_directory(target_id, project_directory, executor)
                .context("raw project is unavailable for resume")?;
        }
        let conversion = match plan {
            ResumePlan::InPlace => None,
            ResumePlan::RawToWorkspace => Some(ResumeConversion::RawToWorkspace(Box::new(
                plan_raw_to_workspace_for_session(&previous, &checkout, &self.config, executor)
                    .context("prepare the raw checkout for its new target")?,
            ))),
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
        if !transferring_workspace
            && previous.state == SessionState::Error
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
        let managed_checkout_present = previous_managed_worktree
            .as_ref()
            .map(|worktree| managed_worktree_checkout_exists(executor, worktree))
            .transpose()?
            .unwrap_or(true);
        // A checkout Mjolnir did not retire remains the truth for a raw session.
        // A retired checkout is recreated from the branch and archive below.
        if managed_checkout_present
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
        super::worker_binary::preflight_worker_binary(&target_template, executor)?;
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
                apply_raw_to_workspace(record, conversion)?;
            }
            Some(ResumeConversion::WorkspaceToRaw(conversion)) => {
                apply_workspace_to_raw(record, conversion);
            }
            None => {
                if let (Some(worktree), Some(refreshed)) = (
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
        crate::database::save_resumed_session(
            &self.state.sessions[session_id],
            selected_container_size
                .as_ref()
                .map(|(host, size)| (host.as_str(), *size)),
        )?;
        if let Some((host, size)) = selected_container_size.as_ref() {
            self.state.remember_container_size(host, *size);
        }

        let mut recreated_managed_worktree = false;
        // Set once a conversion has written its archive, so the success path can
        // retire the archive it replaced and the failure path can remove it.
        let mut conversion_checkpoint_written: Option<mj_core::state::CheckpointMetadata> = None;
        let result = async {
            if let Some(worktree) = previous_managed_worktree.as_ref() {
                recreated_managed_worktree = restore_managed_worktree(executor, worktree)?;
                if !transferring_workspace && recreated_managed_worktree && plan == ResumePlan::RawToWorkspace {
                    if worktree.kind == mj_core::state::ManagedCheckoutKind::Clone {
                        mj_checkpoint::checkpoint::restore_single_repository_into_checkout(
                            &archive_path, &worktree.worktree_root, &SystemGit,
                        )?;
                    } else {
                        mj_checkpoint::checkpoint::restore_single_repository_onto_branch(
                            &archive_path, &worktree.worktree_root, &worktree.branch, &SystemGit,
                        )?;
                    }
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
                if transferring_workspace {
                    // Move installs its separately verified Git/file transfer below.
                } else if conversion.worktree.kind == mj_core::state::ManagedCheckoutKind::Clone {
                    mj_checkpoint::checkpoint::restore_single_repository_into_checkout(
                        &archive_path, &conversion.worktree.worktree_root, &SystemGit,
                    )?;
                } else {
                    mj_checkpoint::checkpoint::restore_single_repository_onto_branch(
                        &archive_path,
                        &conversion.worktree.worktree_root,
                        &conversion.worktree.branch,
                        &SystemGit,
                    )?;
                }
            }
            // A local checkout becomes an isolated workspace by being
            // re-snapshotted into a new archive whose provenance is the
            // accepted repository's network source. Provisioning clones that
            // source, and the restore below lays this snapshot over the clone.
            if let Some(conversion) = conversion
                .as_ref()
                .and_then(ResumeConversion::raw_to_workspace)
                && !transferring_workspace
            {
                let snapshot = raw_checkout_snapshot(
                    &conversion.checkout,
                    &conversion.repository_id,
                    &conversion.source,
                    &conversion.destination,
                    &SystemGit,
                    conversion.retire.as_ref().is_some_and(|checkout| {
                        checkout.kind == mj_core::state::ManagedCheckoutKind::Clone
                    }),
                )
                .context("snapshot the host checkout for its new target")?;
                resume_notices.push(conversion_notice(
                    target_id,
                    previous_project_directory
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
                // Persist the converted archive before provisioning without
                // restoring client-owned fields from this earlier snapshot.
                crate::database::save_resumed_session(
                    &self.state.sessions[session_id],
                    selected_container_size.as_ref().map(|(host, size)| (host.as_str(), *size)),
                )?;
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
            if transferring_workspace {
                self.restore_move_workspace(session_id, executor)?;
            }
            let restore_repositories = !transferring_workspace && ((resumed_project_directory.is_none()
                && conversion.is_none())
                || plan == ResumePlan::RawToWorkspace
                || (recreated_managed_worktree && plan == ResumePlan::InPlace));
            // A conversion restores the archive it just wrote, not the raw one
            // the session was stopped with.
            let restored_archive = conversion_checkpoint_written
                .as_ref()
                .map_or(archive_path.as_path(), |checkpoint| {
                    checkpoint.archive_path.as_path()
                });
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
                        .and_then(|plan| plan.retire.as_ref()),
                },
                executor,
            )
            .await
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
                // Put back whatever this resume wrote to the durable
                // projection, including the failed worker's own lines.
                restore_projection_after_failed_resume(
                    session_id,
                    &canonical_session,
                    discard_queued_prompts,
                );
                if let Some(operation) = move_operation {
                    return Err(self.rollback_move_destination(operation, error, executor)?);
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

        }).await
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
            let current_worktree = self.state.checkout(session_id)?.managed_worktree().cloned();
            let previous_checkout = self.state.checkout_for_record(session_id, previous)?;
            let previous_worktree = previous_checkout.managed_worktree().cloned();
            match (current_worktree.as_ref(), previous_worktree.as_ref()) {
                (_, Some(previous)) if recreated_managed_worktree => retire_managed_worktree(
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                    previous,
                ),
                (Some(current), Some(previous)) if current == previous => Ok(()),
                // The failed resume created this worktree and its branch, and
                // no harness ever ran in it, so the rollback removes both.
                (Some(worktree), _) => cleanup_managed_worktree(
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                    worktree,
                    crate::controller::BranchDisposition::Delete,
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
        crate::database::save_resumed_session(&self.state.sessions[session_id], None)?;
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
            block_on_cross_harness_lane(
                "cross-harness provisioning",
                controller.provision_session_with_failure_disposition(
                    session_id,
                    &provision_executor,
                    github_token,
                    ProvisioningFailureDisposition::Preserve,
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

mod in_place;
pub(crate) use in_place::InPlaceRestartError;

#[cfg(test)]
mod tests;
