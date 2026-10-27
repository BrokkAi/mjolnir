//! The daemon's one capacity poller.
//!
//! Each configured host is probed once per [`crate::pollers::CAPACITY_POLL_INTERVAL`]
//! with one command that samples CPU, memory and the free space on the
//! filesystems Mjolnir writes to. The free space goes to
//! [`crate::target_storage`], which decides whether a host is full; the whole
//! reading goes to whoever subscribes, today the web viewer. A host that does
//! not answer holds only its own probe slot, for at most the probe timeout.

use super::*;

#[derive(Debug, Clone)]
pub(crate) struct ContainerCapacityInput {
    pub(crate) target_ids: Vec<String>,
    pub(crate) total: mj_core::state::HostContainerSize,
}

#[derive(Debug, Clone)]
pub(crate) struct ContainerCapacityCommitment {
    pub(crate) target_ids: Vec<String>,
    pub(crate) allocation: Option<mj_core::state::SessionResourceAllocation>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContainerCapacityReservation {
    leases: usize,
    hard_memory: bool,
    /// Destination deployment target and allocation.
    pub(crate) target_ids: Vec<String>,
    pub(crate) allocation: mj_core::state::SessionResourceAllocation,
    /// The running source a same- or cross-host Move has not retired yet.
    pub(crate) source: Option<ContainerCapacityCommitment>,
}

/// Owns one in-memory resume or move reservation until the lifecycle either
/// publishes its durable `Provisioning` record or ends. Joined holders retain
/// that reservation until the last holder finishes.
pub(crate) struct ContainerCapacityLease {
    state: Arc<RuntimeState>,
    session_id: String,
}

/// Keeps create admission locked until its durable record exists, or owns an
/// existing session's reservation through its lifecycle.
pub(crate) struct ContainerAdmission<'a> {
    _guard: Option<tokio::sync::MutexGuard<'a, ()>>,
    _lease: Option<ContainerCapacityLease>,
    pub(crate) warning: Option<String>,
}

impl ContainerAdmission<'_> {
    pub(crate) fn into_lease(self) -> Option<ContainerCapacityLease> {
        self._lease
    }
}

impl Drop for ContainerCapacityLease {
    fn drop(&mut self) {
        let mut reservations = self
            .state
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let reservation = reservations
            .get_mut(&self.session_id)
            .expect("a live capacity lease retains its reservation");
        reservation.leases -= 1;
        if reservation.leases == 0 {
            reservations.remove(&self.session_id);
        }
    }
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

