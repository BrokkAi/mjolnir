use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// What one scanned native session file parsed into, keyed by the file itself.
#[derive(Debug, Clone)]
enum CachedNativeMetadata {
    /// `claude_native_metadata`, including its "filtered out" verdict.
    Claude(Option<(String, PathBuf, String)>),
    /// `codex_session_metadata`, including its "not interactive" verdict.
    Codex(Option<CodexSessionMetadata>),
    IndexEligible(bool),
    Metadata(Option<NativeMetadata>),
}

#[derive(Debug)]
struct CachedNativeEntry {
    modified_at: SystemTime,
    size_bytes: u64,
    metadata: CachedNativeMetadata,
}

#[derive(Debug, Default)]
struct NativeScanCacheInner {
    entries: HashMap<PathBuf, CachedNativeEntry>,
    parsed_files: u64,
}

/// Remembers what each native session file parsed into, keyed by its path,
/// modified time and size. The parsers are pure functions of a file's content,
/// so an unchanged file never has to be opened again. One cache lives for the
/// process, so reopening the resume dialog reparses only what changed.
#[derive(Debug, Clone, Default)]
pub struct NativeScanCache {
    inner: Arc<Mutex<NativeScanCacheInner>>,
}

impl NativeScanCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cache shared by index discovery and Mjolnir's SessionWiki reads.
    pub fn shared() -> Self {
        static CACHE: std::sync::OnceLock<NativeScanCache> = std::sync::OnceLock::new();
        CACHE.get_or_init(Self::new).clone()
    }

    /// How many files this cache has actually parsed, for tests and diagnostics.
    pub fn parsed_files(&self) -> u64 {
        self.lock().parsed_files
    }

    /// Index discovery excludes identified children, preserving parents even
    /// when their entrypoint or missing working directory prevents importing.
    pub fn index_eligible(&self, kind: HarnessKind, path: &Path) -> Result<bool> {
        if kind == HarnessKind::Claude
            && path
                .components()
                .any(|component| component.as_os_str() == "subagents")
        {
            return Ok(false);
        }
        let metadata = fs::metadata(path)?;
        let modified_at = metadata.modified()?;
        let size_bytes = metadata.len();
        if let Some(CachedNativeMetadata::IndexEligible(eligible)) =
            self.cached(path, modified_at, size_bytes)
        {
            return Ok(eligible);
        }
        let eligible = match kind {
            HarnessKind::Codex => !codex::codex_session_summary(path)?.subagent,
            HarnessKind::Claude => !claude::claude_native_summary(path)?.subagent,
            other => bail!("{other:?} has its own top-level source enumeration"),
        };
        self.store(
            path,
            modified_at,
            size_bytes,
            CachedNativeMetadata::IndexEligible(eligible),
        );
        Ok(eligible)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NativeScanCacheInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cached(
        &self,
        path: &Path,
        modified_at: SystemTime,
        size_bytes: u64,
    ) -> Option<CachedNativeMetadata> {
        let inner = self.lock();
        let entry = inner.entries.get(path)?;
        (entry.modified_at == modified_at && entry.size_bytes == size_bytes)
            .then(|| entry.metadata.clone())
    }

    fn store(
        &self,
        path: &Path,
        modified_at: SystemTime,
        size_bytes: u64,
        metadata: CachedNativeMetadata,
    ) {
        let mut inner = self.lock();
        inner.parsed_files = inner.parsed_files.saturating_add(1);
        inner.entries.insert(
            path.to_owned(),
            CachedNativeEntry {
                modified_at,
                size_bytes,
                metadata,
            },
        );
    }

    /// Claude metadata for one transcript, parsing only on a cache miss.
    /// Errors are returned to the caller and never cached.
    pub(super) fn claude_metadata(
        &self,
        path: &Path,
        modified_at: SystemTime,
        size_bytes: u64,
        parse: impl FnOnce() -> Result<Option<(String, PathBuf, String)>>,
    ) -> Result<Option<(String, PathBuf, String)>> {
        if let Some(CachedNativeMetadata::Claude(metadata)) =
            self.cached(path, modified_at, size_bytes)
        {
            return Ok(metadata);
        }
        let metadata = parse()?;
        self.store(
            path,
            modified_at,
            size_bytes,
            CachedNativeMetadata::Claude(metadata.clone()),
        );
        Ok(metadata)
    }

    pub(super) fn native_metadata(
        &self,
        path: &Path,
        parse: impl FnOnce() -> Result<Option<NativeMetadata>>,
    ) -> Result<Option<NativeMetadata>> {
        let file = fs::metadata(path)?;
        let modified_at = file.modified()?;
        let size = file.len();
        if let Some(CachedNativeMetadata::Metadata(metadata)) = self.cached(path, modified_at, size)
        {
            return Ok(metadata);
        }
        let metadata = parse()?;
        self.store(
            path,
            modified_at,
            size,
            CachedNativeMetadata::Metadata(metadata.clone()),
        );
        Ok(metadata)
    }

    /// Codex metadata for one rollout, parsing only on a cache miss.
    pub(super) fn codex_metadata(
        &self,
        path: &Path,
        modified_at: SystemTime,
        size_bytes: u64,
        parse: impl FnOnce() -> Result<Option<CodexSessionMetadata>>,
    ) -> Result<Option<CodexSessionMetadata>> {
        if let Some(CachedNativeMetadata::Codex(metadata)) =
            self.cached(path, modified_at, size_bytes)
        {
            return Ok(metadata);
        }
        let metadata = parse()?;
        self.store(
            path,
            modified_at,
            size_bytes,
            CachedNativeMetadata::Codex(metadata.clone()),
        );
        Ok(metadata)
    }
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
        HarnessKind::Muse => {
            return muse::locate(&mj_checkpoint::native::muse_sessions_root(home)?, selection);
        }
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
        HarnessKind::Antigravity | HarnessKind::Dsh | HarnessKind::OpenCode => {
            let found = metadata_candidates(harness, home, &NativeScanCache::shared())?
                .into_iter()
                .find(|candidate| match selection {
                    ClaudeSessionSelection::Latest => {
                        candidate.listing.unavailable_reason.is_none()
                    }
                    ClaudeSessionSelection::NativeSessionId(id) => {
                        candidate.listing.native_session_id == *id
                    }
                })
                .with_context(|| {
                    format!(
                        "no matching {} session in {}",
                        harness.display_name(),
                        home.display()
                    )
                })?;
            if let Some(reason) = found.listing.unavailable_reason {
                bail!("{reason}");
            }
            return Ok(LocatedNativeSession {
                native_session_id: found.listing.native_session_id,
                source_path: found.source_path,
            });
        }
    };
    Ok(LocatedNativeSession {
        native_session_id,
        source_path,
    })
}

