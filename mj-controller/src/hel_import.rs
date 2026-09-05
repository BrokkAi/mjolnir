//! Import native harness sessions into Hel's durable archive format.
//

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

use crate::hel_setup::{GithubRepository, github_repository_from_origin};
use hel::hel_archive::{
    ArchiveInput, BundleManifest, GitCollectionSpec, GitHistoryMode, GitSnapshotProgress,
    SystemGit, TargetManifest, collect_git_snapshot_with_progress, write_archive_atomic,
};
use hel::hel_checkpoint::{collect_import_native_artifacts, collect_native_artifacts};
use hel::hel_config::{
    HarnessKind, HelConfig, ProjectBundle, ProjectRepository, TargetTemplate, validate_id,
};
use hel::hel_local_git::main_worktree_root;
use hel::hel_projection::canonical_session_from_materialized;
use hel::hel_state::{
    CheckpointMetadata, HelState, SessionRecord, SessionState, harness_session_title,
    new_session_id, normalize_session_title,
};
use hel::hel_worker::{SequencedEvent, WorkerEvent, strip_hidden_prompt_context};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeSessionSelection {
    NativeSessionId(String),
    Latest,
}

pub type CodexSessionSelection = ClaudeSessionSelection;
pub type KimiSessionSelection = ClaudeSessionSelection;
pub type GrokSessionSelection = ClaudeSessionSelection;

#[derive(Debug, Clone)]
pub struct LocatedClaudeSession {
    pub native_session_id: String,
    pub jsonl_path: PathBuf,
    pub modified_at: SystemTime,
    pub title: String,
    pub cwd: PathBuf,
    pub git_branch: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHistoryMode {
    Legacy,
    Paginated,
}

/// Grok Build's conversation of record inside a session directory.
const CHAT_HISTORY: &str = "chat_history.jsonl";

pub const CODEX_LEGACY_IMPORT_ISSUE: &str = "Legacy Codex history cannot be imported. Run codex migrate-rollouts --apply, then reopen \
     this dialog.";

impl CodexHistoryMode {
    pub fn import_issue(self) -> Option<&'static str> {
        match self {
            Self::Legacy => Some(CODEX_LEGACY_IMPORT_ISSUE),
            Self::Paginated => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocatedCodexSession {
    pub native_session_id: String,
    pub jsonl_path: PathBuf,
    pub modified_at: SystemTime,
    pub title: String,
    pub cwd: PathBuf,
    pub git_branch: String,
    pub size_bytes: u64,
    pub history_mode: CodexHistoryMode,
    /// Archived inside Codex itself. Hel mirrors that one way: the row is
    /// hidden by default and never written back to Codex.
    pub natively_archived: bool,
}

#[derive(Debug, Clone)]
pub struct LocatedKimiSession {
    pub native_session_id: String,
    pub session_path: PathBuf,
    pub modified_at: SystemTime,
    pub title: String,
    pub cwd: PathBuf,
    pub git_branch: String,
    pub size_bytes: u64,
}

/// Grok Build keeps one directory per session, like Kimi Code.
pub type LocatedGrokSession = LocatedKimiSession;

#[derive(Debug, Clone)]
pub struct SessionScanProgress<T> {
    pub scanned: usize,
    pub total: usize,
    pub session: Option<T>,
}

#[derive(Debug)]
struct FileScanCandidate {
    path: PathBuf,
    modified_at: SystemTime,
    size_bytes: u64,
}

#[derive(Debug)]
struct KimiScanCandidate {
    native_session_id: String,
    session_path: PathBuf,
    modified_at: SystemTime,
    title: String,
    cwd: PathBuf,
}

#[derive(Debug)]
struct CodexSessionMetadata {
    id: String,
    cwd: PathBuf,
    git_branch: String,
    history_mode: CodexHistoryMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTranscript {
    pub cwd: PathBuf,
    /// Files reliably reported as edited by the native harness.
    pub edited_paths: Vec<PathBuf>,
    pub events: Vec<SequencedEvent>,
}

pub type CodexTranscript = ClaudeTranscript;
pub type KimiTranscript = ClaudeTranscript;
pub type GrokTranscript = ClaudeTranscript;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleResolution {
    Existing(String),
    /// The caller must ask the user before adding this to their config.
    Synthesized {
        id: String,
        bundle: ProjectBundle,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEditTargets {
    pub git_roots: Vec<PathBuf>,
    /// Git roots under a temporary directory. They are throwaway workspaces
    /// rather than project repositories, so the import omits them.
    pub scratch_git_roots: Vec<PathBuf>,
    pub non_git_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSafetyIssues {
    pub dirty_git_roots: Vec<(PathBuf, String)>,
    pub omitted_non_git_dirs: Vec<PathBuf>,
    pub scratch_git_roots: Vec<PathBuf>,
    pub has_untracked_files: bool,
}

pub fn import_safety_issues(targets: &SessionEditTargets) -> Result<ImportSafetyIssues> {
    let mut dirty_git_roots = Vec::new();
    let mut has_untracked_files = false;
    for root in &targets.git_roots {
        let output = Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=normal"])
            .current_dir(root)
            .output()
            .with_context(|| format!("inspect Git status in {}", root.display()))?;
        ensure!(
            output.status.success(),
            "could not inspect Git status in {}",
            root.display()
        );
        let (tracked, untracked) = String::from_utf8_lossy(&output.stdout).lines().fold(
            (0_usize, 0_usize),
            |(tracked, untracked), line| {
                if line.starts_with("??") {
                    (tracked, untracked + 1)
                } else {
                    (tracked + 1, untracked)
                }
            },
        );
        has_untracked_files |= untracked > 0;
        if tracked + untracked > 0 {
            let mut parts = Vec::new();
            if tracked > 0 {
                parts.push(format!(
                    "{tracked} tracked change{}",
                    if tracked == 1 { "" } else { "s" }
                ));
            }
            if untracked > 0 {
                parts.push(format!(
                    "{untracked} untracked path{}",
                    if untracked == 1 { "" } else { "s" }
                ));
            }
            dirty_git_roots.push((root.clone(), parts.join(" · ")));
        }
    }
    Ok(ImportSafetyIssues {
        dirty_git_roots,
        omitted_non_git_dirs: targets.non_git_dirs.clone(),
        scratch_git_roots: targets.scratch_git_roots.clone(),
        has_untracked_files,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedClaudeSession {
    pub session_id: String,
    pub native_session_id: String,
    pub source_jsonl: PathBuf,
    pub source_cwd: PathBuf,
    pub bundle_id: String,
    pub archive_path: PathBuf,
}

pub type ImportedCodexSession = ImportedClaudeSession;
pub type ImportedKimiSession = ImportedClaudeSession;
pub type ImportedGrokSession = ImportedClaudeSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportArchiveProgress {
    Repository {
        current: usize,
        total: usize,
        id: String,
    },
    UntrackedFile {
        repository_id: String,
        current: usize,
        total: usize,
        path: PathBuf,
    },
    WritingArchive,
}

pub struct ImportControl<'a> {
    pub cancelled: &'a AtomicBool,
    pub progress: &'a (dyn Fn(ImportArchiveProgress) + Sync),
    pub include_untracked: bool,
}

impl ImportControl<'_> {
    fn check_cancelled(&self) -> Result<()> {
        ensure!(!self.cancelled.load(Ordering::Acquire), "import cancelled");
        Ok(())
    }

    fn report(&self, progress: ImportArchiveProgress) -> Result<()> {
        self.check_cancelled()?;
        (self.progress)(progress);
        Ok(())
    }
}

pub struct ClaudeImportRequest<'a> {
    pub claude_home: &'a Path,
    pub source: &'a LocatedClaudeSession,
    pub transcript: &'a ClaudeTranscript,
    pub bundle_id: &'a str,
    pub profile_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub archive_directory: &'a Path,
}

pub struct CodexImportRequest<'a> {
    pub codex_home: &'a Path,
    pub source: &'a LocatedCodexSession,
    pub transcript: &'a CodexTranscript,
    pub bundle_id: &'a str,
    pub profile_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub archive_directory: &'a Path,
}

pub struct KimiImportRequest<'a> {
    pub kimi_home: &'a Path,
    pub source: &'a LocatedKimiSession,
    pub transcript: &'a KimiTranscript,
    pub bundle_id: &'a str,
    pub profile_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub archive_directory: &'a Path,
}

pub struct GrokImportRequest<'a> {
    pub grok_home: &'a Path,
    pub source: &'a LocatedGrokSession,
    pub transcript: &'a GrokTranscript,
    pub bundle_id: &'a str,
    pub profile_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub archive_directory: &'a Path,
}

/// Resolve a harness's configuration home without ever modifying it.
///
/// The environment override wins; otherwise the harness's default directory
/// beneath the user's home is used, the same pair `mj setup` discovers.
pub fn harness_config_home(kind: HarnessKind) -> Result<PathBuf> {
    let name = kind.display_name();
    let home = std::env::var_os(kind.home_env())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(kind.default_home_leaf())))
        .with_context(|| format!("cannot determine {name} home; set {}", kind.home_env()))?;
    ensure!(
        home.is_dir(),
        "{name} home is not a directory: {}",
        home.display()
    );
    Ok(home)
}

/// Resolve the Claude configuration home without ever modifying it.
pub fn claude_config_home() -> Result<PathBuf> {
    harness_config_home(HarnessKind::Claude)
}

/// Resolve the Codex configuration home without ever modifying it.
pub fn codex_config_home() -> Result<PathBuf> {
    harness_config_home(HarnessKind::Codex)
}

/// Resolve the Kimi Code configuration home without ever modifying it.
pub fn kimi_config_home() -> Result<PathBuf> {
    harness_config_home(HarnessKind::Kimi)
}

/// Resolve the Grok Build configuration home without ever modifying it.
pub fn grok_config_home() -> Result<PathBuf> {
    harness_config_home(HarnessKind::Grok)
}

/// One native session located on disk, normalized across harnesses: the id
/// `session/load` takes and the file or directory its transcript is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedNativeSession {
    pub native_session_id: String,
    pub source_path: PathBuf,
}

/// One native session as a picker lists it, normalized across harnesses.
#[derive(Debug, Clone)]
pub struct NativeSessionListing {
    pub native_session_id: String,
    pub title: String,
    pub modified_at: SystemTime,
    pub git_branch: String,
    pub size_bytes: u64,
    pub cwd: PathBuf,
    /// Why this session cannot be imported, when it cannot be.
    pub unavailable_reason: Option<&'static str>,
    /// Archived inside the harness itself. Only Codex reports this today.
    pub natively_archived: bool,
}

/// Locate one native session for any harness.
pub fn locate_native_session(
    harness: HarnessKind,
    home: &Path,
    selection: &ClaudeSessionSelection,
) -> Result<LocatedNativeSession> {
    let (native_session_id, source_path) = match harness {
        HarnessKind::Codex => {
            let located = locate_codex_session(home, selection)?;
            (located.native_session_id, located.jsonl_path)
        }
        HarnessKind::Claude => {
            let located = locate_claude_session(home, selection)?;
            (located.native_session_id, located.jsonl_path)
        }
        HarnessKind::Kimi => {
            let located = locate_kimi_session(home, selection)?;
            (located.native_session_id, located.session_path)
        }
        HarnessKind::Grok => {
            let located = locate_grok_session(home, selection)?;
            (located.native_session_id, located.session_path)
        }
        HarnessKind::Deepseek => bail!(
            "DeepSeek Harness sessions resume through ACP and are not imported from native storage"
        ),
    };
    Ok(LocatedNativeSession {
        native_session_id,
        source_path,
    })
}

/// Project one native session into the canonical transcript, for any harness.
pub fn read_native_transcript(
    harness: HarnessKind,
    source_path: &Path,
) -> Result<ClaudeTranscript> {
    match harness {
        HarnessKind::Codex => read_codex_transcript(source_path),
        HarnessKind::Claude => read_claude_transcript(source_path),
        HarnessKind::Kimi => read_kimi_transcript(source_path),
        HarnessKind::Grok => read_grok_transcript(source_path),
        HarnessKind::Deepseek => bail!(
            "DeepSeek Harness sessions resume through ACP and have no Mjolnir native-import projection"
        ),
    }
}

/// Scan a harness home newest first, reporting after every candidate.
pub fn scan_native_sessions(
    harness: HarnessKind,
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<NativeSessionListing>),
) -> Result<()> {
    let mut forward = |scanned, total, session| {
        report(SessionScanProgress {
            scanned,
            total,
            session,
        });
    };
    match harness {
        HarnessKind::Codex => scan_codex_sessions(home, |progress| {
            let session = progress.session.map(|session| NativeSessionListing {
                unavailable_reason: session.history_mode.import_issue(),
                native_session_id: session.native_session_id,
                title: session.title,
                modified_at: session.modified_at,
                git_branch: session.git_branch,
                size_bytes: session.size_bytes,
                cwd: session.cwd,
                natively_archived: session.natively_archived,
            });
            forward(progress.scanned, progress.total, session);
        }),
        HarnessKind::Claude => scan_claude_sessions(home, |progress| {
            let session = progress.session.map(|session| NativeSessionListing {
                native_session_id: session.native_session_id,
                title: session.title,
                modified_at: session.modified_at,
                git_branch: session.git_branch,
                size_bytes: session.size_bytes,
                cwd: session.cwd,
                unavailable_reason: None,
                natively_archived: false,
            });
            forward(progress.scanned, progress.total, session);
        }),
        HarnessKind::Kimi => scan_kimi_sessions(home, |progress| {
            let session = progress.session.map(|session| NativeSessionListing {
                native_session_id: session.native_session_id,
                title: session.title,
                modified_at: session.modified_at,
                git_branch: session.git_branch,
                size_bytes: session.size_bytes,
                cwd: session.cwd,
                unavailable_reason: None,
                natively_archived: false,
            });
            forward(progress.scanned, progress.total, session);
        }),
        HarnessKind::Grok => scan_grok_sessions(home, |progress| {
            let session = progress.session.map(|session| NativeSessionListing {
                native_session_id: session.native_session_id,
                title: session.title,
                modified_at: session.modified_at,
                git_branch: session.git_branch,
                size_bytes: session.size_bytes,
                cwd: session.cwd,
                unavailable_reason: None,
                natively_archived: false,
            });
            forward(progress.scanned, progress.total, session);
        }),
        HarnessKind::Deepseek => bail!(
            "DeepSeek Harness sessions resume through ACP and are not imported from native storage"
        ),
    }
}

