use super::*;
use crate::daemon::LifecycleKind;

pub(super) const MAX_CONCURRENT_BUNDLE_CREATIONS: usize = 4;
pub(super) const MAX_CONCURRENT_PREFLIGHTS: usize = 4;

/// How one phone action ended when it did not succeed.
///
/// `detail` is the whole error chain and belongs in the daemon log. `refusal`
/// and `reason` are written for the caller at the failure boundary. They may
/// be published on the session; failures without either use generic guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PhoneActionFailure {
    pub(super) detail: String,
    pub(super) refusal: Option<Refusal>,
    pub(super) reason: Option<mj_controller::controller::LifecycleFailureReason>,
}

impl PhoneActionFailure {
    pub(super) fn of(error: &anyhow::Error) -> Self {
        Self {
            detail: format!("{error:#}"),
            refusal: Refusal::of(error),
            reason: error
                .downcast_ref::<mj_controller::controller::LifecycleFailureReason>()
                .copied(),
        }
    }

    /// A failure with no safe reason to report, such as a task that died
    /// before the action could say anything about itself.
    pub(super) fn internal(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            refusal: None,
            reason: None,
        }
    }

    pub(super) fn conversation_notice(&self, action: &str, action_id: u64) -> String {
        let detail = self
            .refusal
            .as_ref()
            .map(|refusal| refusal.message().to_owned())
            .unwrap_or_else(|| format!("see daemon log reference {}", action_reference(action_id)));
        format!("{action} failed: {detail}")
    }

    /// The answer this failure owes the caller. `reference` identifies the log
    /// entry that carries `detail`, and is used only when there is no refusal.
    pub(super) fn outcome(&self, reference: &str) -> ActionOutcome {
        match &self.refusal {
            Some(refusal) => ActionOutcome::Refused(refusal.clone()),
            None => ActionOutcome::Failed {
                reference: reference.to_owned(),
            },
        }
    }
}

pub(super) fn next_phone_action_id() -> u64 {
    static LAST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    LAST_ID.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

/// Identify one action's log entry, so a caller holding a 500 can find the
/// daemon-log line that says what actually failed.
pub(super) fn action_reference(action_id: u64) -> String {
    format!("{}-{action_id}", std::process::id())
}

pub(super) struct PhoneActionStarted {
    pub(super) action_id: u64,
    pub(super) session_id: String,
    /// Registration turned review off because the folder has no Git commits.
    pub(super) review_notice: Option<&'static str>,
    pub(super) capacity_warning: Option<String>,
    pub(super) published: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
}

/// The phone replies the control loop still owes.
///
/// A phone is answered as soon as its action is admitted, because provisioning,
/// resume and close run for minutes and a request held open that long dies on a
/// mobile network. `new` is the one action whose acceptance means more than
/// admission: the phone has no session id until the provisional session is
/// published. Correlated submissions wait for relay acceptance, and mode
/// changes wait for completion so a follow-up prompt cannot race the change.
/// Review starts and resolutions also wait for completion: the review host
/// decides whether they apply, and the API promises 202 only once the review
/// has opened (or the resolution applied) and 409 with the reason otherwise.
/// Their callers show pending content immediately while awaiting this reply.
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
        if matches!(
            action,
            ControllerAction::New { .. }
                | ControllerAction::SetPlanMode { .. }
                | ControllerAction::StartReview { .. }
                | ControllerAction::Prompt {
                    command_id: Some(_),
                    ..
                }
                | ControllerAction::RunShell {
                    command_id: Some(_),
                    ..
                }
        ) {
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
    active_sessions: &mut std::collections::BTreeSet<String>,
    action_sessions: &std::collections::BTreeMap<u64, String>,
    action_cancellations: &std::collections::BTreeMap<u64, PhoneActionControl>,
) -> std::result::Result<Option<String>, ActionOutcome> {
    if matches!(action, ControllerAction::UpgradeContainer { request } if request.selection.settings.is_some())
    {
        // The upgrade owner must be able to join a retry as soon as it has acknowledged it.
        return Ok(None);
    }
    let closing = matches!(
        action,
        ControllerAction::Suspend { .. } | ControllerAction::Destroy { .. }
    );
    let session_id = controller_action_session_id(action);
    if let Some(session_id) = &session_id
        && !active_sessions.insert(session_id.clone())
        && !closing
    {
        // Let Interrupt All reach independent workers during a Move. A
        // second Interrupt All must wait: two raw cancellations could span
        // the end of the old turn and interrupt its queued successor.
        let mut active = action_sessions
            .iter()
            .filter_map(|(id, owner)| (owner == session_id).then_some(id));
        let move_only = matches!(action, ControllerAction::InterruptAll { .. })
            && active
                .next()
                .and_then(|id| action_cancellations.get(id))
                .is_some_and(|control| control.kind == Some(LifecycleKind::Move))
            && active.next().is_none();
        if !move_only {
            return Err(ActionOutcome::SessionBusy);
        }
    }
    Ok(session_id)
}

/// How often the housekeeping tick runs: hourly, unless the test-only
/// environment variable `MJ_PRUNE_TICK_SECONDS` names something shorter.
///
/// Test-only: it exists so a live check of the SessionWiki archive job does not
/// have to wait an hour, and it is read once per daemon.
pub(super) fn prune_tick_interval() -> Duration {
    static INTERVAL: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("MJ_PRUNE_TICK_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .map_or(Duration::from_secs(60 * 60), Duration::from_secs)
    })
}

pub(super) fn request_daemon_controller_reload(
    daemon_runtime: Arc<RuntimeState>,
    reason: &'static str,
) {
    mj_core::runtime::spawn(async move {
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
    pub(super) lifecycle: Option<LifecycleControl>,
    /// The requested lifecycle kind. A Resume can be admitted as a Create.
    pub(super) kind: Option<LifecycleKind>,
}

impl PhoneActionControl {
    pub(super) fn for_action(action: &ControllerAction) -> Self {
        let kind = match action {
            ControllerAction::New { .. } => Some(LifecycleKind::Create),
            ControllerAction::Suspend { .. } => Some(LifecycleKind::Suspend),
            ControllerAction::Resume { .. } => Some(LifecycleKind::Resume),
            ControllerAction::Restart { .. } => Some(LifecycleKind::Restart),
            ControllerAction::Move { .. } => Some(LifecycleKind::Move),
            _ => None,
        };
        let lifecycle = kind.map(|_| LifecycleControl::default());
        let cancelled = lifecycle.as_ref().map_or_else(
            || Arc::new(AtomicBool::new(false)),
            |control| control.cancelled.clone(),
        );
        Self {
            cancelled,
            lifecycle,
            kind,
        }
    }

    /// Preserve a resumable create intent across shutdown, including when
    /// the daemon chose Create to retry a failed phone Resume.
    pub(super) fn stop_for_server_shutdown(&self) {
        match (&self.lifecycle, self.kind) {
            (Some(control), Some(LifecycleKind::Create | LifecycleKind::Resume)) => {
                control.cancel_for_shutdown();
            }
            _ => {
                self.request_cancel("server shutdown");
            }
        }
    }

    pub(super) fn request_cancel(&self, cause: &'static str) -> bool {
        let accepted = self.lifecycle.as_ref().map_or_else(
            || {
                self.cancelled
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            },
            |control| control.request_cancel(cause),
        );
        if accepted {
            self.cancelled.store(true, Ordering::Release);
        }
        accepted
    }

    #[cfg(test)]
    pub(super) fn grant_new_commit(&self) -> bool {
        self.lifecycle
            .as_ref()
            .is_some_and(|control| control.grant_commit())
    }
}
