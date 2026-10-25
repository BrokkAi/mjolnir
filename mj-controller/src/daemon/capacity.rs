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
    /// Destination deployment target and allocation.
    pub(crate) target_ids: Vec<String>,
    pub(crate) allocation: mj_core::state::SessionResourceAllocation,
    /// The running source a same- or cross-host Move has not retired yet.
    pub(crate) source: Option<ContainerCapacityCommitment>,
}

/// Owns one in-memory resume or move reservation until the lifecycle either
/// publishes its durable `Provisioning` record or ends.
pub(crate) struct ContainerCapacityLease {
    state: Arc<RuntimeState>,
    session_id: String,
    owner: bool,
}

impl Drop for ContainerCapacityLease {
    fn drop(&mut self) {
        if self.owner {
            self.state
                .container_reservations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.session_id);
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

    /// A cached host budget for callers on latency-sensitive control paths.
    pub(crate) fn cached_container_capacity_input(
        &self,
        target_id: &str,
    ) -> Option<ContainerCapacityInput> {
        let (target, usage) = self.capacity_feed()?.reading_for_target(target_id)?;
        (target.kind == crate::targets::DeploymentCapacityKind::Host).then_some(
            ContainerCapacityInput {
                target_ids: target.target_ids,
                total: mj_core::state::HostContainerSize {
                    cpus: usage.logical_cores,
                    memory_bytes: usage.memory_total_bytes,
                },
            },
        )
    }

    /// Capacity after overlaying resume reservations. A reserved session's
    /// durable state may still be `Stopped` or `Error`, so exclude that record
    /// and charge its reservation once instead.
    pub(crate) fn capacity_with_reservations<'a>(
        &self,
        input: &ContainerCapacityInput,
        sessions: impl IntoIterator<Item = &'a SessionRecord>,
    ) -> mj_core::state::HostContainerCapacity {
        let reservations = self
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        capacity_with_reservations(input, sessions, &reservations)
    }

    pub(crate) fn container_capacity_reservations(
        &self,
    ) -> BTreeMap<String, ContainerCapacityReservation> {
        self.container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn reserve_container_capacity(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: mj_core::state::SessionResourceAllocation,
    ) -> ContainerCapacityLease {
        self.reserve_container_capacity_with_source(session_id, input, allocation, None)
    }

    pub(crate) fn reserve_move_capacity_lease(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: mj_core::state::SessionResourceAllocation,
        source: Option<ContainerCapacityCommitment>,
    ) -> ContainerCapacityLease {
        self.reserve_container_capacity_with_source(session_id, input, allocation, source)
    }

    pub(crate) fn joined_container_capacity_lease(
        self: &Arc<Self>,
        session_id: &str,
    ) -> ContainerCapacityLease {
        debug_assert!(
            self.container_reservations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(session_id)
        );
        ContainerCapacityLease {
            state: self.clone(),
            session_id: session_id.to_owned(),
            owner: false,
        }
    }

    fn reserve_container_capacity_with_source(
        self: &Arc<Self>,
        session_id: &str,
        input: &ContainerCapacityInput,
        allocation: mj_core::state::SessionResourceAllocation,
        source: Option<ContainerCapacityCommitment>,
    ) -> ContainerCapacityLease {
        let mut reservations = self
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let owner = if let std::collections::btree_map::Entry::Vacant(entry) =
            reservations.entry(session_id.to_owned())
        {
            entry.insert(ContainerCapacityReservation {
                target_ids: input.target_ids.clone(),
                allocation,
                source,
            });
            true
        } else {
            false
        };
        ContainerCapacityLease {
            state: self.clone(),
            session_id: session_id.to_owned(),
            owner,
        }
    }

    /// Reserve host capacity for any accepted operation that will create or
    /// replace an owning container. New sessions hold the admission lock until
    /// their durable record exists; resume and move hold this lease until the
    /// corresponding lifecycle ends.
    pub(crate) async fn reserve_container_capacity_for_launch(
        self: &Arc<Self>,
        session_id: &str,
        target_id: &str,
        allocation: Option<mj_core::state::SessionResourceAllocation>,
    ) -> Result<Option<ContainerCapacityLease>> {
        let checked_target_id = target_id.to_owned();
        let container_target = blocking(move || {
            let controller = Controller::load()?;
            let template = controller
                .config
                .targets
                .get(&checked_target_id)
                .with_context(|| format!("unknown target {checked_target_id:?}"))?;
            Ok(mj_core::config::is_container_target(template))
        })
        .await?;
        if !container_target || self.capacity_feed().is_none() {
            return Ok(None);
        }
        let Some(input) = self
            .container_capacity_input(target_id)
            .await
            .context(Refusal::precondition(
                "Mjolnir could not read this host's CPU and RAM capacity; check the target and retry",
            ))?
        else {
            return Err(anyhow::Error::new(
                Refusal::precondition(
                    "this target's host capacity is not ready yet; refresh the target and retry",
                )
                .with_code("container-capacity-unavailable"),
            ));
        };
        let Some(allocation) = allocation else {
            return Err(anyhow::Error::new(
                Refusal::unusable(
                    "this container session has no recorded CPU and RAM allocation; choose a size before continuing",
                )
                .with_code("container-size-required"),
            ));
        };
        let admission = self.container_admission.lock().await;
        if self
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(session_id)
        {
            let lease = self.reserve_container_capacity(session_id, &input, allocation);
            drop(admission);
            return Ok(Some(lease));
        }
        let admission_state = self.clone();
        let checked_input = input.clone();
        let capacity = blocking(move || {
            let controller = Controller::load()?;
            Ok(admission_state
                .capacity_with_reservations(&checked_input, controller.state.sessions.values()))
        })
        .await?;
        if !capacity.fits(&allocation) {
            return Err(super::create::capacity_refusal(capacity, &allocation));
        }
        let lease = self.reserve_container_capacity(session_id, &input, allocation);
        drop(admission);
        Ok(Some(lease))
    }
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
        assert!(!capacity.fits(&reservations[&active.id].allocation));
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
            !capacity.fits(&mj_core::state::SessionResourceAllocation::Container {
                cpus: 8,
                memory_bytes: 16 << 30,
            })
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

        assert!(
            capacity.fits(&mj_core::state::SessionResourceAllocation::Container {
                cpus: 9,
                memory_bytes: 18 << 30,
            })
        );
    }

    // Hard-won: d27eb9f56: a joined web Move lease deleted the owner's reservation.
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

        let owner = state.reserve_container_capacity("session", &input, allocation.clone());
        let joined = state.reserve_container_capacity("session", &input, allocation);
        drop(joined);
        assert!(
            state
                .container_capacity_reservations()
                .contains_key("session")
        );
        drop(owner);
        assert!(
            !state
                .container_capacity_reservations()
                .contains_key("session")
        );
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
