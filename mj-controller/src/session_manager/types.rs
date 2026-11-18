use super::*;

#[derive(Debug)]
pub(super) struct ProjectionAdvancedError {
    pub(super) event_ordinal: u64,
}

impl std::fmt::Display for ProjectionAdvancedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "another projector committed relay event {} first",
            self.event_ordinal
        )
    }
}

impl std::error::Error for ProjectionAdvancedError {}

/// Delay before the next reconnect attempt after `failures` consecutive
/// failures. Doubles from `RECONNECT_INTERVAL` up to the ceiling.
pub(super) fn reconnect_delay(failures: u32) -> Duration {
    let doubling = failures.saturating_sub(1).min(u32::BITS - 1);
    RECONNECT_INTERVAL
        .saturating_mul(1_u32 << doubling)
        .min(RECONNECT_BACKOFF_CEILING)
}

#[derive(Default)]
pub(super) struct StartingSessions(BTreeMap<String, StartingSessionPhase>);

enum StartingSessionPhase {
    Guarded,
    AwaitingTarget,
    PublishedTarget,
}

impl StartingSessions {
    pub(super) fn reconcile(&mut self, targets: &BTreeMap<String, RelaySessionTarget>) {
        self.0.retain(|id, phase| {
            if matches!(phase, StartingSessionPhase::Guarded) {
                true
            } else if targets.contains_key(id) {
                *phase = StartingSessionPhase::PublishedTarget;
                true
            } else {
                matches!(phase, StartingSessionPhase::AwaitingTarget)
            }
        });
    }

    pub(super) fn reconcile_committed(&mut self, state: &mj_core::state::State) {
        self.0.retain(|id, phase| {
            matches!(phase, StartingSessionPhase::Guarded)
                || state
                    .sessions
                    .get(id)
                    .is_some_and(|record| record.state.can_receive_child_work())
        });
    }

    pub(super) fn event(
        &mut self,
        event: StartingEvent,
        view: Option<ManagedSessionView>,
        committed: Option<&crate::database::CommittedState>,
    ) {
        use mj_client::session::ParentMessageRoute;
        if let Some(committed) = committed {
            self.reconcile_committed(&committed.state);
        }
        match event {
            StartingEvent::Register {
                session_id,
                launches,
                reply,
            } => {
                let result = if committed.is_none() {
                    Err(anyhow::anyhow!(
                        "launch registration requires the durable state feed"
                    ))
                } else if let std::collections::btree_map::Entry::Vacant(entry) =
                    self.0.entry(session_id)
                {
                    let session_id = entry.key().clone();
                    entry.insert(StartingSessionPhase::Guarded);
                    // The reply owns the guard until the creator receives it.
                    // Dropping either side releases an uncommitted entry.
                    Ok(StartingSessionRegistration {
                        session_id: Some(session_id),
                        launches,
                    })
                } else {
                    Err(anyhow::anyhow!("session launch is already registered"))
                };
                let _ = reply.send(result);
            }
            StartingEvent::Published { session_id, reply } => {
                let result = match (self.0.get_mut(&session_id), committed) {
                    (Some(phase), Some(committed)) => {
                        *phase = StartingSessionPhase::AwaitingTarget;
                        // The writer publishes before replying to the commit. Read
                        // its latest state here even if its watch coalesced failure
                        // and removal before this handover reached the manager.
                        self.reconcile_committed(&committed.state);
                        Ok(())
                    }
                    _ => Err(anyhow::anyhow!(
                        "session launch registration is unavailable"
                    )),
                };
                let _ = reply.send(result);
            }
            StartingEvent::Aborted { session_id } => {
                if self
                    .0
                    .get(&session_id)
                    .is_some_and(|phase| matches!(phase, StartingSessionPhase::Guarded))
                {
                    self.0.remove(&session_id);
                }
            }
            StartingEvent::Route { session_id, reply } => {
                let protocol = view
                    .as_ref()
                    .and_then(|view| view.snapshot.as_ref())
                    .and_then(|snapshot| snapshot.operational.relay_protocol_version);
                let route = if let Some(protocol) = protocol {
                    ParentMessageRoute::Published(Some(protocol))
                } else if self.0.contains_key(&session_id) {
                    ParentMessageRoute::Starting
                } else if view.is_some() {
                    ParentMessageRoute::Published(None)
                } else {
                    ParentMessageRoute::Unmanaged
                };
                let _ = reply.send(route);
            }
        }
    }
}
