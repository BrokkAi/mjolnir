use super::*;

pub(super) enum ManagerCommand {
    Session {
        session_id: String,
        reply: oneshot::Sender<Option<ManagedSessionHandle>>,
    },
}

pub(super) enum ActorCommand {
    Submit {
        queued_at: Instant,
        command_id: String,
        command: RelayCommand,
        admission: Option<ReviewDeliveryAdmission>,
        reply: oneshot::Sender<std::result::Result<u64, SubmitFailure>>,
    },
    Sync {
        reply: oneshot::Sender<std::result::Result<(), String>>,
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
        role: Option<String>,
        action: ReviewerAction,
        reply: oneshot::Sender<std::result::Result<ReviewerOutcome, String>>,
    },
    /// The connection is handed over whole, and so is the failure: a caller
    /// that must decide whether to restart the worker needs the typed cause,
    /// which formatting the error to a string would destroy.
    Lease {
        reply: oneshot::Sender<Result<(u64, StandaloneSession)>>,
    },
}

impl ActorCommand {
    pub(super) fn operation_name(&self) -> &'static str {
        match self {
            Self::Submit { .. } => "submit",
            Self::Sync { .. } => "sync",
            Self::RespondElicitation { .. } => "respond_elicitation",
            Self::StopBackgroundTask { .. } => "stop_background_task",
            Self::InstallPromptContext { .. } => "install_prompt_context",
            Self::Reviewer { action, .. } => action.operation_name(),
            Self::Lease { .. } => "lease",
        }
    }

    pub(super) fn reject(self, session_id: &str, message: &str) {
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
                if reply.send(Err(message.to_owned())).is_err() {
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
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "reviewer",
                        "reviewer rejection receiver was already closed"
                    );
                }
            }
            Self::Lease { reply } => {
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
        }
    }
}

pub(super) struct ReturnedConnection {
    pub(super) lease_id: u64,
    pub(super) connection: Option<StandaloneSession>,
}

/// A submission that arrived while a lifecycle operation held the connection.
/// The actor replays these in arrival order once the lease comes back.
pub(super) struct DeferredSubmit {
    pub(super) queued_at: Instant,
    pub(super) command_id: String,
    pub(super) command: RelayCommand,
    pub(super) admission: Option<ReviewDeliveryAdmission>,
    pub(super) reply: oneshot::Sender<std::result::Result<u64, SubmitFailure>>,
}

#[derive(Debug, Default)]
pub(super) struct ActorLifecycle {
    pub(super) active_lease: Option<u64>,
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

    pub(super) fn activate_lease(&mut self, lease_id: u64) {
        debug_assert!(self.active_lease.is_none());
        self.active_lease = Some(lease_id);
    }

    pub(super) fn return_lease(&mut self, lease_id: u64) -> bool {
        if self.active_lease != Some(lease_id) {
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

use mj_client::session::SubmitFailure;