    pub(crate) fn container_capacity_reservations(
        &self,
    ) -> BTreeMap<String, ContainerCapacityReservation> {
        self.container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn reserve_container_capacity(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: mj_core::state::SessionResourceAllocation,
    ) -> ContainerCapacityLease {
        self.reserve_move_capacity_lease(session_id, input, allocation, None)
    }

    #[cfg(test)]
    pub(crate) fn reserve_move_capacity_lease(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: mj_core::state::SessionResourceAllocation,
        source: Option<ContainerCapacityCommitment>,
    ) -> ContainerCapacityLease {
        self.joined_container_capacity_lease(session_id, input, &allocation, false)
            .unwrap()
            .unwrap_or_else(|| {
                self.reserve_container_capacity_with_source(
                    session_id, input, allocation, source, false,
                )
            })
    }

    /// Joining and retaining the reservation are one decision with its release.
    /// A different launch must not inherit a prior operation's admission.
    fn joined_container_capacity_lease(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: &mj_core::state::SessionResourceAllocation,
        hard_memory: bool,
    ) -> Result<Option<ContainerCapacityLease>> {
        let mut reservations = self
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(reservation) = reservations.get_mut(session_id) else {
            return Ok(None);
        };
        if !same_target_group(&reservation.target_ids, &input.target_ids)
            || reservation.allocation != *allocation
            || reservation.hard_memory != hard_memory
        {
            return Err(anyhow::Error::new(Refusal::precondition(
                "another container launch with different sizing or a different target is already in progress; wait for it to finish and retry",
            )));
        }
        reservation.leases += 1;
        Ok(Some(ContainerCapacityLease {
            state: self.clone(),
            session_id: session_id.to_owned(),
        }))
    }

    fn reserve_container_capacity_with_source(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: mj_core::state::SessionResourceAllocation,
        source: Option<ContainerCapacityCommitment>,
        hard_memory: bool,
    ) -> ContainerCapacityLease {
        let mut reservations = self
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match reservations.entry(session_id.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ContainerCapacityReservation {
                    leases: 1,
                    hard_memory,
                    target_ids: input.target_ids.clone(),
                    allocation,
                    source,
                });
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                unreachable!("serialized launch admission already joined any existing reservation");
            }
        }
        ContainerCapacityLease {
            state: self.clone(),
            session_id: session_id.to_owned(),
        }
    }

    /// One serialized decision for create, resume and Move. Create under a hard
    /// RAM limit retains the guard until registration; existing sessions
    /// install a lease before unlock.
    pub(crate) async fn admit_container_launch(
        self: &Arc<Self>,
        target_id: &str,
        allocation: Option<mj_core::state::SessionResourceAllocation>,
        session_id: Option<&str>,
        replaced: BTreeSet<String>,
        source: Option<ContainerCapacityCommitment>,
    ) -> Result<ContainerAdmission<'_>> {
        let checked_target_id = target_id.to_owned();
        let (container, hard_memory) = blocking(move || {
            let controller = Controller::load()?;
            let template = controller
                .config
                .targets
                .get(&checked_target_id)
                .with_context(|| format!("unknown target {checked_target_id:?}"))?;
            Ok((
                mj_core::config::is_container_target(template),
                mj_core::config::container_memory_is_hard_limit(template),
            ))
        })
        .await?;
        let empty = || ContainerAdmission {
            _guard: None,
            _lease: None,
            warning: None,
        };
        if !container {
            return Ok(empty());
        }
        let input = match self.container_capacity_input(target_id).await {
            Ok(Some(input)) => input,
            result if hard_memory => {
                return Err(match result {
                    Err(error) => error.context(Refusal::precondition(
                        "Mjolnir could not read this host's CPU and RAM capacity; check the target and retry")),
                    _ => anyhow::Error::new(Refusal::precondition(
                        "this target's host capacity is not ready yet; refresh the target and retry")
                        .with_code("container-capacity-unavailable")),
                });
            }
            Err(error) => {
                tracing::warn!(
                    target_id,
                    error = format!("{error:#}"),
                    "host capacity is unavailable; allowing container launch"
                );
                return Ok(empty());
            }
            Ok(None) => return Ok(empty()),
        };
        let Some(allocation) = allocation else {
            return Err(anyhow::Error::new(Refusal::unusable(
                "container sessions require an explicit CPU and RAM allocation; choose a size before continuing")
                .with_code("container-size-required")));
        };
        let admission = self.container_admission.lock().await;
        if let Some(session_id) = session_id
            && let Some(lease) =
                self.joined_container_capacity_lease(session_id, &input, &allocation, hard_memory)?
        {
            return Ok(ContainerAdmission {
                _guard: None,
                _lease: Some(lease),
                warning: None,
            });
        }
        let admission_state = self.clone();
        let checked_input = input.clone();
        let capacity = blocking(move || {
            let controller = Controller::load()?;
            Ok(capacity_with_reservations_after(
                &checked_input,
                controller.state.sessions.values(),
                &admission_state.container_capacity_reservations(),
                &replaced,
            ))
        })
        .await?;
        if capacity.refuses(&allocation, hard_memory) {
            return Err(capacity_refusal(capacity, &allocation));
        }
        let warning = capacity.overcommit(&allocation).warning(&capacity);
        if let Some(warning) = &warning {
            tracing::warn!(target_id, session_id, "{warning}");
            if let Some(session_id) = session_id {
                self.push_notice(session_id, warning.clone());
            }
        }
        let (guard, lease) = match session_id {
            Some(session_id) => (
                None,
                Some(self.reserve_container_capacity_with_source(
                    session_id,
                    &input,
                    allocation,
                    source,
                    hard_memory,
                )),
            ),
            // Only a hard limit needs the check and the durable record to be one
            // decision; a warning may be computed concurrently, so batches of
            // other launches do not wait on each other's registration.
            None => (hard_memory.then_some(admission), None),
        };
        Ok(ContainerAdmission {
            _guard: guard,
            _lease: lease,
            warning,
        })
    }
}

fn capacity_refusal(
    capacity: mj_core::state::HostContainerCapacity,
    allocation: &mj_core::state::SessionResourceAllocation,
) -> anyhow::Error {
    let code = if capacity.complete {
        "container-capacity-exceeded"
    } else {
        "container-capacity-unknown"
    };
    anyhow::Error::new(Refusal::unusable(capacity.refusal_message(allocation)).with_code(code))
}

