use super::*;

#[derive(Clone)]
pub struct SessionManagerControl {
    pub(super) commands: mpsc::Sender<ManagerCommand>,
}

#[derive(Clone, Debug)]
pub struct ManagedSessionHandle {
    pub(super) session_id: String,
    pub(super) commands: mpsc::Sender<ActorCommand>,
    pub(super) releases: mpsc::UnboundedSender<ReturnedConnection>,
    pub(super) view: watch::Receiver<ManagedSessionView>,
}

/// A one-command capability issued by the review host while its prompt hold
/// is open. It is intentionally opaque to callers: the session actor checks
/// it against the host's live hold registry before bypassing prompt refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDeliveryAdmission {
    pub(super) session_id: String,
    pub(super) epoch: u64,
    pub(super) command_id: String,
}

impl ReviewDeliveryAdmission {
    pub(crate) fn new(session_id: String, epoch: u64, command_id: String) -> Self {
        Self {
            session_id,
            epoch,
            command_id,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn command_id(&self) -> &str {
        &self.command_id
    }
}

/// Exclusive ownership of a session actor's existing relay connection.
///
/// Lifecycle operations use this instead of opening a competing projection
/// client. Dropping an unreleased lease drops the proxy connection, which in
/// turn cancels any ordinary relay checkpoint barrier.
///
/// Prompt submissions that arrive while the lease is active are not rejected.
/// The actor queues them and forwards them in arrival order once the lease is
/// released or dropped.
pub struct ManagedSessionLease {
    pub(super) session_id: String,
    pub(super) lease_id: Option<u64>,
    pub(super) connection: Option<StandaloneSession>,
    pub(super) releases: mpsc::UnboundedSender<ReturnedConnection>,
}

impl ManagedSessionLease {
    pub fn connection_mut(&mut self) -> &mut StandaloneSession {
        self.connection
            .as_mut()
            .expect("managed session lease has already been released")
    }

    /// Swap the leased proxy after the worker process behind it was replaced.
    /// The actor stays leased, so queued prompts cannot race the new latch.
    pub fn replace_connection(&mut self, connection: StandaloneSession) {
        drop(self.connection.take());
        self.connection = Some(connection);
    }

    pub fn release(mut self) {
        let lease_id = self
            .lease_id
            .take()
            .expect("managed session lease has already been released");
        let connection = self.connection.take();
        if let Err(error) = self.releases.send(ReturnedConnection {
            lease_id,
            connection,
        }) {
            tracing::warn!(
                session_id = %self.session_id,
                operation = "lease_release",
                %error,
                "session actor stopped before receiving released relay connection"
            );
        }
    }
}

impl Drop for ManagedSessionLease {
    fn drop(&mut self) {
        let Some(lease_id) = self.lease_id.take() else {
            return;
        };
        // Drop the proxy before telling the actor to reconnect so the relay
        // observes EOF and releases any abandoned checkpoint barrier first.
        drop(self.connection.take());
        if let Err(error) = self.releases.send(ReturnedConnection {
            lease_id,
            connection: None,
        }) {
            tracing::warn!(
                session_id = %self.session_id,
                operation = "lease_drop",
                %error,
                "session actor stopped before receiving dropped relay lease"
            );
        }
    }
}

impl ManagedSessionHandle {
    /// Narrow this controller-owned handle to the operations a control surface uses.
    pub fn client(&self) -> mj_client::session::SessionHandle {
        mj_client::session::SessionHandle::new(ClientSessionHandle(self.clone()))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn view(&self) -> ManagedSessionView {
        self.view.borrow().clone()
    }

    /// Whether the per-session actor behind this handle has retired. The
    /// manager itself may still be alive with a replacement actor, so callers
    /// holding long-lived handles use this to reacquire the current one.
    pub fn is_stopped(&self) -> bool {
        self.commands.is_closed()
    }

    pub fn has_changed(&self) -> Result<bool> {
        self.view.has_changed().context("session manager stopped")
    }

    pub async fn changed(&mut self) -> Result<ManagedSessionView> {
        self.view
            .changed()
            .await
            .context("session manager stopped")?;
        Ok(self.view())
    }

    pub async fn submit(&self, command_id: String, command: RelayCommand) -> Result<u64> {
        self.enqueue_submit(command_id, command).await?.wait().await
    }

    /// Submit the review's corrective prompt through the one admission that
    /// corresponds to its live prompt hold. Generic submissions continue to
    /// use [`Self::submit`] and remain subject to review refusal.
    pub(crate) async fn submit_review_delivery(
        &self,
        admission: ReviewDeliveryAdmission,
        command: RelayCommand,
    ) -> Result<u64> {
        let command_id = admission.command_id.clone();
        self.enqueue_submit_with_admission(command_id, command, Some(admission))
            .await?
            .wait()
            .await
    }

    pub async fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<PendingRelaySubmit> {
        self.enqueue_submit_with_admission(command_id, command, None)
            .await
    }

    pub(super) async fn enqueue_submit_with_admission(
        &self,
        command_id: String,
        command: RelayCommand,
        admission: Option<ReviewDeliveryAdmission>,
    ) -> Result<PendingRelaySubmit> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Submit {
                queued_at: Instant::now(),
                command_id,
                command,
                admission,
                reply,
            })
            .await
            .context("session manager stopped")?;
        Ok(PendingRelaySubmit { response })
    }

    pub async fn sync_now(&self) -> Result<()> {
        self.enqueue_sync().await?.wait().await
    }

    pub async fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::RespondElicitation {
                elicitation_id,
                response,
                reply,
            })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    pub async fn stop_background_task(&self, background_task_id: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::StopBackgroundTask {
                background_task_id,
                reply,
            })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    /// Install background text the harness reads with the next real prompt.
    /// It creates no transcript turn, so the user never sees it.
    pub async fn install_prompt_context(&self, text: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::InstallPromptContext { text, reply })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    /// Drive the session's second-opinion reviewer.
    ///
    /// The reviewer shares this session's relay connection, so its actions
    /// queue behind the session's own and are refused while a lifecycle
    /// operation holds the connection.
    pub async fn reviewer(&self, action: ReviewerAction) -> Result<ReviewerOutcome> {
        self.reviewer_as(None, action).await
    }

