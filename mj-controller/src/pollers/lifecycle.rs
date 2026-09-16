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
