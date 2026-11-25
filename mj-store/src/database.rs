pub use mj_core::storage::*;
// Normalized controller state and composer history stored in SQLite.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    MaterializedTurnOutcome, OperatorPresence, ProjectionWindow, SessionRecord,
    SessionResourceAllocation, SessionState, State, TargetLocator, TranscriptBody, TranscriptItem,
    validate_relay_event_digest, validate_relay_event_frontier,
};
use mj_core::subagent::SubagentRecord;

use crate::targets::{AdditionalMount, MountAccess};
use mj_core::workspace::{
    ConversationLayout, DEFAULT_WORKSPACE_ID, DetachedDraft, PaneSize, PaneSizes, WorkspaceRecord,
    new_workspace_id, normalize_workspace_name,
};

const SCHEMA_VERSION: i64 = 157;

pub mod web_push;
pub use web_push::*;

mod ec2_grants;
pub use ec2_grants::*;

mod retired_harnesses;
pub use retired_harnesses::*;

mod environments;
pub use environments::*;

mod session_move;
pub use session_move::*;
mod session_create;
pub use session_create::*;
mod session_restart;
pub use session_restart::*;
mod github_watch;
pub use github_watch::*;
mod mailbox;
pub use mailbox::*;

mod legacy_schema;
mod schema;
mod turn_outcome;
use turn_outcome::{deserialize_turn_outcome, serialize_turn_outcome};
mod usage;
pub use usage::*;
mod usage_report;
pub use usage_report::load_session_usage_report;
mod subagent_questions;
pub use subagent_questions::reconcile_pending_subagent_questions;
mod events;
pub use events::*;

#[cfg(test)]
use schema::forget_verified_schema;
pub use schema::{check_read_compatibility, database_path};
use schema::{open, open_reader};

mod writer;
pub use writer::*;
mod committed;
pub use committed::{CommittedState, CommittedTurn};
mod workspaces;
pub use workspaces::*;
mod client_state;
pub use client_state::*;
mod operator_away;
mod state_io;
pub use state_io::*;
mod sessions;
pub use sessions::*;
mod native_agents;
pub use native_agents::*;
mod materialized;
pub use materialized::*;
mod mounts;
pub mod tool_details;
pub use mounts::*;
mod turn_reviews;
pub use turn_reviews::*;
mod review_delivery;
pub use review_delivery::partial_review_events;
mod startup;
pub use startup::StartFollowup;
pub use startup::*;
mod delegation;
pub use delegation::*;
mod worker_restart;
pub use worker_restart::*;
mod host_snapshot_cache;
pub use host_snapshot_cache::*;
mod prompts;
pub use prompts::*;
mod values;
use values::*;
mod config_results;
pub use config_results::*;
mod projects;
pub use projects::*;

#[doc(hidden)]
pub mod test_fixtures;
#[cfg(test)]
mod tests;
#[doc(hidden)]
pub fn test_session(id: &str, bundle: &str) -> SessionRecord {
    SessionRecord {
        oracle_choice: None,
        oracle: true,
        notifications_ignored: false,
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        workspace_id: DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        id: id.into(),
        title: "test session".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        staged_aws_profile: None,
        last_profile: "codex".into(),
        bundle_id: bundle.into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        no_project_memory: false,
        no_github_events: false,
        no_github_credentials: false,
        target_template_id: "local".into(),
        resource_allocation: Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 32 * 1024 * 1024 * 1024,
        }),
        additional_mounts: vec![AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            access: crate::targets::MountAccess::Cow,
        }],
        state: SessionState::Stopped,
        target: Some(Box::new(TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "container-1".into(),
            workspace_storage: Default::default(),
        })),
        native_session_id: Some("native-1".into()),
        acp_session_title: Some("Agent title".into()),
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T01:00:00Z".into(),
        viewed_through_event_ordinal: 7,
        draft_input: String::new(),
        last_error: None,
        dismissed_failure: None,
        failure_generation: 0,
        last_checkpoint_error: Some("temporary recovery failure".into()),
        checkpoint: Some(Box::new(CheckpointMetadata {
            archive_path: PathBuf::from("sessions/test.hel.zip"),
            sha256: "a".repeat(64),
            created_at: "2026-08-12T01:00:00Z".into(),
            event_frontier: 6,
        })),
    }
}

mod quota_cache;
pub use quota_cache::*;

/// Whether a failure is SQLite reporting another writer, which a later trigger
/// simply retries.
pub fn is_busy_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

mod container_size;
mod target_secrets;
pub use target_secrets::scrub_resolved_target_secrets;
