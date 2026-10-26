use super::*;
use crate::targets::ProvisionStage;
use std::time::Duration;

impl Controller {
    /// Historical recreation may have started a replacement before publishing
    /// Running. A live journal wins over every older checkpoint.
    pub(crate) async fn reconnect_legacy_restart(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<bool> {
        crate::worker_lifecycle::run(session_id, "observe legacy Restart worker", executor, async {
            let (backend, root) = self.worker_placement(session_id)?;
            let output = executor.execute(&super::worker_binary::worker_liveness_command(&backend, &root))?;
            ensure!(output.status == 0, "cannot establish legacy worker liveness; retaining its checkpoint and environment");
            if String::from_utf8_lossy(&output.stdout).trim() == "dead" { return Ok(false); }
            executor.begin_resumable_work()?;
            let reconnect = targets::reconnect_plan(&backend, session_id)?.commands.into_iter().next().context("legacy reconnect plan is empty")?;
            let mut connection = super::readiness::connect_started_worker_with_timeout(&reconnect, session_id, executor, &backend, &root, Duration::from_secs(300)).await?;
            if connection.snapshot().operational.execution == mj_core::relay::RelayExecutionState::Closed { executor.end_resumable_work("restore sealed legacy Restart")?; return Ok(false); }
            let harness = self.state.sessions[session_id].harness_kind;
            let native = super::readiness::wait_for_native_session(&mut connection, executor, harness).await?;
            self.mark_worker_connected(session_id, Some(native))?;
            Ok(true)
        }).await
    }

    /// Replace an explicitly selected container under the worker's atomic idle
    /// reservation. The private journal and profile are transferred separately from retained
    /// workspace/cache mounts; no checkpoint or worker-root reset is involved.
    pub(crate) async fn upgrade_container_worker(
        &self,
        session_id: &str,
        plan: &mut targets::RetainedContainer,
        recovering: bool,
        executor: &(impl CommandExecutor + Sync),
        manager: &crate::session_manager::SessionManagerControl,
        commit: impl Fn() -> Result<bool> + Send + Sync,
    ) -> Result<()> {
        use super::worker_binary::{
            WorkerInstall, install_staged_worker_binary, replace_installed_worker_launch_config,
            stage_worker_binary_for_upgrade, start_worker_durably,
            stop_worker_after_target_recovery, worker_liveness_command,
        };
        let record = self
            .state
            .sessions
            .get(session_id)
            .context("session missing")?;
        let target = record
            .target
            .as_ref()
            .context("container upgrade target disappeared")?;
        let backend = backend_locator(target, record, &self.config)?;
        let root = targets::worker_root(&backend, session_id)?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("worker reconnect plan is empty")?;
        loop {
            ensure!(
                !executor.cancellation_requested(),
                "pending container upgrade cancelled; environment retained"
            );
            let attempt = if let Some(owner) =
                crate::worker_lifecycle::WorkerPermit::try_acquire(session_id, "container upgrade")?
            {
                owner.scope(async {
                    owner.verify_target(target)?;
                    let source = plan.source_is_current(executor)?;
                    let mut lease = if source {
                        if recovering {
                            let recovery = targets::target_recovery_plan(&backend, session_id)?;
                            targets::ensure_recovery_target_running(executor, recovery.as_ref())?;
                        }
                        let liveness = executor.execute(&worker_liveness_command(&backend, &root))?;
                        ensure!(liveness.status == 0, "cannot establish source worker liveness; container retained");
                        if String::from_utf8_lossy(&liveness.stdout).trim() == "dead" {
                            ensure!(recovering, "the source worker is unavailable; Restart agent before requesting a container upgrade");
                            None
                        } else {
                            let handle = while_upgrade_active(executor, manager.wait_for_session(session_id, Duration::from_secs(5))).await?;
                            executor.end_resumable_work("container worker swap")?;
                            let Some(mut lease) = while_upgrade_active(executor, super::IdleWorkspaceLease::acquire_for_upgrade(&handle, record.harness_kind)).await? else {
                                executor.begin_resumable_work()?;
                                return Ok(false);
                            };
                            ensure!(lease.protocol_version() >= 21, "this worker predates atomic idle reservation; upgrade its worker in place before upgrading the container");
                            if !lease.verify_for_upgrade().await? { executor.begin_resumable_work()?; return Ok(false); }
                            Some(lease)
                        }
                    } else { None };
                    if !commit()? { executor.begin_resumable_work()?; return Ok(false); }
                    if source {
                        stop_worker_after_target_recovery(executor, &backend, session_id, &root)?;
                        // The source and the committed intent survive handoff.
                        // Copy only control state; mounted caches are excluded.
                        executor.begin_resumable_work()?;
                        plan.capture_control_state(executor)?;
                    }
                    executor.end_resumable_work("replace session container")?;
                    plan.replace(session_id, executor)?;
                    executor.begin_resumable_work()?;
                    let liveness = executor.execute(&worker_liveness_command(&backend, &root))?;
                    ensure!(liveness.status == 0, "cannot establish replacement worker liveness; environment retained");
                    let dead = String::from_utf8_lossy(&liveness.stdout).trim() == "dead";
                    if !plan.control_state_restored {
                        if dead { plan.restore_control_state(executor)?; }
                        plan.control_state_restored = true;
                        crate::database::prepare_container_upgrade(session_id, &plan.operation_id, plan.clone())?;
                    }
                    if dead {
                        // Only positive death permits another start. Never
                        // overwrite a journal a replacement already advanced.
                        owner.settle_dead_restart(target)?;
                        let worker = WorkerInstall::for_target(&backend, executor)?;
                        let staging = stage_worker_binary_for_upgrade(executor, &backend, session_id, &worker)?;
                        let launch = self.current_worker_launch_config(session_id, &backend)?;
                        executor.end_resumable_work("start container worker")?;
                        install_staged_worker_binary(&owner, &staging, executor, &backend, session_id)?;
                        replace_installed_worker_launch_config(executor, &backend, session_id, &launch)?;
                        owner.begin_restart(target, mj_core::worker_build::WORKER_BUILD_ID.into())?;
                        start_worker_durably(&owner, target, executor, &backend, &root)?;
                    }
                    executor.begin_resumable_work()?;
                    let mut connection = super::readiness::connect_started_worker_with_timeout(&reconnect, session_id, executor, &backend, &root, Duration::from_secs(300)).await?;
                    super::readiness::wait_for_native_session(&mut connection, executor, record.harness_kind).await?;
                    if let Some(intent) = crate::database::load_worker_restart(session_id)? {
                        crate::database::finish_worker_restart(session_id, &intent.operation_id)?;
                    }
                    if let Some(lease) = lease.take() { lease.finish_replacement(connection); }
                    Ok(true)
                }).await?
            } else {
                false
            };
            if attempt {
                return Ok(());
            }
            executor.begin_resumable_work()?;
            let _stage = crate::targets::ProvisionStageGuard::new(
                executor,
                ProvisionStage::WaitingForContainerUpgrade,
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub(crate) async fn recover_removed_restart_container(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let record = self
            .state
            .sessions
            .get(session_id)
            .context("session missing")?
            .clone();
        let checkpoint = record
            .checkpoint
            .as_ref()
            .context("legacy upgrade has no recovery archive")?;
        super::resume::verify_resume_checkpoint(session_id, checkpoint)?;
        let configured = self
            .config
            .targets
            .get(&record.target_template_id)
            .context("legacy target configuration missing")?;
        let mut target = backend::backend_target(
            configured,
            record.resource_allocation.as_ref(),
            backend::ContainerOverrides::for_session(&record),
        )?;
        for notice in super::provisioning::enforce_cgroup_capable_limits(&mut target, executor) {
            executor.notify_notice(&notice);
        }
        let canonical = match record.target.as_ref() {
            Some(locator) => locator.clone(),
            None => backend::locator_after_provision_named(
                configured,
                &target,
                session_id,
                None,
                executor,
                &targets::resource_name(session_id)?,
            )?,
        };
        let locator = backend_locator(&canonical, &record, &self.config)?;
        {
            let current = self.state.sessions.get_mut(session_id).unwrap();
            current.target = Some(canonical);
            current.state = SessionState::Closing;
        }
        self.persist_session_state(session_id)?;
        let (_, template) = target
            .image_host()
            .context("legacy target is not a supported container")?;
        let mut mounts = record
            .target_runtime
            .as_ref()
            .and_then(|r| r.container_settings.as_ref())
            .map_or(record.additional_mounts.as_slice(), |s| {
                s.additional_mounts.as_slice()
            })
            .to_vec();
        // Reattach recorded cache storage without discovery, allocation or
        // release. Historical applied settings contain only user attachments.
        let mut retain = |source: PathBuf, destination: PathBuf| -> Result<()> {
            if let Some(mount) = mounts.iter().find(|m| m.destination == destination) {
                ensure!(
                    mount.source == source && mount.access == targets::MountAccess::Rw,
                    "recorded cache mount conflicts with a retained attachment"
                );
            } else {
                mounts.push(targets::AdditionalMount {
                    source,
                    destination,
                    access: targets::MountAccess::Rw,
                });
            }
            Ok(())
        };
        if let Some(cache) = &record.build_cache {
            retain(cache.directory.clone(), cache.directory.clone())?;
            if let Some(root) = &cache.target_root {
                retain(root.clone(), root.clone())?;
            }
        }
        if let Some(cache) = record
            .target_runtime
            .as_ref()
            .and_then(|r| r.tool_cache.as_ref())
        {
            let host = match &locator {
                targets::TargetLocator::SshPodman { ssh, .. }
                | targets::TargetLocator::SshDocker { ssh, .. }
                | targets::TargetLocator::SshAppleContainer { ssh, .. } => {
                    super::cache_host::CacheHost::Ssh(ssh.clone())
                }
                _ => super::cache_host::CacheHost::Local,
            };
            ensure!(
                cache.host == host.key(),
                "recorded native cache host changed; refusing legacy recovery"
            );
            retain(cache.directory.clone(), cache.directory.clone())?;
            retain(
                mj_core::target_path::join(&host.home(executor)?, Path::new(".nx")),
                cache.nx_home.clone(),
            )?;
        }
        let restore_repositories = targets::recover_removed_container(
            &locator,
            template,
            session_id,
            &mounts,
            &targets::container_workspace_root(record.container_workspace.as_deref()),
            executor,
        )?;
        let current = self.state.sessions.get_mut(session_id).unwrap();
        current.state = SessionState::Closing;
        self.persist_session_state(session_id)?;
        self.restore_legacy_container(session_id, restore_repositories, executor)
            .await
    }
    pub(crate) fn plan_container_upgrade(
        &self,
        session_id: &str,
        operation_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<targets::RetainedContainer> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session is missing")?;
        let locator = session
            .target
            .as_ref()
            .context("session has no retained container")?;
        let backend = backend_locator(locator, session, &self.config)?;
        let template = self
            .config
            .targets
            .get(&session.target_template_id)
            .context("session target configuration is missing")?;
        let container = match template {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::SshPodman { container, .. }
            | TargetTemplate::SshDocker { container, .. } => container,
            _ => bail!("this target does not support storage-preserving container upgrades"),
        };
        let workspace = targets::container_workspace_root(session.container_workspace.as_deref());
        let root = targets::worker_root(&backend, session_id)?;
        let plan = targets::inspect_retained_container(
            &backend,
            session_id,
            &container.image,
            operation_id,
            Path::new(&workspace),
            Path::new(&root),
            executor,
        )?;
        ensure!(
            container.pull_policy != mj_core::config::ImagePullPolicy::Never
                || plan.prepared_image.is_some(),
            "target image is not cached and pull_policy is never; fetch the image before requesting an upgrade"
        );
        Ok(plan)
    }

    pub(crate) fn record_container_upgrade(&mut self, session_id: &str, image: &str) -> Result<()> {
        let record = self
            .state
            .sessions
            .get_mut(session_id)
            .context("session disappeared during container upgrade")?;
        // The mounts and resource selection remain the source's applied settings.
        if let Some(settings) = record
            .target_runtime
            .as_mut()
            .and_then(|r| r.container_settings.as_mut())
        {
            settings.default_image =
                Some((image == mj_core::config::DEFAULT_CONTAINER_IMAGE).then(|| image.to_owned()));
        }
        self.persist_session_state(session_id)
    }
}

/// Pending relay waits yield immediately when daemon handoff interrupts them.
async fn while_upgrade_active<T>(
    executor: &impl CommandExecutor,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(super::readiness::CANCELLATION_POLL_INTERVAL) => {
                ensure!(!executor.cancellation_requested(), "container upgrade interrupted; retained intent will recover");
            }
        }
    }
}
