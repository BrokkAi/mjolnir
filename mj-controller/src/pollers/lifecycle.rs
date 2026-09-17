use super::*;

pub enum LifecycleSuccess {
    Created,
    Resumed {
        profile_id: String,
        target_id: String,
    },
    Moved(mj_core::state::MoveOutcome),
    Closed,
    ForceStopped,
    DestroyedStopped,
    ForceDestroyed,
}

pub struct LifecycleUpdate {
    pub session_id: String,
    pub result: std::result::Result<LifecycleSuccess, String>,
    pub deferred_cleanup: bool,
}

/// Whether a close stopped partway and left the record mid-close with its
/// target still present. Such a record cannot be closed again from the start:
/// its worker is gone, so only recovery can finish it.
pub fn is_interrupted_close(session: &SessionRecord) -> bool {
    matches!(
        session.state,
        SessionState::Closing | SessionState::Destroying
    ) && session.target.is_some()
}

pub fn interrupted_close_session_ids(controller: &Controller) -> Vec<String> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| is_interrupted_close(session))
        .map(|session| session.id.clone())
        .collect()
}

/// Why a record left in an in-flight lifecycle state has nobody to finish it,
/// in words the user reads in `mj sessions` and the TUI.
///
/// Every in-flight state needs an owner that will complete it. A durable move
/// intent owns its session, [`is_interrupted_close`] owns a close or teardown
/// that still holds its target, and
/// `database::recover_interrupted_checkpointing_sessions` returns an
/// interrupted `Checkpointing` record to `Running` before the controller
/// loads. What is left is a record whose operation died with the process, and
/// it has to say so instead of waiting forever.
///
/// `None` means the state needs no reconciliation; callers exclude the owned
/// sessions before asking.
pub fn interrupted_lifecycle_cause(session: &SessionRecord) -> Option<String> {
    match session.state {
        // Provisioning has no durable operation behind it. Whatever the dead
        // provision created is not named by this record, so the resource is
        // recovered through `mj recover scan`, which can see it again once the
        // record is no longer in flight.
        SessionState::Provisioning => Some(
            "the daemon stopped while this session was provisioning; anything it created \
             is offered by `mj recover scan`"
                .to_owned(),
        ),
        // An interrupted close or teardown that still holds its target is
        // resumed rather than failed, so only the target-less residue reaches
        // here: there is nothing left to tear down, and no relay through which
        // to finish the close the record claims.
        SessionState::Closing => Some(
            "the daemon stopped while this session was closing, and it has no target left \
             to close"
                .to_owned(),
        ),
        SessionState::Destroying => Some(
            "the daemon stopped while this session was being torn down, and it has no \
             target left to remove"
                .to_owned(),
        ),
        SessionState::Checkpointing
        | SessionState::Running
        | SessionState::Disconnected
        | SessionState::Stopped
        | SessionState::Lost
        | SessionState::Error
        | SessionState::DestroyedWithDataLoss => None,
    }
}

/// Every session whose in-flight lifecycle state has no owner, with the cause
/// to record against it. `owned` names the sessions a durable move intent or
/// another startup recovery has already claimed.
pub fn unowned_interrupted_lifecycles(
    controller: &Controller,
    owned: &std::collections::BTreeSet<String>,
) -> Vec<(String, String)> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| !owned.contains(&session.id) && !is_interrupted_close(session))
        .filter_map(|session| {
            interrupted_lifecycle_cause(session).map(|cause| (session.id.clone(), cause))
        })
        .collect()
}

pub fn spawn_interrupted_close_recovery(
    session_id: String,
    session_manager: SessionManagerControl,
    recovery_observer: crate::recovery_gate::RecoveryObserver,
    cancelled: Arc<AtomicBool>,
    updates: tokio::sync::mpsc::UnboundedSender<LifecycleUpdate>,
    tracker: Option<mj_client::operations::CriticalOperationTracker>,
) -> tokio::task::JoinHandle<()> {
    let guard = tracker.map(|tracker| {
        tracker.begin_cancellable(
            format!(
                "recovering session {}",
                mj_core::state::short_id(&session_id)
            ),
            cancelled.clone(),
        )
    });
    let runtime = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        let operation_session_id = session_id.clone();
        let joined = tokio::task::spawn_blocking(move || {
            (|| -> Result<bool> {
                let _recovery_reservation = reserve_recovery_or_cancel(
                    &recovery_observer,
                    &operation_session_id,
                    &cancelled,
                )?;
                let mut controller = Controller::load()?;
                let executor = CancellableProcessExecutor::new(cancelled);
                runtime.block_on(controller.recover_interrupted_close_managed(
                    &operation_session_id,
                    &executor,
                    &session_manager,
                ))
            })()
            .map_err(|error| format!("{error:#}"))
        })
        .await;
        let (result, deferred_cleanup) = match joined {
            Ok(Ok(deferred_cleanup)) => (Ok(LifecycleSuccess::Closed), deferred_cleanup),
            Ok(Err(error)) => (Err(error), false),
            Err(error) => (
                Err(format!("interrupted close recovery task failed: {error}")),
                false,
            ),
        };
        if let Err(error) = updates.send(LifecycleUpdate {
            session_id: session_id.clone(),
            result,
            deferred_cleanup,
        }) {
            tracing::debug!(%session_id, %error, "interrupted close result dropped after dashboard shutdown");
        }
        drop(guard);
    })
}

pub fn reserve_recovery_or_cancel(
    observer: &crate::recovery_gate::RecoveryObserver,
    session_id: &str,
    cancelled: &AtomicBool,
) -> Result<crate::recovery_gate::RecoveryReservation> {
    let reservation = observer.reserve(session_id);
    // The reservation stops the next copy; cancelling preempts the one already
    // running so a lifecycle operation never queues behind a long or wedged
    // copy.
    observer.cancel_busy(session_id);
    while observer.is_busy(session_id) {
        if cancelled.load(Ordering::Acquire) {
            bail!("operation cancelled while waiting for recovery copy");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(reservation)
}

pub fn project_worker_title(
    controller: &mut Controller,
    update: &WorkerPollUpdate,
) -> Option<Option<String>> {
    let snapshot = update.view.snapshot.as_ref()?;
    let session = controller.state.sessions.get_mut(&update.session_id)?;
    let title = snapshot.resolved_title();
    if session.acp_session_title == title {
        return None;
    }
    session.acp_session_title = title.clone();
    Some(title)
}

pub fn apply_worker_record_update(controller: &mut Controller, update: &WorkerPollUpdate) {
    let Some(title) = project_worker_title(controller, update) else {
        return;
    };
    let session_id = update.session_id.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            crate::database::set_session_acp_title(&session_id, title.as_deref())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not persist relay title"),
            Err(error) => tracing::warn!(%error, "relay title persistence task failed"),
        }
    });
}