/// Locate a Codex rollout exposed by its native interactive resume picker.
pub fn locate_codex_session(
    home: &Path,
    selection: &CodexSessionSelection,
) -> Result<LocatedCodexSession> {
    let mut listed = list_codex_sessions(home)?;
    // `--latest` follows Codex's own default view, which hides what the user
    // archived there. Asking for an id by name still finds it.
    if matches!(selection, CodexSessionSelection::Latest) {
        listed.retain(|session| !session.natively_archived);
    }
    if let CodexSessionSelection::NativeSessionId(session_id) = selection
        && !listed
            .iter()
            .any(|session| session.native_session_id == *session_id)
    {
        return locate_unindexed_codex_session(home, session_id);
    }
    select_jsonl_session(listed, selection, "Codex")
}

fn locate_unindexed_codex_session(home: &Path, session_id: &str) -> Result<LocatedCodexSession> {
    validate_id("Codex session", session_id)?;
    let mut requested = BTreeMap::new();
    requested.insert(session_id.to_owned(), session_id.to_owned());
    let mut candidates = Vec::new();
    let root = home.join("sessions");
    if root.is_dir() {
        collect_codex_candidate_paths(&root, &requested, &mut candidates)?;
    }
    let titles = codex_native_titles(home)?;
    let mut matches = Vec::new();
    for candidate in candidates {
        let Some(metadata) = codex_session_metadata(&candidate.path)? else {
            continue;
        };
        if metadata.id == session_id {
            matches.push(LocatedCodexSession {
                natively_archived: false,
                title: titles
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| session_id.to_owned()),
                native_session_id: metadata.id,
                jsonl_path: candidate.path,
                modified_at: candidate.modified_at,
                cwd: metadata.cwd,
                git_branch: metadata.git_branch,
                size_bytes: candidate.size_bytes,
                history_mode: metadata.history_mode,
            });
        }
    }
    select_jsonl_session(
        matches,
        &CodexSessionSelection::NativeSessionId(session_id.to_owned()),
        "Codex",
    )
}

/// List native Codex sessions newest first.
pub fn list_codex_sessions(home: &Path) -> Result<Vec<LocatedCodexSession>> {
    let mut sessions = Vec::new();
    scan_codex_sessions(home, |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Codex sessions newest first, reporting after every candidate file.
pub fn scan_codex_sessions(
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<LocatedCodexSession>),
) -> Result<()> {
    if let Some(sessions) = codex_indexed_sessions(home)? {
        let total = sessions.len();
        report(SessionScanProgress {
            scanned: 0,
            total,
            session: None,
        });
        for (index, session) in sessions.into_iter().enumerate() {
            report(SessionScanProgress {
                scanned: index + 1,
                total,
                session: Some(session),
            });
        }
        return Ok(());
    }

    // Native Codex only indexes threads with a non-empty preview/name. Its
    // history and session-name index provide the same compact set of IDs,
    // avoiding an expensive parse of every exec and subagent rollout.
    let titles = codex_native_titles(home)?;
    let mut candidates = Vec::new();
    let root = home.join("sessions");
    if root.is_dir() {
        collect_codex_candidate_paths(&root, &titles, &mut candidates)?;
    }
    candidates.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.path.cmp(&left.path))
    });
    let total = candidates.len();
    report(SessionScanProgress {
        scanned: 0,
        total,
        session: None,
    });
    for (index, candidate) in candidates.into_iter().enumerate() {
        let session = codex_session_metadata(&candidate.path)?.map(|metadata| {
            let session_id = metadata.id;
            LocatedCodexSession {
                natively_archived: false,
                title: titles
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_else(|| session_id.clone()),
                native_session_id: session_id,
                jsonl_path: candidate.path,
                modified_at: candidate.modified_at,
                cwd: metadata.cwd,
                git_branch: metadata.git_branch,
                size_bytes: candidate.size_bytes,
                history_mode: metadata.history_mode,
            }
        });
        report(SessionScanProgress {
            scanned: index + 1,
            total,
            session,
        });
    }
    Ok(())
}

fn codex_indexed_sessions(home: &Path) -> Result<Option<Vec<LocatedCodexSession>>> {
    let database = home.join("state_5.sqlite");
    if !database.is_file() {
        return Ok(None);
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let has_history_mode = connection
        .prepare("SELECT history_mode FROM threads LIMIT 0")
        .is_ok();
    let history_mode_column = if has_history_mode {
        "history_mode"
    } else {
        "'legacy'"
    };
    // Codex's own archived threads are listed too, flagged rather than
    // filtered: the resume dialog hides them until "show archived" is on, and
    // Hel never writes this database back.
    let query = format!(
        "SELECT id, rollout_path, updated_at, COALESCE(NULLIF(name, ''), NULLIF(title, ''), id), cwd, \
         COALESCE(NULLIF(git_branch, ''), 'HEAD'), {history_mode_column}, archived \
         FROM threads \
         WHERE source IN ('cli', 'vscode') \
           AND preview <> '' \
           AND rollout_path IS NOT NULL \
         ORDER BY updated_at DESC, id DESC"
    );
    let Ok(mut statement) = connection.prepare(&query) else {
        return Ok(None);
    };
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, bool>(7)?,
        ))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (session_id, path, updated_at, title, cwd, git_branch, history_mode, natively_archived) =
            row?;
        let path = PathBuf::from(path);
        if validate_id("Codex session", &session_id).is_err() || updated_at.is_negative() {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        sessions.push(LocatedCodexSession {
            native_session_id: session_id.clone(),
            jsonl_path: path,
            modified_at: SystemTime::UNIX_EPOCH + Duration::from_secs(updated_at as u64),
            title: normalize_session_title(&title).unwrap_or(session_id),
            cwd: PathBuf::from(cwd),
            git_branch,
            size_bytes: metadata.len(),
            history_mode: parse_codex_history_mode(&history_mode)?,
            natively_archived,
        });
    }
    Ok(Some(sessions))
}

fn collect_codex_candidate_paths(
    root: &Path,
    native_titles: &BTreeMap<String, String>,
    candidates: &mut Vec<FileScanCandidate>,
) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_codex_candidate_paths(&path, native_titles, candidates)?;
            continue;
        }
        if !metadata.is_file() || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }
        if let Some(session_id) = codex_rollout_id_from_path(&path)
            && !native_titles.contains_key(session_id)
        {
            continue;
        }
        candidates.push(FileScanCandidate {
            path,
            modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size_bytes: metadata.len(),
        });
    }
    Ok(())
}

