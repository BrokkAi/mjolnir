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

    /// How many files this cache has actually parsed, for tests and diagnostics.
    pub fn parsed_files(&self) -> u64 {
        self.lock().parsed_files
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
    match harness {
        HarnessKind::Claude => path.file_stem()?.to_str().map(str::to_owned),
        HarnessKind::Codex => codex_rollout_id_from_path(path).map(str::to_owned),
        HarnessKind::Kimi | HarnessKind::Grok => path.file_name()?.to_str().map(str::to_owned),
        HarnessKind::Muse => path.parent()?.file_name()?.to_str().map(str::to_owned),
    }
}

/// Where one native session's transcript lives and when it last changed.
///
/// Cheaper than [`NativeSessionListing`]: no git branch and no directory size,
/// because the search index only needs a stable key and a change token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSessionSource {
    pub native_session_id: String,
    pub source_path: PathBuf,
    /// The newest modification time of the session's own transcript files, not
    /// of the directory that holds them.
    pub modified_at: SystemTime,
}

/// List the sessions of one harness home for indexing.
///
/// Only the harnesses whose sessions Mjolnir itself has to enumerate are
/// supported; Codex and Claude Code keep one file per session, which their
/// own readers walk directly.
pub fn list_native_session_sources(
    harness: HarnessKind,
    home: &Path,
) -> Result<Vec<NativeSessionSource>> {
    let sources = match harness {
        HarnessKind::Kimi => kimi_indexed_candidates(home, &home.join("sessions"))?
            .into_iter()
            .map(|candidate| NativeSessionSource {
                native_session_id: candidate.native_session_id,
                source_path: candidate.session_path,
                modified_at: candidate.modified_at,
            })
            .collect(),
        HarnessKind::Grok => grok::grok_candidates(&home.join("sessions"))?
            .into_iter()
            .map(|candidate| NativeSessionSource {
                native_session_id: candidate.native_session_id,
                source_path: candidate.session_path,
                modified_at: candidate.modified_at,
            })
            .collect(),
        HarnessKind::Muse => muse::list_sources(&mj_checkpoint::native::muse_sessions_root(home)?)?,
        other => bail!("{other:?} keeps one session per file; there is nothing to enumerate"),
    };
    Ok(sources)
}

/// The title the harness itself records for one session, read from that
/// session's own metadata file. `None` when the harness records none, which
/// leaves the caller to derive a title from the conversation.
pub fn native_session_title(harness: HarnessKind, source_path: &Path) -> Option<String> {
    match harness {
        HarnessKind::Kimi => {
            kimi_state_listing_metadata(source_path, Path::new("")).map(|(title, _, _)| title)
        }
        HarnessKind::Grok => grok::grok_listing_metadata(source_path).0,
        _ => None,
    }
}

/// Project one native session into the canonical transcript, for any harness.
pub fn read_native_transcript(
    harness: HarnessKind,
    source_path: &Path,
) -> Result<ClaudeTranscript> {
    match harness {
        HarnessKind::Muse => muse::read_transcript(source_path),
        HarnessKind::Codex => read_codex_transcript(source_path),
        HarnessKind::Claude => read_claude_transcript(source_path),
        HarnessKind::Kimi => read_kimi_transcript(source_path),
        HarnessKind::Grok => read_grok_transcript(source_path),
    }
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
    }
}
