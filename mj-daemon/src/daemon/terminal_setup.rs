use super::terminal_operations::{process_probe, terminal_blocking};
use super::*;
use crate::server::api::SetupOperations;
use mj_client::daemon::FirstRunSetupReport;
use mj_client::http::setup::*;
use mj_client::session::BoxFuture;
use mj_client::web::{WebListenerProcess, WebViewerAccess};
use mj_core::subagent::SubagentOptions;
use mj_core::targets::{
    CommandExecutor, CommandOutput, CommandSpec, CommandTimedOut, DeadlinePause, HostCapacityRow,
};
use std::io::Read;

impl SetupOperations for RuntimeState {
    fn config(self: Arc<Self>) -> BoxFuture<'static, Result<Config>> {
        Box::pin(async move { Ok(self.with_config(Clone::clone)) })
    }

    fn first_run_setup_due(self: Arc<Self>) -> BoxFuture<'static, Result<bool>> {
        Box::pin(async move {
            terminal_blocking(|| {
                mj_setup::setup::automatic_setup_due(
                    &mj_core::config::config_path(),
                    &mj_core::config::data_dir(),
                )
            })
            .await
        })
    }

    fn run_first_run_setup(
        self: Arc<Self>,
        request: RunFirstRunSetupRequest,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<Option<FirstRunSetupReport>>> {
        Box::pin(
            async move { run_first_run_setup(self, request.project_directory, shutdown).await },
        )
    }

    fn run_doctor(self: Arc<Self>) -> BoxFuture<'static, Result<Vec<String>>> {
        Box::pin(async move {
            // Not `blocking`: the doctor only reads, so it takes no upgrade
            // admission, and a handoff abandons it instead of waiting for it.
            process_probe(None, |executor| {
                let executor = PrerequisiteExecutor(executor);
                let checks = mj_setup::doctor::run_with_config_path(
                    &mj_core::config::config_path(),
                    &executor,
                    mj_controller::targets::current_apple_platform(&executor),
                    mj_setup::doctor::DoctorOptions { smoke: false },
                );
                Ok(mj_setup::setup::welcome_errors(&checks))
            })
            .await
        })
    }

    fn save_setup(
        self: Arc<Self>,
        request: SaveSetupRequest,
    ) -> BoxFuture<'static, Result<Config>> {
        Box::pin(async move {
            let SaveSetupRequest { original, updated } = request;

            let saved = terminal_blocking(move || {
                let sessions = mj_controller::database::load_state()?;
                mj_client::settings::save::save_at(
                    &mj_core::config::config_path(),
                    &original,
                    &updated,
                    &sessions,
                )
            })
            .await?;
            refresh_runtime_controller(&self).await;
            Ok(saved)
        })
    }

    fn api_credential(self: Arc<Self>) -> BoxFuture<'static, Result<String>> {
        Box::pin(async move {
            Ok(self
                .api_credential
                .get()
                .context("the HTTP API credential is not ready")?
                .clone())
        })
    }

    fn web_access(self: Arc<Self>) -> BoxFuture<'static, Result<WebViewerAccess>> {
        Box::pin(async move { Ok(self.web_viewer.access()) })
    }

    fn recover_web_viewer(
        self: Arc<Self>,
        request: RecoverWebViewerRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let RecoverWebViewerRequest { action } = request;

            self.web_viewer.recover(action)?;
            Ok(())
        })
    }

    fn inspect_web_listener(
        self: Arc<Self>,
    ) -> BoxFuture<'static, Result<Vec<WebListenerProcess>>> {
        Box::pin(async move {
            let address = self.web_viewer.conflict_address()?;
            let processes = process_probe(None, move |executor| {
                crate::web_viewer::inspect_listener_cancellable(
                    address,
                    executor.cancellation_flag(),
                )
            })
            .await?;
            ensure!(
                self.web_viewer.conflict_address()? == address,
                "The viewer address changed. Inspect again."
            );
            Ok(processes)
        })
    }

    fn save_spinner_style(
        self: Arc<Self>,
        request: SaveSpinnerStyleRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let SaveSpinnerStyleRequest { style } = request;

            // Hold the configuration owner through persistence and publication:
            // an acknowledged save must be visible to the next dashboard.
            let mut inactivity = self.config_mutation.lock().await;
            let config = terminal_blocking(move || {
                Config::update(|config| {
                    config.spinner = style;
                    Ok(())
                })
                .map(|(config, ())| config)
            })
            .await?;
            self.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
            self.review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
            self.owner().install_config(config);
            self.publish_revision();
            Ok(())
        })
    }

    fn set_quota_preferences(
        self: Arc<Self>,
        request: SetQuotaPreferencesRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let SetQuotaPreferencesRequest { prefer } = request;

            // Hold the configuration owner through persistence and publication:
            // an acknowledged save must be visible to the next dashboard.
            let mut inactivity = self.config_mutation.lock().await;
            let config = terminal_blocking(move || {
                Config::update(|config| {
                    config.quota.prefer = prefer;
                    Ok(())
                })
                .map(|(config, ())| config)
            })
            .await?;
            self.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
            self.review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
            self.owner().install_config(config);
            self.publish_revision();
            Ok(())
        })
    }

    fn check_availability(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.request_availability_check()?;
            Ok(())
        })
    }

    fn host_capacity(self: Arc<Self>) -> BoxFuture<'static, Result<Vec<HostCapacityRow>>> {
        Box::pin(async move {
            Ok(self
                .capacity_feed()
                .context("the daemon capacity service is not running")?
                .host_rows())
        })
    }

    fn refresh_capacity(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(
                self.capacity_feed()
                    .is_some_and(|feed| feed.request_refresh()),
                "the daemon capacity service is not running"
            );
            Ok(())
        })
    }

    fn refresh_quota(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.request_quota_refresh()?;
            Ok(())
        })
    }

    fn probe_quota_profiles(
        self: Arc<Self>,
        request: ProbeQuotaProfilesRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let ProbeQuotaProfilesRequest {
                profile_ids,
                forced,
            } = request;

            let config = self.owner().controller().config.clone();
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

            let mut requests = Vec::new();
            for id in &profile_ids {
                let profile = config
                    .profiles
                    .get(id)
                    .with_context(|| format!("no profile named {id:?} is configured"))?;
                requests.push(mj_controller::quota::QuotaRefreshRequest::for_profile(
                    id,
                    profile,
                    cwd.clone(),
                ));
            }
            RuntimeState::probe_quota_profiles(&self, requests, forced)?;
            Ok(())
        })
    }

    fn subagent_options(
        self: Arc<Self>,
        request: SubagentOptionsRequest,
    ) -> BoxFuture<'static, Result<SubagentOptions>> {
        Box::pin(async move {
            let SubagentOptionsRequest {
                profile,
                model,
                config,
            } = request;

            let config = config.unwrap_or_else(|| self.worker_controller_projection().config);
            self.profile_catalog
                .get()
                .context("profile catalogue unavailable")?
                .options_for(&config, &profile, model)
                .await
        })
    }

    fn warm_profile_capabilities(
        self: Arc<Self>,
        request: WarmProfileCapabilitiesRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let WarmProfileCapabilitiesRequest { config } = request;

            self.profile_catalog
                .get()
                .context("profile catalogue unavailable")?
                .ensure(&config);
            Ok(())
        })
    }

    fn rename_profile(
        self: Arc<Self>,
        request: RenameProfileRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let RenameProfileRequest { old_id, new_id } = request;

            let _config_mutation = self.config_mutation.lock().await;
            ensure_no_active_lifecycle(&self)?;
            let controller = terminal_blocking(move || {
                let mut controller = Controller::load()?;
                controller.rename_profile_id(&old_id, &new_id)?;
                Ok(controller)
            })
            .await?;
            install_renamed_controller(&self, controller);
            Ok(())
        })
    }

    fn rename_target(
        self: Arc<Self>,
        request: RenameTargetRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let RenameTargetRequest { old_id, new_id } = request;

            let _config_mutation = self.config_mutation.lock().await;
            ensure_no_active_lifecycle(&self)?;
            let controller = terminal_blocking(move || {
                let mut controller = Controller::load()?;
                controller.rename_target_id(&old_id, &new_id)?;
                Ok(controller)
            })
            .await?;
            install_renamed_controller(&self, controller);
            Ok(())
        })
    }
}

