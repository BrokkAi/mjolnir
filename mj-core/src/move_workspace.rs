//! Retained Move source records from earlier releases.
//!
//! Milestone 5 retired Move's untracked-file inventory, picker, exclusions, and
//! rsync staging. `RetainedMoveSource` and `WorkspacePath` remain so
//! `mj move-sources` can still list and clean up sources retained by earlier
//! releases.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePath {
    pub repository: String,
    pub path: PathBuf,
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

/// A stopped source retained because the user excluded files from a legacy Move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedMoveSource {
    pub operation_id: String,
    pub session_id: String,
    pub source: crate::state::SessionRecord,
    pub exclusions: Vec<WorkspacePath>,
    pub created_at: String,
}