pub(crate) fn capacity_with_reservations<'a>(
    input: &ContainerCapacityInput,
    sessions: impl IntoIterator<Item = &'a SessionRecord>,
    reservations: &BTreeMap<String, ContainerCapacityReservation>,
) -> mj_core::state::HostContainerCapacity {
    capacity_with_reservations_after(input, sessions, reservations, &BTreeSet::new())
}

/// Capacity with reservations and sessions that are about to be replaced
/// removed from the durable counts.
pub(crate) fn capacity_with_reservations_after<'a>(
    input: &ContainerCapacityInput,
    sessions: impl IntoIterator<Item = &'a SessionRecord>,
    reservations: &BTreeMap<String, ContainerCapacityReservation>,
    additionally_replaced: &BTreeSet<String>,
) -> mj_core::state::HostContainerCapacity {
    let sessions = sessions.into_iter().collect::<Vec<_>>();
    let matching = reservations
        .iter()
        .filter(|(_, reservation)| reservation_affects_group(reservation, &input.target_ids))
        .collect::<Vec<_>>();
    let mut reserved_ids = matching
        .iter()
        .map(|(session_id, _)| session_id.as_str())
        .collect::<BTreeSet<_>>();
    reserved_ids.extend(additionally_replaced.iter().map(String::as_str));
    let mut capacity = mj_core::state::host_container_capacity(
        input.total,
        &input.target_ids,
        sessions
            .iter()
            .copied()
            .filter(|session| !reserved_ids.contains(session.id.as_str())),
    );
    let mut complete = capacity.complete;
    for (session_id, reservation) in matching {
        let destination_matches = same_target_group(&reservation.target_ids, &input.target_ids);
        let destination =
            destination_matches.then_some(container_commitment(&reservation.allocation));
        let source = reservation
            .source
            .as_ref()
            .filter(|source| {
                same_target_group(&source.target_ids, &input.target_ids)
                    && source_commitment_is_live(session_id, source, &sessions)
            })
            .map(|source| source.allocation.as_ref().and_then(container_commitment));
        match (source, destination) {
            (Some(Some(source)), Some(Some(destination))) => {
                // Until the source stop is durable both containers can own
                // host resources. Hold the larger allocation. Once the
                // durable record no longer owns that source allocation, only
                // the destination remains charged.
                add_container_commitment(
                    &mut capacity,
                    source.cpus.max(destination.cpus),
                    source.memory_bytes.max(destination.memory_bytes),
                );
            }
            (Some(Some(source)), None) => {
                add_container_commitment(&mut capacity, source.cpus, source.memory_bytes);
            }
            (Some(None), _) => complete = false,
            (None, Some(Some(destination))) => {
                add_container_commitment(&mut capacity, destination.cpus, destination.memory_bytes);
            }
            (Some(Some(_)), Some(None)) | (None, Some(None)) => {
                complete = false;
            }
            (None, None) => {}
        }
    }
    if !complete {
        let usable = mj_core::state::HostContainerSize {
            cpus: input.total.cpus.saturating_sub(capacity.reserved.cpus),
            memory_bytes: input
                .total
                .memory_bytes
                .saturating_sub(capacity.reserved.memory_bytes),
        };
        capacity.complete = false;
        capacity.committed = usable;
        capacity.remaining = mj_core::state::HostContainerSize {
            cpus: 0,
            memory_bytes: 0,
        };
    }
    capacity
}

fn source_commitment_is_live(
    session_id: &str,
    source: &ContainerCapacityCommitment,
    sessions: &[&SessionRecord],
) -> bool {
    sessions
        .iter()
        .find(|session| session.id == session_id)
        .is_some_and(|session| {
            source.target_ids.contains(&session.target_template_id)
                && mj_core::state::session_owns_container(session)
                && session.resource_allocation == source.allocation
        })
}

fn container_commitment(
    allocation: &mj_core::state::SessionResourceAllocation,
) -> Option<mj_core::state::HostContainerSize> {
    match allocation {
        mj_core::state::SessionResourceAllocation::Container { cpus, memory_bytes } => {
            Some(mj_core::state::HostContainerSize {
                cpus: *cpus,
                memory_bytes: *memory_bytes,
            })
        }
        _ => None,
    }
}

fn add_container_commitment(
    capacity: &mut mj_core::state::HostContainerCapacity,
    cpus: u64,
    memory_bytes: u64,
) {
    capacity.committed.cpus = capacity.committed.cpus.saturating_add(cpus);
    capacity.committed.memory_bytes = capacity.committed.memory_bytes.saturating_add(memory_bytes);
    capacity.remaining.cpus = capacity.remaining.cpus.saturating_sub(cpus);
    capacity.remaining.memory_bytes = capacity.remaining.memory_bytes.saturating_sub(memory_bytes);
}

