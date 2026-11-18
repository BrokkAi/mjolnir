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
    AwaitingTarget,
    PublishedTarget,
}

impl StartingSessions {
    pub(super) fn reconcile(&mut self, targets: &BTreeMap<String, RelaySessionTarget>) {
        self.0.retain(|id, targeted| {
            if targets.contains_key(id) {
                *targeted = StartingSessionPhase::PublishedTarget;
                true
            } else {
                matches!(targeted, StartingSessionPhase::AwaitingTarget)
            }
        });
    }

    pub(super) fn command(
        &mut self,
        command: ManagerCommand,
        view: Option<ManagedSessionView>,
    ) -> Option<ManagerCommand> {
        use mj_client::session::ParentMessageRoute;
        match command {
            ManagerCommand::RegisterStarting { session_id, reply } => {
                self.0
                    .entry(session_id)
                    .or_insert(StartingSessionPhase::AwaitingTarget);
                let _ = reply.send(());
            }
            ManagerCommand::ForgetStarting { session_id, reply } => {
                self.0.remove(&session_id);
                let _ = reply.send(());
            }
            ManagerCommand::ParentMessageRoute { session_id, reply } => {
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
            other => return Some(other),
        }
        None
    }
}