fn codex_rollout_id_from_path(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.as_bytes().get(8) == Some(&b'-')
        && id.as_bytes().get(13) == Some(&b'-')
        && id.as_bytes().get(18) == Some(&b'-')
        && id.as_bytes().get(23) == Some(&b'-'))
    .then_some(id)
}

fn codex_session_metadata(path: &Path) -> Result<Option<CodexSessionMetadata>> {
    let file =
        fs::File::open(path).with_context(|| format!("open Codex session {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let record: Value = serde_json::from_str(&line)
            .with_context(|| format!("parse Codex session {}", path.display()))?;
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        if !codex_source_is_interactive(record.pointer("/payload/source")) {
            return Ok(None);
        }
        // Ephemeral Codex threads normally have no rollout path at all. Keep
        // this defensive check so a future writer cannot expose one here.
        if record
            .pointer("/payload/ephemeral")
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Ok(None);
        }
        // Codex ACP loads a rollout by its payload `id`, which is also the
        // UUID embedded in the rollout filename. `session_id` can name a
        // parent thread and therefore is not necessarily resumable itself.
        let id = record
            .pointer("/payload/id")
            .or_else(|| record.pointer("/payload/session_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned);
        if let Some(id) = id {
            validate_id("Codex session", &id)?;
            let cwd = record
                .pointer("/payload/cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_default();
            let git_branch = record
                .pointer("/payload/git/branch")
                .and_then(Value::as_str)
                .filter(|branch| !branch.trim().is_empty())
                .unwrap_or("HEAD")
                .to_owned();
            let history_mode = record
                .pointer("/payload/history_mode")
                .and_then(Value::as_str)
                .map(parse_codex_history_mode)
                .transpose()?
                .unwrap_or(CodexHistoryMode::Legacy);
            return Ok(Some(CodexSessionMetadata {
                id,
                cwd,
                git_branch,
                history_mode,
            }));
        }
    }
    Ok(None)
}

fn parse_codex_history_mode(value: &str) -> Result<CodexHistoryMode> {
    match value {
        "legacy" => Ok(CodexHistoryMode::Legacy),
        "paginated" => Ok(CodexHistoryMode::Paginated),
        other => bail!("unsupported Codex history mode {other:?}"),
    }
}

fn codex_source_is_interactive(source: Option<&Value>) -> bool {
    match source {
        // Older rollouts predate the source field and came from the TUI.
        None => true,
        Some(Value::String(source)) => matches!(source.as_str(), "cli" | "vscode"),
        // Structured sources identify subagents. Other unexpected shapes are
        // not sessions offered by the normal interactive resume picker.
        Some(_) => false,
    }
}

fn codex_native_titles(home: &Path) -> Result<BTreeMap<String, String>> {
    let mut titles = BTreeMap::new();
    // Older Codex stores use history only as their compact interactive-session
    // index. Keep those IDs discoverable, but do not turn prompt text into a
    // session name.
    let history = home.join("history.jsonl");
    if history.is_file() {
        for line in BufReader::new(fs::File::open(&history)?).lines() {
            let record: Value = serde_json::from_str(&line?)?;
            if let (Some(session_id), Some(text)) = (
                record.get("session_id").and_then(Value::as_str),
                record.get("text").and_then(Value::as_str),
            ) && !text.trim().is_empty()
            {
                titles
                    .entry(session_id.to_owned())
                    .or_insert_with(|| session_id.to_owned());
            }
        }
    }
    let index = home.join("session_index.jsonl");
    if index.is_file() {
        for line in BufReader::new(fs::File::open(&index)?).lines() {
            let record: Value = serde_json::from_str(&line?)?;
            if let (Some(session_id), Some(title)) = (
                record.get("id").and_then(Value::as_str),
                record.get("thread_name").and_then(Value::as_str),
            ) && let Some(title) = normalize_session_title(title)
            {
                titles.insert(session_id.to_owned(), title);
            }
        }
    }
    Ok(titles)
}

/// Locate a Kimi session directory. Its on-disk `session_<uuid>` name is the
/// native identifier required by Kimi ACP's `session/load`.
pub fn locate_kimi_session(
    home: &Path,
    selection: &KimiSessionSelection,
) -> Result<LocatedKimiSession> {
    let candidates = list_kimi_sessions(home)?;
    let sessions = home.join("sessions");
    match selection {
        KimiSessionSelection::NativeSessionId(native_session_id) => candidates
            .into_iter()
            .find(|candidate| candidate.native_session_id == *native_session_id)
            .with_context(|| {
                format!(
                    "Kimi session {native_session_id:?} was not found under {}",
                    sessions.display()
                )
            }),
        KimiSessionSelection::Latest => candidates
            .into_iter()
            .next()
            .context("no Kimi session directories were found"),
    }
}

/// List native Kimi sessions newest first.
pub fn list_kimi_sessions(home: &Path) -> Result<Vec<LocatedKimiSession>> {
    let mut sessions = Vec::new();
    scan_kimi_sessions(home, |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Kimi sessions newest first, reporting after every candidate directory.
pub fn scan_kimi_sessions(
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<LocatedKimiSession>),
) -> Result<()> {
    let sessions = home.join("sessions");
    ensure!(
        sessions.is_dir(),
        "Kimi sessions directory is missing: {}",
        sessions.display()
    );
    let mut candidates = kimi_indexed_candidates(home, &sessions)?;
    candidates.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.session_path.cmp(&left.session_path))
    });
    let total = candidates.len();
    report(SessionScanProgress {
        scanned: 0,
        total,
        session: None,
    });
    for (index, candidate) in candidates.into_iter().enumerate() {
        let size_bytes = directory_size(&candidate.session_path)?;
        let native_session_id = candidate.native_session_id;
        let session = LocatedKimiSession {
            title: candidate.title,
            native_session_id,
            session_path: candidate.session_path,
            modified_at: candidate.modified_at,
            git_branch: git_branch_or_head(&candidate.cwd),
            size_bytes,
            cwd: candidate.cwd,
        };
        report(SessionScanProgress {
            scanned: index + 1,
            total,
            session: Some(session),
        });
    }
    Ok(())
}

mod grok;

#[cfg(test)]
use grok::grok_decode_cwd_dirname;
pub use grok::{list_grok_sessions, locate_grok_session, read_grok_transcript, scan_grok_sessions};

fn kimi_indexed_candidates(home: &Path, sessions: &Path) -> Result<Vec<KimiScanCandidate>> {
    let index_path = home.join("session_index.jsonl");
    if !index_path.is_file() {
        return Ok(Vec::new());
    }

    let mut indexed = BTreeMap::<String, (PathBuf, PathBuf)>::new();
    for line in BufReader::new(fs::File::open(&index_path)?).lines() {
        let Ok(record) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let Some(session_id) = record
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session_id| !session_id.is_empty())
        else {
            continue;
        };
        if record.get("deleted").and_then(Value::as_bool) == Some(true) {
            indexed.remove(session_id);
            continue;
        }
        let (Some(session_path), Some(work_dir)) = (
            record
                .get("sessionDir")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            record
                .get("workDir")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        ) else {
            continue;
        };
        if validate_id("Kimi session", session_id).is_err()
            || !session_path.is_absolute()
            || session_path.file_name().and_then(|name| name.to_str()) != Some(session_id)
        {
            continue;
        }
        indexed.insert(session_id.to_owned(), (session_path, work_dir));
    }

    let sessions = sessions.canonicalize()?;
    let mut candidates = Vec::new();
    for (native_session_id, (session_path, indexed_work_dir)) in indexed {
        let Ok(metadata) = fs::symlink_metadata(&session_path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Ok(canonical_session_path) = session_path.canonicalize() else {
            continue;
        };
        if !canonical_session_path.starts_with(&sessions) {
            continue;
        }
        let Some((title, cwd, archived)) =
            kimi_state_listing_metadata(&canonical_session_path, &indexed_work_dir)?
        else {
            continue;
        };
        if archived {
            continue;
        }
        candidates.push(KimiScanCandidate {
            native_session_id,
            modified_at: kimi_session_modified_at(&canonical_session_path, &metadata),
            session_path: canonical_session_path,
            title,
            cwd,
        });
    }
    Ok(candidates)
}

fn kimi_state_listing_metadata(
    session_path: &Path,
    indexed_work_dir: &Path,
) -> Result<Option<(String, PathBuf, bool)>> {
    let state_path = session_path.join("state.json");
    let state = if state_path.is_file() {
        match serde_json::from_slice::<Value>(&fs::read(&state_path)?) {
            Ok(state) => state,
            Err(_) => return Ok(None),
        }
    } else {
        Value::Object(Default::default())
    };
    let string = |key: &str| {
        state
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let title = if state.get("isCustomTitle").is_some_and(Value::is_boolean) {
        string("title")
    } else {
        string("customTitle").or_else(|| string("title"))
    }
    .and_then(normalize_session_title);
    let cwd = string("workDir")
        .or_else(|| string("cwd"))
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_absolute())
        .or_else(|| {
            indexed_work_dir
                .is_absolute()
                .then(|| indexed_work_dir.to_path_buf())
        })
        .unwrap_or_default();
    let title = title.unwrap_or_else(|| {
        session_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled session")
            .to_owned()
    });
    Ok(Some((
        title,
        cwd,
        state
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )))
}

fn kimi_session_modified_at(session_path: &Path, metadata: &fs::Metadata) -> SystemTime {
    let mut modified_at = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let mut consider = |path: &Path| {
        if let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) {
            modified_at = modified_at.max(modified);
        }
    };
    consider(&session_path.join("state.json"));
    consider(&session_path.join("wire.jsonl"));
    let agents = session_path.join("agents");
    if let Ok(entries) = fs::read_dir(agents) {
        for entry in entries.flatten() {
            consider(&entry.path().join("wire.jsonl"));
        }
    }
    modified_at
}

