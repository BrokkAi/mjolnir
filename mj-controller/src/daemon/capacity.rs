//! The daemon's one capacity poller.
//!
//! Each configured host is probed once per [`crate::pollers::CAPACITY_POLL_INTERVAL`]
//! with one command that samples CPU, memory and the free space on the
//! filesystems Mjolnir writes to. The free space goes to
//! [`crate::target_storage`], which decides whether a host is full; the whole
//! reading goes to whoever subscribes, today the web viewer. A host that does
//! not answer holds only its own probe slot, for at most the probe timeout.

use super::*;

/// What the capacity service shares with the surfaces that show capacity.
pub(crate) struct CapacityFeed {
    pub(crate) targets: tokio::sync::watch::Receiver<Vec<crate::targets::DeploymentCapacityTarget>>,
    pub(crate) updates: tokio::sync::broadcast::Sender<crate::pollers::CapacityPollUpdate>,
    refresh: tokio::sync::mpsc::Sender<()>,
}

impl CapacityFeed {
    /// Ask for a new reading of every host. A request that arrives while one
    /// is queued is the same request.
    pub(crate) fn request_refresh(&self) -> bool {
        match self.refresh.try_send(()) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(())) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => false,
        }
    }
}

/// The shortest gap between two recomputations of the probe targets. Every
/// published revision may change the configured hosts or the live EC2
/// instances, and revisions can arrive many times a second.
const TARGET_REFRESH_GAP: Duration = Duration::from_secs(1);

impl RuntimeState {
    pub(crate) fn capacity_feed(&self) -> Option<&CapacityFeed> {
        self.capacity.get()
    }
}

pub(super) fn spawn_capacity_service(
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let (targets_tx, refresh, mut readings) = crate::pollers::spawn_dashboard_capacity_poller();
    crate::target_storage::install_no_space_observer();
    crate::target_storage::attach_refresh(refresh.clone());
    let initial = state
        .active_controller_projection()
        .deployment_capacity_targets();
    targets_tx.send_replace(initial);
    let (updates, _) = tokio::sync::broadcast::channel(64);
    let feed = CapacityFeed {
        targets: targets_tx.subscribe(),
        updates: updates.clone(),
        refresh,
    };
    if state.capacity.set(feed).is_err() {
        tracing::error!("the daemon capacity service was started twice");
    }
    tokio::spawn(async move {
        let mut revisions = state.revisions();
        let mut storage = crate::target_storage::subscribe();
        let mut full_hosts = BTreeSet::new();
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                changed = revisions.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    tokio::select! {
                        _ = cancellation.cancelled() => break,
                        _ = tokio::time::sleep(TARGET_REFRESH_GAP) => {}
                    }
                    revisions.borrow_and_update();
                    let targets = state
                        .active_controller_projection()
                        .deployment_capacity_targets();
                    targets_tx.send_if_modified(|current| {
                        if *current == targets {
                            false
                        } else {
                            *current = targets;
                            true
                        }
                    });
                }
                changed = storage.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    storage.borrow_and_update();
                    announce_full_hosts(&state, &mut full_hosts);
                    state.publish_revision();
                }
                reading = readings.recv() => {
                    let Some(reading) = reading else {
                        tracing::error!("the daemon capacity poller stopped");
                        break;
                    };
                    if let Ok(Some(usage)) = &reading.result {
                        crate::target_storage::record_samples(
                            &usage.storage,
                            reading.sampled_at_epoch_seconds,
                        );
                    }
                    // No subscriber is not an error: the web viewer may be off.
                    let _ = updates.send(reading);
                }
            }
        }
    })
}

/// Tell every surface once when a filesystem on a host becomes full.
fn announce_full_hosts(state: &RuntimeState, announced: &mut BTreeSet<String>) {
    let mut full = BTreeSet::new();
    for view in crate::target_storage::views() {
        for filesystem in view.filesystems.iter().filter(|filesystem| {
            filesystem.condition == mj_core::targets::storage::StorageCondition::Full
        }) {
            let key = format!("{}\n{}", view.host, filesystem.space.mount);
            if announced.insert(key.clone()) {
                state.push_notice(
                    "",
                    format!(
                        "Disk full: {}. Sessions that write there wait instead of retrying; free space to continue.",
                        view.explanation(filesystem)
                    ),
                );
            }
            full.insert(key);
        }
    }
    announced.retain(|key| full.contains(key));
}
