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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySessionTarget {
    pub session_id: String,
    pub spec: CommandSpec,
    /// Prove the exact worker is absent before restarting it in place. Direct
    /// relay clients omit recovery; controller-managed sessions self-heal
    /// without turning a shared transport outage into destructive restarts.
    pub worker_recovery: Option<WorkerRecoveryPlan>,
    pub project_memory: Option<ProjectMemorySyncTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMemorySyncTarget {
    pub canonical_root: std::path::PathBuf,
}

/// The working directory a bare-target worker must be able to enter before it
/// can serve a relay handshake. Container availability is checked separately
/// by the target recovery plan; bare targets have no runtime object to inspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerWorkspace {
    pub target: mj_core::state::ManagedWorktreeTarget,
    pub directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecoveryPlan {
    /// Durable target identity when this plan was built. A stale actor must
    /// never recover a resource after the session moves or starts destruction.
    pub source_target: mj_core::state::TargetLocator,
    pub target: Option<TargetRecoveryPlan>,
    pub workspace: Option<WorkerWorkspace>,
    pub liveness_probe: CommandSpec,
    /// Refresh a stale installed worker before restarting it. The digest is
    /// computed inside the recovery task so hashing a large binary never
    /// blocks a controller UI loop.
    pub binary_refresh: Option<WorkerBinaryRefresh>,
    /// Keep the worker executable and its launch schema paired. Configuration
    /// bytes travel through redacted stdin only when their digest is stale.
    pub launch_refresh: Option<WorkerLaunchRefreshPlan>,
    pub restart: CommandPlan,
}

/// How recovery refreshes a stale installed worker binary before restarting.
///
/// Local targets resolve the source and the copy at plan-build time, which is
/// cheap. Remote targets cannot: choosing the binary needs the target's
/// architecture, and that probe plus hashing the remote binary are blocking
/// ssh round-trips that must not run on the plan-build/UI path. So a remote
/// refresh carries only what is cheap to compute and resolves the rest inside
/// the recovery task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBinaryRefresh {
    Prepared(WorkerBinaryRefreshPlan),
    Remote(RemoteWorkerBinaryRefresh),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBinaryRefreshPlan {
    pub source: PathBuf,
    pub installed_digest: CommandSpec,
    pub replace: CommandPlan,
}

/// A remote worker refresh resolved at recovery time: select the worker binary
/// for the target's own architecture, compare it to the installed one, and
/// copy only when they differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWorkerBinaryRefresh {
    pub locator: TargetLocator,
    pub session_id: String,
    pub installed_digest: CommandSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLaunchRefreshPlan {
    pub expected_sha256: String,
    pub installed_digest: CommandSpec,
    pub replace: CommandPlan,
}
