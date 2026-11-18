//! Types that more than one part of the daemon side names, kept apart from
//! all of them: the plan the session runtime follows to recover a worker,
//! which worker staging fills in, and the failure a harness reports while it
//! prepares.
//!
//! Staging builds these plans and the session runtime executes them. With the
//! types in either one, the other would have to depend on it; here both
//! depend only on this crate. Put a type here when two crates that should not
//! depend on each other both need it, and keep behaviour out: a plan is data.

use std::path::PathBuf;

use mj_core::targets::{CommandPlan, CommandSpec, TargetRecoveryPlan};
use mj_targets::targets::{TargetLocator, WorkerControlPlan};

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
    /// Runs the control worker, so it installs it first.
    pub liveness_probe: WorkerControlPlan,
    /// Prints the dead worker's own exit record, read before a restart
    /// replaces it, so a worker that died of a full disk is not restarted
    /// onto the same full disk.
    pub exit_record: Option<CommandSpec>,
    /// Refresh a stale installed worker before restarting it. The digest is
    /// computed inside the recovery task so hashing a large binary never
    /// blocks a controller UI loop.
    pub binary_refresh: Option<WorkerBinaryRefresh>,
    /// Keep the worker executable and its launch schema paired. Configuration
    /// bytes travel through redacted stdin only when their digest is stale.
    pub launch_refresh: Option<WorkerLaunchRefreshPlan>,
    pub restart: WorkerControlPlan,
}

/// How recovery refreshes a stale installed worker binary before restarting.
///
/// A caller with a known source can supply a prepared copy plan. Controller
/// target planning always defers resolution: selecting the right architecture,
/// downloading and hashing binaries belong in the background recovery task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBinaryRefresh {
    Prepared(WorkerBinaryRefreshPlan),
    Deferred(DeferredWorkerBinaryRefresh),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBinaryRefreshPlan {
    pub source: PathBuf,
    pub installed_digest: CommandSpec,
    pub replace: CommandPlan,
}

/// A worker refresh resolved at recovery time: select the worker binary
/// for the target's own architecture, compare it to the installed one, and
/// copy only when they differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredWorkerBinaryRefresh {
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

/// A harness could not be prepared for a session: the step that failed and
/// what it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessPreparationFailure {
    pub step: String,
    pub error: String,
}

impl std::fmt::Display for HarnessPreparationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "harness preparation failed at {}: {}",
            self.step, self.error
        )
    }
}

impl std::error::Error for HarnessPreparationFailure {}