fn select_jsonl_session(
    candidates: Vec<LocatedCodexSession>,
    selection: &CodexSessionSelection,
    harness: &str,
) -> Result<LocatedCodexSession> {
    match selection {
        CodexSessionSelection::NativeSessionId(native_session_id) => {
            validate_id(&format!("{harness} session"), native_session_id)?;
            candidates
                .into_iter()
                .filter(|candidate| candidate.native_session_id == *native_session_id)
                .max_by(|left, right| {
                    left.modified_at
                        .cmp(&right.modified_at)
                        .then_with(|| left.jsonl_path.cmp(&right.jsonl_path))
                })
                .with_context(|| format!("{harness} session {native_session_id:?} was not found"))
        }
        CodexSessionSelection::Latest => candidates
            .into_iter()
            .max_by(|left, right| {
                left.modified_at
                    .cmp(&right.modified_at)
                    .then_with(|| left.jsonl_path.cmp(&right.jsonl_path))
            })
            .context("no session JSONL files were found"),
    }
}

/// Locate one native Claude rollout. `Latest` compares modified time across
/// every immediate project directory, exactly as Claude's layout requires.
pub fn locate_claude_session(
    home: &Path,
    selection: &ClaudeSessionSelection,
) -> Result<LocatedClaudeSession> {
    let candidates = list_claude_sessions(home)?;
    let projects = home.join("projects");
    match selection {
        ClaudeSessionSelection::NativeSessionId(native_session_id) => {
            validate_id("Claude session", native_session_id)?;
            let mut matches = candidates
                .into_iter()
                .filter(|candidate| candidate.native_session_id == *native_session_id)
                .collect::<Vec<_>>();
            if matches.is_empty() {
                matches = locate_unlisted_claude_sessions(home, native_session_id)?;
            }
            match matches.len() {
                0 => bail!(
                    "Claude session {native_session_id:?} was not found under {}",
                    projects.display()
                ),
                1 => Ok(matches.remove(0)),
                _ => bail!(
                    "Claude session {native_session_id:?} occurs in multiple project directories"
                ),
            }
        }
        ClaudeSessionSelection::Latest => candidates
            .into_iter()
            .next()
            .context("no Claude session JSONL files were found"),
    }
}

fn locate_unlisted_claude_sessions(
    home: &Path,
    native_session_id: &str,
) -> Result<Vec<LocatedClaudeSession>> {
    let projects = home.join("projects");
    let mut matches = Vec::new();
    for project in fs::read_dir(&projects)? {
        let project = project?;
        let project_path = project.path();
        let project_metadata = fs::symlink_metadata(&project_path)?;
        if project_metadata.file_type().is_symlink() || !project_metadata.is_dir() {
            continue;
        }
        let path = project_path.join(format!("{native_session_id}.jsonl"));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let Some((title, cwd, git_branch)) = claude_native_metadata(&path)? else {
            continue;
        };
        matches.push(LocatedClaudeSession {
            native_session_id: native_session_id.to_owned(),
            jsonl_path: path,
            modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            title,
            cwd,
            git_branch,
            size_bytes: metadata.len(),
        });
    }
    Ok(matches)
}

/// List native Claude sessions newest first.
pub fn list_claude_sessions(home: &Path) -> Result<Vec<LocatedClaudeSession>> {
    let mut sessions = Vec::new();
    scan_claude_sessions(home, |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Claude sessions newest first, reporting after every candidate file.
pub fn scan_claude_sessions(
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<LocatedClaudeSession>),
) -> Result<()> {
    let projects = home.join("projects");
    ensure!(
        projects.is_dir(),
        "Claude projects directory is missing: {}",
        projects.display()
    );
    let mut candidates = Vec::new();
    for project in fs::read_dir(&projects)
        .with_context(|| format!("read Claude projects directory {}", projects.display()))?
    {
        let project = project?;
        let project_path = project.path();
        let project_metadata = fs::symlink_metadata(&project_path)?;
        if project_metadata.file_type().is_symlink() || !project_metadata.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&project_path)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(session_id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if session_id.is_empty() {
                continue;
            }
            candidates.push(FileScanCandidate {
                path,
                modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size_bytes: metadata.len(),
            });
        }
    }

    candidates.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.path.cmp(&left.path))
    });
    let total = candidates.len();
    report(SessionScanProgress {
        scanned: 0,
        total,
        session: None,
    });
    let mut visible = 0_usize;
    for (index, candidate) in candidates.into_iter().enumerate() {
        if visible == 50 {
            report(SessionScanProgress {
                scanned: index + 1,
                total,
                session: None,
            });
            continue;
        }
        let session_id = candidate
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
            .expect("Claude candidates were validated during enumeration")
            .to_owned();
        let metadata = match claude_native_metadata(&candidate.path) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => {
                report(SessionScanProgress {
                    scanned: index + 1,
                    total,
                    session: None,
                });
                continue;
            }
            Err(_) => (session_id.clone(), PathBuf::new(), "HEAD".to_owned()),
        };
        let (title, cwd, git_branch) = metadata;
        visible += 1;
        report(SessionScanProgress {
            scanned: index + 1,
            total,
            session: Some(LocatedClaudeSession {
                native_session_id: session_id,
                jsonl_path: candidate.path,
                modified_at: candidate.modified_at,
                title,
                cwd,
                git_branch,
                size_bytes: candidate.size_bytes,
            }),
        });
    }
    Ok(())
}

fn claude_native_metadata(path: &Path) -> Result<Option<(String, PathBuf, String)>> {
    let mut custom_title = None;
    let mut agent_name = None;
    let mut ai_title = None;
    let mut cwd = None;
    let mut git_branch = None;
    let mut entrypoint = None;
    let mut filtered = false;
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let record: Value = serde_json::from_str(&line?)?;
        if record
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || record
                .get("teamName")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty())
            || record.get("sessionKind").and_then(Value::as_str) == Some("daemon-worker")
        {
            filtered = true;
        }
        if entrypoint.is_none() {
            entrypoint = record
                .get("entrypoint")
                .and_then(Value::as_str)
                .filter(|entrypoint| !entrypoint.trim().is_empty())
                .map(str::to_owned);
        }
        if cwd.is_none() {
            cwd = record
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.trim().is_empty())
                .map(PathBuf::from);
        }
        if git_branch.is_none() {
            git_branch = record
                .get("gitBranch")
                .and_then(Value::as_str)
                .filter(|branch| !branch.trim().is_empty())
                .map(str::to_owned);
        }
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                if let Some(native_title) = record
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    custom_title = normalize_session_title(native_title);
                }
            }
            Some("ai-title") => {
                if let Some(native_title) = record
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    ai_title = normalize_session_title(native_title);
                }
            }
            Some("agent-name") => {
                agent_name = record
                    .get("agentName")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                    .and_then(normalize_session_title);
            }
            Some("user") => {
                let content = record.pointer("/message/content").and_then(Value::as_str);
                if content
                    .is_some_and(|content| content.contains("<command-name>/loop</command-name>"))
                {
                    filtered = true;
                }
            }
            _ => {}
        }
    }
    // Claude's native resume picker is for interactive CLI conversations. In
    // particular, its print/SDK entrypoints include the tiny rollouts created
    // by `claude -p /usage`, which must not displace real sessions here.
    if filtered || entrypoint.as_deref().is_some_and(|value| value != "cli") {
        return Ok(None);
    }
    let cwd = cwd.with_context(|| format!("Claude session {} has no cwd", path.display()))?;
    Ok(Some((
        custom_title
            .or(agent_name)
            .or(ai_title)
            .unwrap_or_else(|| "Untitled session".into()),
        cwd,
        git_branch.unwrap_or_else(|| "HEAD".into()),
    )))
}

fn git_branch_or_head(cwd: &Path) -> String {
    if cwd.as_os_str().is_empty() {
        return "HEAD".into();
    }
    git_optional_text(cwd, ["branch", "--show-current"])
        .ok()
        .flatten()
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "HEAD".into())
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut size = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() {
            size = size.saturating_add(metadata.len());
        } else if metadata.is_dir() && !metadata.file_type().is_symlink() {
            size = size.saturating_add(directory_size(&entry.path())?);
        }
    }
    Ok(size)
}

/// Read the native JSONL only far enough to recover a transcript suitable for
/// Hel's chat view. Full tool traffic and reasoning remain in the copied
/// native rollout, not in this lossy projection.
pub fn read_claude_transcript(path: &Path) -> Result<ClaudeTranscript> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("read Claude session {}", path.display()))?;
    let mut cwd = None;
    let mut events = Vec::new();
    let mut saw_raw_user = false;

    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!("parse Claude session {} line {}", path.display(), index + 1)
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        if cwd.is_none() {
            cwd = record
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.trim().is_empty())
                .map(PathBuf::from);
        }
        if record.get("isMeta").and_then(Value::as_bool) == Some(true)
            || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let compaction_boundary = record.get("type").and_then(Value::as_str) == Some("system")
            && matches!(
                record.get("subtype").and_then(Value::as_str),
                Some("compact_boundary" | "compaction")
            );
        let compaction_summary = record
            .get("isCompactSummary")
            .or_else(|| record.pointer("/message/isCompactSummary"))
            .and_then(Value::as_bool)
            == Some(true);
        if compaction_boundary || compaction_summary {
            ensure!(
                saw_raw_user,
                "Claude session contains a compaction artifact before recoverable raw history"
            );
            continue;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(text) = record
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .map(strip_hidden_prompt_context)
                    .filter(|text| !text.trim().is_empty())
                else {
                    continue;
                };
                let request_id = format!("import-{}", events.len() + 1);
                push_event(
                    &mut events,
                    recorded_at_ms,
                    WorkerEvent::PromptAccepted {
                        request_id,
                        text: text.to_owned(),
                        attachments: Vec::new(),
                    },
                );
                saw_raw_user = true;
            }
            Some("assistant") => {
                let Some(content) = record.pointer("/message/content").and_then(Value::as_array)
                else {
                    continue;
                };
                for block in content {
                    let Some(text) = block
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    else {
                        continue;
                    };
                    if block.get("type").and_then(Value::as_str) != Some("text") {
                        continue;
                    }
                    push_event(
                        &mut events,
                        recorded_at_ms,
                        WorkerEvent::Adapter {
                            kind: "session_update".into(),
                            payload: json!({
                                "type": "session_update",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": text},
                                },
                            }),
                        },
                    );
                }
                // Claude marks a completed model response independently of
                // its text/tool blocks. Preserve that lifecycle boundary so
                // the restored durable worker is idle and accepts the next
                // user prompt instead of treating the imported turn as live.
                if matches!(
                    record
                        .pointer("/message/stop_reason")
                        .and_then(Value::as_str),
                    Some("end_turn" | "stop_sequence")
                ) {
                    push_event(&mut events, recorded_at_ms, WorkerEvent::TurnCompleted);
                }
            }
            _ => {}
        }
    }

    let cwd = cwd.context("Claude session does not declare its original cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Claude session cwd is not absolute: {}",
        cwd.display()
    );
    finalize_import_event_times(&mut events, path)?;
    let edited_paths = claude_edited_paths(path)?;
    Ok(ClaudeTranscript {
        cwd,
        edited_paths,
        events,
    })
}

