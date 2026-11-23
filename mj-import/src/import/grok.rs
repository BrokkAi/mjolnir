use super::*;

/// How a refusal names the directory a Grok Build session sits in. Grok Build
/// names that directory after the working directory the session ran in, and the
/// listing and the by-id lookup both refuse it in these words.
const GROK_CWD_DIRECTORY: &str = "working directory";

/// Locate a Grok Build session directory. Its name is the session UUID, which
/// is the native identifier `session/load` takes.
pub fn locate_grok_session(
    home: &Path,
    selection: &GrokSessionSelection,
) -> Result<LocatedGrokSession> {
    let candidates = list_grok_sessions(home)?;
    match selection {
        GrokSessionSelection::NativeSessionId(native_session_id) => candidates
            .into_iter()
            .find(|candidate| candidate.native_session_id == *native_session_id)
            .map(Ok)
            .unwrap_or_else(|| locate_named_grok_session(home, native_session_id)),
        GrokSessionSelection::Latest => candidates
            .into_iter()
            .next()
            .context("no Grok Build session directories were found"),
    }
}

/// Find the session directory a named id points at, for the ids the listing
/// leaves out. The listing skips whatever it cannot make a row from, including
/// a session whose working directory it cannot recover. Naming a session must
/// say why it was skipped rather than report it as missing.
///
/// A directory reached through a symlink is still refused, because the archive
/// step stores only files that sit directly inside the harness home.
fn locate_named_grok_session(home: &Path, native_session_id: &str) -> Result<LocatedGrokSession> {
    validate_id("Grok Build session", native_session_id)?;
    let sessions = home.join("sessions");
    let mut rejected = Vec::new();
    let mut matches = Vec::new();
    for entry in fs::read_dir(&sessions)
        .with_context(|| format!("read Grok Build sessions directory {}", sessions.display()))?
    {
        let cwd_directory = entry?.path();
        let session_path = cwd_directory.join(native_session_id);
        match GROK_STORE.container(&cwd_directory, GROK_CWD_DIRECTORY) {
            NamedEntry::Absent => continue,
            NamedEntry::Rejected(reason) => {
                // A working directory that cannot be archived is worth
                // reporting only when the named session is inside it; an
                // unrelated symlink in the sessions root says nothing about it.
                if session_path.exists() {
                    rejected.push(reason);
                }
                continue;
            }
            NamedEntry::Importable(_) => {}
        }
        let metadata = match GROK_STORE.directory(&session_path) {
            NamedEntry::Absent => continue,
            NamedEntry::Rejected(reason) => {
                rejected.push(reason);
                continue;
            }
            NamedEntry::Importable(metadata) => metadata,
        };
        let (title, summary_cwd) = grok_listing_metadata(&session_path);
        // Grok Build records the working directory in the session summary and
        // again in the name of the directory holding it. The archive is
        // collected relative to that directory, so neither may be missing.
        let Some(cwd) = summary_cwd.or_else(|| grok_decode_cwd_dirname(&cwd_directory)) else {
            rejected.push(GROK_STORE.no_cwd(&session_path));
            continue;
        };
        matches.push(LocatedGrokSession {
            title: title.unwrap_or_else(|| native_session_id.to_owned()),
            native_session_id: native_session_id.to_owned(),
            modified_at: grok_session_modified_at(&session_path, &metadata),
            git_branch: git_branch_or_head(&cwd),
            size_bytes: directory_size(&session_path)?,
            cwd,
            session_path,
        });
    }
    match matches.len() {
        0 if !rejected.is_empty() => Err(GROK_STORE.cannot_import(native_session_id, &rejected)),
        0 => bail!(
            "Grok Build session {native_session_id:?} was not found under {}",
            sessions.display()
        ),
        1 => Ok(matches.remove(0)),
        _ => {
            bail!("Grok Build session {native_session_id:?} occurs in multiple working directories")
        }
    }
}

