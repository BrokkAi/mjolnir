use super::*;

/// Locate a Kimi session directory. Its on-disk `session_<uuid>` name is the
/// native identifier required by Kimi ACP's `session/load`.
pub fn locate_kimi_session(
    home: &Path,
    selection: &KimiSessionSelection,
) -> Result<LocatedKimiSession> {
    let candidates = list_kimi_sessions(home)?;
    match selection {
        KimiSessionSelection::NativeSessionId(native_session_id) => candidates
            .into_iter()
            .find(|candidate| candidate.native_session_id == *native_session_id)
            .map(Ok)
            .unwrap_or_else(|| locate_named_kimi_session(home, native_session_id)),
        KimiSessionSelection::Latest => candidates
            .into_iter()
            .next()
            .context("no Kimi session directories were found"),
    }
}

/// Find the session directory a named id points at, for the ids the listing
/// does not show. The listing follows Kimi's own index: a session it never
/// indexed, one marked deleted there, and one the user archived are all left
/// out. None of that applies once the user names a session, so this lookup
/// reads the directory on disk whatever the index says about it.
///
/// A directory reached through a symlink is still refused, because the archive
/// step stores only files that sit directly inside the harness home. That
/// refusal is reported, so the caller can say why the named path was rejected
/// instead of reporting the session as missing.
pub(super) fn locate_named_kimi_session(
    home: &Path,
    native_session_id: &str,
) -> Result<LocatedKimiSession> {
    validate_id("Kimi session", native_session_id)?;
    let sessions = home.join("sessions");
    let indexed_work_dir = kimi_indexed_work_dir(home, native_session_id);
    let mut rejected = Vec::new();
    let mut matches = Vec::new();
    for session_path in kimi_named_session_paths(&sessions, native_session_id, &mut rejected)? {
        let metadata = match KIMI_STORE.directory(&session_path) {
            NamedEntry::Absent => continue,
            NamedEntry::Rejected(reason) => {
                rejected.push(reason);
                continue;
            }
            NamedEntry::Importable(metadata) => metadata,
        };
        let summary = kimi_session_summary(&session_path, indexed_work_dir.as_deref())?;
        // The archive is collected relative to the directory the session ran
        // in, so a session that records none cannot be imported.
        let Some(cwd) = summary.cwd else {
            rejected.push(KIMI_STORE.no_cwd(&session_path));
            continue;
        };
        matches.push(LocatedKimiSession {
            title: summary.title,
            native_session_id: native_session_id.to_owned(),
            modified_at: kimi_session_modified_at(&session_path, &metadata),
            git_branch: git_branch_or_head(&cwd),
            size_bytes: directory_size(&session_path)?,
            cwd,
            session_path,
        });
    }
    match matches.len() {
        0 if !rejected.is_empty() => Err(KIMI_STORE.cannot_import(native_session_id, &rejected)),
        0 => bail!(
            "Kimi session {native_session_id:?} was not found under {}",
            sessions.display()
        ),
        1 => Ok(matches.remove(0)),
        _ => bail!("Kimi session {native_session_id:?} occurs in multiple workspace directories"),
    }
}

/// Where a named Kimi session directory can sit: directly under `sessions`, or
/// inside one workspace directory there.
fn kimi_named_session_paths(
    sessions: &Path,
    native_session_id: &str,
    rejected: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let mut paths = vec![sessions.join(native_session_id)];
    let entries = match fs::read_dir(sessions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read Kimi sessions directory {}", sessions.display()));
        }
    };
    for entry in entries {
        let workspace = entry?.path();
        let path = workspace.join(native_session_id);
        match KIMI_STORE.container(&workspace, "workspace directory") {
            NamedEntry::Absent => continue,
            NamedEntry::Rejected(reason) => {
                // A workspace that cannot be archived is worth reporting only
                // when the named session is inside it.
                if path.exists() {
                    rejected.push(reason);
                }
            }
            NamedEntry::Importable(_) => paths.push(path),
        }
    }
    Ok(paths)
}

/// The working directory Kimi's own index recorded for one session, whatever
/// else that index says about it.
fn kimi_indexed_work_dir(home: &Path, native_session_id: &str) -> Option<PathBuf> {
    let index_path = home.join("session_index.jsonl");
    let mut work_dir = None;
    for line in BufReader::new(fs::File::open(index_path).ok()?).lines() {
        let Ok(record) = serde_json::from_str::<Value>(&line.ok()?) else {
            continue;
        };
        if record.get("sessionId").and_then(Value::as_str) != Some(native_session_id) {
            continue;
        }
        if let Some(recorded) = record.get("workDir").and_then(Value::as_str) {
            work_dir = Some(PathBuf::from(recorded));
        }
    }
    work_dir
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

pub(super) fn kimi_indexed_candidates(
    home: &Path,
    sessions: &Path,
) -> Result<Vec<KimiScanCandidate>> {
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
            kimi_state_listing_metadata(&canonical_session_path, &indexed_work_dir)
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

/// What one Kimi session's `state.json` says about it.
pub(super) struct KimiSessionSummary {
    /// The listing's verdict: the user archived this session in Kimi.
    /// Importing a session the user named by id ignores this.
    pub archived: bool,
    pub title: String,
    /// Absent when neither `state.json` nor Kimi's own index records an
    /// absolute working directory.
    pub cwd: Option<PathBuf>,
}

/// The listing's view of one session directory: `None` when its `state.json`
/// cannot be read or parsed, so one damaged session does not fail the scan.
pub(super) fn kimi_state_listing_metadata(
    session_path: &Path,
    indexed_work_dir: &Path,
) -> Option<(String, PathBuf, bool)> {
    kimi_session_summary(session_path, Some(indexed_work_dir))
        .ok()
        .map(|summary| {
            (
                summary.title,
                summary.cwd.unwrap_or_default(),
                summary.archived,
            )
        })
}

pub(super) fn kimi_session_summary(
    session_path: &Path,
    indexed_work_dir: Option<&Path>,
) -> Result<KimiSessionSummary> {
    let state_path = session_path.join("state.json");
    let state = if state_path.is_file() {
        serde_json::from_slice::<Value>(&fs::read(&state_path)?)
            .with_context(|| format!("parse Kimi session state {}", state_path.display()))?
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
                .filter(|work_dir| work_dir.is_absolute())
                .map(Path::to_path_buf)
        });
    let title = title.unwrap_or_else(|| {
        session_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled session")
            .to_owned()
    });
    Ok(KimiSessionSummary {
        archived: state
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        title,
        cwd,
    })
}

pub(super) fn kimi_session_modified_at(session_path: &Path, metadata: &fs::Metadata) -> SystemTime {
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

pub(super) fn select_jsonl_session(
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