/// The id [`locate_native_session`] takes, read back from the path that
/// locator produced. `None` when the path has no readable name.
///
/// The inverse of the locators above, so every caller that has only a
/// transcript path — the SessionWiki index stores one per row — names the
/// session the same way `mj import` does. Each harness keeps its id in a
/// different part of the path: Claude Code names the file after the session,
/// Codex appends the thread UUID to a `rollout-` prefix, Kimi and Grok name
/// the session's own directory, and Muse names the directory above
/// `session.jsonl`.
pub fn native_session_id_from_path(harness: HarnessKind, path: &Path) -> Option<String> {
    if harness == HarnessKind::Codex && path.to_string_lossy().contains('\u{1f}') {
        return None;
    }
    super::adapter_transcript::import_adapter(harness, None).native_id(path.to_str()?)
}

/// Project one native session into the canonical transcript, for any harness.
pub fn read_native_transcript(
    harness: HarnessKind,
    source_path: &Path,
) -> Result<ClaudeTranscript> {
    match harness {
        HarnessKind::Muse => {
            super::adapter_transcript::read_muse_transcript(source_path, source_path)
        }
        HarnessKind::Codex => read_codex_transcript(source_path),
        HarnessKind::Claude => read_claude_transcript(source_path),
        HarnessKind::Kimi => read_kimi_transcript(source_path),
        HarnessKind::Grok => read_grok_transcript(source_path),
        HarnessKind::Antigravity | HarnessKind::Dsh | HarnessKind::OpenCode => {
            super::adapter_transcript::read_projection(
                harness,
                source_path,
                native_source_file(harness, source_path),
            )
        }
    }
}

/// The physical artifact for a transcript key, used for filesystem metadata.
pub fn native_source_file(harness: HarnessKind, path: &Path) -> &Path {
    if harness == HarnessKind::OpenCode
        && let Some((database, _)) = opencode::database_key(path)
    {
        return database;
    }
    path
}

