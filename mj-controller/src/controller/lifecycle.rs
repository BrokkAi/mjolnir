//! Session close, force-stop, and permanent-destruction transitions.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::session_manager::{SessionManagerControl, new_command_id, worker_connect_needs_restart};
use mj_core::state::{CheckpointMetadata, ManagedCheckoutKind, SessionRecord, SessionState};

use crate::targets::{self, CommandExecutor, ProcessExecutor, ProvisionStage, ProvisionStageGuard};
use mj_core::relay::{RelayCommand, RelayExecutionState};

use super::backend::backend_locator;
use super::checkpoint::{
    CheckpointDelivery, CheckpointExportPolicy, ControllerRelayLease, LatchExclusivity,
    adopt_restarted_checkpoint_relay, prune_replaced_checkpoint,
    release_projection_behind_checkpoint, verify_installed_checkpoint_gate, wait_for_relay_closed,
};
use super::worker_restart::WorkerRestartLeftNoWorker;
use super::{Controller, now, persist_session_record_transition_or_restore};

pub(super) mod origin_delta;
mod subagent;
use origin_delta::OriginDeltaSuspend;

pub use mj_session_types::LifecycleFailureReason;

/// What destroying a session does with its managed worktree's git branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchDisposition {
    /// Delete the branch with the rest of the session. Only for a branch
    /// nobody has worked on, or when the user asks for it by name.
    Delete,
    /// Leave the branch in the repository. The default for every destroy,
    /// because the branch may hold work the user still wants.
    Keep,
    /// Delete the branch only when every commit on it is reachable from some
    /// other branch, local or remote-tracking, that is not a session branch.
    /// Anything else keeps the branch, exactly as [`BranchDisposition::Keep`]
    /// would. The archive job uses this so a branch whose work has landed
    /// elsewhere does not pile up forever.
    DeleteIfMerged,
}

/// What destroying a session does with its managed worktree's checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutDisposition {
    /// Remove the checkout whatever it holds. Every destroy a person asks for
    /// takes this path, because the confirmation already covered the loss.
    Remove,
    /// Leave the checkout, and its branch, in place when it has uncommitted
    /// changes. Destruction Mjolnir decides on its own never discards work the
    /// user has not seen.
    KeepWhenDirty,
}

/// What a verified close does with the target the session was running in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceTargetDisposition {
    /// Verified checkpoint, sealed relay, then destroy the exact target.
    Destroy,
    /// Verified checkpoint, sealed relay; keep the target and its worker
    /// daemon alive for an in-place harness replacement.
    RetainForInPlaceSwap,
}

/// Work a close runs once the session is about to close: its checkpoint is
/// verified and recorded, and its relay is not sealed yet. This is the
/// close's point of no return. A daemon suspend commits its lifecycle here,
/// so a cancel that arrives later is refused, and stops the session's
/// sub-agents, so a close that fails at its checkpoint leaves them running.
/// An explicit child close whose barrier cannot settle runs this before
/// stopping its worker for checkpoint-only recovery.
/// When this fails, the close does not proceed: the session returns to its
/// previous state with its relay unsealed.
pub type BeforeClose = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;

