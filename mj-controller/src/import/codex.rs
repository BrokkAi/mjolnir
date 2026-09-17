use super::*;

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

/// Find the rollout a named session id points at, for the ids the listing does
/// not show. The listing is a resume picker: it needs a thread Codex itself
/// indexed with a preview, and it hides subagent and ephemeral rollouts. None
/// of that applies once the user names a session, so this lookup takes the
/// rollout whatever the picker would have made of it.
///
/// A rollout reached through a symlink is still refused, because the archive
/// step stores only regular files inside the harness home. That refusal is
/// reported, so the caller can say why the named path was rejected instead of
/// reporting the session as missing.
pub(super) fn locate_unindexed_codex_session(
    home: &Path,
    session_id: &str,
) -> Result<LocatedCodexSession> {
    validate_id("Codex session", session_id)?;
    let mut walk = NamedCodexWalk {
        session_id,
        visited: BTreeSet::new(),
        found: Vec::new(),
        rejected: Vec::new(),
    };
    let root = home.join("sessions");
    if root.is_dir() {
        walk.walk(&root, None)?;
    }
    let titles = codex_native_titles(home)?;
    let mut matches = Vec::new();
    for candidate in std::mem::take(&mut walk.found) {
        let summary = codex_session_summary(&candidate.path)?;
        let Some(metadata) = summary.metadata else {
            walk.rejected.push(format!(
                "{} records no Codex session id",
                candidate.path.display()
            ));
            continue;
        };
        // The rollout at this path belongs to a different thread, so it is not
        // the named session and nothing about it is worth reporting.
        if metadata.id != session_id {
            continue;
        }
        // Codex records the directory a thread ran in once, in its header. The
        // archive is collected relative to that directory.
        if metadata.cwd.as_os_str().is_empty() {
            walk.rejected.push(CODEX_STORE.no_cwd(&candidate.path));
            continue;
        }
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
    if matches.is_empty() && !walk.rejected.is_empty() {
        return Err(CODEX_STORE.cannot_import(session_id, &walk.rejected));
    }
    select_jsonl_session(
        matches,
        &CodexSessionSelection::NativeSessionId(session_id.to_owned()),
        "Codex",
    )
}

/// The walk behind a Codex lookup by session id. Unlike the listing walk it
/// follows symlinked directories, so a rollout that exists only behind one is
/// refused with its reason instead of being reported as missing. `visited`
/// holds the directories already walked, which stops a symlink loop.
struct NamedCodexWalk<'a> {
    session_id: &'a str,
    visited: BTreeSet<PathBuf>,
    found: Vec<FileScanCandidate>,
    rejected: Vec<String>,
}

impl NamedCodexWalk<'_> {
    /// Walk one directory. `via_symlink` names the symlinked directory this
    /// one was reached through, if any: nothing under it can be archived.
    fn walk(&mut self, directory: &Path, via_symlink: Option<&Path>) -> Result<()> {
        let Ok(canonical) = directory.canonicalize() else {
            return Ok(());
        };
        if !self.visited.insert(canonical) {
            return Ok(());
        }
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            // A rollout filename ends in the thread's UUID. Only a path that
            // names this session is worth refusing out loud; a file with no
            // UUID in its name is still read, because its header may name the
            // session anyway.
            let rollout_id = codex_rollout_id_from_path(&path);
            let names_session = rollout_id == Some(self.session_id);
            let may_be_session = rollout_id.is_none_or(|id| id == self.session_id);
            if metadata.file_type().is_symlink() {
                if fs::metadata(&path).is_ok_and(|target| target.is_dir()) {
                    self.walk(&path, via_symlink.or(Some(&path)))?;
                } else if names_session
                    && let NamedEntry::Rejected(reason) = CODEX_STORE.file(&path)
                {
                    self.rejected.push(reason);
                }
                continue;
            }
            if metadata.is_dir() {
                self.walk(&path, via_symlink)?;
                continue;
            }
            if !may_be_session || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                continue;
            }
            if let Some(symlink) = via_symlink {
                if names_session {
                    self.rejected
                        .push(CODEX_STORE.symlinked_container(symlink, "rollout directory"));
                }
                continue;
            }
            match CODEX_STORE.file(&path) {
                NamedEntry::Absent => continue,
                NamedEntry::Rejected(reason) => {
                    if names_session {
                        self.rejected.push(reason);
                    }
                }
                NamedEntry::Importable(metadata) => self.found.push(FileScanCandidate {
                    path,
                    modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    size_bytes: metadata.len(),
                }),
            }
        }
        Ok(())
    }
}