/// The formats on disk that the pinned harnesses can load after restore.
pub fn native_source_import_issue(
    harness: HarnessKind,
    path: &Path,
) -> Result<Option<&'static str>> {
    Ok(match harness {
        HarnessKind::Dsh if !dsh::is_acp_record(path) => Some(
            "the pinned DSH bridge loads flat ACP session records; this row is from the DSH CLI store",
        ),
        HarnessKind::OpenCode => opencode::import_issue(path)?,
        HarnessKind::Antigravity if !antigravity::is_cli_log(path) => Some(
            "Mjolnir imports Antigravity CLI conversations; this row is from the Antigravity IDE",
        ),
        _ => None,
    })
}

/// Scan a harness home newest first, reporting after every candidate.
pub fn scan_native_sessions(
    harness: HarnessKind,
    home: &Path,
    cache: &NativeScanCache,
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
        HarnessKind::Muse => muse::scan(
            &mj_checkpoint::native::muse_sessions_root(home)?,
            |progress| {
                forward(progress.scanned, progress.total, progress.session);
            },
        ),
        HarnessKind::Codex => scan_codex_sessions(home, cache, |progress| {
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
        HarnessKind::Claude => scan_claude_sessions(home, cache, |progress| {
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
        HarnessKind::Antigravity | HarnessKind::Dsh | HarnessKind::OpenCode => {
            let candidates = metadata_candidates(harness, home, cache)?;
            let total = candidates.len();
            forward(0, total, None);
            for (index, candidate) in candidates.into_iter().enumerate() {
                forward(index + 1, total, Some(candidate.listing));
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(super) struct NativeMetadata {
    #[serde(default)]
    pub title: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

pub(super) fn absolute_native_cwd(cwd: String) -> Option<PathBuf> {
    (!cwd.trim().is_empty())
        .then(|| PathBuf::from(cwd))
        .filter(|cwd| cwd.is_absolute())
}

pub(super) struct NativeCandidate {
    pub source_path: PathBuf,
    pub listing: NativeSessionListing,
}

impl NativeCandidate {
    pub(super) fn from_metadata(
        adapter: &dyn sessionwiki::adapters::Adapter,
        source_path: PathBuf,
        file: &Path,
        metadata: NativeMetadata,
        issue: Option<&'static str>,
    ) -> Result<Option<Self>> {
        if metadata.parent_session_id.is_some_and(|id| !id.is_empty()) {
            return Ok(None);
        }
        let Some(native_session_id) = source_path.to_str().and_then(|key| adapter.native_id(key))
        else {
            return Ok(None);
        };
        let file = fs::metadata(file)?;
        let cwd = metadata.cwd.and_then(absolute_native_cwd);
        let unavailable_reason = issue.or_else(|| {
            cwd.is_none()
                .then_some("native session has no absolute recorded working directory")
        });
        Ok(Some(Self {
            source_path,
            listing: NativeSessionListing {
                title: metadata
                    .title
                    .filter(|title| !title.trim().is_empty())
                    .map(|title| {
                        sessionwiki::util::truncate(&sessionwiki::redact::redact(&title), 80)
                    })
                    .unwrap_or_else(|| native_session_id.clone()),
                native_session_id,
                modified_at: metadata
                    .updated_at
                    .map(SystemTime::from)
                    .unwrap_or(file.modified()?),
                size_bytes: file.len(),
                cwd: cwd.unwrap_or_default(),
                git_branch: String::new(),
                unavailable_reason,
                natively_archived: false,
            },
        }))
    }
}

fn metadata_candidates(
    harness: HarnessKind,
    home: &Path,
    cache: &NativeScanCache,
) -> Result<Vec<NativeCandidate>> {
    let adapter = super::adapter_transcript::import_adapter(harness, Some(home));
    let mut candidates = match harness {
        HarnessKind::Antigravity => antigravity::candidates(home, adapter.as_ref(), cache)?,
        HarnessKind::Dsh => dsh::candidates(home, adapter.as_ref(), cache)?,
        HarnessKind::OpenCode => opencode::candidates(home, adapter.as_ref(), cache)?,
        _ => unreachable!("harness already has its own inventory"),
    };
    candidates.sort_by(|a, b| {
        b.listing
            .modified_at
            .cmp(&a.listing.modified_at)
            .then_with(|| a.source_path.cmp(&b.source_path))
    });
    Ok(candidates)
}

pub(super) fn optional_entries(
    path: &Path,
) -> Result<impl Iterator<Item = std::io::Result<fs::DirEntry>>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    Ok(entries.into_iter().flatten())
}
