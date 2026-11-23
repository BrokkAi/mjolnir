use super::terminal_operations::{process_probe, terminal_blocking};
use super::*;
use crate::server::api::RepositoriesOperations;
use mj_client::http::repositories::*;
use mj_client::session::BoxFuture;
use mj_core::local_git::LocalRemoteRepair;
use mj_core::state::{
    ArchiveSpacePreview, ProjectSourceIdentity, RecoveryScan, ResumeRepositorySourcePreflight,
};

impl RepositoriesOperations for RuntimeState {
    fn new_session_remote_repairs(
        self: Arc<Self>,
        request: NewSessionRemoteRepairsRequest,
    ) -> BoxFuture<'static, Result<Vec<LocalRemoteRepair>>> {
        Box::pin(async move {
            let NewSessionRemoteRepairsRequest {
                bundle_id,
                target_id,
            } = request;

            let controller = self.with_config(|config| {
                mj_controller::controller::config_only_controller(config.clone())
            });
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                let result =
                    controller.preflight_new_session(&bundle_id, &target_id, None, executor)?;
                Ok(result.remote_repairs)
            })
            .await
        })
    }

    fn repair_repository_remotes(
        self: Arc<Self>,
        request: RepairRepositoryRemotesRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let RepairRepositoryRemotesRequest { bundle_id, repairs } = request;

            let controller = self.with_config(|config| {
                mj_controller::controller::config_only_controller(config.clone())
            });
            terminal_blocking(move || {
                let bundle = controller
                    .config
                    .bundles
                    .get(&bundle_id)
                    .context("unknown bundle")?;
                let executor = mj_controller::targets::CancellableProcessExecutor::new(
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )
                .with_deadline(Duration::from_secs(30));
                mj_core::local_git::apply_repository_remote_repairs(bundle, &repairs, &executor)
            })
            .await?;
            refresh_runtime_controller(&self).await;
            Ok(())
        })
    }

    fn session_project_source(
        self: Arc<Self>,
        request: SessionProjectSourceRequest,
    ) -> BoxFuture<'static, Result<ProjectSourceIdentity>> {
        Box::pin(async move {
            let SessionProjectSourceRequest { session_id } = request;

            let controller =
                self.with_controller(|controller| mj_controller::controller::Controller {
                    config: controller.config.clone(),
                    state: controller.state.clone(),
                });
            process_probe(Some(Duration::from_secs(8)), move |executor| {
                controller.resolve_session_project_source(&session_id, executor)
            })
            .await
        })
    }

    fn scan_recovery(
        self: Arc<Self>,
        request: ScanRecoveryRequest,
    ) -> BoxFuture<'static, Result<RecoveryScan>> {
        Box::pin(async move {
            let ScanRecoveryRequest { all_instances } = request;

            process_probe(None, move |executor| {
                Ok(Controller::load()?.scan_orphan_workers(executor, all_instances))
            })
            .await
        })
    }

    fn destroy_recovery(
        self: Arc<Self>,
        request: DestroyRecoveryRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let DestroyRecoveryRequest {
                session_id,
                target_id,
                confirmation,
                all_instances,
            } = request;

            ensure_no_active_lifecycle(&self)?;
            let admission = self
                .session_gate
                .enter_foreground(
                    &session_id,
                    "destroy recovered environment",
                    mj_controller::recovery_gate::LifecycleOrigin::Person,
                    &AtomicBool::new(false),
                    &ProcessExecutor,
                )
                .await?;
            let owner =
                mj_controller::worker_lifecycle::WorkerPermit::acquire(&session_id, &admission)?;
            terminal_blocking(move || {
                let _admission = admission;
                owner.scope_blocking(|| {
                    Controller::load()?.destroy_orphan_worker(
                        &session_id,
                        &target_id,
                        &confirmation,
                        all_instances,
                        &ProcessExecutor,
                    )
                })
            })
            .await?;
            Ok(())
        })
    }

    fn resume_repository_preflight(
        self: Arc<Self>,
        request: ResumeRepositoryPreflightRequest,
    ) -> BoxFuture<'static, Result<ResumeRepositorySourcePreflight>> {
        Box::pin(async move {
            let ResumeRepositoryPreflightRequest {
                session_id,
                target_id,
            } = request;

            let cancelled = CancellationToken::new();
            let _cancel_request = cancelled.clone().drop_guard();
            process_probe(Some(Duration::from_secs(30)), move |executor| {
                let controller = Controller::load()?;
                let executor = executor.clone().with_cancellation(cancelled.clone());
                mj_core::runtime::block_on(async {
                    tokio::select! {
                        _ = cancelled.cancelled() => bail!("resume repository preflight cancelled"),
                        result = controller.preflight_resume_repository_sources(
                            &session_id, &target_id, &executor,
                        ) => result,
                    }
                })?
            })
            .await
        })
    }

    fn replace_resume_repository_origin(
        self: Arc<Self>,
        request: ReplaceResumeRepositoryOriginRequest,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<ResumeRepositorySourcePreflight>> {
        Box::pin(async move { replace_resume_repository_origin(self, request, shutdown).await })
    }

    fn archive_space_preview(
        self: Arc<Self>,
        request: ArchiveSpacePreviewRequest,
    ) -> BoxFuture<'static, Result<ArchiveSpacePreview>> {
        Box::pin(async move {
            let ArchiveSpacePreviewRequest { older_than_days } = request;

            terminal_blocking(move || {
                mj_controller::sessionwiki::archive_space_preview(older_than_days)
            })
            .await
        })
    }
}