fn reservation_affects_group(
    reservation: &ContainerCapacityReservation,
    target_ids: &[String],
) -> bool {
    same_target_group(&reservation.target_ids, target_ids)
        || reservation
            .source
            .as_ref()
            .is_some_and(|source| same_target_group(&source.target_ids, target_ids))
}

fn same_target_group(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().all(|target_id| right.contains(target_id))
}

fn container_capacity_projection_for(
    state: &RuntimeState,
    sessions: &mj_core::state::State,
) -> BTreeMap<String, mj_client::runtime_feed::RuntimeContainerCapacity> {
    let Some(feed) = state.capacity_feed() else {
        return BTreeMap::new();
    };
    let reservations = state.container_capacity_reservations();
    let latest = feed.latest.borrow();
    feed.targets
        .borrow()
        .iter()
        .filter(|target| target.kind == crate::targets::DeploymentCapacityKind::Host)
        .filter_map(|target| {
            let usage = latest.get(&target.id)?;
            let capacity = capacity_with_reservations(
                &ContainerCapacityInput {
                    target_ids: target.target_ids.clone(),
                    total: mj_core::state::HostContainerSize {
                        cpus: usage.logical_cores,
                        memory_bytes: usage.memory_total_bytes,
                    },
                },
                sessions.sessions.values(),
                &reservations,
            );
            Some((
                target.id.clone(),
                mj_client::runtime_feed::RuntimeContainerCapacity {
                    target_ids: target.target_ids.clone(),
                    capacity,
                },
            ))
        })
        .collect()
}

impl RuntimeState {
    pub(crate) fn container_capacity_projection(
        &self,
        sessions: &mj_core::state::State,
    ) -> BTreeMap<String, mj_client::runtime_feed::RuntimeContainerCapacity> {
        container_capacity_projection_for(self, sessions)
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
                        // The runtime feed carries this reservation-adjusted
                        // headroom to terminal surfaces.
                        state.publish_revision();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_refusal_names_remaining_ram_and_never_promises_automatic_stop() {
        let capacity = mj_core::state::HostContainerCapacity {
            total: mj_core::state::HostContainerSize {
                cpus: 24,
                memory_bytes: 64 << 30,
            },
            reserved: mj_core::state::HostContainerSize {
                cpus: 0,
                memory_bytes: 12 << 30,
            },
            committed: mj_core::state::HostContainerSize {
                cpus: 20,
                memory_bytes: 44 << 30,
            },
            remaining: mj_core::state::HostContainerSize {
                cpus: 4,
                memory_bytes: (8 << 30) - 1,
            },
            complete: true,
        };
        let allocation = mj_core::state::SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: 16 << 30,
        };

        let error = capacity_refusal(capacity, &allocation);
        let refusal = mj_core::refusal::Refusal::of(&error).expect("user-facing refusal");

        assert_eq!(refusal.code(), Some("container-capacity-exceeded"));
        assert!(refusal.message().contains("7.9 GiB left"));
        assert!(
            refusal
                .message()
                .contains("does not stop sessions automatically")
        );
    }

