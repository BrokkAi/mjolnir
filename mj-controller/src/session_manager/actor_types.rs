use super::*;

pub(super) enum ManagerCommand {
    Session {
        session_id: String,
        reply: oneshot::Sender<Option<ManagedSessionHandle>>,
    },
}

pub(super) enum StartingEvent {
    Register {
        session_id: String,
        launches: mpsc::UnboundedSender<StartingEvent>,
        reply: oneshot::Sender<Result<StartingSessionRegistration>>,
    },
    Published {
        session_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Aborted {
        session_id: String,
    },
    Route {
        session_id: String,
        reply: oneshot::Sender<mj_client::session::ParentMessageRoute>,
    },
}

#[allow(clippy::large_enum_variant)] // Submit owns its relay command; the other requests are small.
pub(super) enum ActorCommand {
    Submit {
        queued_at: Instant,
        command_id: String,
        command: RelayCommand,
        reply: oneshot::Sender<std::result::Result<u64, SubmitFailure>>,
    },
    Sync {
        reply: oneshot::Sender<Result<()>>,
    },
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    StopBackgroundTask {
        background_task_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    /// Background text only the target harness sees, prepended to the next
    /// real prompt. A restored archive installs its hand-off this way.
    InstallPromptContext {
        text: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    Reviewer {
        queued_at: std::time::Instant,
        role: Option<String>,
        action: ReviewerAction,
        reply: oneshot::Sender<anyhow::Result<ReviewerOutcome>>,
    },
    /// The connection is handed over whole, and so is the failure: a caller
    /// that must decide whether to restart the worker needs the typed cause,
    /// which formatting the error to a string would destroy.
    Lease {
        admission: crate::recovery_gate::RelayLeaseAdmission,
        idle_harness: Option<mj_core::config::HarnessKind>,
        reply: oneshot::Sender<Result<ManagedSessionLease>>,
        returns: mpsc::UnboundedSender<ReturnedConnection>,
    },
    /// Periodic work on the actor's own connection. See [`RelayConnectionJob`].
    RelayJob {
        job: Box<dyn RelayConnectionJob>,
    },
}

/// Work run on a session actor's own relay connection, in turn with the
/// actor's other relay traffic. A periodic exchange such as credential sync
/// uses this instead of opening a connection of its own to every worker.
pub trait RelayConnectionJob: Send + Sync + 'static {
    fn deferred_by_lease(&self, _lease: &crate::recovery_gate::RelayLeaseAdmission) {}
    fn deferred_by_reconnect(&self) -> bool {
        false
    }
    fn run<'a>(self: Box<Self>, client: &'a mut RelayClient) -> futures::future::BoxFuture<'a, ()>;

    /// The actor did not run the job; `error` says why.
    fn refuse(self: Box<Self>, error: anyhow::Error);
}

pub(crate) const RELAY_JOB_LEASED: &str = "a lifecycle operation holds the relay connection";
pub(crate) const RELAY_JOB_RECONNECTING: &str = "the relay connection is reconnecting";

/// The actor had no connection to lend: a lifecycle operation holds it, or
/// the actor is reconnecting. The job belongs to the next cycle.
#[derive(Debug)]
pub struct RelayJobDeferred(pub &'static str);

impl std::fmt::Display for RelayJobDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RelayJobDeferred {}

impl RelayJobDeferred {
    pub fn marks(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }
}

impl ActorCommand {
    pub fn operation_name(&self) -> &'static str {
        match self {
            Self::Submit { .. } => "submit",
            Self::Sync { .. } => "sync",
            Self::RespondElicitation { .. } => "respond_elicitation",
            Self::StopBackgroundTask { .. } => "stop_background_task",
            Self::InstallPromptContext { .. } => "install_prompt_context",
            Self::Reviewer { action, .. } => action.operation_name(),
            Self::Lease { .. } => "lease",
            Self::RelayJob { .. } => "relay_job",
        }
    }

    pub fn reject(self, session_id: &str, message: &str) {
        match self {
            Self::Submit { reply, .. } => {
                if reply.send(Err(message.to_owned().into())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "submit",
                        "submit rejection receiver was already closed"
                    );
                }
            }
            Self::Sync { reply } => {
                if reply
                    .send(Err(anyhow::anyhow!(message.to_owned())))
                    .is_err()
                {
                    tracing::debug!(
                        %session_id,
                        operation = "sync",
                        "sync rejection receiver was already closed"
                    );
                }
            }
            Self::RespondElicitation { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "respond_elicitation",
                        "elicitation rejection receiver was already closed"
                    );
                }
            }
            Self::StopBackgroundTask { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "stop_background_task",
                        "background task stop rejection receiver was already closed"
                    );
                }
            }
            Self::InstallPromptContext { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "install_prompt_context",
                        "prompt context rejection receiver was already closed"
                    );
                }
            }
            Self::Reviewer { reply, .. } => {
                if reply.send(Err(anyhow::anyhow!("{message}"))).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "reviewer",
                        "reviewer rejection receiver was already closed"
                    );
                }
            }
            Self::Lease { reply, .. } => {
                if reply
                    .send(Err(anyhow::anyhow!(message.to_owned())))
                    .is_err()
                {
                    tracing::debug!(
                        %session_id,
                        operation = "lease",
                        "lease rejection receiver was already closed"
                    );
                }
            }
            Self::RelayJob { job } => job.refuse(anyhow::anyhow!(message.to_owned())),
        }
    }
}