/// List native Grok Build sessions newest first.
pub fn list_grok_sessions(home: &Path) -> Result<Vec<LocatedGrokSession>> {
    let mut sessions = Vec::new();
    scan_grok_sessions(home, |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Grok Build sessions newest first, reporting after every
/// candidate directory.
pub fn scan_grok_sessions(
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<LocatedGrokSession>),
) -> Result<()> {
    let sessions = home.join("sessions");
    ensure!(
        sessions.is_dir(),
        "Grok Build sessions directory is missing: {}",
        sessions.display()
    );
    let mut candidates = grok_candidates(&sessions)?;
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
        let session = LocatedGrokSession {
            title: candidate.title,
            native_session_id: candidate.native_session_id,
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

/// Walk `sessions/<encoded-cwd>/<session-uuid>`. The sessions root also holds
/// the shared search index and lock files, which are not sessions.
///
/// A working directory the import could not archive is left out, so the listing
/// offers only sessions that can actually be imported and the by-id lookup is
/// left to report why one is missing from it.
pub(super) fn grok_candidates(sessions: &Path) -> Result<Vec<KimiScanCandidate>> {
    let mut candidates = Vec::new();
    for cwd_entry in fs::read_dir(sessions)? {
        let cwd_directory = cwd_entry?.path();
        match GROK_STORE.container(&cwd_directory, GROK_CWD_DIRECTORY) {
            NamedEntry::Absent => continue,
            NamedEntry::Rejected(reason) => {
                tracing::debug!(reason, "skipped a Grok Build working directory");
                continue;
            }
            NamedEntry::Importable(_) => {}
        }
        let decoded_cwd = grok_decode_cwd_dirname(&cwd_directory);
        for session_entry in fs::read_dir(&cwd_directory)? {
            let session_entry = session_entry?;
            let session_path = session_entry.path();
            let metadata = fs::symlink_metadata(&session_path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let Some(native_session_id) = session_path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| validate_id("Grok Build session", name).is_ok())
            else {
                continue;
            };
            let (title, summary_cwd) = grok_listing_metadata(&session_path);
            let Some(cwd) = summary_cwd.or_else(|| decoded_cwd.clone()) else {
                continue;
            };
            candidates.push(KimiScanCandidate {
                native_session_id: native_session_id.to_owned(),
                modified_at: grok_session_modified_at(&session_path, &metadata),
                title: title.unwrap_or_else(|| native_session_id.to_owned()),
                cwd,
                session_path,
            });
        }
    }
    Ok(candidates)
}

/// Grok Build's session-directory name is the URL-encoded working directory,
/// or a `{slug}-{hash}` form for a long path that keeps the real value in a
/// `.cwd` file. Mirrors `decode_cwd_from_dirname` in grok-build.
pub(super) fn grok_decode_cwd_dirname(directory: &Path) -> Option<PathBuf> {
    let name = directory.file_name()?.to_str()?;
    if let Some(decoded) = url_decode(name)
        && decoded.starts_with('/')
    {
        return Some(PathBuf::from(decoded));
    }
    let recorded = fs::read_to_string(directory.join(".cwd")).ok()?;
    let recorded = recorded.trim();
    recorded.starts_with('/').then(|| PathBuf::from(recorded))
}

/// Percent-decoding for the session-directory name. `None` when the text is
/// not valid percent-encoded UTF-8, which means it is not a cwd key.
fn url_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Listing title and cwd from `summary.json`. Never fails the whole scan: an
/// unreadable session is listed with what could be recovered.
pub(super) fn grok_listing_metadata(session_path: &Path) -> (Option<String>, Option<PathBuf>) {
    let summary = fs::read(session_path.join("summary.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let title = summary
        .get("session_summary")
        .and_then(Value::as_str)
        .and_then(normalize_session_title);
    let cwd = summary
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_absolute());
    (title, cwd)
}

fn grok_session_modified_at(session_path: &Path, metadata: &fs::Metadata) -> SystemTime {
    let mut modified_at = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    for name in [CHAT_HISTORY, "events.jsonl", "summary.json"] {
        if let Ok(modified) = fs::metadata(session_path.join(name)).and_then(|file| file.modified())
        {
            modified_at = modified_at.max(modified);
        }
    }
    modified_at
}
