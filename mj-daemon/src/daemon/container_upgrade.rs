use super::*;
use mj_core::state::{ContainerSettingsChange, ContainerUpgradePreview, ContainerUpgradeRequest};

fn retained_upgrade_preview(
    plan: &mj_controller::targets::RetainedContainer,
    session: SessionRecord,
) -> ContainerUpgradePreview {
    if plan.replaces_settings() {
        plan.settings_preview(
            &session.id,
            ContainerSettingsChange {
                resource_allocation: session.resource_allocation,
                mounts: session.additional_mounts,
                mount_history: None,
            },
        )
    } else {
        plan.preview(&session.id)
    }
}

impl RuntimeState {
    /// Save deferred settings or return the confirmation for applying them now.
    pub(crate) async fn set_container_settings(
        self: &Arc<Self>,
        session_id: String,
        settings: ContainerSettingsChange,
    ) -> Result<Option<ContainerUpgradePreview>> {
        let _admission = mj_controller::upgrade::activity("container settings request")?;
        self.validate_container_settings_size(&session_id, settings.resource_allocation.as_ref())
            .await?;
        let preview = blocking(move || {
            ensure!(
                !mj_controller::controller::move_session::move_owns_session(&session_id),
                "session is moving; change container settings after Move finishes"
            );
            let mut controller = Controller::load()?;
            if let Some(plan) = controller.plan_container_settings_replacement(
                &session_id,
                "preview",
                &settings,
                &ProcessExecutor,
            )? {
                return Ok(Some(plan.settings_preview(&session_id, settings)));
            }
            controller.update_session_container_settings(
                &session_id,
                settings.resource_allocation,
                settings.mounts,
                settings.mount_history,
                &ProcessExecutor,
            )?;
            Ok(None)
        })
        .await?;
        if preview.is_none() {
            self.reload_controller().await?;
        }
        Ok(preview)
    }

    pub async fn preview_container_upgrade(
        &self,
        session_id: String,
    ) -> Result<ContainerUpgradePreview> {
        blocking(move || {
            if let Some((intent, session)) =
                mj_controller::database::load_restart_and_session(&session_id)?
                && let Some(plan) = intent.container_upgrade
            {
                return Ok(retained_upgrade_preview(&plan, session));
            }
            Controller::load()?
                .plan_container_upgrade(&session_id, "preview", &ProcessExecutor)
                .map(|plan| plan.preview(&session_id))
        })
        .await
    }

    /// Acceptance returns only after the selection is durable. The lifecycle
    /// continues independently of the caller and of daemon handoff while idle.
    pub async fn upgrade_container(
        self: &Arc<Self>,
        request: ContainerUpgradeRequest,
    ) -> Result<()> {
        ensure!(
            request.acknowledge_writable_layer_loss,
            "container upgrade requires acknowledgment that writable-layer state may be lost"
        );
        let session_id = request.selection.session_id.clone();
        let request_key = serde_json::to_string(&request.selection)?;
        let existing = blocking({
            let session_id = session_id.clone();
            move || mj_controller::database::load_restart_and_session(&session_id)
        })
        .await?;
        let settings = request.selection.settings.as_deref().cloned();
        let existing = if let Some((intent, session)) = existing {
            let mut selection = request.selection.clone();
            if let Some(settings) = &mut selection.settings {
                settings.mount_history = None;
            }
            ensure!(
                intent
                    .container_upgrade
                    .as_ref()
                    .is_some_and(|plan| retained_upgrade_preview(plan, session) == selection),
                "another Restart or upgrade already owns this environment"
            );
            if self
                .owner()
                .lifecycle
                .get(&session_id)
                .is_some_and(|active| {
                    active.kind == LifecycleKind::UpgradeContainer && active.is_running()
                })
            {
                return Ok(());
            }
            Some(intent)
        } else {
            None
        };
        let phase = if existing.as_ref().is_some_and(|intent| {
            intent.phase == mj_controller::database::SessionRestartPhase::RecreatingContainer
        }) {
            LifecyclePhase::RetiringContainer
        } else {
            LifecyclePhase::Executing
        };
        let (accepted, acceptance) = tokio::sync::watch::channel(None);
        let mut operation = self.admit_prepared_lifecycle(
            session_id,
            LifecycleKind::UpgradeContainer,
            super::lifecycle::LifecycleStart { phase, ..super::lifecycle::LifecycleStart::request(request_key, None, None) },
            Some(acceptance),
            move |state, session_id, cancelled, session_admission| async move {
                if let Some(settings) = &settings {
                    state.validate_container_settings_size(&session_id, settings.resource_allocation.as_ref()).await?;
                }
                let admitted = blocking({
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || {
                        if let Some(intent) = existing { return Ok(intent); }
                        let operation_id = new_command_id("container-upgrade")?;
                        let controller = Controller::load()?;
                        let executor = CancellableProcessExecutor::new(cancelled.clone());
                        if let Some(settings) = settings {
                            let plan = controller.plan_container_settings_replacement(&session_id, &operation_id, &settings, &executor)?
                                .context("container settings or preservation preflight changed; review settings again")?;
                            ensure!(plan.settings_preview(&session_id, settings.clone()) == request.selection,
                                "container or settings changed since confirmation; review settings again");
                            let history_host = controller.config.targets.get(&controller.state.sessions[&session_id].target_template_id)
                                .and_then(mj_core::config::mount_history_host);
                            if let Some(host) = history_host {
                                if let Some(history) = &settings.mount_history {
                                    mj_controller::database::replace_mount_history(host, history)?;
                                }
                                mj_controller::database::remember_mount_sources(host, &settings.mounts)?;
                            }
                            return mj_controller::database::begin_container_upgrade(&session_id, &operation_id, cancelled, plan, Some(settings));
                        }
                        let plan = controller.plan_container_upgrade(&session_id, &operation_id, &executor)?;
                        ensure!(plan.preview(&session_id) == request.selection, "container or image selection changed since confirmation; refresh the upgrade preview");
                        ensure!(request.selection.available, "this container already uses the selected image");
                        mj_controller::database::begin_container_upgrade(&session_id, &operation_id, cancelled, plan, None)
                    }
                }).await;
                let intent = admitted?;
                state.bind_restart_intent(&session_id, &intent.operation_id)?;
                accepted.send_replace(Some(Ok(())));
                state.restart_admitted(&session_admission, session_id, cancelled, Some(intent), false).await
            },
        ).await?;
        let runtime = self.clone();
        let result = operation.result.clone();
        mj_core::runtime::spawn(async move {
            if let Err(error) = Self::wait_lifecycle_result(result.clone()).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "container upgrade retained its environment and recovery intent"
                );
            }
            runtime.remove_completed_lifecycle(&result);
        });
        operation.confirm_acceptance().await.map(|_| ())
    }
}
