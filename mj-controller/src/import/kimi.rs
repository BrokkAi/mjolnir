use super::*;

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

pub(super) fn kimi_state_listing_metadata(
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
