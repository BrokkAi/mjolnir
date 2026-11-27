//! Multiplexed controller-side ownership of durable ACP relay sessions.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::{mpsc, oneshot, watch};

use crate::database::{ProjectionApplyOutcome, ProjectionIntegrityError, apply_projection_page};
use crate::worker_client::{RelayClient, RelayEventPage, RelayRejected, RelayTransportDead};
use mj_checkpoint::archive::verify_archive_streaming;
use mj_core::credentials::{
    CredentialSyncSignal, relay_event_credential_sync_reason, relay_event_login_fingerprint,
};
use mj_core::elicitation::ElicitationResponse;
use mj_core::state::{ManagedSessionSnapshot, MaterializedSession};
use mj_transcript::projection::{
    ProjectionIndex, apply_committed_projection_event_indexed, materialized_session_from_canonical,
    project_relay_event_indexed,
};

use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandSpec, TargetRecoveryOutcome,
    ensure_recovery_target_running,
};
use mj_core::relay::{RelayCommand, RelayCursor, RelayOperationalState};

/// Build the same idempotent read consequence on receipt and relay attachment.
pub fn notifications_read(
    live: &RelayOperationalState,
    through: u64,
) -> Option<(String, RelayCommand, u64)> {
    let through = through.min(live.latest_ordinal);
    (live.supports_notification_stack()
        && live.checkpoint_barrier.is_none()
        && through > live.notifications_read_through)
        .then(|| {
            (
                format!("notifications-read-{through}"),
                RelayCommand::NotificationsRead { through },
                through,
            )
        })
}

pub use mj_client::session::{
    ManagedSessionView, ReviewerAction, ReviewerOutcome, ViewError, new_command_id,
};
/// Sync cadence while the worker owns work, so streamed events show promptly.
const SESSION_SYNC_INTERVAL: Duration = Duration::from_millis(150);
/// Sync cadence while the worker is quiet. See `actor::sync_delay`.
const QUIET_SESSION_SYNC_INTERVAL: Duration = Duration::from_secs(2);
/// A quiet session whose worker reports changes as they happen is synced as
/// often as its worker CPU is read, which the change wait's bound matches.
const CHANGE_WAIT_SYNC_INTERVAL: Duration = mj_core::relay::RELAY_CHANGE_WAIT_BOUND;
/// Release SQLite's single writer between bounded pieces of a large relay
/// catch-up. One transport page can contain thousands of terminal events and
/// must not prevent every other session actor from publishing its view.
const PROJECTION_TRANSACTION_EVENT_BUDGET: usize = 128;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
/// Ceiling for reconnect backoff. A worker that exited stays gone until the
/// user acts, so retrying it every second only burns process spawns.
const RECONNECT_BACKOFF_CEILING: Duration = Duration::from_secs(30);
const UNREACHABLE_FAILURE_THRESHOLD: u32 = 2;
const WORKER_RESTART_TIMEOUT: Duration = Duration::from_secs(60);
const WORKER_RESTART_COOLDOWN: Duration = Duration::from_secs(60);
const SESSION_MANAGER_SHUTDOWN_GRACE: Duration = Duration::from_millis(750);

mod types;
pub use mj_session_types::{
    DeferredWorkerBinaryRefresh, ProjectMemorySyncTarget, RelaySessionTarget, WorkerBinaryRefresh,
    WorkerBinaryRefreshPlan, WorkerLaunchRefreshPlan, WorkerRecoveryPlan, WorkerWorkspace,
};
use types::*;
mod recovery;
pub use recovery::*;
mod delegation;
pub use delegation::*;
mod channels;
pub use channels::*;
mod handle;
pub use handle::*;
mod client_backend;
use client_backend::*;
mod actor_types;
use actor_types::*;
pub use actor_types::{
    RELAY_JOB_ACTOR_STOPPED, RELAY_JOB_LEASED, RELAY_JOB_RECONNECTING, RELAY_JOB_TARGET_CHANGING,
};
pub use actor_types::{RelayConnectionJob, RelayJobDeferred};
mod remote;
pub use remote::*;
mod spawn;
pub use spawn::*;
mod readiness;
use readiness::HarnessStartupWatch;
pub use readiness::{HarnessFailureHandler, HarnessStartupFailure};
mod actor;
pub use actor::session_has_ended;
use actor::*;
mod standalone;
mod subagent_queue;
pub use standalone::*;

// The stand-in relay; the `test-support` feature lends it to mj-daemon's tests.
#[doc(hidden)]
pub mod stand_in;
#[cfg(test)]
pub mod tests;

mod model_policy;

/// Internals that the tests of the crates above drive directly.
#[doc(hidden)]
pub mod test_internals {
    pub use super::actor::{publish_view, run_session_actor, sync_actor_connection};
    pub use super::actor_types::ActorCommand;
    pub use super::model_policy::SessionModelPolicy;
    pub use super::readiness::HARNESS_READINESS_TIMEOUT;
}
pub mod move_ownership;
use model_policy::SessionModelPolicy;

mod pollable;
pub use pollable::session_target_is_pollable;
#[doc(hidden)]
pub use pollable::take_pollability_visits;
