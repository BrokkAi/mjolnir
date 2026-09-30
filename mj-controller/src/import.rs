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

mod named_session;
use named_session::{CLAUDE_STORE, CODEX_STORE, GROK_STORE, KIMI_STORE, NamedEntry, cannot_read};
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
pub(crate) mod claude;
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

/// Read only project metadata from the newest native sessions. Never imports
/// conversations, computes checkout sizes, or probes Git for this seed.
pub(crate) struct NativeProjectSeed {
    pub directories: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
}

pub(crate) fn recent_project_directories(
    harness: HarnessKind,
    home: &Path,
    limit: usize,
    executor: &impl crate::targets::CommandExecutor,
) -> Result<NativeProjectSeed> {
    ensure!(home.is_dir(), "profile home is missing: {}", home.display());
    let files = match harness {
        HarnessKind::Codex => {
            if let Some(sessions) = codex::codex_indexed_sessions_limited(home, limit)? {
                return Ok(NativeProjectSeed {
                    directories: sessions.into_iter().map(|session| session.cwd).collect(),
                    errors: Vec::new(),
                });
            }
            let mut candidates = Vec::new();
            codex::collect_codex_candidate_paths(
                &home.join("sessions"),
                0,
                &codex::codex_native_titles(home)?,
                &mut candidates,
            )?;
            candidates.sort_by(|a, b| {
                b.modified_at
                    .cmp(&a.modified_at)
                    .then_with(|| b.path.cmp(&a.path))
            });
            candidates
        }
        HarnessKind::Claude => claude::claude_candidates(home)?,
        HarnessKind::Kimi | HarnessKind::Grok => {
            let mut candidates = if harness == HarnessKind::Kimi {
                kimi::kimi_indexed_candidates(home, &home.join("sessions"))?
            } else {
                grok::grok_candidates(&home.join("sessions"))?
            };
            candidates.sort_by(|a, b| {
                b.modified_at
                    .cmp(&a.modified_at)
                    .then_with(|| b.native_session_id.cmp(&a.native_session_id))
            });
            return Ok(NativeProjectSeed {
                directories: candidates
                    .into_iter()
                    .take(limit)
                    .map(|candidate| candidate.cwd)
                    .collect(),
                errors: Vec::new(),
            });
        }
        HarnessKind::Muse => return muse::recent_directories(home, limit, executor),
    };
    let mut seed = NativeProjectSeed {
        directories: Vec::new(),
        errors: Vec::new(),
    };
    for candidate in files.into_iter().take(limit) {
        ensure!(
            !executor.cancellation_requested(),
            "project discovery cancelled"
        );
        match native_project_directory(&candidate.path, harness, executor) {
            Ok(cwd) => seed.directories.push(cwd),
            Err(error) => seed.errors.push((
                candidate.path.clone(),
                format!("{}: {error:#}", candidate.path.display()),
            )),
        }
    }
    Ok(seed)
}

/// Project metadata lives near the start of each log. Bound the read and stop
/// as soon as it is found, without parsing the conversation after it.
pub(crate) fn native_project_directory(
    path: &Path,
    harness: HarnessKind,
    executor: &impl crate::targets::CommandExecutor,
) -> Result<PathBuf> {
    use std::io::Read;
    let reader = BufReader::new(fs::File::open(path)?).take(1024 * 1024);
    for line in reader.lines() {
        ensure!(
            !executor.cancellation_requested(),
            "project discovery cancelled"
        );
        let line = line?;
        if !line.contains("cwd") && !line.contains("workspace_root") {
            continue;
        }
        let record: Value = serde_json::from_str(&line)?;
        let cwd = match harness {
            HarnessKind::Codex => record.get("payload").and_then(|payload| payload.get("cwd")),
            HarnessKind::Claude => record.get("cwd"),
            HarnessKind::Muse => record.pointer("/payload/record/workspace_root"),
            _ => None,
        }
        .and_then(Value::as_str);
        if let Some(cwd) = cwd {
            let cwd = PathBuf::from(cwd);
            ensure!(
                cwd.is_absolute(),
                "native project directory is not absolute"
            );
            return Ok(cwd);
        }
    }
    bail!("native log has no project directory in its metadata header")
}
