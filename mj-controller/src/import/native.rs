use super::*;

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
    }
}