/// Project a Codex rollout into the canonical transcript used by Hel chat.
pub fn read_codex_transcript(path: &Path) -> Result<CodexTranscript> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("read Codex session {}", path.display()))?;
    let mut cwd = None;
    let mut history_mode = None;
    let mut events = Vec::new();
    let mut edited_paths = BTreeSet::new();
    let mut saw_user = false;
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!("parse Codex session {} line {}", path.display(), index + 1)
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            if cwd.is_none() {
                cwd = record
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .filter(|cwd| !cwd.trim().is_empty())
                    .map(PathBuf::from);
            }
            if history_mode.is_none() {
                history_mode = Some(
                    record
                        .pointer("/payload/history_mode")
                        .and_then(Value::as_str)
                        .map(parse_codex_history_mode)
                        .transpose()?
                        .unwrap_or(CodexHistoryMode::Legacy),
                );
            }
            continue;
        }
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        if record.pointer("/payload/type").and_then(Value::as_str) == Some("item_completed")
            && record.pointer("/payload/item/type").and_then(Value::as_str) == Some("FileChange")
            && record
                .pointer("/payload/item/status")
                .and_then(Value::as_str)
                == Some("completed")
            && let Some(changes) = record
                .pointer("/payload/item/changes")
                .and_then(Value::as_object)
        {
            edited_paths.extend(changes.keys().map(PathBuf::from));
        }
        match record.pointer("/payload/type").and_then(Value::as_str) {
            Some("item_completed")
                if record.pointer("/payload/item/type").and_then(Value::as_str)
                    == Some("UserMessage") =>
            {
                let Some(text) = codex_completed_item_text(&record) else {
                    continue;
                };
                let text = strip_hidden_prompt_context(&text);
                if text.trim().is_empty() {
                    continue;
                }
                finish_imported_turn(&mut events, None);
                let request_id = format!("import-{}", events.len() + 1);
                push_event(
                    &mut events,
                    recorded_at_ms,
                    WorkerEvent::PromptAccepted {
                        request_id,
                        text: text.to_owned(),
                        attachments: Vec::new(),
                    },
                );
                saw_user = true;
            }
            Some("item_completed")
                if record.pointer("/payload/item/type").and_then(Value::as_str)
                    == Some("AgentMessage") =>
            {
                let Some(text) = codex_completed_item_text(&record) else {
                    continue;
                };
                push_event(
                    &mut events,
                    recorded_at_ms,
                    WorkerEvent::Adapter {
                        kind: "session_update".into(),
                        payload: json!({
                            "type": "session_update",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": text},
                            },
                        }),
                    },
                );
            }
            Some("turn_complete" | "turn_aborted") => {
                finish_imported_turn(&mut events, recorded_at_ms)
            }
            _ => {}
        }
    }
    ensure!(
        history_mode == Some(CodexHistoryMode::Paginated),
        "{CODEX_LEGACY_IMPORT_ISSUE}"
    );
    ensure!(
        saw_user,
        "Codex paginated session contains no importable user messages"
    );
    finish_imported_turn(&mut events, None);
    let cwd = cwd.context("Codex session does not declare its original cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Codex session cwd is not absolute: {}",
        cwd.display()
    );
    finalize_import_event_times(&mut events, path)?;
    Ok(CodexTranscript {
        cwd,
        edited_paths: edited_paths.into_iter().collect(),
        events,
    })
}

fn codex_completed_item_text(record: &Value) -> Option<String> {
    let parts = record
        .pointer("/payload/item/content")?
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Project a Kimi session directory. The main wire stream contains prompts and
/// generated text; tool traffic and thought blocks stay only in native files.
pub fn read_kimi_transcript(session_path: &Path) -> Result<KimiTranscript> {
    let state_path = session_path.join("state.json");
    let state: Value = serde_json::from_slice(&fs::read(&state_path)?)
        .with_context(|| format!("parse Kimi session state {}", state_path.display()))?;
    let cwd = state
        .get("workDir")
        .or_else(|| state.get("cwd"))
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
        .map(PathBuf::from)
        .context("Kimi session state does not declare workDir or cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Kimi session workDir is not absolute: {}",
        cwd.display()
    );
    let wire_path = session_path.join("agents/main/wire.jsonl");
    let body = fs::read_to_string(&wire_path)
        .with_context(|| format!("read Kimi wire stream {}", wire_path.display()))?;
    let mut events = Vec::new();
    let mut saw_raw_user = false;
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "parse Kimi wire stream {} line {}",
                wire_path.display(),
                index + 1
            )
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        if matches!(
            record.get("type").and_then(Value::as_str),
            Some("context.compaction" | "context.compacted" | "compaction")
        ) {
            ensure!(
                saw_raw_user,
                "Kimi session contains a compaction artifact before recoverable raw history"
            );
            continue;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("turn.prompt" | "turn.steer")
                if record.pointer("/origin/kind").and_then(Value::as_str) == Some("user") =>
            {
                finish_imported_turn(&mut events, None);
                let text = record
                    .pointer("/input")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = strip_hidden_prompt_context(&text);
                if !text.trim().is_empty() {
                    let request_id = format!("import-{}", events.len() + 1);
                    push_event(
                        &mut events,
                        recorded_at_ms,
                        WorkerEvent::PromptAccepted {
                            request_id,
                            text: text.to_owned(),
                            attachments: Vec::new(),
                        },
                    );
                    saw_raw_user = true;
                }
            }
            Some("context.append_loop_event")
                if record.pointer("/event/type").and_then(Value::as_str)
                    == Some("content.part")
                    && record.pointer("/event/part/type").and_then(Value::as_str)
                        == Some("text") =>
            {
                let Some(text) = record
                    .pointer("/event/part/text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                push_event(
                    &mut events,
                    recorded_at_ms,
                    WorkerEvent::Adapter {
                        kind: "session_update".into(),
                        payload: json!({
                            "type": "session_update",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": text},
                            },
                        }),
                    },
                );
            }
            _ => {}
        }
    }
    finish_imported_turn(&mut events, None);
    finalize_import_event_times(&mut events, &wire_path)?;
    let edited_paths = kimi_edited_paths(session_path)?;
    Ok(KimiTranscript {
        cwd,
        edited_paths,
        events,
    })
}

fn claude_edited_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![path.to_path_buf()];
    if let (Some(parent), Some(session_id)) = (
        path.parent(),
        path.file_stem().and_then(|value| value.to_str()),
    ) {
        let subagents = parent.join(session_id).join("subagents");
        if subagents.is_dir() {
            collect_files_named(&subagents, "jsonl", &mut files)?;
        }
    }
    let mut edited = BTreeSet::new();
    for file in files {
        let body = fs::read_to_string(&file)?;
        let mut calls = BTreeMap::<String, PathBuf>::new();
        let mut completed = BTreeSet::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let record: Value = serde_json::from_str(line)?;
            if record.get("type").and_then(Value::as_str) == Some("file-history-delta") {
                let Some(tracking) = record.get("trackingPath").and_then(Value::as_str) else {
                    continue;
                };
                let tracking = PathBuf::from(tracking);
                let path = if tracking.is_absolute() {
                    tracking
                } else if let Some(parent) = record
                    .pointer("/backup/realParentDir")
                    .and_then(Value::as_str)
                {
                    PathBuf::from(parent).join(
                        tracking
                            .file_name()
                            .expect("non-empty tracking path has a file name"),
                    )
                } else {
                    tracking
                };
                edited.insert(path);
            }
            if record.get("type").and_then(Value::as_str) == Some("assistant") {
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use")
                        || !matches!(
                            block.get("name").and_then(Value::as_str),
                            Some("Edit" | "Write" | "NotebookEdit")
                        )
                    {
                        continue;
                    }
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(path) = block
                        .pointer("/input/file_path")
                        .or_else(|| block.pointer("/input/notebook_path"))
                        .or_else(|| block.pointer("/input/path"))
                        .and_then(Value::as_str)
                    {
                        calls.insert(id.to_owned(), PathBuf::from(path));
                    }
                }
            }
            if record.get("type").and_then(Value::as_str) == Some("user") {
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("is_error").and_then(Value::as_bool) != Some(true)
                        && let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                    {
                        completed.insert(id.to_owned());
                    }
                }
            }
        }
        edited.extend(
            calls
                .into_iter()
                .filter(|(id, _)| completed.contains(id))
                .map(|(_, path)| path),
        );
    }
    Ok(edited.into_iter().collect())
}

fn kimi_edited_paths(session_path: &Path) -> Result<Vec<PathBuf>> {
    let agents = session_path.join("agents");
    if !agents.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_files_named(&agents, "jsonl", &mut files)?;
    let mut edited = BTreeSet::new();
    for file in files {
        let body = fs::read_to_string(file)?;
        let mut calls = BTreeMap::<String, PathBuf>::new();
        let mut completed = BTreeSet::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let record: Value = serde_json::from_str(line)?;
            if record.get("type").and_then(Value::as_str) != Some("context.append_loop_event") {
                continue;
            }
            let event = &record["event"];
            if event.get("type").and_then(Value::as_str) == Some("tool.call")
                && matches!(
                    event.get("name").and_then(Value::as_str),
                    Some("Edit" | "Write")
                )
                && let (Some(id), Some(path)) = (
                    event.get("toolCallId").and_then(Value::as_str),
                    event
                        .pointer("/args/path")
                        .or_else(|| event.pointer("/args/file_path"))
                        .and_then(Value::as_str),
                )
            {
                calls.insert(id.to_owned(), PathBuf::from(path));
            }
            if event.get("type").and_then(Value::as_str) == Some("tool.result")
                && event.pointer("/result/isError").and_then(Value::as_bool) != Some(true)
                && let Some(id) = event.get("toolCallId").and_then(Value::as_str)
            {
                completed.insert(id.to_owned());
            }
        }
        edited.extend(
            calls
                .into_iter()
                .filter(|(id, _)| completed.contains(id))
                .map(|(_, path)| path),
        );
    }
    Ok(edited.into_iter().collect())
}

fn collect_files_named(root: &Path, extension: &str, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_files_named(&path, extension, output)?;
        } else if metadata.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some(extension)
        {
            output.push(path);
        }
    }
    Ok(())
}

