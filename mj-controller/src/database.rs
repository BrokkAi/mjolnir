pub use mj_core::storage::*;
// Normalized controller state and composer history stored in SQLite.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use mj_core::config::data_dir;
use mj_core::state::{
    CheckpointMetadata, HostContainerSize, ManagedWorktree, MaterializedExecutionState,
    MaterializedQueuedPrompt, MaterializedSession, MaterializedSessionSummary, MaterializedTurn,
    MaterializedTurnOutcome, ProjectionWindow, SessionRecord, SessionResourceAllocation,
    SessionState, State, TargetLocator, TranscriptBody, TranscriptItem,
    validate_relay_event_digest, validate_relay_event_frontier,
};
use mj_core::subagent::SubagentRecord;

use crate::targets::{AdditionalMount, MountAccess};
use mj_core::workspace::{
    ConversationLayout, DEFAULT_WORKSPACE_ID, DetachedDraft, PaneSize, PaneSizes, WorkspaceRecord,
    new_workspace_id, normalize_workspace_name,
};

const SCHEMA_VERSION: i64 = 44;

mod session_move;
pub use session_move::*;

mod schema;
mod usage;
pub use usage::*;
mod events;
pub use events::*;

pub use schema::database_path;
#[cfg(test)]
use schema::forget_verified_schema;
use schema::{open, open_reader};

mod writer;
pub use writer::*;
mod workspaces;
pub use workspaces::*;
mod client_state;
pub use client_state::*;
mod state_io;
pub use state_io::*;
mod sessions;
pub use sessions::*;
mod native_agents;
pub use native_agents::*;
mod materialized;
pub use materialized::*;
mod mounts;
pub use mounts::*;
mod reviews;
pub use reviews::*;
mod prompts;
pub use prompts::*;
mod values;
use values::*;
mod profile_cache;
pub use profile_cache::*;

#[cfg(test)]
mod tests;

mod quota_cache;
pub(crate) use quota_cache::*;
