use super::*;
use mj_core::state::{ContainerUpgradePreview, ContainerUpgradeRequest};

impl RuntimeState {
    pub async fn preview_container_upgrade(
        &self,
        session_id: String,
    ) -> Result<ContainerUpgradePreview> {
        blocking(move || {
            if let Some(plan) = crate::database::load_session_restarts()?
                .into_iter()
                .find(|intent| intent.session_id == session_id)
                .and_then(|intent| intent.container_upgrade)
            {
                return Ok(plan.preview(&session_id));
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
            move || {
                Ok(crate::database::load_session_restarts()?
                    .into_iter()
                    .find(|intent| intent.session_id == session_id))
            }
        })
        .await?;
        if let Some(intent) = &existing {
            ensure!(
                intent
                    .container_upgrade
                    .as_ref()
                    .is_some_and(|plan| plan.preview(&session_id) == request.selection),
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
        }
        let phase = if existing.as_ref().is_some_and(|intent| {
            intent.phase == crate::database::SessionRestartPhase::RecreatingContainer
        }) {
            LifecyclePhase::RetiringContainer
        } else {
            LifecyclePhase::Executing
        };
        let (accepted, acceptance) = tokio::sync::oneshot::channel();
        let operation = self.admit_lifecycle(
            session_id,
            LifecycleKind::UpgradeContainer,
            super::lifecycle::LifecycleStart {
                resume_workspace_id: None,
                request_key: Some(request_key),
                control: None,
                phase,
                move_operation_id: None,
                deferrable_admission: None,
            },
            move |state, session_id, cancelled| async move {
                let admitted = blocking({
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || {
                        if let Some(intent) = existing { return Ok(intent); }
                        let operation_id = new_command_id("container-upgrade")?;
                        let plan = Controller::load()?.plan_container_upgrade(&session_id, &operation_id, &CancellableProcessExecutor::new(cancelled.clone()))?;
                        ensure!(plan.preview(&session_id) == request.selection, "container or image selection changed since confirmation; refresh the upgrade preview");
                        ensure!(request.selection.available, "this container already uses the selected image");
                        crate::database::begin_container_upgrade(&session_id, &operation_id, cancelled, plan)
                    }
                }).await;
                let intent = match admitted {
                    Ok(intent) => intent,
                    Err(error) => {
                        let _ = accepted.send(Err(format!("{error:#}")));
                        return Err(error);
                    }
                };
                state.bind_restart_intent(&session_id, &intent.operation_id)?;
                let _ = accepted.send(Ok(()));
                state.restart_admitted(session_id, cancelled, Some(intent), false).await
            },
        )?;
        let runtime = self.clone();
        tokio::spawn(async move {
            if let Err(error) = Self::wait_lifecycle_result(operation.clone()).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "container upgrade retained its environment and recovery intent"
                );
            }
            runtime.remove_completed_lifecycle(&operation);
        });
        acceptance
            .await
            .context("container upgrade admission stopped before acceptance")?
            .map_err(anyhow::Error::msg)
    }
}