impl Controller {
    /// Whether an interrupted suspend of this session is an accepted
    /// origin-delta suspend. Its worker owns the durable stop request, and the
    /// wait for its processes can last indefinitely, so startup recovery of
    /// it must not hold daemon handoff admission.
    pub fn suspend_wait_is_worker_owned(&self, session_id: &str) -> bool {
        let Ok((backend, _)) = self.worker_placement(session_id) else {
            return false;
        };
        if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
            return OriginDeltaSuspend::load(self, session_id, &ProcessExecutor)
                .is_ok_and(|attempt| attempt.accepted());
        }
        // Remote and container suspend waits are resumable from durable
        // Closing state and must never block daemon startup or hold handoff
        // admission on synchronous SSH/container I/O.
        let Ok(checkout) = self.state.checkout(session_id) else {
            return false;
        };
        match checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                worktree.uses_origin_delta_suspend()
            }
            mj_core::state::Checkout::ManagedWorkspace => {
                !self.state.is_subagent_session(session_id)
            }
            _ => false,
        }
    }

    #[cfg(test)]
    fn explicit_suspend_export_policy(&self, session_id: &str) -> Result<CheckpointExportPolicy> {
        self.explicit_suspend_export_policy_with_executor(session_id, &ProcessExecutor)
    }

    fn explicit_suspend_export_policy_with_executor(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<CheckpointExportPolicy> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let checkout = self.state.checkout(session_id)?;
        if !self.state.is_subagent_session(session_id)
            && checkout.project_directory().is_none()
            && super::backend::session_history_cutoff(session)?.is_some()
        {
            return Ok(CheckpointExportPolicy::OriginDeltaCutoff);
        }
        let origin_delta = match checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. }
                if worktree.uses_origin_delta_suspend() =>
            {
                OriginDeltaSuspend::applies(self, session_id, executor)?
            }
            mj_core::state::Checkout::ManagedWorkspace
                if !self.state.is_subagent_session(session_id) && session.target.is_some() =>
            {
                OriginDeltaSuspend::applies(self, session_id, executor)?
            }
            _ => false,
        };
        if origin_delta {
            Ok(CheckpointExportPolicy::OriginDelta)
        } else if matches!(checkout, mj_core::state::Checkout::Attached { .. })
            && !session
                .project
                .as_ref()
                .is_some_and(|project| project.primary_is_plain_directory())
        {
            Ok(CheckpointExportPolicy::OriginDeltaRaw)
        } else {
            Ok(CheckpointExportPolicy::ReuseUnchangedArchive)
        }
    }

    /// Run read-only workspace inventory (and optional dry-run size estimate) off
    /// the async worker so CLI, TUI, and Web UI can show blockers, untracked files,
    /// in-progress operations, `.mj/keep/`, and existing checkpoint status.
    pub fn suspend_preflight(
        &self,
        session_id: &str,
        dry_run: bool,
        without_checkpoint: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<mj_core::suspend_consent::SuspendPreflight> {
        self.checkpoint_preflight(session_id, dry_run, without_checkpoint, None, executor)
            .map(|(preflight, _)| preflight)
    }

    pub(super) fn checkpoint_preflight(
        &self,
        session_id: &str,
        dry_run: bool,
        without_checkpoint: bool,
        move_keep: Option<&mj_core::session_keep::MoveKeep>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<(
        mj_core::suspend_consent::SuspendPreflight,
        Vec<(PathBuf, u64)>,
    )> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let children_not_handed_back: Vec<String> = self
            .state
            .subagents
            .values()
            .filter(|sub| sub.parent_session_id == session_id)
            .filter(|sub| {
                self.state
                    .sessions
                    .get(&sub.child_session_id)
                    .is_some_and(|child| child.state.is_active())
            })
            .map(|sub| sub.child_session_id.clone())
            .collect();
        let existing_checkpoint = session.checkpoint.as_deref().map(|checkpoint| {
            let verified = verify_installed_checkpoint_gate(session_id, checkpoint).is_ok();
            let age_seconds = chrono::DateTime::parse_from_rfc3339(&checkpoint.created_at)
                .ok()
                .map(|created| {
                    chrono::Utc::now()
                        .signed_duration_since(created.with_timezone(&chrono::Utc))
                        .num_seconds()
                        .max(0) as u64
                });
            mj_core::suspend_consent::ExistingCheckpointSummary {
                verified,
                created_at: Some(checkpoint.created_at.clone()),
                age_seconds,
            }
        });

        let mut preflight = mj_core::suspend_consent::SuspendPreflight {
            session_id: session_id.to_owned(),
            children_not_handed_back,
            existing_checkpoint,
            ..Default::default()
        };

        if without_checkpoint {
            return Ok((preflight, Vec::new()));
        }

        let checkout = self.state.checkout(session_id)?;
        let export_policy =
            self.explicit_suspend_export_policy_with_executor(session_id, executor)?;

        let is_remote_target = session.target.as_deref().is_some_and(|locator| {
            !matches!(locator, mj_core::state::TargetLocator::LocalBare { .. })
        });

        let repo_specs: Vec<mj_core::suspend_consent::WorkerPreflightRepo> = match export_policy {
            CheckpointExportPolicy::OriginDelta => match checkout {
                mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                    vec![mj_core::suspend_consent::WorkerPreflightRepo {
                        id: super::checkpoint::HOST_CHECKOUT_REPOSITORY.to_owned(),
                        path: worktree.worktree_root.clone(),
                        raw: false,
                        exclude_commits: None,
                        keep_dir: Some(mj_checkpoint::checkpoint::checkout_keep_dir(
                            &worktree.worktree_root,
                        )),
                        origin_override: origin_delta::managed_origin_override(worktree),
                    }]
                }
                mj_core::state::Checkout::ManagedWorkspace => {
                    let bundle = session
                        .project_bundle(&self.config)
                        .context("session bundle is missing")?;
                    let (backend, _) = self.worker_placement(session_id)?;
                    let workspace_root =
                        PathBuf::from(crate::controller::network_git::workspace_root(
                            &backend,
                            session.container_workspace.as_deref(),
                        ));
                    bundle
                        .repositories
                        .iter()
                        .map(|repository| -> Result<_> {
                            let path = workspace_root.join(&repository.destination);
                            Ok(mj_core::suspend_consent::WorkerPreflightRepo {
                                id: repository.id.clone(),
                                keep_dir: Some(mj_checkpoint::checkpoint::checkout_keep_dir(&path)),
                                path,
                                raw: false,
                                exclude_commits: None,
                                origin_override: None,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?
                }
                _ => Vec::new(),
            },
            CheckpointExportPolicy::OriginDeltaCutoff => {
                let cutoff = super::backend::session_history_cutoff(&session)?;
                let mut exclude_commits = Vec::new();
                if let Some(cutoff) = cutoff {
                    exclude_commits.push(cutoff.head);
                    if let Some(base) = cutoff.base
                        && !exclude_commits.contains(&base)
                    {
                        exclude_commits.push(base);
                    }
                }
                match checkout {
                    mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                        let path = worktree.worktree_root.clone();
                        vec![mj_core::suspend_consent::WorkerPreflightRepo {
                            id: super::checkpoint::HOST_CHECKOUT_REPOSITORY.to_owned(),
                            keep_dir: Some(mj_checkpoint::checkpoint::checkout_keep_dir(&path)),
                            path,
                            raw: false,
                            exclude_commits: Some(exclude_commits),
                            origin_override: None,
                        }]
                    }
                    mj_core::state::Checkout::Attached { path } => {
                        let path = path.to_path_buf();
                        vec![mj_core::suspend_consent::WorkerPreflightRepo {
                            id: super::checkpoint::HOST_CHECKOUT_REPOSITORY.to_owned(),
                            keep_dir: Some(mj_checkpoint::checkpoint::checkout_keep_dir(&path)),
                            path,
                            raw: false,
                            exclude_commits: Some(exclude_commits),
                            origin_override: None,
                        }]
                    }
                    mj_core::state::Checkout::ManagedWorkspace => {
                        if let Some(bundle) = session.project_bundle(&self.config) {
                            let (backend, _) = self.worker_placement(session_id)?;
                            let workspace_root =
                                PathBuf::from(crate::controller::network_git::workspace_root(
                                    &backend,
                                    session.container_workspace.as_deref(),
                                ));
                            bundle
                                .repositories
                                .iter()
                                .map(|repository| -> Result<_> {
                                    let path = workspace_root.join(&repository.destination);
                                    Ok(mj_core::suspend_consent::WorkerPreflightRepo {
                                        id: repository.id.clone(),
                                        keep_dir: Some(
                                            mj_checkpoint::checkpoint::checkout_keep_dir(&path),
                                        ),
                                        path,
                                        raw: false,
                                        exclude_commits: Some(exclude_commits.clone()),
                                        origin_override: None,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?
                        } else {
                            let fallback_id = session
                                .checkout
                                .as_ref()
                                .map(|exact| exact.repository_id.clone())
                                .unwrap_or_else(|| "workspace".to_owned());
                            session
                                .target
                                .as_deref()
                                .and_then(|locator| {
                                    backend_locator(locator, &session, &self.config).ok().map(
                                        |backend| {
                                            PathBuf::from(targets::workspace_root(&backend, None))
                                        },
                                    )
                                })
                                .into_iter()
                                .map(|path| -> Result<_> {
                                    Ok(mj_core::suspend_consent::WorkerPreflightRepo {
                                        id: fallback_id.clone(),
                                        keep_dir: Some(
                                            mj_checkpoint::checkpoint::checkout_keep_dir(&path),
                                        ),
                                        path,
                                        raw: false,
                                        exclude_commits: Some(exclude_commits.clone()),
                                        origin_override: None,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?
                        }
                    }
                    mj_core::state::Checkout::Borrowed { .. } => Vec::new(),
                }
            }
            CheckpointExportPolicy::OriginDeltaRaw => checkout
                .project_directory()
                .into_iter()
                .map(|dir| mj_core::suspend_consent::WorkerPreflightRepo {
                    id: super::checkpoint::HOST_CHECKOUT_REPOSITORY.to_owned(),
                    path: dir.to_path_buf(),
                    raw: true,
                    exclude_commits: None,
                    keep_dir: None,
                    origin_override: None,
                })
                .collect(),
            _ => Vec::new(),
        };

        if !repo_specs.is_empty() {
            if is_remote_target {
                let locator = session
                    .target
                    .as_deref()
                    .context("remote target locator missing")?;
                let backend = backend_locator(locator, &session, &self.config)?;
                let worker_spec = mj_core::suspend_consent::WorkerSuspendPreflightSpec {
                    session_id: session_id.to_owned(),
                    repositories: repo_specs.clone(),
                    dry_run,
                };
                let spec_bytes = serde_json::to_vec(&worker_spec)?;
                let worker_result: mj_core::suspend_consent::WorkerSuspendPreflightResult =
                    mj_core::runtime::off_async_worker(|| -> Result<_> {
                        let command = crate::checkpoint_transfer::suspend_preflight_stdin_command(
                            &backend, session_id,
                        )?;
                        let mut reader = std::io::Cursor::new(&spec_bytes);
                        let mut output = executor.execute_with_stdin(&command, &mut reader)?;
                        if output.status != 0 {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            if stderr.contains("unrecognized subcommand")
                                && stderr.contains("suspend-preflight")
                            {
                                // Pre-upgrade SSH/container workers lack `suspend-preflight`.
                                // Stage the current worker binary beside `{root}/hel` without
                                // disturbing the running worker, run preflight once, and let
                                // `PreparedWorkerBinary`'s drop guard remove the temporary copy.
                                let worker = super::worker_binary::WorkerInstall::for_target(
                                    &backend, executor,
                                )
                                .context("resolve current worker binary for suspend preflight")?;
                                let staging =
                                    super::worker_binary::stage_worker_binary_for_upgrade(
                                        executor, &backend, session_id, &worker,
                                    )
                                    .context("stage current worker binary for suspend preflight")?;
                                let staged_command = crate::checkpoint_transfer::suspend_preflight_stdin_command_with_binary(
                                    &backend,
                                    session_id,
                                    &staging,
                                )?;
                                let mut retry_reader = std::io::Cursor::new(&spec_bytes);
                                output = executor
                                    .execute_with_stdin(&staged_command, &mut retry_reader)?;
                            }
                        }
                        if output.status != 0 {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            bail!(
                                "target suspend-preflight failed (status {}): {}",
                                output.status,
                                stderr.trim()
                            );
                        }
                        serde_json::from_slice(&output.stdout)
                            .context("decode target suspend-preflight result")
                    })?;
                preflight.untracked = worker_result.untracked;
                preflight.nested_repositories = worker_result.nested_repositories;
                preflight.untracked_stashes = worker_result.untracked_stashes;
                preflight.keep_dir_entries = worker_result.keep_dir_entries;
                preflight.keep_dir_bytes = worker_result.keep_dir_bytes;
                preflight.operations = worker_result.operations;
                preflight.raw = worker_result.raw;
                preflight.blockers = worker_result.blockers;
                preflight.dry_run = worker_result.dry_run;
            } else {
                let git = super::worktree::ExecutorGit::local(executor);
                let mut git_delta_total: Option<u64> = if dry_run { Some(0) } else { None };
                let mut git_delta_unavailable: Option<String> = None;
                let mut total_keep_bytes: u64 = 0;

                mj_core::runtime::off_async_worker(|| -> Result<_> {
                    for repo in &repo_specs {
                        let mut input = if repo.raw {
                            mj_checkpoint::checkpoint::raw_snapshot_input(
                                &repo.id,
                                &repo.path,
                                None,
                                None,
                                PathBuf::new(),
                            )
                        } else if let Some(exclude_commits) = &repo.exclude_commits {
                            mj_checkpoint::checkpoint::cutoff_snapshot_input(
                                &repo.id,
                                &repo.path,
                                Some(session_id),
                                exclude_commits.clone(),
                                PathBuf::new(),
                            )?
                        } else {
                            mj_checkpoint::checkpoint::managed_clone_snapshot_input(
                                &repo.id,
                                &repo.path,
                                Some(session_id),
                                PathBuf::new(),
                                repo.origin_override.clone(),
                            )?
                        };
                        input.keep_dir = repo.keep_dir.clone();
                        let inventories = mj_checkpoint::snapshot::session_inventory(
                            &git,
                            std::slice::from_ref(&input),
                        )?;
                        let (_, inventory) = inventories
                            .into_iter()
                            .next()
                            .context("session inventory returned no repository")?;
                        if let Some(keep_dir) = &input.keep_dir {
                            let (keep_entries, keep_bytes) =
                                mj_checkpoint::snapshot::scan_keep_dir(
                                    &inventory.checkout,
                                    keep_dir,
                                )?;
                            preflight.keep_dir_entries.extend(keep_entries);
                            total_keep_bytes = total_keep_bytes.saturating_add(keep_bytes);
                        }
                        if dry_run && git_delta_unavailable.is_none() {
                            if let Some(op) = &inventory.operation {
                                git_delta_total = None;
                                git_delta_unavailable = Some(format!(
                                    "Git size estimate unavailable during {}",
                                    op.kind
                                ));
                            } else {
                                match mj_checkpoint::snapshot::estimate_repository_delta(
                                    &git, &input,
                                ) {
                                    Ok(est) => {
                                        if let (Some(acc), Some(bytes)) =
                                            (git_delta_total, est.total_bytes)
                                        {
                                            git_delta_total = Some(acc.saturating_add(bytes));
                                        } else {
                                            git_delta_total = None;
                                        }
                                    }
                                    Err(err) => {
                                        git_delta_total = None;
                                        git_delta_unavailable =
                                            Some(format!("Git size estimate unavailable: {err}"));
                                    }
                                }
                            }
                        }

                        preflight.untracked.extend(inventory.untracked);
                        preflight
                            .nested_repositories
                            .extend(inventory.nested_repositories);
                        preflight.untracked_stashes.extend(
                            inventory.untracked_stashes.into_iter().map(|s| {
                                mj_core::suspend_consent::SuspendUntrackedStash {
                                    checkout: s.checkout,
                                    commit: s.value.commit,
                                    message: s.value.message,
                                }
                            }),
                        );
                        if let Some(op) = inventory.operation {
                            preflight.operations.push(op);
                        }
                        preflight.raw |= inventory.raw;
                        preflight
                            .blockers
                            .extend(inventory.blockers.into_iter().map(|b| {
                                mj_core::suspend_consent::SuspendBlocker {
                                    kind: format!("{:?}", b.kind),
                                    paths: b
                                        .paths
                                        .into_iter()
                                        .map(|p| p.value.display().to_string())
                                        .collect(),
                                    remedy: b.remedy,
                                }
                            }));
                    }
                    Ok(())
                })?;

                preflight.keep_dir_bytes = total_keep_bytes;
                if dry_run {
                    preflight.dry_run = Some(mj_core::suspend_consent::DryRunEstimate {
                        git_delta_bytes: git_delta_total,
                        git_delta_unavailable_reason: git_delta_unavailable,
                        session_state_bytes: None,
                        keep_dir_bytes: total_keep_bytes,
                        transfer_route: None,
                    });
                }
            }
        } else if dry_run {
            preflight.dry_run = Some(mj_core::suspend_consent::DryRunEstimate {
                git_delta_bytes: None,
                git_delta_unavailable_reason: None,
                session_state_bytes: None,
                keep_dir_bytes: 0,
                transfer_route: None,
            });
        }

        let mut skip_keep_sizes = Vec::new();
        if let Some(keep) = move_keep {
            skip_keep_sizes.extend(
                keep.skipped_paths()
                    .iter()
                    .cloned()
                    .map(|path| (path, 0_u64)),
            );
            let keep_roots = repo_specs
                .iter()
                .filter_map(|repo| Some((repo.id.as_str(), repo.keep_dir.as_deref()?)))
                .collect::<std::collections::BTreeMap<_, _>>();
            preflight.keep_dir_entries.retain(|entry| {
                let Some(root) = keep_roots.get(entry.checkout.repository_id.as_str()) else {
                    return true;
                };
                if let Some(relative) = keep.relative_path(root, &entry.path) {
                    for (skip, bytes) in &mut skip_keep_sizes {
                        if relative.starts_with(skip) {
                            *bytes = bytes.saturating_add(entry.bytes);
                        }
                    }
                }
                !keep.carries(root, &entry.path)
            });
            preflight.keep_dir_bytes = preflight
                .keep_dir_entries
                .iter()
                .map(|entry| entry.bytes)
                .sum();
            if let Some(estimate) = &mut preflight.dry_run {
                estimate.keep_dir_bytes = preflight.keep_dir_bytes;
            }
        }
        Ok((preflight, skip_keep_sizes))
    }

    pub fn preflight_origin_delta_suspension(
        &self,
        session_id: &str,
        consent: &mj_core::suspend_consent::SuspendConsent,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.check_origin_delta_preflight(session_id, consent, None, executor)
    }

    fn check_origin_delta_preflight(
        &self,
        session_id: &str,
        consent: &mj_core::suspend_consent::SuspendConsent,
        move_keep: Option<&mj_core::session_keep::MoveKeep>,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if consent.without_checkpoint {
            return Ok(());
        }
        if !matches!(
            self.explicit_suspend_export_policy_with_executor(session_id, executor)?,
            CheckpointExportPolicy::OriginDelta | CheckpointExportPolicy::OriginDeltaCutoff
        ) {
            return Ok(());
        }
        let (preflight, _) =
            self.checkpoint_preflight(session_id, false, false, move_keep, executor)?;
        if preflight.has_blockers() {
            let summary = preflight
                .blockers
                .iter()
                .map(|b| b.remedy.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(mj_core::refusal::Refusal::precondition(summary).into());
        }
        for op in &preflight.operations {
            if !consent.abort_operations.covers(op) {
                return Err(mj_core::refusal::Refusal::precondition(format!(
                    "in-progress {} requires `--abort-operation` consent before suspending",
                    op.kind
                ))
                .into());
            }
        }
        let mut uncovered_paths = Vec::new();
        for item in match preflight.shown_untracked_consent() {
            mj_core::suspend_consent::UntrackedConsent::Shown(items) => items,
            _ => Vec::new(),
        } {
            if !consent.discard_untracked.covers(&item) {
                let display = match &item {
                    mj_core::suspend_consent::UntrackedItem::Path { path, .. }
                    | mj_core::suspend_consent::UntrackedItem::NestedRepository { path, .. } => {
                        path.display().to_string()
                    }
                    mj_core::suspend_consent::UntrackedItem::KeepDirFile { path, .. } => {
                        format!(".mj/keep/{}", path.display())
                    }
                    mj_core::suspend_consent::UntrackedItem::Stash {
                        commit, message, ..
                    } => {
                        format!("stash {} ({message})", &commit[..commit.len().min(8)])
                    }
                };
                uncovered_paths.push(display);
            }
        }
        if !uncovered_paths.is_empty() {
            let format_refusal = if move_keep.is_some() {
                mj_core::suspend_consent::format_move_untracked_refusal
            } else {
                mj_core::suspend_consent::format_untracked_refusal
            };
            return Err(mj_core::refusal::Refusal::precondition(format_refusal(
                uncovered_paths.len(),
                &uncovered_paths,
            ))
            .into());
        }
        Ok(())
    }

    pub fn record_origin_delta_suspend_acceptance(&self, session_id: &str) -> Result<()> {
        if matches!(
            self.explicit_suspend_export_policy_with_executor(session_id, &ProcessExecutor)?,
            CheckpointExportPolicy::OriginDelta | CheckpointExportPolicy::OriginDeltaCutoff
        ) {
            OriginDeltaSuspend::load(self, session_id, &ProcessExecutor)?
                .accept(&ProcessExecutor)?;
        }
        Ok(())
    }

    /// Checkpoint, ask the harness to close, and only then tear down the exact
    /// provisioned target. Checkpoint failure is deliberately non-destructive,
    /// except when the checkpoint's worker restart left no live worker: that
    /// records `Error` and keeps the target for a later resume or forced close.
    pub async fn suspend_session(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
    ) -> Result<()> {
        self.suspend_session_controlled(admission, session_id, &ProcessExecutor)
            .await
    }

    pub async fn suspend_session_controlled(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if self.state.subagents.contains_key(session_id) {
            return self
                .suspend_subagent_controlled(admission, session_id, executor, None, None)
                .await
                .map(|_| ());
        }
        let export_policy =
            self.explicit_suspend_export_policy_with_executor(session_id, executor)?;
        let default_consent = mj_core::suspend_consent::SuspendConsent::default();
        if self
            .suspend_session_controlled_with_manager(
                admission,
                session_id,
                executor,
                None,
                None,
                SourceTargetDisposition::Destroy,
                export_policy,
                &default_consent,
                None,
                None,
                LatchExclusivity::HoldThroughClose,
                None,
            )
            .await?
        {
            self.cleanup_stopped_target(session_id, executor)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // Session ownership is an explicit capability.
    pub async fn suspend_session_managed_controlled(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        consent: &mj_core::suspend_consent::SuspendConsent,
        before_close: Option<BeforeClose>,
    ) -> Result<bool> {
        if consent.without_checkpoint {
            return crate::worker_lifecycle::run_admitted(admission, session_id, async {
                self.verify_existing_checkpoint_for_without_checkpoint(
                    session_id,
                    consent.confirm_no_checkpoint,
                    executor,
                )?;
                if let Some(before_close) = before_close
                    && let Err(error) = before_close.await
                {
                    if executor.cancellation_requested() {
                        return Err(error);
                    }
                    let warning = format!(
                        "Stopping sub-agents before suspend without checkpoint failed for session {session_id}: {error:#}. Continuing best-effort teardown."
                    );
                    tracing::warn!(session_id, warning, "suspend without checkpoint warning");
                    executor.notify_notice(&warning);
                }
                let outcome = self.suspend_without_checkpoint(
                    session_id,
                    consent.confirm_no_checkpoint,
                    executor,
                )?;
                for warning in &outcome.warnings {
                    tracing::warn!(session_id, warning, "suspend without checkpoint warning");
                    executor.notify_notice(warning);
                }
                Ok(outcome.deferred)
            })
            .await;
        }
        let export_policy =
            self.explicit_suspend_export_policy_with_executor(session_id, executor)?;
        self.suspend_session_controlled_with_manager(
            admission,
            session_id,
            executor,
            Some(manager),
            None,
            SourceTargetDisposition::Destroy,
            export_policy,
            consent,
            before_close,
            None,
            LatchExclusivity::HoldThroughClose,
            None,
        )
        .await
    }

    /// Seal a session handoff while retaining the existing target and full
    /// recovery checkpoint. Restart keeps the profile and destination.
    ///
    /// An `automatic` Restart, which nobody asked for, never interrupts work:
    /// a session that is working makes it fail with a deferral instead (see
    /// [`super::checkpoint::checkpoint_was_deferred`]) and stays running.
    pub async fn suspend_session_for_restart(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        before_close: Option<BeforeClose>,
        automatic: bool,
    ) -> Result<()> {
        self.suspend_session_controlled_with_manager(
            admission,
            session_id,
            executor,
            Some(manager),
            None,
            SourceTargetDisposition::RetainForInPlaceSwap,
            if automatic {
                CheckpointExportPolicy::Always
            } else {
                CheckpointExportPolicy::SessionHandoff
            },
            &mj_core::suspend_consent::SuspendConsent::default(),
            before_close,
            None,
            if automatic {
                LatchExclusivity::HoldThroughIdleClose
            } else {
                LatchExclusivity::HoldThroughClose
            },
            None,
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn suspend_session_for_move(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        operation: &mut mj_core::state::MoveOperation,
        preparation: Option<&mj_core::state::MovePreparation>,
        disposition: SourceTargetDisposition,
        mut source_relay: super::move_session::MoveSourceRelay,
    ) -> Result<bool> {
        let retained = source_relay.owner();
        crate::worker_lifecycle::run_with_owner(
            admission,
            session_id,
            "suspend session for move",
            executor,
            retained,
            async {
                self.prepare_move_source_checkpoint(
                    admission,
                    session_id,
                    executor,
                    manager,
                    operation,
                    &mut source_relay,
                )
                .await?;
                let export_policy = if operation.in_place
                    && disposition == SourceTargetDisposition::RetainForInPlaceSwap
                {
                    CheckpointExportPolicy::SessionHandoff
                } else if !operation.in_place {
                    // Cross-target Move captures with the same OriginDelta
                    // quiescence and policy as suspend, keeping the archive on
                    // the source target for direct streaming to destination.
                    self.explicit_suspend_export_policy_with_executor(session_id, executor)?
                } else {
                    CheckpointExportPolicy::ReuseUnchangedArchive
                };
                let exclusivity = if operation.quota_stop_reason.is_some() {
                    LatchExclusivity::HoldThroughQuotaClose
                } else {
                    LatchExclusivity::HoldThroughClose
                };
                let consent = operation.selection.consent.clone();
                self.suspend_session_controlled_with_manager(
                    admission,
                    session_id,
                    executor,
                    Some(manager),
                    Some((operation, preparation)),
                    disposition,
                    export_policy,
                    &consent,
                    None,
                    source_relay.take(),
                    exclusivity,
                    None,
                )
                .await
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn suspend_session_controlled_with_manager(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        mut move_intent: Option<(
            &mut mj_core::state::MoveOperation,
            Option<&mj_core::state::MovePreparation>,
        )>,
        disposition: SourceTargetDisposition,
        checkpoint_export_policy: CheckpointExportPolicy,
        consent: &mj_core::suspend_consent::SuspendConsent,
        before_close: Option<BeforeClose>,
        mut held_relay: Option<super::checkpoint::ControllerRelayLease>,
        exclusivity: LatchExclusivity,
        harness_failure: Option<&mj_core::relay::WorkerHarnessStartupFailure>,
    ) -> Result<bool> {
        crate::worker_lifecycle::run_admitted(admission, session_id, async {
            crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
        let previous = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let is_origin_delta = matches!(
            checkpoint_export_policy,
            CheckpointExportPolicy::OriginDelta | CheckpointExportPolicy::OriginDeltaCutoff
        );
        let yield_origin_delta_preflight = move_intent.is_some() && is_origin_delta;
        if yield_origin_delta_preflight {
            executor.begin_resumable_work()?;
        }
        let origin_delta = is_origin_delta
            .then(|| OriginDeltaSuspend::load(self, session_id, executor))
            .transpose();
        let preflight_result = (|| -> Result<_> {
            let mut capture_consent = consent.capture_consent();
            if is_origin_delta
                && let Some((operation, _)) = move_intent.as_ref()
                && !operation.in_place
            {
                capture_consent.move_keep =
                    Some(self.move_keep_selection(&previous, &operation.selection, executor)?);
            }
            if let Ok(Some(attempt)) = &origin_delta
                && (!attempt.accepted() || (before_close.is_none() && !attempt.request_recorded()))
            {
                self.check_origin_delta_preflight(
                    session_id, consent, capture_consent.move_keep.as_ref(), executor,
                )?;
            }
            Ok(capture_consent)
        })();
        if yield_origin_delta_preflight {
            executor.end_resumable_work("persist Move source stop")?;
        }
        let mut origin_delta = origin_delta?;
        let legacy_closing_intent = previous.state == SessionState::Closing
            && origin_delta.as_ref().is_some_and(|attempt| !attempt.accepted());
        let capture_consent = match preflight_result {
            Ok(consent) => consent,
            Err(error) => {
                let error = if legacy_closing_intent {
                    mj_core::refusal::Refusal::precondition(format!(
                        "suspend requested before the upgrade needs consent: {error:#}"
                    ))
                    .into()
                } else {
                    error
                };
                if previous.state == SessionState::Closing {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    apply_close_checkpoint_failure(record, &previous, &error, now(), harness_failure);
                    return Err(self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error));
                }
                return Err(error);
            }
        };
        if let Some(attempt) = origin_delta.as_mut()
            && !attempt.accepted()
        {
            attempt.accept(executor)?;
        }
        let record = self.state.sessions.get_mut(session_id).unwrap();
        // Persist the close intent before beginning its checkpoint. A process
        // exit anywhere below must leave enough state for the next controller
        // to retry the close, even when no checkpoint has been installed yet.
        apply_close_checkpoint_started(record, now());
        self.persist_session_transition_or_restore(
            session_id,
            &previous,
            "persist closing state before checkpointing the session",
        )?;

        if let Some(attempt) = origin_delta.as_mut() {
            if mj_core::test_hooks::test_hooks_enabled() {
                mj_core::runtime::off_async_worker(|| {
                    mj_core::test_hooks::reach_test_hook("origin_delta_after_preflight")
                })?;
            }
            // An accepted suspend owns this Closing record. The worker's
            // receipt carries the stop across a controller handoff.
            if let Some(reason) = attempt.restart_reason(executor)? {
                if attempt.request_recorded() {
                    attempt.wait_for_stop(executor).await?;
                }
                drop(held_relay.take());
                return Err(self.restart_after_refused_origin_delta(
                    admission, session_id, executor, manager, attempt, &previous, anyhow::anyhow!(reason),
                ).await);
            }
            if before_close.is_some() {
                executor.begin_resumable_work()?;
                let children = executor.before_move_source_stop().await;
                executor.end_resumable_work("start managed clone quiescence")?;
                if let Err(error) = children {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    apply_close_checkpoint_failure(record, &previous, &error, now(), harness_failure);
                    return Err(self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error));
                }
            }
            if !attempt.request_recorded() && !attempt.worker_capable(executor)? {
                // The upgrade takes its own idle lease to replace the worker.
                if let Some(relay) = held_relay.take() {
                    relay.release();
                }
                let mut already_recovered_worker = false;
                let recovery_result: Result<()> = if let Some(pending) =
                    crate::database::load_worker_restart(session_id)?
                    && self.state.sessions[session_id].target.as_deref() == Some(&pending.target)
                {
                    // A prior suspend worker swap crashed mid-flight. Finish or
                    // recover it through `start_worker` (reattaching a live
                    // replacement or spawning the installed binary if the old
                    // worker was stopped) before checking whether an upgrade is
                    // still needed.
                    match attempt
                        .start_worker(
                            self,
                            admission,
                            session_id,
                            executor,
                            manager,
                            mj_core::worker_launch::WorkerRunMode::Harness,
                        )
                        .await
                    {
                        Ok(relay) => {
                            relay.release();
                            retire_worker_restart_intent(session_id)
                        }
                        Err(error) => {
                            already_recovered_worker = true;
                            Err(error)
                        }
                    }
                } else {
                    Ok(())
                };
                let upgrade_result = match recovery_result {
                    Err(error) => Err(error),
                    Ok(()) if attempt.worker_capable(executor)? => Ok(()),
                    Ok(()) => {
                        if let Some(manager) = manager {
                            self.upgrade_session_worker_for_suspend(
                                admission, session_id, executor, manager,
                            )
                            .await
                        } else {
                            Err(anyhow::anyhow!(
                                "older worker cannot be upgraded for origin-delta suspend without a session manager"
                            ))
                        }
                    }
                };
                if let Err(error) = upgrade_result {
                    let final_error = if !already_recovered_worker
                        && WorkerRestartLeftNoWorker::marks(&error)
                    {
                        // Swap failed after stopping the old worker: attempt ONE
                        // in-place recovery restart with the installed binary.
                        match attempt
                            .start_worker(
                                self,
                                admission,
                                session_id,
                                executor,
                                manager,
                                mj_core::worker_launch::WorkerRunMode::Harness,
                            )
                            .await
                        {
                            Ok(relay) => {
                                relay.release();
                                if let Err(cleanup) = retire_worker_restart_intent(session_id) {
                                    tracing::warn!(
                                        session_id,
                                        %cleanup,
                                        "could not clear worker restart intent after in-place recovery"
                                    );
                                }
                                // Recovery succeeded: strip WorkerRestartLeftNoWorker so
                                // apply_close_checkpoint_failure leaves the session Running.
                                anyhow::anyhow!("{error:#}")
                            }
                            Err(recovery_error) => error
                                .context(format!(
                                    "in-place worker recovery also failed; retry suspend or use --without-checkpoint: {recovery_error:#}"
                                ))
                                .context(WorkerRestartLeftNoWorker),
                        }
                    } else {
                        error
                    };
                    if let Err(cleanup) = attempt.clear_after_restart(executor) {
                        tracing::warn!(
                            session_id,
                            %cleanup,
                            "could not clear worker suspend records after failed worker upgrade"
                        );
                    }
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    apply_close_checkpoint_failure(
                        record,
                        &previous,
                        &final_error,
                        now(),
                        harness_failure,
                    );
                    return Err(self.persist_failed_checkpoint_state_or_restore(
                        session_id,
                        &previous,
                        final_error,
                    ));
                }
                if !attempt.worker_capable(executor)? {
                    let error = anyhow::anyhow!(
                        "upgraded worker did not advertise origin-delta suspend capability"
                    );
                    if let Err(cleanup) = attempt.clear_after_restart(executor) {
                        tracing::warn!(
                            session_id,
                            %cleanup,
                            "could not clear worker suspend records after missing capability"
                        );
                    }
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    apply_close_checkpoint_failure(record, &previous, &error, now(), harness_failure);
                    return Err(self.persist_failed_checkpoint_state_or_restore(
                        session_id,
                        &previous,
                        error,
                    ));
                }
            }
            if !attempt.request_recorded() {
                // Move hands us its exclusive source lease. Keep that same
                // connection through interruption and worker quiescence.
                if held_relay.is_none() && let Some(manager) = manager {
                    let handle = manager.wait_for_session(session_id, Duration::from_secs(5)).await?;
                    let lease = handle.lease_connection(admission).await?;
                    held_relay = Some(ControllerRelayLease::Managed {
                        handle,
                        lease: Some(lease),
                    });
                }
                if let Some(relay) = held_relay.as_mut()
                    && relay.sync_snapshot().await?.operational.execution == RelayExecutionState::Running
                {
                    relay.submit(new_command_id("suspend-interrupt")?, RelayCommand::CancelTurn).await?;
                }
            }
            if let Err(error) = attempt.request_stop(executor) {
                // If delivery was lost after the durable request was written,
                // recovery will ask its owner for the stop receipt.
                if !OriginDeltaSuspend::load(self, session_id, executor)?.request_recorded() {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    apply_close_checkpoint_failure(record, &previous, &error, now(), harness_failure);
                    return Err(self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error));
                }
                tracing::warn!(session_id, %error, "waiting for the worker's durable suspend request");
            }
            let receipt = attempt.wait_for_stop(executor).await?;
            // The receipt proves this worker stopped. Drop its stale proxy
            // before adopting the checkpoint-only or recovery connection.
            drop(held_relay.take());
            let post_stop_restart_reason =
                if executor.cancellation_requested() && !executor.shutdown_interrupted() {
                    attempt.restart_reason(&ProcessExecutor)?
                } else {
                    attempt.restart_reason(executor)?
                };
            if let Some(reason) = post_stop_restart_reason {
                return Err(self.restart_after_refused_origin_delta(
                    admission, session_id, executor, manager, attempt, &previous, anyhow::anyhow!(reason),
                ).await);
            }
            if let Some(reason) = receipt.error {
                return Err(self.restart_after_refused_origin_delta(
                    admission, session_id, executor, manager, attempt, &previous,
                    mj_core::refusal::Refusal::precondition(reason).into(),
                ).await);
            }
            held_relay = Some(match attempt.start_worker(
                self, admission, session_id, executor, manager,
                mj_core::worker_launch::WorkerRunMode::CheckpointOnly,
            ).await {
                Ok(relay) => relay,
                Err(error) if executor.shutdown_interrupted() => return Err(error),
                Err(error) => {
                    return Err(self.restart_after_refused_origin_delta(
                        admission, session_id, executor, manager, attempt, &previous, error,
                    ).await);
                }
            });
        }

        let yield_capture = origin_delta.is_some() || move_intent.is_some();
        if yield_capture {
            executor.begin_resumable_work()?;
        }

        // Close seals the relay at the exact latched cursor, so this checkpoint
        // keeps its exclusive connection until the relay reports Closed.
        let delivery = if move_intent.as_ref().is_some_and(|(op, _)| !op.in_place) {
            CheckpointDelivery::KeepOnSourceTarget
        } else {
            CheckpointDelivery::DownloadToDaemon
        };
        let checkpoint = self
            .checkpoint_session_latched_for_operation_with_delivery(
                admission,
                session_id,
                executor,
                manager,
                exclusivity,
                checkpoint_export_policy,
                delivery,
                move_intent.as_ref().map(|(operation, _)| operation.operation_id.as_str()),
                held_relay,
                &capture_consent,
            )
            .await;
        if yield_capture {
            executor.end_resumable_work(if origin_delta.is_some() {
                "record managed clone capture outcome"
            } else {
                "record Move source capture outcome"
            })?;
        }
        let mut latched = match checkpoint {
            Ok(latched) => latched,
            Err(error) => {
                if let Some(attempt) = origin_delta.as_ref() {
                    return Err(self.restart_after_refused_origin_delta(
                        admission, session_id, executor, manager, attempt, &previous, error,
                    ).await);
                }
                let record = self.state.sessions.get_mut(session_id).unwrap();
                if exclusivity.close_purpose().is_some()
                    && super::checkpoint::checkpoint_was_deferred(&error)
                {
                    // Nothing failed: the session is working, so the close
                    // nobody asked for stands down and leaves no error behind.
                    record.state = state_after_unsealed_close(&previous);
                    record.last_checkpoint_error = previous.last_checkpoint_error.clone();
                    record.updated_at = now();
                } else {
                    // The target is kept even when the restart left no worker:
                    // a forced destroy and a resume's pre-clean both use it to
                    // tear down the dead container.
                    apply_close_checkpoint_failure(
                        record,
                        &previous,
                        &error,
                        now(),
                        harness_failure,
                    );
                }
                return Err(
                    self.persist_failed_checkpoint_state_or_restore(session_id, &previous, error)
                        .context(if harness_failure.is_some() {
                            LifecycleFailureReason::HarnessUnavailable
                        } else {
                            LifecycleFailureReason::Checkpoint
                        })
                );
            }
        };

        let artifact = latched.artifact.clone();
        // Retained environments are not being published or retired. A profile
        // switch must not wait on the repository's network remote. Move
        // captures never overwrite `session.checkpoint` on the daemon: the
        // daemon copy remains whatever the last suspend produced.
        let session_handoff = checkpoint_export_policy == CheckpointExportPolicy::SessionHandoff;
        let preserves_session_checkpoint = session_handoff || move_intent.is_some();
        let publication = if preserves_session_checkpoint
            || matches!(
                checkpoint_export_policy,
                CheckpointExportPolicy::OriginDelta | CheckpointExportPolicy::OriginDeltaCutoff
            )
            || disposition == SourceTargetDisposition::RetainForInPlaceSwap
        {
            None
        } else {
            let checkout = self.state.checkout(session_id)?;
            match &checkout {
                mj_core::state::Checkout::ManagedWorktree { worktree, .. }
                    if worktree.kind == ManagedCheckoutKind::Clone =>
                {
                    Some(super::publication::assess_clone_checkpoint_for_worktree(
                        worktree,
                        &artifact.metadata,
                    ))
                }
                mj_core::state::Checkout::ManagedWorkspace => {
                    Some(super::publication::assess_network_checkpoint(
                        &previous,
                        &artifact.metadata,
                        &self.config,
                    ))
                }
                mj_core::state::Checkout::Borrowed { .. }
                    if checkout.project_directory().is_none() =>
                {
                    Some(super::publication::assess_network_checkpoint(
                        &previous,
                        &artifact.metadata,
                        &self.config,
                    ))
                }
                mj_core::state::Checkout::Attached { .. }
                | mj_core::state::Checkout::ManagedWorktree { .. }
                | mj_core::state::Checkout::Borrowed { .. } => None,
            }
        };
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Closing;
        record.native_session_id = Some(artifact.native_session_id.clone());
        if !preserves_session_checkpoint {
            record.checkpoint = Some(Box::new(artifact.metadata.clone()));
            record.publication = publication.map(Box::new);
        }
        record.updated_at = now();
        record.last_error = None;
        record.last_checkpoint_error = None;
        if preserves_session_checkpoint {
            if let Some((operation, _)) = move_intent.as_mut() {
                if artifact.on_source_target {
                    let (queued_prompts, mailbox) =
                        artifact.captured_queue.clone().unwrap_or_default();
                    operation.source_snapshot = Some(Box::new(mj_core::state::MoveSourceSnapshot {
                        archive_path: artifact.metadata.archive_path.clone(),
                        sha256: artifact.metadata.sha256.clone(),
                        manifest: artifact.manifest.clone(),
                        queued_prompts,
                        mailbox,
                    }));
                    operation.checkpoint = None;
                } else {
                    operation.checkpoint = Some(artifact.metadata.clone());
                }
                operation.updated_at = now();
                crate::database::save_move_operation(operation)?;
            } else {
                let intent = crate::database::load_session_restart(session_id)?
                    .context("Restart handoff has no lifecycle owner")?;
                crate::database::record_session_restart_handoff(
                    session_id,
                    &intent.operation_id,
                    &artifact.metadata,
                )?;
            }
            self.persist_session_transition_or_restore(
                session_id,
                &previous,
                "persist closing state before sealing the session handoff",
            )?;
        } else {
            self.persist_checkpoint_transition_or_restore(
                session_id,
                &previous,
                "persist verified checkpoint and closing state before sealing the relay",
            )?;
        }
        if let Some((operation, preparation)) = move_intent.as_mut() {
            // The source is still behind an unsealed barrier. A destination
            // preflight error must release it and leave its processes alive.
            let validation = match self
                .github_token_for_repository_preflight(session_id)
                .await
            {
                Ok(token) => self.validate_move_checkpoint(
                    operation,
                    *preparation,
                    token.as_deref(),
                    executor,
                ),
                Err(error) => Err(error),
            };
            if let Err(error) = validation {
                if let Err(cancel) = latched.relay.cancel_abandoned_barrier(admission).await {
                    tracing::warn!(
                        session_id,
                        error = format!("{cancel:#}"),
                        "could not release the checkpoint barrier after move preflight failure"
                    );
                }
                if let Some(attempt) = origin_delta.as_ref() {
                    latched.relay.release();
                    return Err(self
                        .restart_after_refused_origin_delta(
                            admission, session_id, executor, manager, attempt, &previous, error,
                        )
                        .await);
                }
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.state = previous.state;
                record.last_error = Some(format!("{error:#}"));
                self.persist_session_transition_or_restore(
                    session_id,
                    &previous,
                    "restore source after move preflight failure",
                )?;
                return Err(error);
            }
        }
        if let Some(before_close) = before_close
            && let Err(error) = before_close.await
        {
            // The session stays live: dropping the barrier resumes dispatch.
            // The record or lifecycle intent keeps the verified archive.
            if let Err(cancel) = latched.relay.cancel_abandoned_barrier(admission).await {
                tracing::warn!(
                    session_id,
                    error = format!("{cancel:#}"),
                    "could not release the checkpoint barrier of a close that did not proceed"
                );
            }
            if let Some(attempt) = origin_delta.as_ref() {
                latched.relay.release();
                return Err(self.restart_after_refused_origin_delta(
                    admission, session_id, executor, manager, attempt, &previous, error,
                ).await);
            }
            let record = self.state.sessions.get_mut(session_id).unwrap();
            record.state = state_after_unsealed_close(&previous);
            // An automatic close that stands down did not fail.
            if exclusivity.close_purpose().is_none()
                || !super::checkpoint::checkpoint_was_deferred(&error)
            {
                record.last_error = Some(format!("{error:#}"));
            }
            record.updated_at = now();
            self.persist_session_transition_or_restore(
                session_id,
                &previous,
                "restore a session whose close did not proceed past its checkpoint",
            )?;
            return Err(error);
        }
        if !preserves_session_checkpoint {
            prune_replaced_checkpoint(previous.checkpoint.as_deref(), &artifact.metadata);
            // A stopping session will not checkpoint again, so this is its last
            // chance to release what its checkpoint now covers.
            release_projection_behind_checkpoint(session_id, &artifact.metadata);
        }

        // Cross-target OriginDelta Move keeps the source relay UNSEALED and
        // stops only the temporary `CheckpointOnly` worker: all session
        // processes are already stopped by `OriginDeltaSuspend` quiescence, so
        // nothing writes during transfer, and a failure before DestinationOwns
        // can tear down the destination and restart the source agent in place.
        if origin_delta.is_some()
            && let Some((operation, _)) = move_intent.as_mut()
            && !operation.in_place
        {
            if let Err(cancel) = latched.relay.cancel_abandoned_barrier(admission).await {
                tracing::warn!(
                    session_id,
                    error = format!("{cancel:#}"),
                    "could not cancel checkpoint barrier on cross-target Move source"
                );
            }
            latched.relay.release();
            if let Ok((backend, _)) = self.worker_placement(session_id)
                && let Ok(stop_plan) = targets::stop_worker_plan(
                    &backend,
                    session_id,
                    "stop CheckpointOnly worker after cross-target Move capture",
                )
                && let Err(stop_err) =
                    mj_core::runtime::off_async_worker(|| stop_plan.execute(executor, &super::ControlWorker))
            {
                tracing::warn!(
                    session_id,
                    error = format!("{stop_err:#}"),
                    "could not stop CheckpointOnly worker after cross-target Move capture"
                );
            }
            let mut sealed = previous;
            sealed.state = SessionState::Closing;
            operation.recovery_session = Some(sealed);
            crate::database::save_move_operation(operation)?;
            return Ok(false);
        }

        let close_command_id = new_command_id("close")?;
        let barrier_command_id = latched.barrier_command_id.clone();
        let close_result = {
            let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
            latched
                .relay
                .connection_mut()
                .submit(
                    close_command_id,
                    RelayCommand::Close {
                        barrier_command_id: barrier_command_id.clone(),
                        expected: latched.cursor.clone(),
                    },
                )
                .await
        };
        if let Err(error) = close_result {
            let error = error.context("seal verified checkpoint for close");
            if !close_was_refused(&error) {
                // The Close may have been accepted before the answer was
                // lost, so only the interrupted-close recovery may decide.
                self.record_interrupted_close(session_id, &error)?;
                return Err(error);
            }
            // The worker answered and did not accept the Close, so its relay
            // is not sealed and the session is still live. Nothing about this
            // close can be resumed: release the barrier and return the
            // session to work, keeping the checkpoint it just verified.
            if let Err(cancel) = latched.relay.cancel_abandoned_barrier(admission).await {
                tracing::warn!(
                    session_id,
                    error = format!("{cancel:#}"),
                    "could not release the checkpoint barrier of a refused close"
                );
            }
            let record = self.state.sessions.get_mut(session_id).unwrap();
            record.state = state_after_unsealed_close(&previous);
            record.last_error = Some(format!("{error:#}"));
            record.updated_at = now();
            self.persist_session_transition_or_restore(
                session_id,
                &previous,
                "restore a session whose worker refused its close",
            )?;
            return Err(error);
        }
        let close_result = {
            let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
            latched
                .relay
                .connection_mut()
                .submit(
                    new_command_id("checkpoint-complete")?,
                    RelayCommand::CompleteCheckpoint { barrier_command_id },
                )
                .await
        };
        if let Err(error) = close_result {
            self.record_interrupted_close(session_id, &error)?;
            return Err(error.context("release verified close checkpoint"));
        }
        let close_result = {
            let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
            wait_for_relay_closed(latched.relay.connection_mut()).await
        };
        if let Err(error) = close_result {
            self.record_interrupted_close(session_id, &error)?;
            return Err(error);
        }
        latched.relay.release();

        if let Some((operation, _)) = move_intent {
            let mut sealed = previous;
            sealed.state = SessionState::Closing;
            operation.recovery_session = Some(sealed);
            crate::database::save_move_operation(operation)?;
        }
        if session_handoff || disposition == SourceTargetDisposition::RetainForInPlaceSwap {
            // The record stays `Closing` with its target, and the lifecycle
            // owns the sealed archive. The worker daemon stays running: a crash
            // between here and the in-place restore recovers through
            // `recover_interrupted_close_managed`, which needs the daemon to
            // answer `Closed`.
            return Ok(false);
        }
        match self.destroy_after_verified_checkpoint(session_id, &artifact.metadata, executor) {
            Ok(deferred) => Ok(deferred),
            Err(error) => {
                self.record_interrupted_close(session_id, &error)?;
                Err(error.context(LifecycleFailureReason::Cleanup))
            }
        }

        }).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn restart_after_refused_origin_delta(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        attempt: &OriginDeltaSuspend,
        previous: &SessionRecord,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let recovered = async {
            let cancelled = executor.cancellation_requested() && !executor.shutdown_interrupted();
            if cancelled {
                attempt.record_restart(&error, &ProcessExecutor)?;
            } else {
                attempt.record_restart(&error, executor)?;
            }
            executor.notify_notice("Restarting the agent in the retained checkout");
            if cancelled {
                // Cancellation ends the suspend. The retained checkout still
                // needs its agent back after quiescence finished.
                executor.begin_resumable_work()?;
                let restarted = attempt
                    .start_worker(
                        self,
                        admission,
                        session_id,
                        &ProcessExecutor,
                        manager,
                        mj_core::worker_launch::WorkerRunMode::Harness,
                    )
                    .await;
                executor.end_resumable_work("record restart after cancelled suspend")?;
                restarted?.release();
            } else {
                attempt
                    .start_worker(
                        self,
                        admission,
                        session_id,
                        executor,
                        manager,
                        mj_core::worker_launch::WorkerRunMode::Harness,
                    )
                    .await?
                    .release();
            }
            let record = self
                .state
                .sessions
                .get_mut(session_id)
                .context("session disappeared during suspend")?;
            record.state = state_after_unsealed_close(previous);
            record.last_checkpoint_error = Some(format!("{error:#}"));
            record.updated_at = now();
            self.persist_session_state(session_id)?;
            let cleanup = if cancelled {
                attempt.clear_after_restart(&ProcessExecutor)
            } else {
                attempt.clear_after_restart(executor)
            };
            if let Err(cleanup) = cleanup {
                tracing::warn!(session_id, %cleanup, "could not clear worker suspend records");
            }
            anyhow::Ok(())
        }
        .await;
        match recovered {
            Ok(()) => error,
            Err(restart) => error.context(format!("restart the retained session: {restart:#}")),
        }
    }

    /// Restart a retained cross-target Move source in place after tearing down
    /// a failed or interrupted destination (before `DestinationOwns`).
    ///
    /// Returns `Ok(true)` when the source worker and agent were restarted and
    /// `SessionState::Running` was restored; `Ok(false)` when this session did
    /// not use `OriginDeltaSuspend` quiescence (e.g. a legacy sealed source).
    pub(super) async fn restart_retained_move_source(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        previous: &SessionRecord,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<bool> {
        let Some(record) = self.state.sessions.get(session_id) else {
            return Ok(false);
        };
        if record.target.is_none() || !OriginDeltaSuspend::applies(self, session_id, executor)? {
            return Ok(false);
        }
        let attempt = OriginDeltaSuspend::load(self, session_id, executor)?;
        executor.notify_notice("Restarting the source agent in place after failed Move");
        // The Move owns the source placement until rollback is published.
        // The session manager intentionally excludes that placement while
        // Move owns it. Prove worker readiness on the owner's connection.
        let relay = attempt
            .start_worker(
                self,
                admission,
                session_id,
                executor,
                None,
                mj_core::worker_launch::WorkerRunMode::Harness,
            )
            .await?;
        let record = self
            .state
            .sessions
            .get_mut(session_id)
            .context("session disappeared while restarting Move source")?;
        record.state = state_after_unsealed_close(previous);
        record.updated_at = now();
        self.persist_session_state(session_id)?;
        relay.release();
        if let Err(cleanup) = attempt.clear_after_restart(executor) {
            tracing::warn!(
                session_id,
                %cleanup,
                "could not clear worker suspend records after restarting Move source"
            );
        }
        Ok(true)
    }

    /// Resume the durable closing state after a controller restart. If the
    /// relay had accepted Close, wait for it and destroy through the exact
    /// installed checkpoint gate. If it had not, take a fresh checkpoint;
    /// the previously installed archive may have become stale after EOF
    /// released its barrier.
    ///
    /// The daemon also closes every live session this way, because it marks
    /// the record `Closing` before the close starts. So the fresh checkpoint's
    /// publication check uses `acknowledge_unpublished_work` as the caller
    /// sent it. A relay already sealed was checked by the close that sealed
    /// it, and that close can only go forward.
    ///
    /// A worker that died because its harness could not open the session is
    /// started again without the harness, and the close finishes from there.
    /// A close that then fails stops that worker, and a failed checkpoint
    /// leaves the session in `Error` rather than running on a worker that
    /// cannot run a turn (#1619).
    #[allow(clippy::too_many_arguments)] // Session ownership is an explicit capability.
    pub async fn recover_interrupted_close_managed(
        &mut self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        consent: &mj_core::suspend_consent::SuspendConsent,
        before_close: Option<BeforeClose>,
    ) -> Result<bool> {
        if consent.without_checkpoint {
            return crate::worker_lifecycle::run_admitted(admission, session_id, async {
                self.verify_existing_checkpoint_for_without_checkpoint(
                    session_id,
                    consent.confirm_no_checkpoint,
                    executor,
                )?;
                if let Some(before_close) = before_close
                    && let Err(error) = before_close.await
                {
                    if executor.cancellation_requested() {
                        return Err(error);
                    }
                    let warning = format!(
                        "Stopping sub-agents before suspend without checkpoint failed for session {session_id}: {error:#}. Continuing best-effort teardown."
                    );
                    tracing::warn!(session_id, warning, "suspend without checkpoint warning");
                    executor.notify_notice(&warning);
                }
                let outcome = self.suspend_without_checkpoint(
                    session_id,
                    consent.confirm_no_checkpoint,
                    executor,
                )?;
                for warning in &outcome.warnings {
                    tracing::warn!(session_id, warning, "suspend without checkpoint warning");
                    executor.notify_notice(warning);
                }
                Ok(outcome.deferred)
            })
            .await;
        }
        crate::worker_lifecycle::run_admitted(admission, session_id, async {
        let (state, verified) = {
            let session = self
                .state
                .sessions
                .get(session_id)
                .with_context(|| format!("unknown session {session_id}"))?;
            ensure!(
                matches!(
                    session.state,
                    SessionState::Closing | SessionState::Destroying
                ),
                "session {session_id} has no interrupted close to recover"
            );
            (session.state, session.checkpoint.clone())
        };
        if state == SessionState::Destroying {
            let verified = verified.context("destroying session has no verified checkpoint")?;
            if let Some(before_close) = before_close {
                before_close.await?;
            }
            return self
                .destroy_after_verified_checkpoint(session_id, &verified, executor)
                .context(LifecycleFailureReason::Cleanup);
        }
        ensure!(
            state == SessionState::Closing,
            "session {session_id} has no relay close to recover"
        );
        let started = std::time::Instant::now();
        let mut phase = "wait for session actor";
        tracing::info!(
            session_id,
            ?state,
            checkpoint_frontier = ?verified.as_ref().map(|checkpoint| checkpoint.event_frontier),
            "recovering suspension: asking the worker whether its relay was sealed"
        );
        let connection = async {
            let handle = manager
                .wait_for_session(session_id, Duration::from_secs(5))
                .await?;
            phase = "lease worker connection";
            let lease = handle.lease_connection(admission).await?;
            let mut relay = ControllerRelayLease::Managed {
                handle,
                lease: Some(lease),
            };
            phase = "read worker execution state";
            let execution = relay.connection_mut().sync().await?.operational.execution;
            anyhow::Ok((relay, execution))
        }
        .await;
        let suspend_policy = if before_close.is_some() {
            let policy =
                self.explicit_suspend_export_policy_with_executor(session_id, executor)?;
            matches!(
                policy,
                CheckpointExportPolicy::OriginDelta | CheckpointExportPolicy::OriginDeltaCutoff
            )
            .then_some(policy)
        } else {
            None
        };
        let origin_delta = suspend_policy
            .map(|_| OriginDeltaSuspend::load(self, session_id, executor))
            .transpose()?;
        let accepted_origin_delta = origin_delta.as_ref().is_some_and(OriginDeltaSuspend::accepted);
        let resumable_origin_delta = origin_delta.as_ref().is_some_and(OriginDeltaSuspend::request_recorded);
        let (mut relay, execution, harness_failure) = match connection {
            Ok((relay, execution)) => (relay, execution, None),
            // The worker owns this suspend's durable stop request, and its
            // worker may have exited for the stop: continue that suspend.
            Err(_) if resumable_origin_delta => {
                return Box::pin(self.suspend_session_controlled_with_manager(
                    admission,
                    session_id,
                    executor,
                    Some(manager),
                    None,
                    SourceTargetDisposition::Destroy,
                    suspend_policy.unwrap_or(CheckpointExportPolicy::OriginDelta),
                    consent,
                    before_close,
                    None,
                    LatchExclusivity::HoldThroughClose,
                    None,
                )).await;
            }
            Err(error) => {
                tracing::warn!(
                    session_id,
                    phase,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = format!("{error:#}"),
                    "suspension could not query its worker"
                );
                let probe = self.diagnose_suspension_worker(session_id, phase).await;
                // Only a worker proved dead, whose own exit record says its
                // harness could not open the session, is started again here.
                // Anything else keeps its target and checkpoint untouched.
                let failure = probe
                    .filter(|probe| probe.pids.is_empty() && worker_connect_needs_restart(&error))
                    .and_then(|probe| probe.exit)
                    .and_then(|exit| exit.harness_failed_to_open().cloned());
                let Some(failure) = failure else {
                    return Err(error.context(format!("{phase} while recovering suspension"))
                        .context(LifecycleFailureReason::WorkerContact));
                };
                let (relay, execution) = async {
                    let mut relay = self
                        .reopen_closing_relay_without_harness(
                            admission, session_id, executor, manager, &failure,
                        )
                        .await?;
                    let execution = relay.connection_mut().sync().await?.operational.execution;
                    anyhow::Ok((relay, execution))
                }
                .await
                .with_context(|| {
                    format!("start the worker without its harness, which failed to open: {failure}")
                })
                .context(LifecycleFailureReason::WorkerContact)?;
                (relay, execution, Some(failure))
            }
        };
        tracing::info!(
            session_id,
            ?execution,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "suspension recovery read the worker's execution state"
        );
        let closed = async {
            // Legacy sealed-intent compatibility branch (removable only once
            // the upgrade floor guarantees no older daemon could have left a
            // sealed legacy Closing intent; also serves crashes after seal on
            // the current release): wait out Closing and complete teardown from
            // the already-installed verified checkpoint without any new export.
            match execution {
                RelayExecutionState::Closed => {}
                RelayExecutionState::Closing => {
                    let _closing = ProvisionStageGuard::new(executor, ProvisionStage::Closing);
                    wait_for_relay_closed(relay.connection_mut()).await?;
                }
                RelayExecutionState::Idle | RelayExecutionState::Running => {
                    relay.release();
                    return self
                        .suspend_session_controlled_with_manager(admission,
                            session_id,
                            executor,
                            Some(manager),
                            None,
                            SourceTargetDisposition::Destroy,
                            if accepted_origin_delta {
                                suspend_policy.unwrap_or(CheckpointExportPolicy::OriginDelta)
                            } else {
                                self.explicit_suspend_export_policy_with_executor(
                                    session_id,
                                    executor,
                                )?
                            },
                            consent,
                            before_close,
                            None,
                            LatchExclusivity::HoldThroughClose,
                            harness_failure.as_ref(),
                        )
                        .await;
                }
            }
            relay.release();
            let verified = verified.context("closed relay has no verified checkpoint")?;
            // The relay is sealed already, so this close can only go forward.
            if let Some(before_close) = before_close {
                before_close.await?;
            }
            self.destroy_after_verified_checkpoint(session_id, &verified, executor)
                .context(LifecycleFailureReason::Cleanup)
        }
        .await;
        if closed.is_err() && harness_failure.is_some() {
            // The worker started without its harness existed only to finish
            // this close. Left running, a Resume would adopt it instead of
            // starting the harness again, and a running record would point at
            // a worker that cannot run a turn.
            self.stop_worker_started_without_harness(session_id, executor);
        }
        closed

        }).await
    }

    /// Stop the worker a close started without its harness, once the close
    /// has failed. Failing to stop it does not change the close's outcome; it
    /// is reported here.
    fn stop_worker_started_without_harness(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) {
        let stopped = (|| {
            let (backend, worker_root) = self.worker_placement(session_id)?;
            let owner = crate::worker_lifecycle::require(session_id)?;
            super::worker_binary::stop_worker(&owner, executor, &backend, &worker_root)
        })();
        if let Err(error) = stopped {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not stop the worker a failed close started without its harness; \
                 a Resume may attach to it instead of starting the harness"
            );
        }
    }

    /// Start a closing session's worker again without its harness, so the
    /// close can ask its relay whether it was sealed and checkpoint it if not.
    ///
    /// The worker died because its harness could not open the session, and
    /// starting that harness again would only repeat the failure (#1619). A
    /// close needs only the relay journal and the files on the target, as a
    /// checkpoint whose restarted harness did not come back does (R4-3).
    async fn reopen_closing_relay_without_harness(
        &self,
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: &SessionManagerControl,
        failure: &mj_core::relay::WorkerHarnessStartupFailure,
    ) -> Result<ControllerRelayLease> {
        tracing::warn!(
            session_id,
            %failure,
            "the closing session's harness failed to open; finishing the close without it"
        );
        executor.notify_notice(&format!(
            "Saving the session without starting its harness, which failed to open: {failure}"
        ));
        let (backend, worker_root) = self.worker_placement(session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let connection = self
            .restart_worker_for_checkpoint_only(
                session_id,
                executor,
                &backend,
                &worker_root,
                &reconnect,
            )
            .await?;
        adopt_restarted_checkpoint_relay(admission, session_id, Some(manager), connection).await
    }

    /// A missing socket does not establish worker death or a safe checkpoint
    /// cut. Gather its own records and process state without changing either,
    /// and return them for the caller to judge.
    async fn diagnose_suspension_worker(
        &self,
        session_id: &str,
        phase: &str,
    ) -> Option<super::worker_binary::WorkerProbe> {
        let placement = self.worker_placement(session_id);
        let probe = match placement {
            Ok((backend, worker_root)) => mj_core::runtime::spawn_blocking(move || {
                // A diagnosis reports; it never installs or waits on the store.
                super::worker_binary::diagnose_worker_read_only(
                    &targets::BoundedProcessExecutor::new(Duration::from_secs(5)),
                    &backend,
                    &worker_root,
                )
            })
            .await
            .context("join the suspension worker diagnostic probe")
            .and_then(std::convert::identity),
            Err(error) => Err(error.context("resolve the retained suspension worker")),
        };
        match probe {
            Ok(probe) => {
                tracing::warn!(
                    session_id,
                    phase,
                    worker_pids = ?probe.pids,
                    worker_startup_step = ?probe.step(),
                    worker_state = %probe,
                    "suspension failure worker diagnostic"
                );
                Some(probe)
            }
            Err(error) => {
                tracing::warn!(
                    session_id,
                    phase,
                    error = format!("{error:#}"),
                    "suspension failure worker diagnostic was unavailable; worker state remains unknown"
                );
                None
            }
        }
    }

    /// Record that an in-flight lifecycle state has no operation left to
    /// finish it, so the session stops waiting for one.
    ///
    /// The state is re-checked against the freshly loaded record, because the
    /// caller decided what to reconcile from a startup snapshot. Returns
    /// whether anything changed.
    pub fn fail_interrupted_lifecycle(&mut self, session_id: &str, cause: &str) -> Result<bool> {
        self.fail_interrupted_lifecycle_with(
            session_id,
            cause,
            crate::database::save_lifecycle_session,
        )
    }

    fn fail_interrupted_lifecycle_with(
        &mut self,
        session_id: &str,
        cause: &str,
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<bool> {
        let Some(record) = self.state.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        if crate::controller::interrupted_lifecycle_cause(record).is_none() {
            return Ok(false);
        }
        let previous = record.clone();
        record.state = SessionState::Error;
        record.updated_at = now();
        record.last_error = Some(cause.to_owned());
        persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            &previous,
            "persist the failure of an interrupted lifecycle state",
            &persist,
        )?;
        Ok(true)
    }

    /// Record that a live session's harness never became usable, so a driver
    /// can close it and provision a replacement instead of waiting on a
    /// session that will never take a prompt (#1090).
    ///
    /// Only a record that still looks exactly as the daemon observed it is
    /// failed: a lifecycle operation that started after the observation owns
    /// the session, and its own outcome must not be overwritten by this one.
    /// Returns whether anything changed.
    pub fn fail_unready_session(
        &mut self,
        session_id: &str,
        cause: &str,
        observed_updated_at: &str,
    ) -> Result<bool> {
        let cause = match self.state.sessions.get(session_id) {
            Some(record) => super::provisioning::launch_failure_text(
                &self.config,
                &record.last_profile,
                cause,
                "use Restart agent in the existing session",
            ),
            None => cause.to_owned(),
        };
        let Some(record) = crate::database::fail_unready_session(
            session_id,
            &cause,
            SessionState::Running,
            observed_updated_at,
        )
        .context("persist a session whose harness never became usable")?
        else {
            return Ok(false);
        };
        self.state.sessions.insert(session_id.to_owned(), record);
        Ok(true)
    }

    /// Persist a worker-reported harness failure from an async lifecycle
    /// method that only has a shared controller reference. The revision check
    /// still belongs to the controller loaded from the current store.
    pub(crate) async fn persist_harness_preparation_failure(
        &self,
        session_id: &str,
        cause: &str,
        observed_updated_at: &str,
    ) -> Result<()> {
        let session_id = session_id.to_owned();
        let cause = cause.to_owned();
        let observed_updated_at = observed_updated_at.to_owned();
        mj_core::runtime::spawn_blocking(move || {
            let mut controller = Controller::load()?;
            controller.fail_unready_session(&session_id, &cause, &observed_updated_at)
        })
        .await
        .context("record harness preparation failure task panicked")??;
        Ok(())
    }

    /// Publish a safe lifecycle failure even if a previous internal error exists.
    /// Detailed diagnostics belong in logs; the stored outcome must be visible
    /// on all control surfaces. Reports whether the session still exists.
    pub fn record_failed_close(&mut self, session_id: &str, cause: &str) -> Result<bool> {
        let Some(record) = self.state.sessions.get(session_id) else {
            return Ok(false);
        };
        // The supervisor logs the detailed failure. Always publish this safe
        // outcome, including when an older raw error was already recorded.
        let mut record = record.clone();
        record.last_error = Some(cause.to_owned());
        record.updated_at = now();
        // The writer decides from the durable close owner in the same
        // transaction as failure publication; admission may have been refused.
        let saved = crate::database::save_failed_close(&record)
            .context("persist the reason a close did not finish")?;
        self.state.sessions.insert(session_id.to_owned(), saved);
        Ok(true)
    }

    /// Forget only the public lifecycle failure observed at turn start. Raw
    /// errors and failures recorded after that observation are left alone.
    pub fn clear_recorded_lifecycle_failure(
        &mut self,
        session_id: &str,
        recorded_error: &str,
    ) -> Result<bool> {
        let Some(record) = self.state.sessions.get(session_id) else {
            return Ok(false);
        };
        if record.public_error() != Some(recorded_error) {
            return Ok(false);
        }
        let updated_at = now();
        if !crate::database::clear_recorded_lifecycle_failure(record, &updated_at)? {
            return Ok(false);
        }
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.last_error = None;
        record.updated_at = updated_at;
        Ok(true)
    }

    /// Close a session that has nothing to checkpoint.
    /// Suspend a session without taking a fresh checkpoint:
    /// 1. Verify any existing checkpoint first. If verified, keep it as the
    ///    Resume source; if missing or corrupt, clear it so it is never
    ///    presented as a Resume source.
    /// 2. Stop worker and environment with best-effort cleanup (completes with
    ///    warnings if the target is unreachable; respects cancellation).
    /// 3. Keep the session record, history, and verified checkpoint (if any),
    ///    preserving `target` if cleanup failed or was deferred, and settle as `Stopped`.
    pub fn suspend_without_checkpoint(
        &mut self,
        session_id: &str,
        confirm_no_checkpoint: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<SuspendWithoutCheckpointOutcome> {
        self.suspend_without_checkpoint_with(
            session_id,
            confirm_no_checkpoint,
            executor,
            crate::database::save_lifecycle_session,
        )
    }

    pub fn ensure_without_checkpoint_allowed(
        &self,
        session_id: &str,
        confirm_no_checkpoint: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.verify_existing_checkpoint_for_without_checkpoint(
            session_id,
            confirm_no_checkpoint,
            executor,
        )
        .map(|_| ())
    }

    fn verify_existing_checkpoint_for_without_checkpoint(
        &self,
        session_id: &str,
        confirm_no_checkpoint: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<(
        Option<mj_core::state::CheckpointMetadata>,
        super::worktree::CloneRetirementPrerequisite,
    )> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        ensure!(
            session.state.is_active(),
            "session {session_id} is already inactive"
        );
        let (verified_checkpoint, clone_prerequisite) = match session.checkpoint.as_deref() {
            Some(checkpoint) => {
                let verified = {
                    let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
                    verify_installed_checkpoint_gate(session_id, checkpoint).is_ok()
                };
                if verified {
                    let prereq =
                        super::worktree::clone_retirement_prerequisite_for_checkpoint(checkpoint);
                    (Some(checkpoint.clone()), prereq)
                } else {
                    tracing::warn!(
                        session_id,
                        "existing checkpoint failed verification during suspend --without-checkpoint; clearing it so Resume will not offer it"
                    );
                    (None, super::worktree::CloneRetirementPrerequisite::None)
                }
            }
            None => (None, super::worktree::CloneRetirementPrerequisite::None),
        };
        if verified_checkpoint.is_none() && !confirm_no_checkpoint {
            return Err(mj_core::refusal::Refusal::precondition(
                "No verified checkpoint exists for this session; suspending without a checkpoint cannot be resumed. Confirm explicitly (CLI: pass --yes) to proceed.",
            )
            .into());
        }
        Ok((verified_checkpoint, clone_prerequisite))
    }

    fn suspend_without_checkpoint_with(
        &mut self,
        session_id: &str,
        confirm_no_checkpoint: bool,
        executor: &(impl CommandExecutor + Sync),
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<SuspendWithoutCheckpointOutcome> {
        let (verified_checkpoint, clone_prerequisite) = self
            .verify_existing_checkpoint_for_without_checkpoint(
                session_id,
                confirm_no_checkpoint,
                executor,
            )?;
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        crate::worker_lifecycle::run_blocking(
            session_id,
            "suspend without checkpoint",
            executor,
            || {
                let mut deferred = false;
                let mut cleanup_failed = false;
                let mut warnings = Vec::new();
                if let Some(locator) = &session.target {
                    match backend_locator(locator, &session, &self.config) {
                        Ok(backend) => {
                            let cleanup_result = if self.state.subagents.contains_key(session_id) {
                                super::environment::stop_borrower(
                                    session_id,
                                    &self.config,
                                    executor,
                                )
                                .and_then(|()| retire_worker_restart_intent(session_id))
                            } else if targets::quiesce_plan(&backend, session_id)
                                .ok()
                                .flatten()
                                .is_some()
                            {
                                deferred = true;
                                super::environment::quiesce_session_environment(
                                    &session,
                                    &self.config,
                                    executor,
                                )
                            } else {
                                execute_target_cleanup(
                                    &session,
                                    &self.config,
                                    executor,
                                    clone_prerequisite,
                                )
                            };
                            if let Err(error) = cleanup_result {
                                if executor.cancellation_requested() {
                                    return Err(error);
                                }
                                deferred = false;
                                cleanup_failed = true;
                                tracing::warn!(
                                    session_id,
                                    target_id = %session.target_template_id,
                                    error = format!("{error:#}"),
                                    "best-effort target cleanup failed during suspend --without-checkpoint; settling session as Stopped"
                                );
                                warnings.push(format!(
                                    "Cleanup of the environment on target {} failed during suspend without checkpoint for session {session_id}. Resources may remain; details are in the daemon logs.",
                                    session.target_template_id
                                ));
                            }
                        }
                        Err(error) => {
                            if executor.cancellation_requested() {
                                return Err(error);
                            }
                            cleanup_failed = true;
                            tracing::warn!(
                                session_id,
                                target_id = %session.target_template_id,
                                error = format!("{error:#}"),
                                "could not resolve target backend during suspend --without-checkpoint; settling session as Stopped"
                            );
                            warnings.push(format!(
                                "Cleanup of the environment on target {} failed during suspend without checkpoint for session {session_id}. Resources may remain; details are in the daemon logs.",
                                session.target_template_id
                            ));
                        }
                    }
                }
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.state = SessionState::Stopped;
                record.checkpoint = verified_checkpoint.map(Box::new);
                if !deferred && !cleanup_failed {
                    record.target = None;
                }
                record.updated_at = now();
                record.last_error = None;
                record.last_checkpoint_error = None;
                persist_session_record_transition_or_restore(
                    &mut self.state,
                    session_id,
                    &session,
                    "persist stopped state after suspend without checkpoint",
                    &persist,
                )?;
                Ok(SuspendWithoutCheckpointOutcome { deferred, warnings })
            },
        )
    }

    fn record_interrupted_close(&mut self, session_id: &str, error: &anyhow::Error) -> Result<()> {
        let record = self.state.sessions.get_mut(session_id).unwrap();
        apply_interrupted_close_error(record, error, &now());
        self.persist_session_state(session_id)
    }

    /// Execute cleanup only after the close state machine has installed a
    /// verified checkpoint on the record.
    fn destroy_after_verified_checkpoint(
        &mut self,
        session_id: &str,
        verified: &CheckpointMetadata,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<bool> {
        self.destroy_after_verified_checkpoint_with(
            session_id,
            verified,
            executor,
            crate::database::save_closed_session,
        )
    }

    pub fn destroy_after_verified_checkpoint_with(
        &mut self,
        session_id: &str,
        verified: &CheckpointMetadata,
        executor: &(impl CommandExecutor + Sync),
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<bool> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "destroy after verified checkpoint with",
            executor,
            || {
                let session = self
                    .state
                    .sessions
                    .get(session_id)
                    .with_context(|| format!("unknown session {session_id}"))?
                    .clone();
                ensure!(
                    matches!(
                        session.state,
                        SessionState::Closing | SessionState::Destroying
                    ),
                    "refusing to destroy session {session_id}: it is not closing or destroying"
                );
                ensure!(
                    session.checkpoint.as_deref() == Some(verified),
                    "refusing to destroy session {session_id}: verified checkpoint gate is stale"
                );
                if session.state == SessionState::Closing {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::Destroying;
                    record.updated_at = now();
                    record.last_error = None;
                    persist_session_record_transition_or_restore(
                        &mut self.state,
                        session_id,
                        &session,
                        "persist destroying state before target cleanup",
                        &persist,
                    )?;
                }

                let destroying = self
                    .state
                    .sessions
                    .get(session_id)
                    .expect("destroying session disappeared")
                    .clone();
                {
                    let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
                    verify_installed_checkpoint_gate(session_id, verified)?;
                }
                let locator = destroying
                    .target
                    .as_ref()
                    .context("session has no target")?;
                let backend = backend_locator(locator, &destroying, &self.config)?;
                let deferred = if self.state.subagents.contains_key(session_id) {
                    super::environment::stop_borrower(session_id, &self.config, executor)?;
                    false
                } else if targets::quiesce_plan(&backend, session_id)?.is_some() {
                    super::environment::quiesce_session_environment(
                        &destroying,
                        &self.config,
                        executor,
                    )?;
                    true
                } else {
                    let clone_prerequisite =
                        super::worktree::clone_retirement_prerequisite_for_checkpoint(verified);
                    execute_target_cleanup(
                        &destroying,
                        &self.config,
                        executor,
                        clone_prerequisite,
                    )?;
                    false
                };
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.state = SessionState::Stopped;
                if !deferred {
                    record.target = None;
                }
                record.updated_at = now();
                record.last_error = None;
                persist_session_record_transition_or_restore(
                    &mut self.state,
                    session_id,
                    &destroying,
                    "persist stopped state after target cleanup",
                    &persist,
                )?;
                Ok(deferred)
            },
        )
    }

    /// Finish storage cleanup for a stopped Podman target retained by the
    /// quiescence transition. The locator stays durable until every command
    /// succeeds, making daemon restart and explicit retry idempotent.
    pub fn cleanup_stopped_target(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.cleanup_stopped_target_with(
            session_id,
            executor,
            crate::database::save_lifecycle_session,
        )
    }

    /// Deferred cleanup can restart from the stopped locator. Only the final
    /// session-record write needs daemon admission.
    pub fn cleanup_stopped_target_resumable(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.cleanup_stopped_target_with(session_id, executor, |record| {
            executor.end_resumable_work("settle deferred cleanup")?;
            ensure!(
                !executor.shutdown_interrupted(),
                "deferred cleanup interrupted"
            );
            crate::database::save_lifecycle_session(record)
        })
    }

    fn cleanup_stopped_target_with(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        persist: impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<()> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "cleanup stopped target with",
            executor,
            || {
                let previous = self
                    .state
                    .sessions
                    .get(session_id)
                    .with_context(|| format!("unknown session {session_id}"))?
                    .clone();
                ensure!(
                    previous.state == SessionState::Stopped,
                    "refusing deferred cleanup for active session {session_id}"
                );
                let Some(locator) = previous.target.as_ref() else {
                    return Ok(());
                };
                let backend = backend_locator(locator, &previous, &self.config)?;
                ensure!(
                    targets::quiesce_plan(&backend, session_id)?.is_some(),
                    "session {session_id} retained a non-Podman target after stopping"
                );
                let clone_prerequisite = previous.checkpoint.as_deref().map_or(
                    super::worktree::CloneRetirementPrerequisite::None,
                    super::worktree::clone_retirement_prerequisite_for_checkpoint,
                );
                if let Err(error) =
                    execute_target_cleanup(&previous, &self.config, executor, clone_prerequisite)
                {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.updated_at = now();
                    record.last_error = Some(format!("deferred target cleanup failed: {error:#}"));
                    let persisted = persist_session_record_transition_or_restore(
                        &mut self.state,
                        session_id,
                        &previous,
                        "persist deferred target cleanup failure",
                        &persist,
                    );
                    return match persisted {
                Ok(()) => Err(error),
                Err(persist_error) => Err(error.context(format!(
                    "also failed to persist deferred target cleanup failure: {persist_error:#}"
                ))),
            };
                }
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.target = None;
                record.updated_at = now();
                record.last_error = None;
                persist_session_record_transition_or_restore(
                    &mut self.state,
                    session_id,
                    &previous,
                    "persist completion of deferred Podman target cleanup",
                    &persist,
                )
            },
        )
    }

    /// Tear down whatever target the session holds and settle its record in
    /// `Stopped`.
    fn stop_target_and_settle(
        &mut self,
        session_id: &str,
        session: &SessionRecord,
        executor: &(impl CommandExecutor + Sync),
        clone_prerequisite: super::worktree::CloneRetirementPrerequisite,
        persist: &impl Fn(&SessionRecord) -> Result<()>,
    ) -> Result<bool> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "stop target and settle",
            executor,
            || {
                let mut deferred = false;
                if let Some(locator) = &session.target {
                    let backend = backend_locator(locator, session, &self.config)?;
                    // A sub-agent borrows its parent's target: only its own worker and
                    // private state go, as they do when a live child's close finishes.
                    // A parked child reaches this, with its worker already stopped.
                    if self.state.subagents.contains_key(session_id) {
                        super::environment::stop_borrower(session_id, &self.config, executor)?;
                        retire_worker_restart_intent(session_id)?;
                    } else if targets::quiesce_plan(&backend, session_id)?.is_some() {
                        super::environment::quiesce_session_environment(
                            session,
                            &self.config,
                            executor,
                        )?;
                        deferred = true;
                    } else {
                        execute_target_cleanup(
                            session,
                            &self.config,
                            executor,
                            clone_prerequisite,
                        )?;
                    }
                }
                let record = self.state.sessions.get_mut(session_id).unwrap();
                record.state = SessionState::Stopped;
                if !deferred {
                    record.target = None;
                }
                record.updated_at = now();
                record.last_error = None;
                record.last_checkpoint_error = None;
                persist_session_record_transition_or_restore(
                    &mut self.state,
                    session_id,
                    session,
                    "persist stopped state after tearing down the current target",
                    persist,
                )?;
                Ok(deferred)
            },
        )
    }

    /// Permanently destroy an inactive session and every artifact Hel owns for it.
    /// External cleanup happens before the durable record is dropped so failures
    /// remain visible and retryable.
    pub fn destroy_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.destroy_session_controlled_with(session_id, executor, BranchDisposition::Keep)
    }

    /// The same, with a say in what happens to the managed worktree's branch.
    ///
    /// A session that is already `Stopped` has had its checkout removed by
    /// [`super::worktree::retire_managed_worktree`], so usually only the branch is left for
    /// [`super::worktree::cleanup_managed_worktree`] to take. With [`BranchDisposition::Keep`]
    /// the record, the checkpoint, and the attachments go and the branch
    /// stays, which is what a destroy does unless the user asks otherwise.
    pub fn destroy_session_controlled_with(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
    ) -> Result<()> {
        self.destroy_session_controlled_with_checkout(
            session_id,
            executor,
            branch,
            CheckoutDisposition::Remove,
        )
        .map(|_| ())
    }

    /// The same, with a say in whether a checkout holding uncommitted changes
    /// survives the destruction.
    ///
    /// Answers with the checkout path that was kept, so the caller can name it
    /// where the person will see it.
    pub fn destroy_session_controlled_with_checkout(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
        checkout: CheckoutDisposition,
    ) -> Result<Option<PathBuf>> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "destroy stopped session",
            executor,
            || self.destroy_stopped_session_steps(session_id, executor, branch, checkout),
        )
    }

    fn destroy_stopped_session_steps(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
        checkout: CheckoutDisposition,
    ) -> Result<Option<PathBuf>> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if session.state.is_active() {
            bail!("refusing to destroy active session {session_id}");
        }
        if let Some(mut operation) = crate::database::load_move_operation(session_id)? {
            self.cleanup_prepared_move_destination(&mut operation, executor)?;
            super::move_session::recovery::cleanup_local_move_key_files(&operation.granted_keys);
        }
        let retained_checkout = if self.state.subagents.contains_key(session_id) {
            super::environment::stop_borrower(session_id, &self.config, executor)?;
            None
        } else {
            super::environment::retire_inactive_environments(
                &session,
                &self.config,
                executor,
                branch,
                checkout,
            )?
        };
        if let Err(error) = self.remove_raw_launch_record(&session, executor) {
            tracing::warn!(
                session_id = %session.id,
                target_id = %session.target_template_id,
                error = format!("{error:#}"),
                "Failed to remove raw launch record during stopped session destroy; continuing"
            );
        }
        super::checkpoint::remove_session_checkpoint_archives(
            &mj_core::config::sessions_dir(),
            session_id,
            session.checkpoint.as_deref(),
        )?;
        mj_core::attachment::AttachmentStore::controller(session_id)?
            .remove_session_data()
            .context("remove session image attachments")?;
        crate::database::delete_session(session_id)
            .context("destroy stopped session in database")?;
        self.state.subagents.remove(session_id);
        self.state.destroy_stopped_session(session_id)?;
        Ok(retained_checkout)
    }

    /// Permanently destroy a session from any state, without checkpointing
    /// and without requiring a recovery archive.
    ///
    /// Unlike [`Controller::destroy_session_controlled`], this accepts active
    /// states: it tears the live target down with the same close plan a
    /// verified close uses, so the owning process group dies before any files
    /// go. External cleanup is attempted before dropping the durable record;
    /// failures become warnings. Removing the recovery archive makes the
    /// destruction irreversible. The managed
    /// checkout is removed only after target teardown succeeds; its branch
    /// goes only when `branch` says so.
    pub fn force_destroy_session(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
    ) -> Result<DestroyOutcome> {
        self.force_destroy_session_with(
            session_id,
            executor,
            branch,
            crate::database::delete_session,
        )
    }

    fn force_destroy_session_with(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
        delete: impl Fn(&str) -> Result<()>,
    ) -> Result<DestroyOutcome> {
        self.destroy_session_with_cleanup(
            session_id,
            executor,
            branch,
            delete,
            DestroyCleanup::BestEffort,
        )
    }

    /// Automatic child stopping must prove teardown before forgetting its worker.
    pub fn stop_subagent(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<DestroyOutcome> {
        self.destroy_session_with_cleanup(
            session_id,
            executor,
            BranchDisposition::Keep,
            crate::database::delete_session,
            DestroyCleanup::Required,
        )
    }

    fn destroy_session_with_cleanup(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
        delete: impl Fn(&str) -> Result<()>,
        cleanup: DestroyCleanup,
    ) -> Result<DestroyOutcome> {
        let result =
            self.force_destroy_session_steps(session_id, executor, branch, delete, cleanup);
        if result.is_err() {
            self.settle_failed_destroy(session_id);
        }
        result
    }

    /// A destroy that stopped partway leaves a record whose worker it may have
    /// stopped already. The destroy is the decision, so the record stops being
    /// a live session: `Error` is not polled, so the daemon does not keep
    /// reconnecting a relay proxy to the worker it stopped (I2-9). The target
    /// stays recorded, and the next destroy finishes the removal.
    fn settle_failed_destroy(&mut self, session_id: &str) {
        let Some(record) = self.state.sessions.get(session_id) else {
            return;
        };
        if !record.state.is_active() || record.state == SessionState::Error {
            return;
        }
        let previous = record.clone();
        let record = self.state.sessions.get_mut(session_id).unwrap();
        record.state = SessionState::Error;
        record.updated_at = now();
        if let Err(error) = persist_session_record_transition_or_restore(
            &mut self.state,
            session_id,
            &previous,
            "persist the failed destroy",
            &crate::database::save_lifecycle_session,
        ) {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not record a failed destroy"
            );
        }
    }

    fn force_destroy_session_steps(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        branch: BranchDisposition,
        delete: impl Fn(&str) -> Result<()>,
        cleanup: DestroyCleanup,
    ) -> Result<DestroyOutcome> {
        crate::worker_lifecycle::run_blocking(
            session_id,
            "force destroy session steps",
            executor,
            || {
                crate::worker_lifecycle::require(session_id)?.verify_cached_target(&self.state)?;
                let session = self
                    .state
                    .sessions
                    .get(session_id)
                    .with_context(|| format!("unknown session {session_id}"))?
                    .clone();
                let mut outcome = DestroyOutcome::default();
                if let Some(mut operation) = crate::database::load_move_operation(session_id)? {
                    outcome.cleanup(
                        self.cleanup_prepared_move_destination(&mut operation, executor),
                        &session,
                        "prepared Move destination",
                        cleanup,
                        executor,
                    )?;
                    super::move_session::recovery::cleanup_local_move_key_files(
                        &operation.granted_keys,
                    );
                }
                outcome.cleanup(
                    if self.state.subagents.contains_key(session_id) {
                        super::environment::stop_borrower(session_id, &self.config, executor)
                    } else {
                        super::environment::destroy_session_environments(
                            &session,
                            &self.config,
                            executor,
                            branch,
                        )
                    },
                    &session,
                    "session environment",
                    cleanup,
                    executor,
                )?;
                outcome.cleanup(
                    self.remove_raw_launch_record(&session, executor),
                    &session,
                    "raw launch record",
                    cleanup,
                    executor,
                )?;
                retire_worker_restart_intent(session_id)?;
                super::checkpoint::remove_session_checkpoint_archives(
                    &mj_core::config::sessions_dir(),
                    session_id,
                    session.checkpoint.as_deref(),
                )?;
                match super::checkpoint::cleanup_session_host_snapshot_cache(session_id, executor) {
                    Ok(warnings) => outcome.warnings.extend(warnings),
                    Err(error) => {
                        tracing::warn!(
                            session_id,
                            error = %format!("{error:#}"),
                            "could not list host snapshot cache entries during Destroy"
                        );
                    }
                }
                mj_core::attachment::AttachmentStore::controller(session_id)?
                    .remove_session_data()
                    .context("remove session image attachments")?;
                delete(session_id).context("force destroy session in database")?;
                self.state.subagents.remove(session_id);
                self.state.destroy_session_force(session_id)?;
                Ok(outcome)
            },
        )
    }

    fn remove_raw_launch_record(
        &self,
        session: &SessionRecord,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if self.state.subagents.contains_key(&session.id)
            || session.managed_worktree.is_some()
            || session
                .project
                .as_ref()
                .is_some_and(|project| project.primary_is_plain_directory())
        {
            return Ok(());
        }
        let Some(project_directory) = session.project_directory.as_deref() else {
            return Ok(());
        };
        let Some(template) = self.config.targets.get(&session.target_template_id) else {
            return Ok(());
        };
        match template {
            mj_core::config::TargetTemplate::LocalBare => {
                if !project_directory.is_dir() {
                    return Ok(());
                }
                if mj_core::repository::resolve_local_directory(project_directory, executor)
                    .is_ok_and(|resolved| resolved.plain_directory)
                {
                    return Ok(());
                }
                let git = super::worktree::ExecutorGit::local(executor);
                mj_checkpoint::snapshot::remove_launch_record(&git, project_directory, &session.id)
            }
            mj_core::config::TargetTemplate::SshBare { ssh, .. } => {
                ensure!(
                    !session.id.is_empty()
                        && session
                            .id
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                    "invalid session id {:?} for remote raw launch record removal",
                    session.id
                );
                let script = concat!(
                    "dir=$1; id=$2; ",
                    "[ -d \"$dir\" ] || exit 0; ",
                    "common=$(git -C \"$dir\" rev-parse --git-common-dir 2>/dev/null) || exit 0; ",
                    "case \"$common\" in /*) ;; *) common=\"$dir/$common\" ;; esac; ",
                    "rm -f \"$common/mj/sessions/$id/launch\"; ",
                    "rmdir \"$common/mj/sessions/$id\" 2>/dev/null || true",
                );
                let spec = targets::ssh_command_owned(
                    targets::SshTrafficClass::Control,
                    &targets::SshTarget::from(ssh),
                    vec![
                        "sh".into(),
                        "-c".into(),
                        script.into(),
                        "mj-remove-launch".into(),
                        project_directory.to_string_lossy().into_owned(),
                        session.id.clone(),
                    ],
                );
                let output = executor
                    .execute(&spec)
                    .context("remove remote raw launch record")?;
                if output.status != 0 {
                    bail!(
                        "remote raw launch record removal failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuspendWithoutCheckpointOutcome {
    pub deferred: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct DestroyOutcome {
    /// Safe messages for attached surfaces; diagnostics stay in daemon logs.
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy)]
enum DestroyCleanup {
    Required,
    BestEffort,
}

impl DestroyOutcome {
    fn cleanup(
        &mut self,
        result: Result<()>,
        session: &SessionRecord,
        resource: &str,
        policy: DestroyCleanup,
        executor: &impl CommandExecutor,
    ) -> Result<bool> {
        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                if matches!(policy, DestroyCleanup::Required) || executor.cancellation_requested() {
                    return Err(error);
                }
                tracing::warn!(session_id = %session.id, target_id = %session.target_template_id, resource, error = format!("{error:#}"), "Destroy is proceeding after cleanup failed; target resources may remain");
                self.warnings.push(format!("Cleanup of the {resource} on target {} failed during Destroy for session {}. Resources may remain; details are in the daemon logs.", session.target_template_id, session.id));
                Ok(false)
            }
        }
    }
}

/// Whether a close of this session has no workspace to archive.
///
/// A sub-agent borrows its parent's workspace and never archives it. Other
/// sessions qualify only when they cannot be holding live work. A record still
/// `Provisioning` has never had a worker connected, so there is no relay to
/// latch and no harness state to capture. A record mid-close or already
/// failed, with no target locator left, has no target to read a workspace
/// from at all. Every other state may hold work and must take the graceful
/// close's checkpoint.
///
/// `subagent` says the session is a Mjolnir sub-agent. A child is never
/// resumed on its own, so a close keeps no archive of it; its conversation
/// and report are already in the store. A parked child's worker is stopped,
/// and so is that of a failed one: its first prompt was refused for good and
/// its worker was stopped when it was recorded as failed (I1-2).
pub fn has_nothing_to_checkpoint(session: &SessionRecord, subagent: bool) -> bool {
    if subagent {
        return true;
    }
    match session.state {
        SessionState::Provisioning | SessionState::Parked | SessionState::StartupCleanup => true,
        SessionState::Closing
        | SessionState::Destroying
        | SessionState::Error
        | SessionState::Lost => session.target.is_none(),
        SessionState::Running
        | SessionState::Disconnected
        | SessionState::Checkpointing
        | SessionState::Stopped
        | SessionState::DestroyedWithDataLoss => false,
    }
}

/// Remove a session's own target: its worker, container or workspace. Once
/// the target is confirmed gone, the mbx build state of the workspaces it held
/// is released; that release never fails the cleanup.
fn execute_target_cleanup(
    session: &SessionRecord,
    config: &mj_core::config::Config,
    executor: &(impl CommandExecutor + Sync),
    clone_prerequisite: super::worktree::CloneRetirementPrerequisite,
) -> Result<()> {
    super::environment::retire_session_environment_with_clone_prerequisite(
        session,
        config,
        executor,
        BranchDisposition::Keep,
        clone_prerequisite,
    )
}

/// Exact teardown proved there is no process left whose boot must be
/// preserved. Retire the intent before publishing a different placement. A
/// sub-agent's teardown removes only its own worker and private state, which
/// is all its intent covers.
fn retire_worker_restart_intent(session_id: &str) -> Result<()> {
    if let Some(intent) = crate::database::load_worker_restart(session_id)? {
        crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
    }
    Ok(())
}

fn apply_close_checkpoint_started(record: &mut SessionRecord, updated_at: String) {
    record.state = SessionState::Closing;
    record.updated_at = updated_at;
    record.last_checkpoint_error = None;
}

/// Record a close whose checkpoint failed.
///
/// An ordinary failure is non-destructive: the session returns to the state it
/// had. A restart that left no live worker cannot return to Running, because
/// nothing is listening there any more; it records `Error` so the session stops
/// being polled, and keeps its target for a later resume or forced close.
/// Neither can a close running on a worker started without its harness, which
/// failed to open (`harness_failure`): nothing there can run a turn.
fn apply_close_checkpoint_failure(
    record: &mut SessionRecord,
    previous: &SessionRecord,
    error: &anyhow::Error,
    updated_at: String,
    harness_failure: Option<&mj_core::relay::WorkerHarnessStartupFailure>,
) {
    if let Some(failure) = harness_failure {
        record.state = SessionState::Error;
        record.last_error = Some(format!(
            "the harness failed to open the session ({failure}), and the close could not save \
             the session without it; resume to retry the harness, or explicitly destroy it \
             with mj destroy: {error:#}"
        ));
    } else if WorkerRestartLeftNoWorker::marks(error) {
        record.state = SessionState::Error;
        record.last_error = Some(format!(
            "close failed and left the session without a live worker; retry the close, \
             resume from its checkpoint, or explicitly destroy it with mj destroy: {error:#}"
        ));
    } else {
        record.state = state_after_unsealed_close(previous);
    }
    record.last_checkpoint_error = Some(format!("{error:#}"));
    record.updated_at = updated_at;
}

/// The state a session goes back to when its close stops before sealing the
/// relay. `Closing` there is only the intent of this close, or of one
/// interrupted before it sealed the relay, and the relay is still open, so the
/// session is running.
/// Whether the worker answered a Close with a definite refusal. A validation
/// rejection means the relay never accepted the command; a lost or retryable
/// answer does not say that, so it is not a refusal.
fn close_was_refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<crate::worker_client::RelayRejected>()
            .is_some_and(|rejected| !rejected.is_retryable())
    })
}

fn state_after_unsealed_close(previous: &SessionRecord) -> SessionState {
    if previous.state == SessionState::Closing {
        SessionState::Running
    } else {
        previous.state
    }
}

fn apply_interrupted_close_error(
    record: &mut SessionRecord,
    error: &anyhow::Error,
    updated_at: &str,
) {
    let destroying = record.state == SessionState::Destroying;
    if !destroying {
        record.state = SessionState::Closing;
    }
    record.updated_at = updated_at.to_owned();
    record.last_error = Some(if destroying {
        format!("target cleanup is safely retryable from its verified checkpoint: {error:#}")
    } else {
        format!("close is safely resumable from its verified checkpoint: {error:#}")
    });
}

#[cfg(test)]
mod tests;
// Shared with mj-daemon's tests through the `test-support` feature.
#[doc(hidden)]
pub mod test_fixtures;