fn finish_imported_turn(events: &mut Vec<SequencedEvent>, recorded_at_ms: Option<i64>) {
    if !events.is_empty()
        && !matches!(
            events.last().map(|event| &event.event),
            Some(WorkerEvent::TurnCompleted)
        )
    {
        push_event(events, recorded_at_ms, WorkerEvent::TurnCompleted);
    }
}

fn push_event(events: &mut Vec<SequencedEvent>, recorded_at_ms: Option<i64>, event: WorkerEvent) {
    events.push(SequencedEvent {
        seq: events.len() as u64 + 1,
        recorded_at_ms,
        request_id: None,
        event,
    });
}

fn native_recorded_at_ms(record: &Value) -> Option<i64> {
    record
        .get("timestamp")
        .or_else(|| record.get("time"))
        .and_then(Value::as_str)
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp_millis())
}

/// Native streams predate Hel's durable event clock in some harness versions.
/// Preserve their record timestamps when available; otherwise use the source
/// artifact's modification time. Clamping regressions keeps the imported
/// sequence and its activity watermark monotonic even if the native clock
/// moved backwards while the session was being recorded.
fn finalize_import_event_times(events: &mut [SequencedEvent], source_path: &Path) -> Result<()> {
    let Some(first) = events.first() else {
        return Ok(());
    };
    let mut last_recorded_at_ms = match events.iter().find_map(|event| event.recorded_at_ms) {
        Some(recorded_at_ms) => recorded_at_ms,
        None => DateTime::<Utc>::from(
            fs::metadata(source_path)
                .with_context(|| format!("stat import source {}", source_path.display()))?
                .modified()
                .with_context(|| format!("read import source mtime {}", source_path.display()))?,
        )
        .timestamp_millis(),
    };
    last_recorded_at_ms = first
        .recorded_at_ms
        .unwrap_or(last_recorded_at_ms)
        .max(last_recorded_at_ms);
    for event in events {
        last_recorded_at_ms = event
            .recorded_at_ms
            .unwrap_or(last_recorded_at_ms)
            .max(last_recorded_at_ms);
        event.recorded_at_ms = Some(last_recorded_at_ms);
    }
    Ok(())
}

pub fn session_edit_targets(
    transcript: &ClaudeTranscript,
    profile_home: &Path,
) -> Result<SessionEditTargets> {
    session_edit_targets_with_scratch_prefixes(transcript, profile_home, &scratch_prefixes())
}

/// Directories whose repositories are throwaway workspaces rather than
/// projects. A session that writes into one of them is still anchored on its
/// own repository.
fn scratch_prefixes() -> Vec<PathBuf> {
    let mut prefixes = Vec::new();
    let mut remember = |path: PathBuf| {
        let path = fs::canonicalize(&path).unwrap_or(path);
        if !prefixes.contains(&path) {
            prefixes.push(path);
        }
    };
    remember(std::env::temp_dir());
    for literal in ["/tmp", "/var/tmp", "/dev/shm"] {
        remember(PathBuf::from(literal));
    }
    prefixes
}

fn session_edit_targets_with_scratch_prefixes(
    transcript: &ClaudeTranscript,
    profile_home: &Path,
    scratch_prefixes: &[PathBuf],
) -> Result<SessionEditTargets> {
    let profile_home =
        fs::canonicalize(profile_home).unwrap_or_else(|_| profile_home.to_path_buf());
    let mut paths = transcript
        .edited_paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                transcript.cwd.join(path)
            }
        })
        .filter(|path| {
            let comparable = canonicalize_existing_ancestor(path);
            !comparable.starts_with(&profile_home)
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        paths.push(transcript.cwd.clone());
    }

    let cwd_root = git_root_for_path(&transcript.cwd)?.with_context(|| {
        format!(
            "session cwd is not in a usable Git worktree: {}",
            transcript.cwd.display()
        )
    })?;
    // The session's own repository is authoritative even when it lives under a
    // temporary directory.
    let mut git_roots = BTreeSet::from([cwd_root.clone()]);
    let mut scratch_git_roots = BTreeSet::new();
    let mut non_git_dirs = BTreeSet::new();
    for path in paths {
        if let Some(root) = git_root_for_path(&path)? {
            if root != cwd_root && is_scratch_root(&root, scratch_prefixes) {
                scratch_git_roots.insert(root);
            } else {
                git_roots.insert(root);
            }
        } else {
            non_git_dirs.insert(edited_directory(&path));
        }
    }
    Ok(SessionEditTargets {
        git_roots: git_roots.into_iter().collect(),
        scratch_git_roots: scratch_git_roots.into_iter().collect(),
        non_git_dirs: non_git_dirs.into_iter().collect(),
    })
}

fn is_scratch_root(root: &Path, scratch_prefixes: &[PathBuf]) -> bool {
    scratch_prefixes
        .iter()
        .any(|prefix| root.starts_with(prefix))
}

fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(existing) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(name) = existing.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
}

fn edited_directory(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    }
}