/// List native Codex sessions newest first.
pub fn list_codex_sessions(home: &Path) -> Result<Vec<LocatedCodexSession>> {
    let mut sessions = Vec::new();
    scan_codex_sessions(home, &NativeScanCache::new(), |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Codex sessions newest first, reporting after every candidate file.
pub fn scan_codex_sessions(
    home: &Path,
    cache: &NativeScanCache,
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
        let session = cache
            .codex_metadata(
                &candidate.path,
                candidate.modified_at,
                candidate.size_bytes,
                || codex_session_metadata(&candidate.path),
            )?
            .map(|metadata| {
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

pub(super) fn codex_indexed_sessions(home: &Path) -> Result<Option<Vec<LocatedCodexSession>>> {
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

pub(super) fn collect_codex_candidate_paths(
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

pub(super) fn codex_rollout_id_from_path(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.as_bytes().get(8) == Some(&b'-')
        && id.as_bytes().get(13) == Some(&b'-')
        && id.as_bytes().get(18) == Some(&b'-')
        && id.as_bytes().get(23) == Some(&b'-'))
    .then_some(id)
}

/// What one Codex rollout's own header says about it.
pub(super) struct CodexSessionSummary {
    /// The resume picker's verdict: a subagent rollout or an ephemeral thread.
    /// Importing a session the user named by id ignores this.
    pub filtered: bool,
    /// Absent when no `session_meta` record in the header names a session.
    pub metadata: Option<CodexSessionMetadata>,
}

/// The picker's view of one rollout: `None` when the file is filtered out of
/// the resume list or names no session at all.
pub(super) fn codex_session_metadata(path: &Path) -> Result<Option<CodexSessionMetadata>> {
    let summary = codex_session_summary(path)?;
    Ok(if summary.filtered {
        None
    } else {
        summary.metadata
    })
}

pub(super) fn codex_session_summary(path: &Path) -> Result<CodexSessionSummary> {
    let file =
        fs::File::open(path).with_context(|| format!("open Codex session {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut filtered = false;
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
        // A structured source identifies a subagent, and an ephemeral thread
        // normally has no rollout path at all; the ephemeral check stays
        // defensive so a future writer cannot expose one in the picker.
        filtered = filtered
            || !codex_source_is_interactive(record.pointer("/payload/source"))
            || record
                .pointer("/payload/ephemeral")
                .and_then(Value::as_bool)
                == Some(true);
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
            return Ok(CodexSessionSummary {
                filtered,
                metadata: Some(CodexSessionMetadata {
                    id,
                    cwd,
                    git_branch,
                    history_mode,
                }),
            });
        }
    }
    Ok(CodexSessionSummary {
        filtered,
        metadata: None,
    })
}

pub(super) fn parse_codex_history_mode(value: &str) -> Result<CodexHistoryMode> {
    match value {
        "legacy" => Ok(CodexHistoryMode::Legacy),
        "paginated" => Ok(CodexHistoryMode::Paginated),
        other => bail!("unsupported Codex history mode {other:?}"),
    }
}

pub(super) fn codex_source_is_interactive(source: Option<&Value>) -> bool {
    match source {
        // Older rollouts predate the source field and came from the TUI.
        None => true,
        Some(Value::String(source)) => matches!(source.as_str(), "cli" | "vscode"),
        // Structured sources identify subagents. Other unexpected shapes are
        // not sessions offered by the normal interactive resume picker.
        Some(_) => false,
    }
}

pub(super) fn codex_native_titles(home: &Path) -> Result<BTreeMap<String, String>> {
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
