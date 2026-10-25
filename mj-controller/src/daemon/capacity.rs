//! The daemon's one capacity poller.
//!
//! Each configured host is probed once per [`crate::pollers::CAPACITY_POLL_INTERVAL`]
//! with one command that samples CPU, memory and the free space on the
//! filesystems Mjolnir writes to. The free space goes to
//! [`crate::target_storage`], which decides whether a host is full; the whole
//! reading goes to whoever subscribes, today the web viewer. A host that does
//! not answer holds only its own probe slot, for at most the probe timeout.

use super::*;

pub(crate) struct ContainerCapacityInput {
    pub(crate) target_ids: Vec<String>,
    pub(crate) total: mj_core::state::HostContainerSize,
}

/// What the capacity service shares with the surfaces that show capacity.
pub(crate) struct CapacityFeed {
    pub(crate) targets: tokio::sync::watch::Receiver<Vec<crate::targets::DeploymentCapacityTarget>>,
    pub(crate) updates: tokio::sync::broadcast::Sender<crate::pollers::CapacityPollUpdate>,
    latest: tokio::sync::watch::Receiver<BTreeMap<String, crate::targets::DeploymentCapacityUsage>>,
    refresh: tokio::sync::mpsc::Sender<()>,
}

impl CapacityFeed {
    /// The latest successful reading for the deployment host that contains
    /// `target_id`. A failed probe deliberately leaves the last good total and
    /// observed usage in place; admission can still rely on the durable
    /// committed-allocation count rather than an empty reading.
    pub(crate) fn reading_for_target(
        &self,
        target_id: &str,
    ) -> Option<(
        crate::targets::DeploymentCapacityTarget,
        crate::targets::DeploymentCapacityUsage,
    )> {
        let target = self
            .targets
            .borrow()
            .iter()
            .find(|target| target.target_ids.iter().any(|id| id == target_id))?
            .clone();
        let usage = self.latest.borrow().get(&target.id)?.clone();
        Some((target, usage))
    }

    /// The deployment target containing `target_id`, even when no successful
    /// reading has arrived yet.
    pub(crate) fn target_for(
        &self,
        target_id: &str,
    ) -> Option<crate::targets::DeploymentCapacityTarget> {
        self.targets
            .borrow()
            .iter()
            .find(|target| target.target_ids.iter().any(|id| id == target_id))
            .cloned()
    }

    pub(crate) fn usage_for_target(
        &self,
        target_id: &str,
    ) -> Option<crate::targets::DeploymentCapacityUsage> {
        self.reading_for_target(target_id).map(|(_, usage)| usage)
    }

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

    /// The physical CPU and RAM totals for the host containing `target_id`.
    /// A first launch may arrive before the periodic poll has produced a
    /// reading, so this samples the existing deployment target on demand.
    pub(crate) async fn container_capacity_input(
        &self,
        target_id: &str,
    ) -> Result<Option<ContainerCapacityInput>> {
        let Some(feed) = self.capacity_feed() else {
            return Ok(None);
        };
        let Some(target) = feed.target_for(target_id) else {
            return Ok(None);
        };
        if target.kind != crate::targets::DeploymentCapacityKind::Host {
            return Ok(None);
        }
        let usage = match feed.usage_for_target(target_id) {
            Some(usage) => usage,
            None => crate::pollers::sample_capacity_target(target.clone())
                .await?
                .with_context(|| {
                    format!("no CPU and memory reading is available for target {target_id:?}")
                })?,
        };
        Ok(Some(ContainerCapacityInput {
            target_ids: target.target_ids,
            total: mj_core::state::HostContainerSize {
                cpus: usage.logical_cores,
                memory_bytes: usage.memory_total_bytes,
            },
        }))
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
    let (latest_tx, latest_rx) = tokio::sync::watch::channel(BTreeMap::<
        String,
        crate::targets::DeploymentCapacityUsage,
    >::new());
    let (updates, _) = tokio::sync::broadcast::channel(64);
    let feed = CapacityFeed {
        targets: targets_tx.subscribe(),
        updates: updates.clone(),
        latest: latest_rx,
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
                    latest_tx.send_if_modified(|latest| {
                        let before = latest.len();
                        latest.retain(|id, _| targets.iter().any(|target| target.id == *id));
                        latest.len() != before
                    });
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
                        latest_tx.send_modify(|latest| {
                            latest.insert(reading.target_id.clone(), usage.clone());
                        });
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