pub(super) struct ReturnedConnection {
    pub(super) lease_id: u64,
    /// Boxed: the unbounded channel allocates whole 32-slot blocks of this
    /// message per actor, and a session connection is a large struct.
    pub(super) connection: Option<Box<StandaloneSession>>,
}

/// A submission that arrived while a lifecycle operation held the connection.
/// The actor replays these in arrival order once the lease comes back.
pub(super) struct DeferredSubmit {
    pub(super) queued_at: Instant,
    pub(super) command_id: String,
    pub(super) command: RelayCommand,
    pub(super) reply: oneshot::Sender<std::result::Result<u64, SubmitFailure>>,
}

#[derive(Default)]
pub(super) struct ActorLifecycle {
    pub(super) active_lease: Option<(u64, crate::recovery_gate::RelayLeaseAdmission)>,
    pub(super) retirement_requested: bool,
}

impl ActorLifecycle {
    pub(super) fn set_retirement_requested(&mut self, requested: bool) {
        self.retirement_requested = requested;
    }

    pub(super) fn is_leased(&self) -> bool {
        self.active_lease.is_some()
    }

    pub(super) fn should_stop(&self) -> bool {
        self.retirement_requested && !self.is_leased()
    }

    pub(super) fn accepts_new_work(&self) -> bool {
        !self.retirement_requested
    }

    pub(super) fn activate_lease(
        &mut self,
        lease_id: u64,
        admission: crate::recovery_gate::RelayLeaseAdmission,
    ) {
        debug_assert!(self.active_lease.is_none());
        self.active_lease = Some((lease_id, admission));
    }

    pub(super) fn return_lease(&mut self, lease_id: u64) -> bool {
        if self.active_lease.as_ref().map(|(id, _)| *id) != Some(lease_id) {
            return false;
        }
        self.active_lease = None;
        true
    }
}

pub(super) struct ActorRegistration {
    pub(super) target: RelaySessionTarget,
    pub(super) commands: mpsc::Sender<ActorCommand>,
    pub(super) releases: mpsc::UnboundedSender<ReturnedConnection>,
    pub(super) retirement: watch::Sender<bool>,
    pub(super) view: watch::Receiver<ManagedSessionView>,
    pub(super) abort: tokio::task::AbortHandle,
}

pub(super) struct RemoteActorRegistration {
    pub(super) commands: mpsc::Sender<ActorCommand>,
    pub(super) releases: mpsc::UnboundedSender<ReturnedConnection>,
    pub(super) view: watch::Receiver<ManagedSessionView>,
    pub(super) view_tx: watch::Sender<ManagedSessionView>,
    pub(super) abort: tokio::task::AbortHandle,
}

pub(super) enum RemoteManagerUpdate {
    Publish {
        session_id: String,
        view: ManagedSessionView,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReconcileAction {
    Idle,
    Spawn,
    Keep,
    Retire,
}

#[derive(Debug)]
pub(super) struct SubmitFailure {
    pub failure: mj_client::session::SubmitFailure,
    pub code: Option<mj_core::relay::RelayErrorCode>,
}
impl std::ops::Deref for SubmitFailure {
    type Target = mj_client::session::SubmitFailure;
    fn deref(&self) -> &Self::Target {
        &self.failure
    }
}
impl From<mj_client::session::SubmitFailure> for SubmitFailure {
    fn from(failure: mj_client::session::SubmitFailure) -> Self {
        Self {
            failure,
            code: None,
        }
    }
}
impl From<&str> for SubmitFailure {
    fn from(message: &str) -> Self {
        mj_client::session::SubmitFailure::from(message).into()
    }
}
impl From<String> for SubmitFailure {
    fn from(message: String) -> Self {
        mj_client::session::SubmitFailure::from(message).into()
    }
}