    /// Drive one reviewing role. `None` is the default role, which is the one
    /// plan review uses; a turn review in the extended tier names its
    /// supervisor, its intent analyst, and each specialist lane.
    pub async fn reviewer_as(
        &self,
        role: Option<String>,
        action: ReviewerAction,
    ) -> Result<ReviewerOutcome> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::Reviewer {
                role,
                action,
                reply,
            })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    pub async fn enqueue_sync(&self) -> Result<PendingRelaySync> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Sync { reply })
            .await
            .context("session manager stopped")?;
        Ok(PendingRelaySync { response })
    }

    pub async fn lease_connection(&self) -> Result<ManagedSessionLease> {
        self.lease_connection_for(None).await
    }

    /// Background replacement must never take a busy session's control channel.
    pub async fn lease_idle_connection(
        &self,
        harness: mj_core::config::HarnessKind,
    ) -> Result<Option<ManagedSessionLease>> {
        match self.lease_connection_for(Some(harness)).await {
            Ok(lease) => Ok(Some(lease)),
            Err(error) if error.is::<SessionNotIdle>() => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn lease_connection_for(
        &self,
        idle_harness: Option<mj_core::config::HarnessKind>,
    ) -> Result<ManagedSessionLease> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Lease {
                idle_harness,
                reply,
            })
            .await
            .context("session manager stopped")?;
        let (lease_id, connection) = response.await.context("session manager stopped")??;
        Ok(ManagedSessionLease {
            session_id: self.session_id.clone(),
            lease_id: Some(lease_id),
            connection: Some(connection),
            releases: self.releases.clone(),
        })
    }
}

#[derive(Debug)]
pub(super) struct SessionNotIdle;

impl std::fmt::Display for SessionNotIdle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session still has work in flight; upgrade deferred")
    }
}

impl std::error::Error for SessionNotIdle {}

pub struct PendingRelaySubmit {
    pub(super) response:
        oneshot::Receiver<std::result::Result<u64, mj_client::session::SubmitFailure>>,
}

impl PendingRelaySubmit {
    pub async fn wait(self) -> Result<u64> {
        self.response
            .await
            .map_err(|error| {
                anyhow::Error::new(error).context(mj_client::session::DeliveryUnconfirmed)
            })?
            .map_err(|failure| {
                let error = anyhow::Error::msg(failure.message);
                if failure.unconfirmed {
                    error.context(mj_client::session::DeliveryUnconfirmed)
                } else {
                    error
                }
            })
    }
}

pub struct PendingRelaySync {
    pub(super) response: oneshot::Receiver<std::result::Result<(), String>>,
}

impl PendingRelaySync {
    pub async fn wait(self) -> Result<()> {
        self.response
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }
}