    // Hard-won: #1375: summed cgroup ceilings refused idle Podman hosts.
    #[tokio::test]
    async fn shared_admission_preserves_apple_ram_and_warns_for_cgroup_overcommit() {
        use crate::controller::test_support::{IsolatedTest, test_name};
        use mj_core::state::SessionResourceAllocation;
        const NAME: &str = "shared_admission_preserves_apple_ram_and_warns_for_cgroup_overcommit";
        const CHILD: &str = "MJ_TEST_SHARED_CONTAINER_ADMISSION";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), NAME))
                .env(CHILD, "1")
                .env("MJ_INSTANCE", "shared-container-admission")
                .isolated_store(directory.path())
                .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut config = Config::default();
        let container = serde_json::from_value::<mj_core::config::ContainerTemplate>(
            serde_json::json!({"image": "unused-test-image"}),
        )
        .unwrap();
        config.targets.insert(
            "podman".into(),
            mj_core::config::TargetTemplate::LocalPodman {
                container: container.clone(),
            },
        );
        config.targets.insert(
            "apple".into(),
            mj_core::config::TargetTemplate::AppleContainer { container },
        );
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: mj_core::config::data_dir().join("unused-capacity-profile"),
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
                codex_profile: None,
                muse_auto_review: None,
                legacy_subagents: None,
            },
        );
        config.save().unwrap();
        let state = crate::daemon::tests::test_runtime_state();
        let target_ids = vec!["podman".into(), "apple".into()];
        let target = crate::targets::DeploymentCapacityTarget {
            id: "local".into(),
            host: "local".into(),
            target_ids: target_ids.clone(),
            kind: crate::targets::DeploymentCapacityKind::Host,
            local: true,
            probes: Vec::new(),
            local_storage_paths: Vec::new(),
            probe_error: None,
        };
        let usage = crate::targets::DeploymentCapacityUsage {
            cpu_percent: Some(0),
            memory_used_bytes: 0,
            memory_total_bytes: 40 << 30,
            logical_cores: 4,
            disk_total_bytes: None,
            storage: Vec::new(),
        };
        let (targets, _) = tokio::sync::watch::channel(vec![target]);
        let (updates, _) = tokio::sync::broadcast::channel(1);
        let (_latest, latest) =
            tokio::sync::watch::channel(BTreeMap::from([("local".into(), usage)]));
        let (refresh, _) = tokio::sync::mpsc::channel(1);
        assert!(
            state
                .capacity
                .set(CapacityFeed {
                    targets: targets.subscribe(),
                    updates,
                    latest,
                    refresh
                })
                .is_ok()
        );
        let allocation = |memory_gib: u64| SessionResourceAllocation::Container {
            cpus: 4,
            memory_bytes: memory_gib << 30,
        };
        let initial = state
            .admit_container_launch("apple", Some(allocation(24)), None, BTreeSet::new(), None)
            .await
            .unwrap();
        assert!(
            state.container_admission.try_lock().is_err(),
            "create holds admission until registration"
        );
        let workspace = crate::database::create_workspace("capacity").unwrap();
        let mut busy = crate::daemon::tests::runtime_test_session(
            "busy",
            &workspace.id,
            SessionState::Provisioning,
        );
        busy.target_template_id = "apple".into();
        busy.target = None;
        busy.resource_allocation = Some(allocation(24));
        crate::database::save_session(&busy).unwrap();
        drop(initial);

        for session_id in [None, Some("resume"), Some("move")] {
            let replaced = if session_id == Some("move") {
                BTreeSet::from(["move".into()])
            } else {
                BTreeSet::new()
            };
            let result = state
                .admit_container_launch("apple", Some(allocation(16)), session_id, replaced, None)
                .await;
            let error = result
                .err()
                .expect("Apple RAM overcommit must refuse create, resume and move");
            assert_eq!(
                Refusal::of(&error).unwrap().code(),
                Some("container-capacity-exceeded")
            );
        }
        // Drive the actual daemon entry points too, so a caller cannot silently
        // bypass the common admission function in a later refactor.
        let create = serde_json::from_value(serde_json::json!({
            "remember_launch_choices": false, "remember_container_size": false,
            "create_managed_worktree": false, "subagents": mj_core::subagent::SubagentPolicy::None,
            "no_project_memory": false, "no_github_events": false,
            "workspace_id": workspace.id, "profile_id": "codex", "bundle_id": "",
            "target_template_id": "apple", "title": "capacity refusal",
            "additional_mounts": [], "resource_allocation": allocation(16)
        }))
        .unwrap();
        let error = state
            .start_create_session(create)
            .await
            .expect_err("create must refuse Apple RAM overcommit");
        assert_eq!(
            Refusal::of(&error).unwrap().code(),
            Some("container-capacity-exceeded")
        );
        let mut stopped = busy.clone();
        stopped.id = "resumable".into();
        stopped.state = SessionState::Stopped;
        stopped.target_template_id = "podman".into();
        stopped.last_profile = "codex".into();
        stopped.resource_allocation = Some(allocation(16));
        crate::database::save_session(&stopped).unwrap();
        state.owner().edit_sessions(|sessions| {
            sessions.insert(stopped.id.clone(), stopped.clone());
        });
        let error = state
            .resume_session(ResumeSessionRequest {
                session_id: stopped.id.clone(),
                workspace_id: workspace.id.clone(),
                profile_id: "codex".into(),
                target_template_id: "apple".into(),
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: false,
                repository_preflight: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            Refusal::of(&error).unwrap().code(),
            Some("container-capacity-exceeded")
        );
        let move_request = serde_json::from_value(serde_json::json!({
            "preparation": {
                "selection": {"session_id": stopped.id, "target_template_id": "apple"},
                "source_profile_id": "codex", "source_target_template_id": "podman",
                "cross_harness": false, "active": false, "queued_commands": [],
                "fingerprint": "prepared", "operation_id": "capacity-move"
            },
            "queue": null, "acknowledge_interruption": true
        }))
        .unwrap();
        let error = state
            .move_session(move_request)
            .await
            .expect_err("move must refuse Apple RAM overcommit");
        assert_eq!(
            Refusal::of(&error).unwrap().code(),
            Some("container-capacity-exceeded")
        );

        let admitted = state
            .admit_container_launch(
                "podman",
                Some(allocation(16)),
                Some("podman-launch"),
                BTreeSet::new(),
                None,
            )
            .await
            .unwrap();
        let warning = admitted.warning.as_deref().unwrap();
        assert!(warning.contains("more CPUs"));
        assert!(warning.contains("more RAM"));
        assert_eq!(state.notices.lock().unwrap().back().unwrap().text, warning);
        let notice_count = state.notices.lock().unwrap().len();
        let joined = state
            .admit_container_launch(
                "podman",
                Some(allocation(16)),
                Some("podman-launch"),
                BTreeSet::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            state.notices.lock().unwrap().len(),
            notice_count,
            "joining must not duplicate launch warnings"
        );
        // A soft-budget launch must never lend its admission to an Apple
        // request, even if both templates share the physical host.
        assert!(
            state
                .admit_container_launch(
                    "apple",
                    Some(allocation(16)),
                    Some("podman-launch"),
                    BTreeSet::new(),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            state
                .admit_container_launch(
                    "podman",
                    Some(allocation(4)),
                    Some("podman-launch"),
                    BTreeSet::new(),
                    None
                )
                .await
                .is_err()
        );
        drop(admitted);
        assert!(
            state
                .container_capacity_reservations()
                .contains_key("podman-launch")
        );
        drop(joined);
        let admitted = state
            .admit_container_launch(
                "apple",
                Some(allocation(4)),
                Some("apple-cpu"),
                BTreeSet::new(),
                None,
            )
            .await
            .unwrap();
        let warning = admitted.warning.as_deref().unwrap();
        assert!(warning.contains("more CPUs"));
        assert!(!warning.contains("more RAM"));
        drop(admitted);

        // Missing owner sizing refuses Apple only; the cgroup warning describes
        // uncertainty instead of treating sentinel zeroes as measured capacity.
        busy.resource_allocation = None;
        crate::database::save_session(&busy).unwrap();
        assert!(
            state
                .admit_container_launch("apple", Some(allocation(4)), None, BTreeSet::new(), None)
                .await
                .is_err()
        );
        let admitted = state
            .admit_container_launch("podman", Some(allocation(4)), None, BTreeSet::new(), None)
            .await
            .unwrap();
        assert!(
            admitted
                .warning
                .as_deref()
                .unwrap()
                .contains("no recorded size")
        );
        drop(admitted);
        // No successful reading exists on this separate runtime.
        let unavailable = crate::daemon::tests::test_runtime_state();
        assert!(
            unavailable
                .admit_container_launch("apple", Some(allocation(4)), None, BTreeSet::new(), None)
                .await
                .is_err()
        );
        assert!(
            unavailable
                .admit_container_launch("podman", Some(allocation(4)), None, BTreeSet::new(), None)
                .await
                .is_ok()
        );
    }

    #[test]
    fn reservation_replaces_the_same_sessions_active_commitment() {
        let mut active =
            crate::controller::test_support::checkpoint_test_session("reserved-session");
        active.state = mj_core::state::SessionState::Running;
        active.target_template_id = "podman".into();
        active.target = Some(mj_core::state::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        });
        active.resource_allocation = Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 2,
            memory_bytes: 4 << 30,
        });
        let reservations = BTreeMap::from([(
            active.id.clone(),
            ContainerCapacityReservation {
                leases: 1,
                hard_memory: false,
                target_ids: vec!["podman".into()],
                allocation: mj_core::state::SessionResourceAllocation::Container {
                    cpus: 4,
                    memory_bytes: 8 << 30,
                },
                source: None,
            },
        )]);
        let input = ContainerCapacityInput {
            target_ids: vec!["podman".into()],
            total: mj_core::state::HostContainerSize {
                cpus: 4,
                memory_bytes: 8 << 30,
            },
        };

        let capacity = capacity_with_reservations(&input, [&active], &reservations);

        assert_eq!(
            capacity.committed,
            mj_core::state::HostContainerSize {
                cpus: 4,
                memory_bytes: 8 << 30,
            }
        );
        assert!(capacity.refuses(&reservations[&active.id].allocation, true));
    }

    // Hard-won: d27eb9f56: a same-host Move released its running source too early.
    // Hard-won: 03034132f: the reservation then kept charging it after it stopped.
    #[test]
    fn same_host_move_tracks_the_source_until_its_durable_stop() {
        let mut source = crate::controller::test_support::checkpoint_test_session("moving-session");
        source.state = mj_core::state::SessionState::Running;
        source.target_template_id = "podman".into();
        source.target = Some(mj_core::state::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        });
        source.resource_allocation = Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 16 << 30,
        });
        let input = ContainerCapacityInput {
            target_ids: vec!["podman".into()],
            total: mj_core::state::HostContainerSize {
                cpus: 10,
                memory_bytes: 40 << 30,
            },
        };
        let reservations = BTreeMap::from([(
            source.id.clone(),
            ContainerCapacityReservation {
                leases: 1,
                hard_memory: false,
                target_ids: vec!["podman".into()],
                allocation: mj_core::state::SessionResourceAllocation::Container {
                    cpus: 2,
                    memory_bytes: 4 << 30,
                },
                source: Some(ContainerCapacityCommitment {
                    target_ids: vec!["podman".into()],
                    allocation: source.resource_allocation.clone(),
                }),
            },
        )]);

        let capacity = capacity_with_reservations(&input, [&source], &reservations);

        assert_eq!(capacity.committed.cpus, 8);
        assert_eq!(capacity.remaining.cpus, 2);
        assert!(
            capacity
                .overcommit(&mj_core::state::SessionResourceAllocation::Container {
                    cpus: 8,
                    memory_bytes: 16 << 30,
                })
                .cpus
        );

        let mut stopped = source.clone();
        stopped.state = mj_core::state::SessionState::Stopped;
        let capacity = capacity_with_reservations(&input, [&stopped], &reservations);
        assert_eq!(capacity.committed.cpus, 2);
        assert_eq!(capacity.remaining.cpus, 8);

        let mut destination = source.clone();
        destination.state = mj_core::state::SessionState::Provisioning;
        destination.target = None;
        destination.resource_allocation =
            Some(mj_core::state::SessionResourceAllocation::Container {
                cpus: 2,
                memory_bytes: 4 << 30,
            });
        let capacity = capacity_with_reservations(&input, [&destination], &reservations);
        assert_eq!(capacity.committed.cpus, 2);
        assert_eq!(capacity.remaining.cpus, 8);
    }

    // Hard-won: d27eb9f56: a cross-host Move could release its source host early.
    #[test]
    fn cross_host_move_charges_each_deployment_host_once() {
        let mut source = crate::controller::test_support::checkpoint_test_session("moving-session");
        source.state = mj_core::state::SessionState::Running;
        source.target_template_id = "podman".into();
        source.target = Some(mj_core::state::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        });
        source.resource_allocation = Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 16 << 30,
        });
        let reservations = BTreeMap::from([(
            source.id.clone(),
            ContainerCapacityReservation {
                leases: 1,
                hard_memory: false,
                target_ids: vec!["docker".into()],
                allocation: mj_core::state::SessionResourceAllocation::Container {
                    cpus: 4,
                    memory_bytes: 8 << 30,
                },
                source: Some(ContainerCapacityCommitment {
                    target_ids: vec!["podman".into()],
                    allocation: source.resource_allocation.clone(),
                }),
            },
        )]);
        let input = |target_id: &str| ContainerCapacityInput {
            target_ids: vec![target_id.into()],
            total: mj_core::state::HostContainerSize {
                cpus: 10,
                memory_bytes: 40 << 30,
            },
        };

        let source_capacity =
            capacity_with_reservations(&input("podman"), [&source], &reservations);
        let destination_capacity =
            capacity_with_reservations(&input("docker"), [&source], &reservations);

        assert_eq!(source_capacity.committed.cpus, 8);
        assert_eq!(destination_capacity.committed.cpus, 4);

        let mut stopped = source;
        stopped.state = mj_core::state::SessionState::Stopped;
        let retired_source =
            capacity_with_reservations(&input("podman"), [&stopped], &reservations);
        assert_eq!(retired_source.committed.cpus, 0);
    }

    // Hard-won: d27eb9f56: destination admission counted a same-host source twice.
    #[test]
    fn same_host_move_admission_does_not_double_count_the_source() {
        let mut source = crate::controller::test_support::checkpoint_test_session("moving-session");
        source.state = mj_core::state::SessionState::Running;
        source.target_template_id = "podman".into();
        source.target = Some(mj_core::state::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        });
        source.resource_allocation = Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 16 << 30,
        });
        let input = ContainerCapacityInput {
            target_ids: vec!["podman".into()],
            total: mj_core::state::HostContainerSize {
                cpus: 10,
                memory_bytes: 40 << 30,
            },
        };
        let replaced = BTreeSet::from([source.id.clone()]);

        let capacity =
            capacity_with_reservations_after(&input, [&source], &BTreeMap::new(), &replaced);

        assert!(!capacity.refuses(
            &mj_core::state::SessionResourceAllocation::Container {
                cpus: 9,
                memory_bytes: 18 << 30,
            },
            true
        ));
    }

    // Hard-won: d27eb9f56: a joined web Move lease deleted the owner's reservation.
    // Hard-won: #1375: the first holder could finish while a join still needed its reservation.
    #[tokio::test]
    async fn joined_lease_cannot_remove_the_owning_reservation() {
        let state = crate::daemon::tests::test_runtime_state();
        let input = ContainerCapacityInput {
            target_ids: vec!["podman".into()],
            total: mj_core::state::HostContainerSize {
                cpus: 8,
                memory_bytes: 16 << 30,
            },
        };
        let allocation = mj_core::state::SessionResourceAllocation::Container {
            cpus: 2,
            memory_bytes: 4 << 30,
        };

        for owner_finishes_first in [false, true] {
            let owner = state.reserve_container_capacity("session", &input, allocation.clone());
            let joined = state.reserve_container_capacity("session", &input, allocation.clone());
            let (first, last) = if owner_finishes_first {
                (owner, joined)
            } else {
                (joined, owner)
            };
            drop(first);
            assert!(
                state
                    .container_capacity_reservations()
                    .contains_key("session")
            );
            drop(last);
            assert!(
                !state
                    .container_capacity_reservations()
                    .contains_key("session")
            );
        }
    }

    // Hard-won: d27eb9f56: the terminal feed omitted daemon-only reservations.
    #[tokio::test]
    async fn runtime_capacity_projection_includes_the_move_reservation() {
        let state = crate::daemon::tests::test_runtime_state();
        let target = crate::targets::DeploymentCapacityTarget {
            id: "local".into(),
            host: "local".into(),
            target_ids: vec!["podman".into()],
            kind: crate::targets::DeploymentCapacityKind::Host,
            local: true,
            probes: Vec::new(),
            local_storage_paths: Vec::new(),
            probe_error: None,
        };
        let usage = crate::targets::DeploymentCapacityUsage {
            cpu_percent: Some(20),
            memory_used_bytes: 8 << 30,
            memory_total_bytes: 40 << 30,
            logical_cores: 10,
            disk_total_bytes: None,
            storage: Vec::new(),
        };
        let (targets, _targets_rx) = tokio::sync::watch::channel(vec![target]);
        let (updates, _) = tokio::sync::broadcast::channel(1);
        let (_latest, latest_rx) =
            tokio::sync::watch::channel(BTreeMap::from([("local".to_owned(), usage)]));
        let (refresh, _) = tokio::sync::mpsc::channel(1);
        assert!(
            state
                .capacity
                .set(CapacityFeed {
                    targets: targets.subscribe(),
                    updates,
                    latest: latest_rx,
                    refresh,
                })
                .is_ok()
        );
        let input = ContainerCapacityInput {
            target_ids: vec!["podman".into()],
            total: mj_core::state::HostContainerSize {
                cpus: 10,
                memory_bytes: 40 << 30,
            },
        };
        let mut source = crate::controller::test_support::checkpoint_test_session("moving-session");
        source.state = mj_core::state::SessionState::Running;
        source.target_template_id = "podman".into();
        source.target = Some(mj_core::state::TargetLocator::LocalPodman {
            container_id: "container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        });
        source.resource_allocation = Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 16 << 30,
        });
        let mut sessions = mj_core::state::State::default();
        sessions.sessions.insert(source.id.clone(), source.clone());
        let _lease = state.reserve_move_capacity_lease(
            &source.id,
            &input,
            mj_core::state::SessionResourceAllocation::Container {
                cpus: 9,
                memory_bytes: 18 << 30,
            },
            Some(ContainerCapacityCommitment {
                target_ids: vec!["podman".into()],
                allocation: source.resource_allocation,
            }),
        );

        let projection = state.container_capacity_projection(&sessions);

        assert_eq!(
            projection["local"].capacity.committed,
            mj_core::state::HostContainerSize {
                cpus: 9,
                memory_bytes: 18 << 30,
            }
        );
        assert_eq!(projection["local"].capacity.remaining.cpus, 1);
    }
}
