//! Multiplexed controller-side ownership of durable ACP relay sessions.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::{mpsc, oneshot, watch};

use crate::database::{
    ProjectionApplyOutcome, ProjectionIntegrityError, apply_projection_page,
    save_materialized_session,
};
use crate::worker_client::{RelayClient, RelayEventPage, RelayRejected, RelayTransportDead};
use mj_checkpoint::archive::verify_archive_streaming;
use mj_core::credentials::{CredentialSyncSignal, relay_event_credential_sync_reason};
use mj_core::elicitation::ElicitationResponse;
use mj_core::state::{ManagedSessionSnapshot, MaterializedSession};
use mj_transcript::projection::{
    ProjectionIndex, apply_committed_projection_event_indexed, materialized_session_from_canonical,
    project_relay_event_indexed,
};

use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandPlan, CommandSpec, TargetLocator,
    TargetRecoveryOutcome, TargetRecoveryPlan, ensure_recovery_target_running,
};
use mj_core::relay::{RelayCommand, RelayCursor, RelayOperationalState};

pub use mj_client::session::{
    ManagedSessionView, ReviewerAction, ReviewerOutcome, ViewError, new_command_id,
};
#[cfg(test)]
use mj_core::worker_launch::ReviewerLaunchConfig;

const SESSION_SYNC_INTERVAL: Duration = Duration::from_millis(150);
/// Release SQLite's single writer between bounded pieces of a large relay
/// catch-up. One transport page can contain thousands of terminal events and
/// must not prevent every other session actor from publishing its view.
const PROJECTION_TRANSACTION_EVENT_BUDGET: usize = 128;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
/// Ceiling for reconnect backoff. A worker that exited stays gone until the
/// user acts, so retrying it every second only burns process spawns.
const RECONNECT_BACKOFF_CEILING: Duration = Duration::from_secs(30);
const UNREACHABLE_FAILURE_THRESHOLD: u32 = 2;
const WORKER_RESTART_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_RESTART_COOLDOWN: Duration = Duration::from_secs(60);
const SESSION_MANAGER_SHUTDOWN_GRACE: Duration = Duration::from_millis(750);

mod types;
pub use types::*;
mod recovery;
pub(crate) use recovery::*;
mod channels;
pub use channels::*;
mod handle;
pub use handle::*;
mod client_backend;
use client_backend::*;
mod actor_types;
use actor_types::*;
mod remote;
pub use remote::*;
mod spawn;
pub use spawn::*;
mod actor;
use actor::*;
mod standalone;
pub use standalone::*;

#[cfg(test)]
mod tests;
