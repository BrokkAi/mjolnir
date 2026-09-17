//! Import native harness sessions into Hel's durable archive format.
//

mod muse;
#[cfg(test)]
mod native_tests;
#[cfg(test)]
pub(crate) mod test_fixtures;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde_json::{Value, json};

use crate::setup::{GithubRepository, github_repository_from_origin};
use mj_checkpoint::archive::{
    ArchiveInput, BundleManifest, GitCollectionSpec, GitHistoryMode, GitSnapshotProgress,
    SystemGit, TargetManifest, collect_git_snapshot_with_progress, write_archive_atomic,
};
use mj_checkpoint::checkpoint::collect_import_native_artifacts;

use mj_core::config::{
    Config, HarnessKind, ProjectBundle, ProjectRepository, TargetTemplate, validate_id,
};
use mj_core::local_git::main_worktree_root;
use mj_core::remote_git::resolve_repository;
use mj_core::state::{
    CheckpointMetadata, SessionRecord, SessionState, State, harness_session_title, new_session_id,
    normalize_session_title,
};
use mj_transcript::projection::canonical_session_from_materialized;

use crate::targets::ProcessExecutor;
use mj_core::relay::{SequencedEvent, WorkerEvent, strip_hidden_prompt_context};

mod types;
pub use types::*;
mod safety;
pub use safety::*;
mod native;
pub use native::*;
mod codex;
pub use codex::*;
mod kimi;
pub use kimi::*;
mod claude;
pub use claude::*;
mod transcripts;
pub use transcripts::*;
mod edit_targets;
pub use edit_targets::*;
mod bundles;
pub use bundles::*;
mod native_import;
pub use native_import::*;

mod grok;

#[cfg(test)]
use grok::grok_decode_cwd_dirname;
pub use grok::{list_grok_sessions, locate_grok_session, read_grok_transcript, scan_grok_sessions};

#[cfg(test)]
mod tests;
