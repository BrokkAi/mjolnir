use super::*;

#[derive(Clone)]
pub(super) struct ClientSessionHandle(pub(super) ManagedSessionHandle);

impl mj_client::session::SessionHandleBackend for ClientSessionHandle {
    fn search_prompts(
        &self,
        bundle_id: String,
        scope: mj_core::storage::HistoryScope,
        query: String,
    ) -> mj_client::session::BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>> {
        let session_id = self.0.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::search_prompts(&session_id, &bundle_id, scope, &query)
            })
            .await
            .context("history search task")?
        })
    }
    fn review_state(
        &self,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::ReviewState>> {
        let session_id = self.0.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                Ok(mj_client::session::ReviewState {
                    review: crate::database::active_review(&session_id)?,
                    defaults: crate::database::reviewer_defaults()?,
                })
            })
            .await
            .context("review restoration task")?
        })
    }

    fn config_result(
        &self,
        command_id: String,
    ) -> mj_client::session::BoxFuture<'_, Result<Option<Option<String>>>> {
        let session_id = self.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::load_config_result(&session_id, &command_id)
            })
            .await
            .context("read configuration completion task")?
        })
    }

    fn clone_box(&self) -> Box<dyn mj_client::session::SessionHandleBackend> {
        Box::new(self.clone())
    }

    fn session_id(&self) -> &str {
        self.0.session_id()
    }

    fn view(&self) -> ManagedSessionView {
        self.0.view()
    }

    fn is_stopped(&self) -> bool {
        self.0.is_stopped()
    }

    fn has_changed(&self) -> Result<bool> {
        self.0.has_changed()
    }

    fn changed(&mut self) -> mj_client::session::BoxFuture<'_, Result<ManagedSessionView>> {
        Box::pin(self.0.changed())
    }

    fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::PendingRelaySubmit>> {
        Box::pin(async move {
            let pending = self.0.enqueue_submit(command_id, command).await?;
            Ok(mj_client::session::PendingRelaySubmit::new(Box::pin(
                pending.wait(),
            )))
        })
    }

    fn enqueue_sync(
        &self,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::PendingRelaySync>> {
        Box::pin(async move {
            let pending = self.0.enqueue_sync().await?;
            Ok(mj_client::session::PendingRelaySync::new(Box::pin(
                pending.wait(),
            )))
        })
    }

    fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> mj_client::session::BoxFuture<'_, Result<()>> {
        Box::pin(self.0.respond_elicitation(elicitation_id, response))
    }

    fn stop_background_task(
        &self,
        background_task_id: String,
    ) -> mj_client::session::BoxFuture<'_, Result<()>> {
        Box::pin(self.0.stop_background_task(background_task_id))
    }

    fn reviewer(
        &self,
        role: Option<String>,
        action: ReviewerAction,
    ) -> mj_client::session::BoxFuture<'_, Result<ReviewerOutcome>> {
        Box::pin(self.0.reviewer_as(role, action))
    }
}

#[derive(Clone)]
pub(super) struct ClientSessionControl(pub(super) SessionManagerControl);

impl mj_client::session::SessionControlBackend for ClientSessionControl {
    fn session(
        &self,
        session_id: String,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::SessionHandle>> {
        Box::pin(async move { Ok(self.0.session(session_id).await?.client()) })
    }
}

impl SessionManagerControl {
    /// Narrow this controller-owned manager to session lookup for a control surface.
    pub fn client(&self) -> mj_client::session::SessionControl {
        mj_client::session::SessionControl::new(ClientSessionControl(self.clone()))
    }

    pub async fn session(&self, session_id: impl Into<String>) -> Result<ManagedSessionHandle> {
        let session_id = session_id.into();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ManagerCommand::Session {
                session_id: session_id.clone(),
                reply,
            })
            .await
            .context("session manager stopped")?;
        response
            .await
            .context("session manager stopped")?
            .with_context(|| format!("session {session_id} is not managed"))
    }

    pub async fn wait_for_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<ManagedSessionHandle> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.session(session_id.to_owned()).await {
                    Ok(handle) => return Ok(handle),
                    Err(error) => {
                        tracing::trace!(session_id, "waiting for session actor: {error:#}");
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "session {session_id} did not become available within {} seconds",
                timeout.as_secs()
            )
        })?
    }
}