fn git_root_for_path(path: &Path) -> Result<Option<PathBuf>> {
    let mut probe = edited_directory(path);
    while !probe.is_dir() {
        if !probe.pop() {
            return Ok(None);
        }
    }
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&probe)
        .output()
        .with_context(|| format!("start git in {}", probe.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let root = String::from_utf8(output.stdout).context("decode Git repository root")?;
    let root = PathBuf::from(root.trim());
    Ok(Some(fs::canonicalize(&root).unwrap_or(root)))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RepositoryIdentity {
    Github(String, String),
    Local(PathBuf),
}

fn root_identity(root: &Path) -> Result<RepositoryIdentity> {
    let origin = git_optional_text(root, ["remote", "get-url", "origin"])?;
    if let Some(github) = origin.as_deref().and_then(github_repository_from_origin) {
        return Ok(RepositoryIdentity::Github(
            github.owner.to_ascii_lowercase(),
            github.repository.to_ascii_lowercase(),
        ));
    }
    // A linked worktree shares the identity of its main working tree.
    let root = main_worktree_root(root)?;
    Ok(RepositoryIdentity::Local(
        fs::canonicalize(&root).unwrap_or(root),
    ))
}

fn configured_repository_identity(repository: &ProjectRepository) -> Option<RepositoryIdentity> {
    if let Some(source) = repository.github.as_deref() {
        let github = github_repository_from_origin(source)?;
        return Some(RepositoryIdentity::Github(
            github.owner.to_ascii_lowercase(),
            github.repository.to_ascii_lowercase(),
        ));
    }
    repository.local.as_ref().map(|path| {
        RepositoryIdentity::Local(fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
    })
}

/// Reuse an exact configured bundle or synthesize one from all detected roots.
pub fn resolve_bundle(
    config: &HelConfig,
    cwd: &Path,
    targets: &SessionEditTargets,
    requested_bundle: Option<&str>,
) -> Result<BundleResolution> {
    let cwd_root = git_root_for_path(cwd)?.context("session cwd is not in a Git worktree")?;
    // A linked worktree stands in for its main repository, so bundles are named
    // after and point at the main working tree.
    let cwd_root = main_worktree_root(&cwd_root)?;
    let primary_identity = root_identity(&cwd_root)?;
    let detected = targets
        .git_roots
        .iter()
        .map(|root| root_identity(root))
        .collect::<Result<BTreeSet<_>>>()?;

    if let Some(bundle_id) = requested_bundle {
        let bundle = config
            .bundles
            .get(bundle_id)
            .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
        ensure!(
            bundle_matches(bundle, &detected, &primary_identity),
            "bundle {bundle_id:?} does not exactly match the session's edited Git roots and cwd primary repository"
        );
        return Ok(BundleResolution::Existing(bundle_id.to_owned()));
    }
    if let Some(id) = config.bundles.iter().find_map(|(id, bundle)| {
        bundle_matches(bundle, &detected, &primary_identity).then(|| id.clone())
    }) {
        return Ok(BundleResolution::Existing(id));
    }

    let primary_name = cwd_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository");
    let bundle_id = unique_bundle_id(config, &setup_style_id(primary_name));
    let mut used_ids = BTreeSet::new();
    let mut repositories = Vec::new();
    let mut primary_repo = None;
    let mut roots = targets
        .git_roots
        .iter()
        .map(|root| main_worktree_root(root))
        .collect::<Result<Vec<_>>>()?;
    roots.sort_by_key(|root| root != &cwd_root);
    // Checkouts and worktrees of one repository share an identity. Keep the
    // first root per identity so the cwd repository stays primary.
    let mut used_identities = BTreeSet::new();
    for root in roots {
        if !used_identities.insert(root_identity(&root)?) {
            continue;
        }
        let base = setup_style_id(
            root.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repository"),
        );
        let mut id = base.clone();
        for suffix in 2_u32.. {
            if used_ids.insert(id.clone()) {
                break;
            }
            id = format!("{base}-{suffix}");
        }
        if root == cwd_root {
            primary_repo = Some(id.clone());
        }
        let origin = git_optional_text(&root, ["remote", "get-url", "origin"])?;
        let github = origin
            .as_deref()
            .and_then(github_repository_from_origin)
            .map(|source| format!("{}/{}", source.owner, source.repository));
        repositories.push(ProjectRepository {
            id: id.clone(),
            local: github.is_none().then_some(root),
            github,
            destination: PathBuf::from(id),
            git_ref: None,
        });
    }
    Ok(BundleResolution::Synthesized {
        id: bundle_id,
        bundle: ProjectBundle {
            primary_repo: primary_repo.context("detected roots omitted the cwd repository")?,
            repositories,
        },
    })
}

fn bundle_matches(
    bundle: &ProjectBundle,
    detected: &BTreeSet<RepositoryIdentity>,
    primary: &RepositoryIdentity,
) -> bool {
    let identities = bundle
        .repositories
        .iter()
        .filter_map(configured_repository_identity)
        .collect::<BTreeSet<_>>();
    identities.len() == bundle.repositories.len()
        && &identities == detected
        && bundle
            .primary()
            .and_then(configured_repository_identity)
            .as_ref()
            == Some(primary)
}

/// Return the matching configured bundle for an origin. It accepts setup's
/// `owner/repository` shorthand as well as normal GitHub remote URLs.
pub fn configured_bundle_for_origin(
    config: &HelConfig,
    origin: &GithubRepository,
) -> Option<String> {
    config.bundles.iter().find_map(|(id, bundle)| {
        let primary = bundle.primary()?;
        let configured = github_repository_from_origin(primary.github.as_deref()?)?;
        same_github_repository(&configured, origin).then(|| id.clone())
    })
}

pub fn configured_bundle_for_local(config: &HelConfig, local: &Path) -> Option<String> {
    let local = fs::canonicalize(local).unwrap_or_else(|_| local.to_path_buf());
    config.bundles.iter().find_map(|(id, bundle)| {
        let configured = bundle.primary()?.local.as_ref()?;
        let configured = fs::canonicalize(configured).unwrap_or_else(|_| configured.to_path_buf());
        (configured == local).then(|| id.clone())
    })
}

fn same_github_repository(left: &GithubRepository, right: &GithubRepository) -> bool {
    left.owner.eq_ignore_ascii_case(&right.owner)
        && left.repository.eq_ignore_ascii_case(&right.repository)
}

pub(crate) fn setup_style_id(value: &str) -> String {
    let mut id = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .take(64)
        .collect::<String>();
    if id.is_empty() || matches!(id.as_str(), "." | "..") {
        id = "repository".into();
    }
    id
}

pub(crate) fn unique_bundle_id(config: &HelConfig, base: &str) -> String {
    if !config.bundles.contains_key(base) {
        return base.into();
    }
    let base = format!("import-{base}");
    if !config.bundles.contains_key(&base) {
        return base;
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}-{suffix}");
        if !config.bundles.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 bundle suffixes are finite")
}

/// Build, verify, and install a local archive, then update the in-memory state.
/// The caller saves `state` only after this returns successfully.
pub fn import_claude_session(
    config: &HelConfig,
    state: &mut HelState,
    request: ClaudeImportRequest<'_>,
) -> Result<ImportedClaudeSession> {
    import_claude_session_inner(config, state, request, None)
}

pub fn import_claude_session_with_control(
    config: &HelConfig,
    state: &mut HelState,
    request: ClaudeImportRequest<'_>,
    control: &ImportControl<'_>,
) -> Result<ImportedClaudeSession> {
    import_claude_session_inner(config, state, request, Some(control))
}

fn import_claude_session_inner(
    config: &HelConfig,
    state: &mut HelState,
    request: ClaudeImportRequest<'_>,
    control: Option<&ImportControl<'_>>,
) -> Result<ImportedClaudeSession> {
    let ClaudeImportRequest {
        claude_home,
        source,
        transcript,
        bundle_id,
        profile_id,
        title,
        archive_directory,
    } = request;
    let bundle = config
        .bundles
        .get(bundle_id)
        .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
    let session_title_override = title.map(str::to_owned);
    let title = match session_title_override.as_deref() {
        Some(title) if !title.trim().is_empty() => title.to_owned(),
        Some(_) => bail!("import title must not be empty"),
        None => harness_session_title(&transcript.events)
            .unwrap_or_else(|| format!("Imported Claude session {}", source.native_session_id)),
    };
    let targets = session_edit_targets(transcript, claude_home)?;
    let repositories = collect_local_repositories(bundle, &targets.git_roots, control)?;
    let native_artifacts = collect_native_artifacts(
        HarnessKind::Claude,
        claude_home,
        &source.native_session_id,
        false,
    )?;
    let session_id = new_session_id()?;
    let canonical_session =
        canonical_import_session(&session_id, &transcript.events, &source.jsonl_path)?;
    let timestamp = timestamp();
    let profile_id = import_profile_id(config, profile_id, HarnessKind::Claude, claude_home)?;
    let target_id = default_import_target_id(config);
    let raw_project = raw_project_import(config, &targets);
    let archive_path = archive_directory.join(format!("{session_id}.hel.zip"));
    if let Some(control) = control {
        control.report(ImportArchiveProgress::WritingArchive)?;
    }
    let verified = write_archive_atomic(
        &archive_path,
        &ArchiveInput {
            session: hel::hel_archive::SessionManifest {
                id: session_id.clone(),
                title: title.clone(),
                harness_kind: HarnessKind::Claude,
                profile_id: profile_id.clone(),
                native_session_id: source.native_session_id.clone(),
                created_at: timestamp.clone(),
                checkpointed_at: timestamp.clone(),
                hel_version: env!("CARGO_PKG_VERSION").into(),
                relay_version: env!("CARGO_PKG_VERSION").into(),
                adapter_version: "acp-v1".into(),
            },
            target: TargetManifest {
                template_id: target_id.clone(),
                target_kind: "import".into(),
                details: BTreeMap::from([("source".into(), "claude-import".into())]),
            },
            bundle: BundleManifest {
                id: bundle_id.to_owned(),
                primary_repository: bundle.primary_repo.clone(),
            },
            canonical_session,
            native_artifacts,
            repositories,
        },
    )?;
    if let Some(control) = control
        && let Err(error) = control.check_cancelled()
    {
        let _ = fs::remove_file(&archive_path);
        return Err(error);
    }
    let checkpoint = CheckpointMetadata {
        archive_path: archive_path.clone(),
        sha256: verified.archive_sha256,
        created_at: timestamp.clone(),
        event_frontier: transcript.events.last().map_or(0, |event| event.seq),
    };
    state.sessions.insert(
        session_id.clone(),
        SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.clone(),
            title,
            harness_kind: HarnessKind::Claude,
            last_profile: profile_id,
            bundle_id: bundle_id.to_owned(),
            project_directory: raw_project.as_ref().map(|(directory, _)| directory.clone()),
            managed_worktree: None,
            target_template_id: raw_project.map_or(target_id, |(_, raw_target_id)| raw_target_id),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Stopped,
            target: None,
            native_session_id: Some(source.native_session_id.clone()),
            acp_session_title: None,
            session_title_override,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: Some(checkpoint),
        },
    );
    Ok(ImportedClaudeSession {
        session_id,
        native_session_id: source.native_session_id.clone(),
        source_jsonl: source.jsonl_path.clone(),
        source_cwd: transcript.cwd.clone(),
        bundle_id: bundle_id.to_owned(),
        archive_path,
    })
}

pub fn import_codex_session(
    config: &HelConfig,
    state: &mut HelState,
    request: CodexImportRequest<'_>,
) -> Result<ImportedCodexSession> {
    import_codex_session_inner(config, state, request, None)
}

pub fn import_codex_session_with_control(
    config: &HelConfig,
    state: &mut HelState,
    request: CodexImportRequest<'_>,
    control: &ImportControl<'_>,
) -> Result<ImportedCodexSession> {
    import_codex_session_inner(config, state, request, Some(control))
}

fn import_codex_session_inner(
    config: &HelConfig,
    state: &mut HelState,
    request: CodexImportRequest<'_>,
    control: Option<&ImportControl<'_>>,
) -> Result<ImportedCodexSession> {
    let CodexImportRequest {
        codex_home,
        source,
        transcript,
        bundle_id,
        profile_id,
        title,
        archive_directory,
    } = request;
    import_native_session(
        config,
        state,
        NativeImportRequest {
            harness: HarnessKind::Codex,
            harness_home: codex_home,
            native_session_id: &source.native_session_id,
            source_path: &source.jsonl_path,
            transcript,
            bundle_id,
            profile_id,
            title,
            archive_directory,
        },
        control,
    )
}

pub fn import_grok_session(
    config: &HelConfig,
    state: &mut HelState,
    request: GrokImportRequest<'_>,
) -> Result<ImportedGrokSession> {
    import_grok_session_inner(config, state, request, None)
}

pub fn import_grok_session_with_control(
    config: &HelConfig,
    state: &mut HelState,
    request: GrokImportRequest<'_>,
    control: &ImportControl<'_>,
) -> Result<ImportedGrokSession> {
    import_grok_session_inner(config, state, request, Some(control))
}

fn import_grok_session_inner(
    config: &HelConfig,
    state: &mut HelState,
    request: GrokImportRequest<'_>,
    control: Option<&ImportControl<'_>>,
) -> Result<ImportedGrokSession> {
    let GrokImportRequest {
        grok_home,
        source,
        transcript,
        bundle_id,
        profile_id,
        title,
        archive_directory,
    } = request;
    import_native_session(
        config,
        state,
        NativeImportRequest {
            harness: HarnessKind::Grok,
            harness_home: grok_home,
            native_session_id: &source.native_session_id,
            source_path: &source.session_path,
            transcript,
            bundle_id,
            profile_id,
            title,
            archive_directory,
        },
        control,
    )
}

pub fn import_kimi_session(
    config: &HelConfig,
    state: &mut HelState,
    request: KimiImportRequest<'_>,
) -> Result<ImportedKimiSession> {
    import_kimi_session_inner(config, state, request, None)
}

pub fn import_kimi_session_with_control(
    config: &HelConfig,
    state: &mut HelState,
    request: KimiImportRequest<'_>,
    control: &ImportControl<'_>,
) -> Result<ImportedKimiSession> {
    import_kimi_session_inner(config, state, request, Some(control))
}

fn import_kimi_session_inner(
    config: &HelConfig,
    state: &mut HelState,
    request: KimiImportRequest<'_>,
    control: Option<&ImportControl<'_>>,
) -> Result<ImportedKimiSession> {
    let KimiImportRequest {
        kimi_home,
        source,
        transcript,
        bundle_id,
        profile_id,
        title,
        archive_directory,
    } = request;
    import_native_session(
        config,
        state,
        NativeImportRequest {
            harness: HarnessKind::Kimi,
            harness_home: kimi_home,
            native_session_id: &source.native_session_id,
            source_path: &source.session_path,
            transcript,
            bundle_id,
            profile_id,
            title,
            archive_directory,
        },
        control,
    )
}

pub struct NativeImportRequest<'a> {
    pub harness: HarnessKind,
    pub harness_home: &'a Path,
    pub native_session_id: &'a str,
    pub source_path: &'a Path,
    pub transcript: &'a ClaudeTranscript,
    pub bundle_id: &'a str,
    pub profile_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub archive_directory: &'a Path,
}

/// Import one already-located, already-parsed native session, for any harness.
/// The per-harness `import_*_session` wrappers are thin adapters over this.
pub fn import_native_session_with_control(
    config: &HelConfig,
    state: &mut HelState,
    request: NativeImportRequest<'_>,
    control: &ImportControl<'_>,
) -> Result<ImportedClaudeSession> {
    import_native_session(config, state, request, Some(control))
}

fn import_native_session(
    config: &HelConfig,
    state: &mut HelState,
    request: NativeImportRequest<'_>,
    control: Option<&ImportControl<'_>>,
) -> Result<ImportedClaudeSession> {
    let NativeImportRequest {
        harness,
        harness_home,
        native_session_id,
        source_path,
        transcript,
        bundle_id,
        profile_id,
        title,
        archive_directory,
    } = request;
    let bundle = config
        .bundles
        .get(bundle_id)
        .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
    let session_title_override = title.map(str::to_owned);
    let title = match session_title_override.as_deref() {
        Some(title) if !title.trim().is_empty() => title.to_owned(),
        Some(_) => bail!("import title must not be empty"),
        None => harness_session_title(&transcript.events).unwrap_or_else(|| {
            format!(
                "Imported {} session {native_session_id}",
                harness.display_name()
            )
        }),
    };
    let targets = session_edit_targets(transcript, harness_home)?;
    let repositories = collect_local_repositories(bundle, &targets.git_roots, control)?;
    let native_artifacts =
        collect_import_native_artifacts(harness, harness_home, native_session_id, source_path)?;
    let session_id = new_session_id()?;
    let canonical_session =
        canonical_import_session(session_id.as_str(), &transcript.events, source_path)?;
    let timestamp = timestamp();
    let profile_id = import_profile_id(config, profile_id, harness, harness_home)?;
    let target_id = default_import_target_id(config);
    let raw_project = raw_project_import(config, &targets);
    let archive_path = archive_directory.join(format!("{session_id}.hel.zip"));
    if let Some(control) = control {
        control.report(ImportArchiveProgress::WritingArchive)?;
    }
    let verified = write_archive_atomic(
        &archive_path,
        &ArchiveInput {
            session: hel::hel_archive::SessionManifest {
                id: session_id.clone(),
                title: title.clone(),
                harness_kind: harness,
                profile_id: profile_id.clone(),
                native_session_id: native_session_id.to_owned(),
                created_at: timestamp.clone(),
                checkpointed_at: timestamp.clone(),
                hel_version: env!("CARGO_PKG_VERSION").into(),
                relay_version: env!("CARGO_PKG_VERSION").into(),
                adapter_version: "acp-v1".into(),
            },
            target: TargetManifest {
                template_id: target_id.clone(),
                target_kind: "import".into(),
                details: BTreeMap::from([("source".into(), format!("{}-import", harness.id()))]),
            },
            bundle: BundleManifest {
                id: bundle_id.to_owned(),
                primary_repository: bundle.primary_repo.clone(),
            },
            canonical_session,
            native_artifacts,
            repositories,
        },
    )?;
    if let Some(control) = control
        && let Err(error) = control.check_cancelled()
    {
        let _ = fs::remove_file(&archive_path);
        return Err(error);
    }
    let checkpoint = CheckpointMetadata {
        archive_path: archive_path.clone(),
        sha256: verified.archive_sha256,
        created_at: timestamp.clone(),
        event_frontier: transcript.events.last().map_or(0, |event| event.seq),
    };
    state.sessions.insert(
        session_id.clone(),
        SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.clone(),
            title,
            harness_kind: harness,
            last_profile: profile_id,
            bundle_id: bundle_id.to_owned(),
            project_directory: raw_project.as_ref().map(|(directory, _)| directory.clone()),
            managed_worktree: None,
            target_template_id: raw_project.map_or(target_id, |(_, raw_target_id)| raw_target_id),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Stopped,
            target: None,
            native_session_id: Some(native_session_id.to_owned()),
            acp_session_title: None,
            session_title_override,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: Some(checkpoint),
        },
    );
    Ok(ImportedClaudeSession {
        session_id,
        native_session_id: native_session_id.to_owned(),
        source_jsonl: source_path.to_path_buf(),
        source_cwd: transcript.cwd.clone(),
        bundle_id: bundle_id.to_owned(),
        archive_path,
    })
}