async fn run_first_run_setup(
    state: Arc<RuntimeState>,
    project_directory: Option<PathBuf>,
    shutdown: CancellationToken,
) -> Result<Option<FirstRunSetupReport>> {
    // Lock acquisition and discovery are cancellable. Only the final local
    // save and publication keep the handoff open.
    let executor = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
        .with_cancellation(shutdown);
    let _cancel = executor.cancel_on_drop();
    mj_core::runtime::spawn(async move {
        let (report, admission) = terminal_blocking(move || {
            let mut admission = None;
            let report = mj_setup::setup::run_setup(
                &mj_core::config::config_path(),
                &mj_core::config::data_dir(),
                project_directory.as_deref(),
                &PrerequisiteExecutor(&executor),
                &|| executor.is_cancelled_without_deadline(),
                || {
                    admission = Some(
                        mj_controller::upgrade::gate().enter_unless_draining("first-run setup")?,
                    );
                    Ok(())
                },
            );
            Ok((report, admission))
        })
        .await?;
        if admission.is_some() {
            refresh_runtime_controller(&state).await;
        }
        report
    })
    .await
    .context("first-run setup task panicked")?
}

/// Preserve the prerequisite timeout for each command while sharing the
/// HTTP request's cancellation across the whole discovery pass.
struct PrerequisiteExecutor<'a>(&'a CancellableProcessExecutor);

impl CommandExecutor for PrerequisiteExecutor<'_> {
    fn pause_deadline(&self) -> DeadlinePause {
        DeadlinePause::none()
    }

    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.0.execute_without_deadline(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn Read + Send),
    ) -> Result<CommandOutput> {
        self.0.execute_with_stdin_without_deadline(command, input)
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.0.is_cancelled_without_deadline()
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let timeout = mj_setup::doctor::PROBE_TIMEOUT;
        let executor = self.0.clone().with_deadline(timeout);
        executor.execute(command).map_err(|error| {
            if executor.is_cancelled() && !executor.is_cancelled_without_deadline() {
                CommandTimedOut {
                    program: command.program.clone(),
                    purpose: command.purpose.clone(),
                    timeout,
                }
                .into()
            } else {
                error
            }
        })
    }

    fn cancellation_requested(&self) -> bool {
        self.0.is_cancelled()
    }
}
