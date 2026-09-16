//! Background feeds for the control surfaces.
//!
//! Everything here runs off the event loop and reports back over a channel:
//! harness quota refreshes, worker session polling, per-session resource and
//! deployment capacity probes, credential-sync scheduling, and the one-shot
//! tasks that recover interrupted closes. The loop that consumes them never
//! blocks; see [`Feed`] for the wait-then-drain shape they all share.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mj_core::clock::epoch_seconds;
use mj_core::config::Config;
use mj_core::credentials::{
    CredentialSyncCause, CredentialSyncHandle, CredentialSyncReason, CredentialSyncSignal,
    CredentialSyncTarget,
};
use mj_core::state::{
    ManagedSessionSnapshot, MaterializedSession, SessionRecord, SessionResourceAllocation,
    SessionState, State,
};

use crate::controller::Controller;
use crate::quota::{QuotaManager, QuotaRefreshOutcome, QuotaRefreshRequest};
use crate::recovery::{RecoveryCoordinator, RecoveryResult};
use crate::session_manager::{
    ManagedSessionView, RelaySessionTarget, RemoteSessionRequest, SessionManagerControl,
    SessionManagerShutdown, SessionManagerUpdate, SessionManagerUpdates, ViewError,
    spawn_remote_session_manager,
};
use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec,
    DeploymentCapacityKind, DeploymentCapacityTarget, DeploymentCapacityUsage, ImageRefresh,
    SessionResourceProbe, SessionResourceUsage,
};
use crate::worker_client::CredentialSyncCoordinator;

use crate::daemon;
use mj_core::state::short_id;
use mj_core::subagent::SubagentRecord;

#[cfg(test)]
mod runtime_feed_tests;

pub const QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// When a quota reading stops counting as current. A reading only goes stale
/// once a scheduled refresh should already have replaced it, so this is
/// derived from the refresh interval rather than chosen next to it: a shorter
/// threshold would label every healthy quota "stale" for part of every cycle.
/// The extra interval is slack for a refresh that is itself still running.
pub const QUOTA_STALE_AFTER: Duration = Duration::from_secs(2 * QUOTA_REFRESH_INTERVAL.as_secs());
/// How often the daemon looks for a newer copy of every container image its
/// targets use. Launches no longer pull, so this is what makes a remote
/// `:latest` tag current, and it has to be rare enough to stay off the
/// registry's back.
pub const IMAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The daemon has startup work of its own, and a pull competes with it for the
/// network. The first refresh waits this long, then the interval takes over.
const IMAGE_REFRESH_DELAY: Duration = Duration::from_secs(30);
pub const RESOURCE_POLL_INTERVAL: Duration = Duration::from_secs(60);
const RESOURCE_POLL_TIMEOUT: Duration = Duration::from_secs(15);
pub const CAPACITY_POLL_INTERVAL: Duration = Duration::from_secs(30);

mod feed;
pub use feed::*;
mod types;
pub use types::*;
mod quota;
pub use quota::*;
mod worker_targets;
pub use worker_targets::*;
mod credential_sync;
pub use credential_sync::*;
mod resources;
pub use resources::*;
mod capacity;
pub use capacity::*;
mod runtime_feed;
pub use runtime_feed::*;
mod remote;
pub use remote::*;
mod lifecycle;
pub use lifecycle::*;

#[cfg(test)]
mod tests;
