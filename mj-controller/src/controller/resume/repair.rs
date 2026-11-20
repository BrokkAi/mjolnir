//! Resume never restores an archive over a retained environment.
use super::*;
use crate::controller::worker_binary::{WorkerInstall, prepare_managed_harness_for_upgrade};
use crate::controller::worker_restart::{InstalledWorkerRestart, WorkerRestartMessages};
use mj_core::targets::SshTrafficClass;
use sha2::Digest;

/// Installation is a completed action under the lifecycle owner, not another
/// platform/build probe against a second snapshot of the same target.
pub(in crate::controller) enum RetainedWorker {
    Live,
    Missing,
    Installed,
    Reserved(crate::upgrade::Work),
}

const REPAIR: WorkerRestartMessages = WorkerRestartMessages {
    stop: "stop the failed session worker for repair",
    replace: "install the worker for repair",
    start: "start the repaired session worker",
    connect: "reconnect the retained session journal",
    project_memory: "project memory sync unavailable after repair",
    native_session: "wait for the retained native session",
};

impl Controller {
    pub(in crate::controller) async fn repair_existing_environment(
        &mut self,
        session_id: &str,
        worker_state: RetainedWorker,
        discard_queue: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        let (worker_state, mut swap_admission) = match worker_state {
            RetainedWorker::Reserved(admission) => (RetainedWorker::Installed, Some(admission)),
            state => (state, None),
        };
        executor.notify_notice(
            "Resuming in the existing environment; current files and sub-agents are kept.",
        );
        let result = async {
            // Preparation and journal replay can resume from this retained
            // placement after daemon handoff; they must not hold admission.
            executor.begin_resumable_work()?;
            let (backend, root) = self.worker_placement(session_id)?;
            let reconnect = targets::reconnect_plan(&backend, session_id)?
                .commands
                .into_iter()
                .next()
                .context("missing reconnect command")?;
            let attached = if matches!(&worker_state, RetainedWorker::Live) {
                // A lost handshake cannot prove death. Only the worker's terminal
                // preparation failure permits replacing an otherwise live worker:
                // that boot has no coordinator and can never dispatch ACP work.
                match self
                    .observe_started_worker(
                        session_id,
                        executor,
                        InstalledWorkerRestart {
                            backend: &backend,
                            worker_root: &root,
                            reconnect: &reconnect,
                            launch: None,
                            prepared: true,
                            discard_queued_prompts: if discard_queue { Some(new_command_id("resume-clear-queue")?) } else { None },
                            swap_admission: None,
                            messages: &REPAIR,
                        },
                    )
                    .await
                {
                    Ok(relay) => Some(relay),
                    Err(error)
                        if error
                            .downcast_ref::<super::super::readiness::HarnessPreparationFailure>()
                            .is_some() =>
                    {
                        executor.notify_notice(&format!(
                            "The existing worker's harness preparation failed: {}. Restarting that worker in the same environment.",
                            super::super::worker_binary::failure_line(&error)
                        ));
                        if let Some(intent) = crate::database::load_worker_restart(session_id)?
                            && self.state.sessions[session_id].target.as_deref()
                                == Some(&intent.target)
                        {
                            crate::database::finish_worker_restart(
                                session_id,
                                &intent.operation_id,
                            )?;
                        }
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            if attached.is_none() && self.state.sessions[session_id].native_session_id.is_some() {
                let profile = self.config.named_profile(&self.state.sessions[session_id].last_profile)?;
                let native = super::super::target_profile_home(&backend, session_id, profile);
                if !super::super::environment::directory_available(&backend, &native, executor)? {
                    executor.end_resumable_work("recover missing parent native history")?;
                    super::super::worker_binary::stop_worker_after_target_recovery(executor, &backend, session_id, &root)?;
                    return Box::pin(self.recover_missing_parent_data(session_id, discard_queue, executor)).await;
                }
            }
            let mut relay = if let Some(relay) = attached {
                relay
            } else {
                // A worker that is not running starts from the profile files
                // its environment kept, whose login is the one it held when it
                // stopped; a renewal since then revoked it. Stage the profile
                // again so the worker starts with the current login, as every
                // other start path does. An installed worker had its files
                // staged just before, and a reserved swap replaces a worker
                // that was receiving pushes.
                if swap_admission.is_none() && !matches!(&worker_state, RetainedWorker::Installed) {
                    self.prepare_worker_files_for(
                        session_id,
                        &backend,
                        &root,
                        executor,
                        super::super::worker_binary::WorkerFilePreparation::RefreshProfileOnly,
                        super::super::worker_binary::LaunchHistory::Stored,
                    )?;
                }
                let launch = self.current_worker_launch_config(session_id, &backend)?;
                if !matches!(&worker_state, RetainedWorker::Installed) {
                    let worker = WorkerInstall::for_target(&backend, executor)?;
                    prepare_managed_harness_for_upgrade(executor, &backend, session_id, &worker, &launch)?;
                }
                self.restart_worker_with_installed_binary(
                    session_id,
                    executor,
                    InstalledWorkerRestart {
                        backend: &backend,
                        worker_root: &root,
                        reconnect: &reconnect,
                        launch: Some(&launch),
                        prepared: matches!(&worker_state, RetainedWorker::Installed),
                        discard_queued_prompts: if discard_queue { Some(new_command_id("resume-clear-queue")?) } else { None },
                        swap_admission: swap_admission.take(),
                        messages: &REPAIR,
                    },
                )
                .await?
            };
            executor.end_resumable_work("resume repair publication")?;
            let snapshot = relay.sync().await?;
            self.mark_worker_connected(
                session_id,
                relay.snapshot().operational.native_session_id.clone(),
            )?;
            if let Some(intent) = crate::database::load_worker_restart(session_id)?
                && self.state.sessions[session_id].target.as_deref() == Some(&intent.target)
            {
                crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
            }
            let notice = self.finish_parent_recovery_notice(session_id, &backend, &root, discard_queue, executor, &mut relay).await?;
            Ok(if notice { relay.sync().await?.materialized } else { snapshot.materialized })
        }
        .await;
        if let Err(error) = &result {
            let record = self
                .state
                .sessions
                .get_mut(session_id)
                .context("repair session disappeared")?;
            record.state = SessionState::Error;
            record.updated_at = now();
            record.last_error = Some(format!(
                "Resume repair failed: {error:#}. The environment was kept. Retry Resume when the target is reachable."
            ));
            crate::database::save_lifecycle_session(record)?;
        }
        result
    }
}

impl Controller {
    pub(in crate::controller) async fn recover_missing_parent_data(
        &mut self,
        session_id: &str,
        discard_queue: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        let record = self.state.sessions[session_id].clone();
        let (backend, _) = self.worker_placement(session_id)?;
        if record.checkpoint.is_none() {
            let profile = self.config.named_profile(&record.last_profile)?;
            let home = super::super::target_profile_home(&backend, session_id, profile);
            if super::super::environment::directory_available(&backend, &home, executor)? {
                return self
                    .recover_parent_runtime_from_store(session_id, discard_queue, executor)
                    .await;
            }
        }
        let checkpoint = record.checkpoint.as_ref().context(
            "The current checkout was kept, but parent runtime/history is missing and no recovery copy holds it. Supply a recovery archive and retry Resume; current files and children remain untouched.")?;
        let archive = verify_resume_checkpoint(session_id, checkpoint)?;
        ensure!(
            record
                .native_session_id
                .as_deref()
                .is_none_or(|id| id == archive.manifest.session.native_session_id),
            "The recovery copy does not contain this parent's native session. Current files and children were kept; supply a recovery copy for the current conversation and retry Resume."
        );
        let (backend, root) = self.worker_placement(session_id)?;
        let output = super::super::worker_binary::probe_worker_liveness(
            executor, &backend, session_id, &root,
        )?;
        ensure!(
            output.status == 0 && String::from_utf8_lossy(&output.stdout).trim() == "dead",
            "Cannot recover parent files while its worker may be live; everything was kept. Retry Resume."
        );
        executor.begin_resumable_work()?;
        let profile = self.config.named_profile(&record.last_profile)?.clone();
        let checkout =
            super::super::environment::resume_checkout_directory(self, session_id, &backend)?;
        let workspace = self
            .state
            .checkout(session_id)?
            .project_directory()
            .map(|path| {
                path.parent()
                    .context("checkout has no parent")
                    .map(|p| p.to_path_buf())
            })
            .transpose()?
            .unwrap_or_else(|| PathBuf::from(&checkout));
        let target_path = |path: &Path| match &backend {
            targets::TargetLocator::SshBare { .. } | targets::TargetLocator::AwsEc2 { .. }
                if !path.is_absolute() =>
            {
                PathBuf::from("~").join(path)
            }
            _ => path.to_path_buf(),
        };
        // Publish the retry input before installing any parent state. The
        // environment probe recognizes it until a worker is ready again.
        execute_checked(
            executor,
            targets::command_on_locator(
                SshTrafficClass::Control,
                &backend,
                session_id,
                vec!["mkdir".into(), "-p".into(), root.clone()],
                "create missing parent worker root",
            )?,
        )?;
        let remote_archive = format!("{root}/resume-parent-recovery.hel.zip");
        let remote_spec = format!("{root}/resume-parent-recovery.json");
        let remote_seed = format!("{root}/resume-parent-current-seed.json");
        let spec = CheckpointRestoreSpec {
            archive_path: restore_archive_path(
                &backend,
                &archive.archive_path,
                &target_path(Path::new(&remote_archive)),
            ),
            workspace_root: target_path(&workspace),
            report_root: None,
            relay_root: target_path(Path::new(&root)),
            harness_home: target_path(Path::new(&super::super::target_profile_home(
                &backend, session_id, &profile,
            ))),
            restore_repositories: false,
            restore_native: true,
            queue_policy: if discard_queue {
                mj_checkpoint::checkpoint::QueueRestorePolicy::Discard
            } else {
                mj_checkpoint::checkpoint::QueueRestorePolicy::Restore
            },
            primary_repository_root: Some(target_path(Path::new(&checkout))),
        };
        let staging = tempfile::tempdir()?;
        let local_spec = staging.path().join("resume-parent-recovery.json");
        std::fs::write(&local_spec, serde_json::to_vec(&spec)?)?;
        super::super::checkpoint::upload_checkpoint_spec(
            executor,
            &backend,
            session_id,
            &local_spec,
            &remote_spec,
        )?;
        if should_upload_restore_archive(&backend) {
            super::super::checkpoint::upload_checkpoint_spec(
                executor,
                &backend,
                session_id,
                &archive.archive_path,
                &remote_archive,
            )?;
        }
        // The controller's durable queue/frontier may be newer than the copy.
        // Recover native files from the archive without replaying work already
        // completed since that archive was captured.
        let canonical = current_parent_projection(session_id, Some(&archive.canonical_session))?;
        let seed =
            mj_checkpoint::checkpoint::relay_seed_for_restore(&canonical, true, spec.queue_policy)?;
        let local_seed = staging.path().join("current-relay-seed.json");
        std::fs::write(&local_seed, serde_json::to_vec(&seed)?)?;
        super::super::checkpoint::upload_checkpoint_spec(
            executor,
            &backend,
            session_id,
            &local_seed,
            &remote_seed,
        )?;
        self.prepare_worker_files(
            session_id,
            &backend,
            &root,
            executor,
            super::super::worker_binary::LaunchHistory::Restored(&archive.canonical_session),
        )?;
        // Build the argv before SSH/container wrapping. Old workers reject
        // this explicit mode rather than treating it as a whole restore.
        let command = targets::command_on_locator(
            SshTrafficClass::Bulk,
            &backend,
            session_id,
            vec![
                format!("{root}/hel"),
                "worker".into(),
                "restore-checkpoint".into(),
                "--spec".into(),
                remote_spec,
                "--missing-parent".into(),
                "--relay-seed".into(),
                remote_seed,
            ],
            "recover missing parent checkpoint state",
        )?;
        let output = execute_checked(executor, command)?;
        let _outcome: mj_checkpoint::checkpoint::ParentRecoveryOutcome =
            serde_json::from_slice(&output.stdout)
                .context("read missing parent recovery result")?;
        if self.state.sessions[session_id].native_session_id.is_none()
            && let Some(native_session_id) = archive.native_session_to_load(true)
        {
            self.state
                .sessions
                .get_mut(session_id)
                .unwrap()
                .native_session_id = Some(native_session_id.to_owned());
            self.persist_session_state(session_id)?;
        }
        executor.end_resumable_work("resume recovered parent")?;
        self.repair_existing_environment(
            session_id,
            RetainedWorker::Installed,
            discard_queue,
            executor,
        )
        .await
    }

    async fn finish_parent_recovery_notice(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        root: &str,
        discard_queue: bool,
        executor: &impl CommandExecutor,
        relay: &mut crate::session_manager::StandaloneSession,
    ) -> Result<bool> {
        let mut recorded = false;
        loop {
            let output = executor.execute(
                &super::super::worker_binary::worker_recovery_command(backend, root),
            )?;
            ensure!(
                output.status == 0,
                "could not finish recovery receipt; retry Resume"
            );
            let kind = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let (filename, prefix) = match kind.as_str() {
                "partial" => ("resume-parent-recovery.json", "resume-parent-recovery"),
                "checkout-partial" => ("resume-checkout-recovery.json", "resume-checkout-recovery"),
                _ => return Ok(recorded),
            };
            let checkpoint = self.state.sessions[session_id].checkpoint.as_ref();
            let seed = if kind == "partial" {
                let output = execute_checked(
                    executor,
                    targets::command_on_locator(
                        SshTrafficClass::Control,
                        backend,
                        session_id,
                        vec![
                            "cat".into(),
                            format!("{root}/resume-parent-current-seed.json"),
                        ],
                        "read completed parent recovery queue",
                    )?,
                )?;
                let seed: mj_core::relay::RestoredRelaySeed =
                    serde_json::from_slice(&output.stdout)?;
                for prompt in seed.queued_prompts.iter().filter(|_| !discard_queue) {
                    if !matches!(&prompt.kind, CanonicalQueuedCommandKind::Prompt) {
                        continue;
                    }
                    let blocks = prompt
                        .content
                        .iter()
                        .cloned()
                        .map(serde_json::from_value)
                        .collect::<serde_json::Result<Vec<ContentBlock>>>()?;
                    let mut missing = false;
                    for reference in mj_core::attachment::references(&blocks)? {
                        if !relay.attachment_present(&reference).await? {
                            missing = true;
                            break;
                        }
                    }
                    if missing {
                        let text = format!(
                            "Queued prompt {} could not recover its image attachments. Re-send this prompt with its images; its content and references remain in the transcript.",
                            prompt.command_id
                        );
                        executor.notify_notice(&text);
                        relay
                            .submit(
                                format!(
                                    "resume-missing-image-{}",
                                    mj_core::hex::lower_hex(sha2::Sha256::digest(
                                        prompt.command_id.as_bytes()
                                    ))
                                ),
                                RelayCommand::RecordNotice { text },
                            )
                            .await?;
                    }
                }
                Some(seed)
            } else {
                None
            };
            let text = if kind == "partial" {
                if let Some(checkpoint) = checkpoint {
                    format!(
                        "Native history was restored from the recovery copy taken at {} where it was missing; files are current, existing native files and live children were kept.",
                        checkpoint.created_at
                    )
                } else {
                    "The missing parent runtime was rebuilt from durable state; native history, current files and live children were kept.".to_owned()
                }
            } else {
                let checkpoint = checkpoint.context("recovery copy disappeared")?;
                format!(
                    "Checkout recovery completed using the recovery copy taken at {}; any current files and parent history were kept.",
                    checkpoint.created_at
                )
            };
            let notice_id = if let Some(checkpoint) = checkpoint {
                format!("{prefix}-{}", checkpoint.sha256)
            } else {
                format!(
                    "resume-parent-store-{}",
                    seed.as_ref()
                        .context("parent recovery queue disappeared")?
                        .event_frontier_digest
                )
            };
            executor.notify_notice(&text);
            relay
                .submit(notice_id, RelayCommand::RecordNotice { text })
                .await?;
            if kind == "checkout-partial" {
                let checkpoint = checkpoint.context("recovery copy disappeared")?;
                let output = execute_checked(
                    executor,
                    targets::command_on_locator(
                        SshTrafficClass::Control,
                        backend,
                        session_id,
                        vec!["cat".into(), format!("{root}/{filename}")],
                        "read completed checkout recovery",
                    )?,
                )?;
                let recovery: mj_checkpoint::checkpoint::CheckoutRecoverySpec =
                    serde_json::from_slice(&output.stdout)?;
                let stage = recovery
                    .staging_root
                    .strip_prefix("~")
                    .unwrap_or(&recovery.staging_root);
                let destination = recovery
                    .destination_checkout
                    .strip_prefix("~")
                    .unwrap_or(&recovery.destination_checkout);
                ensure!(
                    recovery.version == 1
                        && recovery.checkpoint_sha256 == checkpoint.sha256
                        && stage.parent()
                            == self
                                .state
                                .checkout(session_id)?
                                .managed_worktree()
                                .map(|worktree| worktree.worktree_root.as_path())
                                .unwrap_or(destination)
                                .parent()
                        && !stage
                            .components()
                            .any(|part| part == std::path::Component::ParentDir)
                        && stage.file_name().is_some_and(|name| name
                            .to_string_lossy()
                            .starts_with(&format!(".mj-resume-{session_id}-"))),
                    "checkout recovery staging ownership changed; keeping it for inspection"
                );
                execute_checked(
                    executor,
                    targets::command_on_locator(
                        SshTrafficClass::Control,
                        backend,
                        session_id,
                        vec![
                            "rm".into(),
                            "-rf".into(),
                            "--".into(),
                            stage.to_string_lossy().into_owned(),
                        ],
                        "remove private checkout reconstruction staging",
                    )?,
                )?;
                if let mj_core::state::Checkout::ManagedWorktree { worktree, .. } =
                    self.state.checkout(session_id)?.effective()
                    && worktree.kind == mj_core::state::ManagedCheckoutKind::Worktree
                    && destination == worktree.worktree_root
                {
                    execute_checked(
                        executor,
                        super::super::worktree::managed_git_command(
                            SshTrafficClass::Control,
                            &worktree.target,
                            &worktree.source_repository,
                            ["worktree", "prune"],
                            "prune completed checkout reconstruction registration",
                        ),
                    )?;
                }
            }
            execute_checked(
                executor,
                targets::command_on_locator(
                    SshTrafficClass::Control,
                    backend,
                    session_id,
                    vec!["rm".into(), "--".into(), format!("{root}/{filename}")],
                    "finish recovery receipt",
                )?,
            )?;
            // Each iteration removes its durable retry input. A crash before
            // removal replays the same notice command ID, never another line.
            recorded = true;
        }
    }
}

impl Controller {
    pub(in crate::controller) async fn recover_missing_checkout(
        &mut self,
        session_id: &str,
        discard_queue: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        use mj_checkpoint::checkpoint::{CheckoutRecoveryPublication, CheckoutRecoverySpec};
        let record = self.state.sessions[session_id].clone();
        let checkpoint = record.checkpoint.as_ref().context(
            "The parent history was kept but its checkout is missing and no recovery archive holds the files. Supply a recovery archive or restore the checkout, then retry Resume.")?;
        let archive = verify_resume_checkpoint(session_id, checkpoint)?;
        ensure!(
            !archive
                .manifest
                .repositories
                .iter()
                .any(|repository| repository.metadata.plain_directory),
            "The missing attached directory was not captured in the recovery copy. Restore that directory and retry Resume; parent history and children were kept."
        );
        let github_token = self
            .github_token_for_repository_preflight(session_id)
            .await?;
        if let ResumeRepositorySourcePreflight::RepositoryMoved(_) = self
            .preflight_repository_sources(
                session_id,
                &record.target_template_id,
                false,
                github_token.as_deref(),
                executor,
            )?
        {
            bail!(
                "A repository source moved while Resume was starting. Everything was kept; retry Resume to confirm its replacement."
            );
        }
        let (backend, root) = self.worker_placement(session_id)?;
        executor.begin_resumable_work()?;
        let checkout = self.state.checkout(session_id)?;
        let (mut destination, worktree) = match checkout.effective() {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                let project = checkout
                    .project_directory()
                    .context("managed project directory is missing")?;
                let root_present = super::super::environment::directory_available(
                    &backend,
                    &worktree.worktree_root.to_string_lossy(),
                    executor,
                )?;
                (
                    if root_present {
                        project.to_path_buf()
                    } else {
                        worktree.worktree_root.clone()
                    },
                    Some((*worktree).clone()),
                )
            }
            mj_core::state::Checkout::ManagedWorkspace => (
                PathBuf::from(super::super::environment::resume_checkout_directory(
                    self, session_id, &backend,
                )?),
                None,
            ),
            _ => bail!(
                "The missing attached checkout was not captured for reconstruction. Restore it and retry Resume; parent history and children were kept."
            ),
        };
        let target_path = |path: &Path| match &backend {
            targets::TargetLocator::SshBare { .. } | targets::TargetLocator::AwsEc2 { .. }
                if !path.is_absolute() =>
            {
                PathBuf::from("~").join(path)
            }
            _ => path.to_path_buf(),
        };
        let live_path = |path: &Path| path.strip_prefix("~").unwrap_or(path).to_path_buf();
        let pending = executor.execute(&super::super::worker_binary::worker_recovery_command(
            &backend, &root,
        ))?;
        ensure!(
            pending.status == 0,
            "cannot inspect checkout recovery; everything was kept"
        );
        let remote_spec = format!("{root}/resume-checkout-recovery.json");
        let recovery: CheckoutRecoverySpec =
            if String::from_utf8_lossy(&pending.stdout).trim() == "checkout-partial" {
                let output = execute_checked(
                    executor,
                    targets::command_on_locator(
                        SshTrafficClass::Control,
                        &backend,
                        session_id,
                        vec!["cat".into(), remote_spec.clone()],
                        "read pending checkout recovery",
                    )?,
                )?;
                serde_json::from_slice(&output.stdout)
                    .context("read pending checkout recovery specification")?
            } else {
                let stage_parent = worktree
                    .as_ref()
                    .map(|w| w.worktree_root.as_path())
                    .unwrap_or(&destination)
                    .parent()
                    .context("checkout has no parent")?;
                let stage = stage_parent.join(format!(
                    ".mj-resume-{session_id}-{}",
                    crate::worker_lifecycle::require(session_id)?.operation_id()
                ));
                let (staged_checkout, workspace) = if let Some(original) = &worktree {
                    let staged_repository = stage.join(
                        original
                            .worktree_root
                            .file_name()
                            .context("checkout has no name")?,
                    );
                    let project = checkout
                        .project_directory()
                        .context("managed project directory is missing")?;
                    let staged_project =
                        staged_repository.join(project.strip_prefix(&original.worktree_root)?);
                    let workspace = staged_project
                        .parent()
                        .context("staged project has no parent")?
                        .to_path_buf();
                    (
                        if destination == original.worktree_root {
                            staged_repository
                        } else {
                            staged_project
                        },
                        workspace,
                    )
                } else {
                    let primary = record
                        .project_bundle(&self.config)
                        .and_then(|bundle| bundle.primary())
                        .context("current primary repository is missing")?;
                    (stage.join(&primary.destination), stage.clone())
                };
                let profile = self.config.named_profile(&record.last_profile)?;
                CheckoutRecoverySpec {
                    version: 1,
                    checkpoint_sha256: checkpoint.sha256.clone(),
                    staging_root: target_path(&stage),
                    staged_checkout: target_path(&staged_checkout),
                    destination_checkout: target_path(&destination),
                    additional_checkouts: if worktree.is_none() {
                        let current_workspace =
                            PathBuf::from(super::super::network_git::workspace_root(
                                &backend,
                                record.container_workspace.as_deref(),
                            ));
                        super::super::environment::resume_checkout_directories(
                            self, session_id, &backend,
                        )?
                        .into_iter()
                        .filter(|path| path != &destination)
                        .map(|path| {
                            Ok(CheckoutRecoveryPublication {
                                staged_checkout: target_path(
                                    &stage.join(path.strip_prefix(&current_workspace)?),
                                ),
                                destination_checkout: target_path(&path),
                            })
                        })
                        .collect::<Result<Vec<_>>>()?
                    } else {
                        Vec::new()
                    },
                    restore: CheckpointRestoreSpec {
                        archive_path: restore_archive_path(
                            &backend,
                            &archive.archive_path,
                            &target_path(Path::new(&format!(
                                "{root}/resume-checkout-recovery.hel.zip"
                            ))),
                        ),
                        workspace_root: target_path(&workspace),
                        report_root: None,
                        relay_root: target_path(Path::new(&root)),
                        harness_home: target_path(Path::new(&super::super::target_profile_home(
                            &backend, session_id, profile,
                        ))),
                        restore_repositories: true,
                        restore_native: false,
                        queue_policy: mj_checkpoint::checkpoint::QueueRestorePolicy::Restore,
                        primary_repository_root: None,
                    },
                }
            };
        let allowed_destination = recovery.destination_checkout == target_path(&destination)
            || worktree.as_ref().is_some_and(|original| {
                recovery.destination_checkout == target_path(&original.worktree_root)
                    || checkout.project_directory().is_some_and(|project| {
                        recovery.destination_checkout == target_path(project)
                    })
            });
        let allowed_checkouts =
            super::super::environment::resume_checkout_directories(self, session_id, &backend)?;
        ensure!(
            recovery.additional_checkouts.iter().all(|publication| {
                allowed_checkouts
                    .iter()
                    .any(|path| target_path(path) == publication.destination_checkout)
                    && publication
                        .staged_checkout
                        .starts_with(&recovery.staging_root)
                    && [
                        &publication.staged_checkout,
                        &publication.destination_checkout,
                    ]
                    .iter()
                    .all(|path| {
                        !path
                            .components()
                            .any(|part| part == std::path::Component::ParentDir)
                    })
            }),
            "pending bundle recovery identity changed; all files were kept"
        );
        destination = live_path(&recovery.destination_checkout);
        let stage_parent = worktree
            .as_ref()
            .map(|w| w.worktree_root.as_path())
            .unwrap_or(&destination)
            .parent()
            .context("checkout has no parent")?;
        ensure!(
            recovery.version == 1
                && recovery.checkpoint_sha256 == checkpoint.sha256
                && allowed_destination
                && recovery.staging_root.parent() == Some(target_path(stage_parent).as_path())
                && recovery.staging_root.file_name().is_some_and(|name| name
                    .to_string_lossy()
                    .starts_with(&format!(".mj-resume-{session_id}-")))
                && recovery.staged_checkout.starts_with(&recovery.staging_root)
                && recovery
                    .restore
                    .workspace_root
                    .starts_with(&recovery.staging_root)
                && [
                    &recovery.staging_root,
                    &recovery.staged_checkout,
                    &recovery.restore.workspace_root
                ]
                .iter()
                .all(|path| !path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)),
            "pending checkout recovery identity changed; all files were kept"
        );
        execute_checked(
            executor,
            targets::command_on_locator(
                SshTrafficClass::Control,
                &backend,
                session_id,
                vec!["mkdir".into(), "-p".into(), root.clone()],
                "create parent root for checkout recovery",
            )?,
        )?;
        let staging = tempfile::tempdir()?;
        let local_spec = staging.path().join("checkout-spec.json");
        std::fs::write(&local_spec, serde_json::to_vec(&recovery)?)?;
        super::super::checkpoint::upload_checkpoint_spec(
            executor,
            &backend,
            session_id,
            &local_spec,
            &remote_spec,
        )?;
        let mut missing = !super::super::environment::directory_available(
            &backend,
            &destination.to_string_lossy(),
            executor,
        )?;
        for publication in &recovery.additional_checkouts {
            missing |= !super::super::environment::directory_available(
                &backend,
                &live_path(&publication.destination_checkout).to_string_lossy(),
                executor,
            )?;
        }
        if missing {
            if let Some(mut staged) = worktree {
                let original_root = staged.worktree_root.clone();
                staged.worktree_root = live_path(&recovery.staging_root)
                    .join(original_root.file_name().context("checkout has no name")?);
                if destination != original_root {
                    staged.kind = mj_core::state::ManagedCheckoutKind::Clone;
                }
                if staged.kind == mj_core::state::ManagedCheckoutKind::Worktree {
                    let recovery_ref =
                        super::super::worktree::preserve_retained_managed_worktree_branch(
                            executor, &staged,
                        )?;
                    executor.notify_notice(&format!("The retained branch tip is kept at {recovery_ref} while the missing checkout is recovered."));
                }
                super::super::worktree::restore_managed_worktree(executor, &staged)?;
            } else {
                let bundle =
                    super::super::backend::backend_session_bundle(&record, &self.config, executor)?;
                let mut existing_repositories = std::collections::BTreeSet::new();
                for repository in &bundle.repositories {
                    let git = live_path(&recovery.restore.workspace_root)
                        .join(&repository.destination)
                        .join(".git");
                    if super::super::environment::directory_available(
                        &backend,
                        &git.to_string_lossy(),
                        executor,
                    )? {
                        existing_repositories.insert(repository.destination.clone());
                    }
                }
                targets::recover_checkout_plan(
                    &backend,
                    session_id,
                    &bundle,
                    &live_path(&recovery.restore.workspace_root).to_string_lossy(),
                    github_token.as_deref(),
                    &existing_repositories,
                )?
                .execute_concurrent(executor)?;
            }
        }
        self.prepare_worker_files(
            session_id,
            &backend,
            &root,
            executor,
            super::super::worker_binary::LaunchHistory::Stored,
        )?;
        if should_upload_restore_archive(&backend) {
            super::super::checkpoint::upload_checkpoint_spec(
                executor,
                &backend,
                session_id,
                &archive.archive_path,
                &format!("{root}/resume-checkout-recovery.hel.zip"),
            )?;
        }
        execute_checked(
            executor,
            targets::command_on_locator(
                SshTrafficClass::Bulk,
                &backend,
                session_id,
                vec![
                    format!("{root}/hel"),
                    "worker".into(),
                    "restore-checkpoint".into(),
                    "--spec".into(),
                    remote_spec,
                    "--missing-checkout".into(),
                ],
                "publish recovered checkout without replacement",
            )?,
        )?;
        executor.end_resumable_work("resume recovered checkout")?;
        let liveness = super::super::worker_binary::probe_worker_liveness(
            executor, &backend, session_id, &root,
        )?;
        ensure!(
            liveness.status == 0,
            "cannot establish parent worker state after checkout recovery; everything was kept, retry Resume"
        );
        match String::from_utf8_lossy(&liveness.stdout).trim() {
            "alive" | "starting" => {
                self.restart_worker_after_checkout_recovery(session_id, discard_queue, executor)
                    .await
            }
            "dead" => {
                self.recover_missing_parent_data(session_id, discard_queue, executor)
                    .await
            }
            _ => bail!(
                "cannot establish parent worker state after checkout recovery; everything was kept, retry Resume"
            ),
        }
    }
}

fn current_parent_projection(
    session_id: &str,
    archived: Option<&CanonicalSessionSnapshot>,
) -> Result<CanonicalSessionSnapshot> {
    crate::database::load_materialized_session(session_id)?
        .map(mj_transcript::projection::canonical_session_from_materialized_owned)
        .transpose()?.or_else(|| archived.cloned())
        .context("No durable parent relay state or recovery copy is available; files and children were kept. Supply a recovery copy and retry Resume.")
}

impl Controller {
    async fn recover_parent_runtime_from_store(
        &mut self,
        session_id: &str,
        discard_queue: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<MaterializedSession> {
        let (backend, root) = self.worker_placement(session_id)?;
        let dead = super::super::worker_binary::probe_worker_liveness(
            executor, &backend, session_id, &root,
        )?;
        ensure!(
            dead.status == 0 && String::from_utf8_lossy(&dead.stdout).trim() == "dead",
            "Parent runtime cannot be reconstructed while its worker may be live; everything was kept. Retry Resume."
        );
        let canonical = current_parent_projection(session_id, None)?;
        let seed = mj_checkpoint::checkpoint::relay_seed_for_restore(
            &canonical,
            true,
            if discard_queue {
                mj_checkpoint::checkpoint::QueueRestorePolicy::Discard
            } else {
                mj_checkpoint::checkpoint::QueueRestorePolicy::Restore
            },
        )?;
        executor.begin_resumable_work()?;
        self.prepare_worker_files(
            session_id,
            &backend,
            &root,
            executor,
            super::super::worker_binary::LaunchHistory::Stored,
        )?;
        let workspace =
            super::super::environment::resume_checkout_directory(self, session_id, &backend)?;
        let path = |text: &str| {
            if matches!(
                backend,
                targets::TargetLocator::SshBare { .. } | targets::TargetLocator::AwsEc2 { .. }
            ) && !Path::new(text).is_absolute()
            {
                PathBuf::from("~").join(text)
            } else {
                PathBuf::from(text)
            }
        };
        let profile = self
            .config
            .named_profile(&self.state.sessions[session_id].last_profile)?;
        let spec = CheckpointRestoreSpec {
            archive_path: path(&root).join("resume-parent-recovery.hel.zip"),
            workspace_root: path(&workspace),
            report_root: None,
            relay_root: path(&root),
            harness_home: path(&super::super::target_profile_home(
                &backend, session_id, profile,
            )),
            restore_repositories: false,
            restore_native: false,
            queue_policy: if discard_queue {
                mj_checkpoint::checkpoint::QueueRestorePolicy::Discard
            } else {
                mj_checkpoint::checkpoint::QueueRestorePolicy::Restore
            },
            primary_repository_root: None,
        };
        let staging = tempfile::tempdir()?;
        let local_spec = staging.path().join("parent-store-spec.json");
        let local_seed = staging.path().join("parent-store-seed.json");
        std::fs::write(&local_spec, serde_json::to_vec(&spec)?)?;
        std::fs::write(&local_seed, serde_json::to_vec(&seed)?)?;
        let remote_spec = format!("{root}/resume-parent-recovery.json");
        let remote_seed = format!("{root}/resume-parent-current-seed.json");
        super::super::checkpoint::upload_checkpoint_spec(
            executor,
            &backend,
            session_id,
            &local_spec,
            &remote_spec,
        )?;
        super::super::checkpoint::upload_checkpoint_spec(
            executor,
            &backend,
            session_id,
            &local_seed,
            &remote_seed,
        )?;
        execute_checked(
            executor,
            targets::command_on_locator(
                SshTrafficClass::Bulk,
                &backend,
                session_id,
                vec![
                    format!("{root}/hel"),
                    "worker".into(),
                    "restore-checkpoint".into(),
                    "--spec".into(),
                    remote_spec,
                    "--missing-parent".into(),
                    "--relay-seed".into(),
                    remote_seed,
                ],
                "rebuild missing parent relay from durable state",
            )?,
        )?;
        executor.end_resumable_work("start reconstructed parent runtime")?;
        Box::pin(self.repair_existing_environment(
            session_id,
            RetainedWorker::Installed,
            discard_queue,
            executor,
        ))
        .await
    }
}