fn default_import_target_id(config: &HelConfig) -> String {
    config
        .targets
        .get_key_value("podman")
        .map(|(id, _)| id)
        .or_else(|| {
            config.targets.iter().find_map(|(id, target)| {
                matches!(
                    target,
                    TargetTemplate::LocalPodman { .. } | TargetTemplate::LocalDocker { .. }
                )
                .then_some(id)
            })
        })
        .or_else(|| config.targets.keys().next())
        .cloned()
        .unwrap_or_else(|| "import".into())
}

/// Target that hosts raw project sessions on this machine.
fn raw_import_target_id(config: &HelConfig) -> Option<String> {
    let local_bare = |template: &TargetTemplate| matches!(template, TargetTemplate::LocalBare);
    config
        .targets
        .get_key_value("localhost")
        .filter(|(_, template)| local_bare(template))
        .map(|(id, _)| id.clone())
        .or_else(|| {
            config
                .targets
                .iter()
                .find_map(|(id, template)| local_bare(template).then(|| id.clone()))
        })
}

/// A session that only wrote to its own repository can keep working in that
/// directory, so import it as a raw project session instead of a bundle
/// session. `session_edit_targets` always records the cwd root, so a single
/// durable root is that root.
fn raw_project_import(
    config: &HelConfig,
    targets: &SessionEditTargets,
) -> Option<(PathBuf, String)> {
    let [cwd_root] = targets.git_roots.as_slice() else {
        return None;
    };
    Some((cwd_root.clone(), raw_import_target_id(config)?))
}

fn collect_local_repositories(
    bundle: &ProjectBundle,
    detected_roots: &[PathBuf],
    control: Option<&ImportControl<'_>>,
) -> Result<Vec<hel::hel_archive::RepositorySnapshot>> {
    let detected = detected_roots
        .iter()
        .map(|root| Ok((root_identity(root)?, root.clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let repository_paths = bundle
        .repositories
        .iter()
        .map(|repository| {
            let identity = configured_repository_identity(repository)
                .with_context(|| format!("repository {:?} has no usable source", repository.id))?;
            let path = detected.get(&identity).cloned().with_context(|| {
                format!(
                    "repository {:?} was not detected in the native session",
                    repository.id
                )
            })?;
            Ok((repository.id.clone(), path))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let git = SystemGit;
    let repository_count = bundle.repositories.len();
    bundle
        .repositories
        // Indexed parallel iteration keeps repository and manifest order
        // identical to the configured bundle.
        .par_iter()
        .enumerate()
        .map(|(index, repository)| {
            if let Some(control) = control {
                control.report(ImportArchiveProgress::Repository {
                    current: index + 1,
                    total: repository_count,
                    id: repository.id.clone(),
                })?;
            }
            let path = repository_paths
                .get(&repository.id)
                .expect("repository paths cover the validated bundle")
                .clone();
            ensure!(
                path.is_dir(),
                "local repository {:?} is missing at {}",
                repository.id,
                path.display()
            );
            let history = if repository.is_local() {
                // The local-repository proxy serves committed history and
                // provisioning fetches it, so the archive only has to carry
                // identity and dirty state.
                GitHistoryMode::NoBundle
            } else {
                // Import starts from the common ancestor of the local checkout
                // and the tracked remote, so unpushed commits are included in
                // the committed delta bundle.
                GitHistoryMode::DeltaFrom(import_delta_base(&path)?)
            };
            collect_git_snapshot_with_progress(
                &git,
                &path,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.destination.clone(),
                    history,
                    origin_override: repository
                        .is_local()
                        .then(|| format!("mj-local:{}", repository.id)),
                },
                control.is_none_or(|control| control.include_untracked),
                &|progress| {
                    let Some(control) = control else {
                        return Ok(());
                    };
                    match progress {
                        GitSnapshotProgress::UntrackedFile {
                            current,
                            total,
                            path,
                        } => control.report(ImportArchiveProgress::UntrackedFile {
                            repository_id: repository.id.clone(),
                            current,
                            total,
                            path,
                        }),
                    }
                },
            )
            .with_context(|| format!("collect local repository {:?}", repository.id))
        })
        .collect()
}

fn canonical_import_session(
    session_id: &str,
    events: &[SequencedEvent],
    source_path: &Path,
) -> Result<hel::hel_archive::CanonicalSessionSnapshot> {
    let mut events = events.to_vec();
    finalize_import_event_times(&mut events, source_path)?;
    let mut materialized = hel::hel_projection::imported_materialized_session(session_id, &events);
    materialized.session_title = harness_session_title(&events);
    if let Some(last_activity_at_ms) = events.iter().filter_map(|event| event.recorded_at_ms).max()
    {
        materialized.last_activity_at_ms = Some(
            materialized
                .last_activity_at_ms
                .map_or(last_activity_at_ms, |current| {
                    current.max(last_activity_at_ms)
                }),
        );
    }
    canonical_session_from_materialized(&materialized)
}

fn default_profile(config: &HelConfig, harness: HarnessKind, home: &Path) -> String {
    let source = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    config
        .profiles
        .iter()
        .find(|(_, profile)| {
            profile.kind == harness
                && fs::canonicalize(&profile.home).unwrap_or_else(|_| profile.home.clone())
                    == source
        })
        .or_else(|| {
            config
                .profiles
                .iter()
                .find(|(_, profile)| profile.kind == harness)
        })
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| format!("{}-import", harness.id()))
}

fn import_profile_id(
    config: &HelConfig,
    requested: Option<&str>,
    harness: HarnessKind,
    home: &Path,
) -> Result<String> {
    let Some(requested) = requested else {
        return Ok(default_profile(config, harness, home));
    };
    let profile = config
        .profiles
        .get(requested)
        .with_context(|| format!("unknown import profile {requested:?}"))?;
    ensure!(
        profile.kind == harness,
        "import profile {requested:?} does not use {harness:?}"
    );
    Ok(requested.to_owned())
}

/// The upstream revision an imported repository deltas from. A repository
/// without remote-tracking refs cannot tell us which ancestry a newly
/// provisioned clone has, and Hel never bundles full history, so it fails here.
fn import_delta_base(path: &Path) -> Result<String> {
    let upstream = git_optional_text(path, ["rev-parse", "--verify", "--quiet", "@{upstream}"])?
        .or(git_optional_text(
            path,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/HEAD",
            ],
        )?);
    upstream.with_context(|| {
        format!(
            "repository {} has no remote-tracking refs to import against; fetch its remote first",
            path.display()
        )
    })
}

fn git_optional_text<const N: usize>(cwd: &Path, arguments: [&str; N]) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("start git in {}", cwd.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8(output.stdout).context("decode Git output")?;
    Ok((!text.trim().is_empty()).then(|| text.trim().to_owned()))
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests;
