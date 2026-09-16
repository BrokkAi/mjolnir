//! Where turn review actually runs.
//!
//! The review driver is a pure state machine (`mj_core::review::driver`); this is the
//! process that feeds it. It lives in the controller daemon, which is the one
//! process that owns every session whether or not anyone is watching: it pumps
//! each session's relay every 150 ms, it is the only SQLite writer, and it
//! hosts the phone server. That is why review lives here and not in a UI. A
//! review started from the terminal survives the terminal closing; a session
//! driven only from a phone is reviewed on the same terms; a session nobody is
//! attached to is reviewed too.
//!
//! Every surface is a projection: the terminal and the phone both render
//! [`RuntimeReviewView`] and both resolve a review by asking the host. Neither
//! owns any part of the review.
//!
//! Shape: one task owns all review state and processes [`HostEvent`]s in
//! order. Everything slow -- capturing a delta, staging a reviewer profile,
//! reading a role's journal -- happens in a spawned task that sends its result
//! back as another event. Nothing here holds a lock across an await, and no
//! two reviews can interleave their state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::database::TurnReviewState;
use crate::session_manager::{
    ManagedSessionHandle, ManagedSessionView, ReviewDeliveryAdmission, ReviewerAction,
    ReviewerOutcome, SessionManagerControl, new_command_id,
};
use mj_core::config::ReviewConfig;
use mj_core::state::{MaterializedExecutionState, MaterializedSession};

use mj_core::relay::{RelayCommand, RelayEvent, RelayObservation};

use mj_core::review::lanes::{ReviewTier, UserMessage};
use mj_core::review::verdict::ReviewVerdict;
use mj_review::driver::{
    INTENT_ROLE, PendingForward, Resolution, ReviewRequest, SUPERVISOR_ROLE, TurnReviewDriver,
    TurnReviewPhase, TurnReviewSeed,
};

pub use mj_client::review::{RuntimeReviewView, VerdictKind, VerdictView, role_session_id};

mod prompts;
pub use prompts::*;
mod environment;
pub use environment::*;
mod host;
pub use host::*;
mod events;
use events::*;
mod begin;
mod dispatch;
mod notices;
mod persist;
mod resolve;
mod roles;
mod slots;
use slots::*;
mod reviewer;
pub use reviewer::*;

#[cfg(test)]
mod tests;