async fn replace_resume_repository_origin(
    state: Arc<RuntimeState>,
    request: ReplaceResumeRepositoryOriginRequest,
    shutdown: CancellationToken,
) -> Result<ResumeRepositorySourcePreflight> {
    use mj_controller::controller::resume::ResumeOriginPreparation;
    let ReplaceResumeRepositoryOriginRequest {
        session_id,
        repository_id,
        replacement,
    } = request;

    let shutdown = shutdown.child_token();
    let _cancel_request = shutdown.clone().drop_guard();
    let executor = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
        .with_deadline(Duration::from_secs(30))
        .with_cancellation(shutdown.clone());
    let _cancel = executor.cancel_on_drop();
    let prepared = terminal_blocking({
        let executor = executor.clone();
        let session_id = session_id.clone();
        move || {
            let controller = Controller::load()?;
            let preparation = mj_core::runtime::block_on(async {
                tokio::select! {
                    _ = shutdown.cancelled() => bail!("repository origin verification cancelled during daemon shutdown"),
                    result = controller.prepare_resume_repository_origin(&session_id, &repository_id, &replacement, &executor) => result,
                }
            })??;
            Ok((controller, preparation))
        }
    }).await?;
    let (mut controller, repair) = match prepared {
        (_, ResumeOriginPreparation::Mismatch(mismatch)) => {
            return Ok(ResumeRepositorySourcePreflight::RepositoryMoved(mismatch));
        }
        (controller, ResumeOriginPreparation::Verified(repair)) => (controller, repair),
    };

    // The network verification above and the remaining-source checks below
    // can both be abandoned during handoff. Only save and publication hold it.
    let admission =
        mj_controller::upgrade::gate().enter_unless_draining("repository origin replacement")?;
    let (controller, saved) = mj_core::runtime::spawn(async move {
        let _admission = admission;
        let result = terminal_blocking(move || {
            ensure!(
                !mj_controller::controller::move_session::move_owns_session(&session_id),
                "session is moving; repair its repository after Move finishes"
            );
            let saved = repair.save(&mut controller)?;
            Ok((controller, saved))
        })
        .await;
        refresh_runtime_controller(&state).await;
        result
    })
    .await
    .context("repository origin replacement task panicked")??;
    terminal_blocking(move || saved.check_remaining(&controller, &executor)).await
}
