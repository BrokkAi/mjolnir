use super::*;

pub(super) const MAX_CONCURRENT_PHONE_ACTIONS: usize = 4;
pub(super) const MAX_CONCURRENT_BUNDLE_CREATIONS: usize = 4;
pub(super) const MAX_CONCURRENT_PREFLIGHTS: usize = 4;

pub(super) struct PhoneActionStarted {
    pub(super) action_id: u64,
    pub(super) session: SessionRecord,
    pub(super) published: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
}

/// The phone replies the control loop still owes.
///
/// A phone is answered as soon as its action is admitted, because provisioning,
/// resume and close run for minutes and a request held open that long dies on a
/// mobile network. `new` is the one action whose acceptance means more than
/// admission: the phone has no session id until the provisional session is
/// published, so its reply is parked here until the loop publishes it — or
/// until the action ends without ever getting that far.
#[derive(Default)]
pub(super) struct PendingActionReplies(
    std::collections::BTreeMap<u64, tokio::sync::oneshot::Sender<ActionOutcome>>,
);

impl PendingActionReplies {
    pub(super) fn accept(
        &mut self,
        action_id: u64,
        action: &ControllerAction,
        reply: tokio::sync::oneshot::Sender<ActionOutcome>,
    ) {
        if matches!(action, ControllerAction::New { .. }) {
            self.0.insert(action_id, reply);
        } else {
            if reply.send(ActionOutcome::accepted()).is_err() {
                tracing::debug!(
                    action_id,
                    "phone action acceptance reply dropped after client disconnect"
                );
            }
        }
    }

    pub(super) fn resolve(&mut self, action_id: u64, outcome: ActionOutcome) {
        if let Some(reply) = self.0.remove(&action_id)
            && reply.send(outcome).is_err()
        {
            tracing::debug!(
                action_id,
                "phone action completion reply dropped after client disconnect"
            );
        }
    }
}

/// Admission control for one phone action, run before any work starts so the
/// answer to the phone never waits on the operation itself. Reports the session
/// the action occupies, or the outcome that refuses it.
pub(super) fn admit_phone_action(
    action: &ControllerAction,
    running_actions: usize,
    active_sessions: &mut std::collections::BTreeSet<String>,
) -> std::result::Result<Option<String>, ActionOutcome> {
    let closing = matches!(
        action,
        ControllerAction::Close { .. } | ControllerAction::ForceClose { .. }
    );
    if !closing && !phone_action_capacity_available(running_actions) {
        return Err(ActionOutcome::Busy);
    }
    let session_id = controller_action_session_id(action);
    if let Some(session_id) = &session_id
        && !active_sessions.insert(session_id.clone())
        && !closing
    {
        return Err(ActionOutcome::SessionBusy);
    }
    Ok(session_id)
}

pub(super) struct ReadReceiptPersisted {
    pub(super) session_id: String,
    pub(super) result: std::result::Result<u64, String>,
    pub(super) reply: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
}

pub(super) struct ControllerReloaded {
    pub(super) result: std::result::Result<Controller, String>,
}

pub(super) struct BundleCreated {
    pub(super) result: std::result::Result<
        crate::controller::QuickBundleCreation,
        crate::controller::QuickBundleFailure,
    >,
    pub(super) reply:
        tokio::sync::oneshot::Sender<std::result::Result<String, crate::server::BundleFailure>>,
}

pub(super) struct MovePrepared {
    pub(super) result: std::result::Result<mj_core::state::MovePreparation, String>,
    pub(super) reply:
        tokio::sync::oneshot::Sender<std::result::Result<mj_core::state::MovePreparation, String>>,
}

/// Loads durable controller state without occupying the phone control loop.
/// The outer task observes blocking-task panics and reports a closed result
/// channel instead of silently abandoning the refresh.
pub(super) fn spawn_controller_reload(
    completed: tokio::sync::mpsc::UnboundedSender<ControllerReloaded>,
) {
    spawn_controller_reload_with(completed, Controller::load);
}

pub(super) fn spawn_controller_reload_with(
    completed: tokio::sync::mpsc::UnboundedSender<ControllerReloaded>,
    load: impl FnOnce() -> Result<Controller> + Send + 'static,
) {
    tokio::spawn(async move {
        let result = match tokio::task::spawn_blocking(load).await {
            Ok(result) => result.map_err(|error| format!("{error:#}")),
            Err(error) => Err(format!("controller reload task failed: {error}")),
        };
        if completed.send(ControllerReloaded { result }).is_err() {
            tracing::debug!("controller reload completed after the phone control loop stopped");
        }
    });
}

pub(super) fn request_controller_reload(
    in_flight: &mut bool,
    requested: &mut bool,
    completed: &tokio::sync::mpsc::UnboundedSender<ControllerReloaded>,
) {
    if *in_flight {
        *requested = true;
    } else {
        *in_flight = true;
        spawn_controller_reload(completed.clone());
    }
}

pub(super) fn request_daemon_controller_reload(
    daemon_runtime: Arc<RuntimeState>,
    reason: &'static str,
) {
    tokio::spawn(async move {
        if let Err(error) = daemon_runtime.reload_controller().await {
            tracing::warn!(
                error = format!("{error:#}"),
                reason,
                "phone operation could not refresh dashboard controller state"
            );
        }
    });
}

/// What one phone read receipt actually needs.
#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
pub(super) enum ReadReceiptPlan {
    UnknownSession,
    /// The cursor has not advanced, so the receipt needs no work at all.
    AlreadyRead,
    /// The cursor advanced: persist it, then refresh the snapshot.
    Persist,
}

#[cfg(test)]
pub(super) fn plan_read_receipt(state: &State, session_id: &str, through: u64) -> ReadReceiptPlan {
    let Some(session) = state.sessions.get(session_id) else {
        return ReadReceiptPlan::UnknownSession;
    };
    if through > session.viewed_through_event_ordinal {
        ReadReceiptPlan::Persist
    } else {
        ReadReceiptPlan::AlreadyRead
    }
}

/// Record a persisted receipt in the in-memory projection, reporting whether
/// the cursor moved. That is exactly when the snapshot revision has to move,
/// so surfaces showing unread state refresh and nothing else does.
#[cfg(test)]
pub(super) fn apply_read_receipt(state: &mut State, session_id: &str, receipt: u64) -> bool {
    let Some(session) = state.sessions.get_mut(session_id) else {
        return false;
    };
    if receipt <= session.viewed_through_event_ordinal {
        return false;
    }
    session.viewed_through_event_ordinal = receipt;
    true
}

#[derive(Clone)]
pub(super) struct PhoneActionControl {
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) create: Option<CreateSessionControl>,
}

impl PhoneActionControl {
    pub(super) fn for_action(action: &ControllerAction) -> Self {
        let create =
            matches!(action, ControllerAction::New { .. }).then(CreateSessionControl::default);
        let cancelled = create.as_ref().map_or_else(
            || Arc::new(AtomicBool::new(false)),
            |control| control.cancelled.clone(),
        );
        Self { cancelled, create }
    }

    pub(super) fn request_cancel(&self) -> bool {
        let accepted = self.create.as_ref().map_or_else(
            || {
                self.cancelled
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            },
            |control| control.request_cancel(),
        );
        if accepted {
            self.cancelled.store(true, Ordering::Release);
        }
        accepted
    }

    #[cfg(test)]
    pub(super) fn grant_new_commit(&self) -> bool {
        self.create
            .as_ref()
            .is_some_and(|control| control.grant_commit())
    }
}
