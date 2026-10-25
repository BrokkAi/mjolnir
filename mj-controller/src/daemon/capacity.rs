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
pub(crate) struct ContainerCapacityReservation {
    pub(crate) target_ids: Vec<String>,
    pub(crate) allocation: mj_core::state::SessionResourceAllocation,
}

/// Owns one in-memory resume or move reservation until the lifecycle either
/// publishes its durable `Provisioning` record or ends.
pub(crate) struct ContainerCapacityLease {
    state: Arc<RuntimeState>,
    session_id: String,
}

impl Drop for ContainerCapacityLease {
    fn drop(&mut self) {
        self.state
            .container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session_id);
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
        self.container_reservations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session_id.to_owned())
            .or_insert_with(|| ContainerCapacityReservation {
                target_ids: input.target_ids.clone(),
                allocation,
            });
        ContainerCapacityLease {
            state: self.clone(),
            session_id: session_id.to_owned(),
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
    let reserved_ids = reservations
        .iter()
        .filter(|(_, reservation)| same_target_group(&reservation.target_ids, &input.target_ids))
        .map(|(session_id, _)| session_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut capacity = mj_core::state::host_container_capacity(
        input.total,
        &input.target_ids,
        sessions
            .into_iter()
            .filter(|session| !reserved_ids.contains(session.id.as_str())),
    );
    for reservation in reservations
        .values()
        .filter(|reservation| same_target_group(&reservation.target_ids, &input.target_ids))
    {
        let mj_core::state::SessionResourceAllocation::Container { cpus, memory_bytes } =
            &reservation.allocation
        else {
            continue;
        };
        capacity.committed.cpus = capacity.committed.cpus.saturating_add(*cpus);
        capacity.committed.memory_bytes = capacity
            .committed
            .memory_bytes
            .saturating_add(*memory_bytes);
        capacity.remaining.cpus = capacity.remaining.cpus.saturating_sub(*cpus);
        capacity.remaining.memory_bytes = capacity
            .remaining
            .memory_bytes
            .saturating_sub(*memory_bytes);
    }
    capacity
}

fn same_target_group(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().all(|target_id| right.contains(target_id))
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
}
