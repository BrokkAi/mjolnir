use super::*;

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
                // A listed row can carry an empty cwd: `scan_claude_sessions`
                // keeps a transcript it failed to parse in the picker rather
                // than hiding it. Such a row cannot be imported, so the by-id
                // lookup re-reads the file and reports the parse failure or
                // the missing cwd by name.
                .filter(|candidate| !candidate.cwd.as_os_str().is_empty())
                .collect::<Vec<_>>();
            let mut rejected = Vec::new();
            if matches.is_empty() {
                let unlisted = locate_unlisted_claude_sessions(home, native_session_id)?;
                matches = unlisted.matches;
                rejected = unlisted.rejected;
            }
            match matches.len() {
                0 if !rejected.is_empty() => {
                    Err(CLAUDE_STORE.cannot_import(native_session_id, &rejected))
                }
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

/// What looking a named Claude session up by path found: the transcripts that
/// can be imported, and why any file that sits at the named path cannot be.
pub(super) struct UnlistedClaudeSessions {
    pub matches: Vec<LocatedClaudeSession>,
    pub rejected: Vec<String>,
}

/// Find the transcript a named session id points at, for the ids the listing
/// does not show. The listing is a resume picker: it stops at Claude's own
/// display limit and hides sidechains, team sessions, daemon workers and
/// non-interactive entrypoints. None of that applies once the user names a
/// session, so this lookup reads `projects/<project>/<id>.jsonl` whatever the
/// picker would have made of it.
///
/// A transcript reached through a symlink is still refused, because the
/// archive step only stores regular files inside the harness home and would
/// otherwise copy content from outside it. That refusal is reported, so the
/// caller can say why the named path was rejected instead of "not found".
pub(super) fn locate_unlisted_claude_sessions(
    home: &Path,
    native_session_id: &str,
) -> Result<UnlistedClaudeSessions> {
    let projects = home.join("projects");
    let mut matches = Vec::new();
    let mut rejected = Vec::new();
    for project in fs::read_dir(&projects)? {
        let project = project?;
        let project_path = project.path();
        let project_metadata = fs::symlink_metadata(&project_path)?;
        let path = project_path.join(format!("{native_session_id}.jsonl"));
        if project_metadata.file_type().is_symlink() {
            if path.exists() {
                rejected.push(CLAUDE_STORE.symlinked_container(&project_path, "project directory"));
            }
            continue;
        }
        if !project_metadata.is_dir() {
            continue;
        }
        let metadata = match CLAUDE_STORE.file(&path) {
            NamedEntry::Absent => continue,
            NamedEntry::Rejected(reason) => {
                rejected.push(reason);
                continue;
            }
            NamedEntry::Importable(metadata) => metadata,
        };
        let summary = claude_native_summary(&path)?;
        // A transcript that never records a cwd cannot be imported: the
        // archive is collected relative to the directory the session ran in.
        let Some(cwd) = summary.cwd else {
            rejected.push(CLAUDE_STORE.no_cwd(&path));
            continue;
        };
        matches.push(LocatedClaudeSession {
            native_session_id: native_session_id.to_owned(),
            jsonl_path: path,
            modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            title: summary.title,
            cwd,
            git_branch: summary.git_branch,
            size_bytes: metadata.len(),
        });
    }
    Ok(UnlistedClaudeSessions { matches, rejected })
}

/// List native Claude sessions newest first.
pub fn list_claude_sessions(home: &Path) -> Result<Vec<LocatedClaudeSession>> {
    let mut sessions = Vec::new();
    scan_claude_sessions(home, &NativeScanCache::new(), |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Claude sessions newest first, reporting after every candidate file.
pub fn scan_claude_sessions(
    home: &Path,
    cache: &NativeScanCache,
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
        let metadata = match cache.claude_metadata(
            &candidate.path,
            candidate.modified_at,
            candidate.size_bytes,
            || claude_native_metadata(&candidate.path),
        ) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => {
                report(SessionScanProgress {
                    scanned: index + 1,
                    total,
                    session: None,
                });
                continue;
            }
            // A transcript Mjolnir cannot parse still belongs in the picker:
            // dropping it is how a named session became invisible in the
            // first place. The row carries an empty cwd, which the by-id
            // lookup refuses, so it re-reads the file and reports the parse
            // failure rather than importing a session with no directory.
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

/// Whether one transcript line can still change what `claude_native_metadata`
/// returns. Every key that function reads has a byte substring here, so a line
/// that matches none of them parses to nothing the function would use.
fn claude_line_may_matter(
    line: &str,
    needs_cwd: bool,
    needs_git_branch: bool,
    needs_entrypoint: bool,
) -> bool {
    (needs_cwd && line.contains("\"cwd\""))
        || (needs_git_branch && line.contains("\"gitBranch\""))
        || (needs_entrypoint && line.contains("\"entrypoint\""))
        || line.contains("\"customTitle\"")
        || line.contains("\"aiTitle\"")
        || line.contains("\"agentName\"")
        // Filter markers: a sidechain, a team session, a daemon worker, or a
        // `/loop` command record.
        || json_field_is_true(line, "isSidechain")
        || line.contains("\"teamName\"")
        || line.contains("daemon-worker")
        || line.contains("<command-name>/loop</command-name>")
}

/// Whether `"<field>"` is followed by `true` somewhere in this line, allowing
/// for whitespace around the colon.
fn json_field_is_true(line: &str, field: &str) -> bool {
    let mut rest = line;
    loop {
        let Some(index) = rest.find(field) else {
            return false;
        };
        rest = &rest[index + field.len()..];
        if let Some(after) = rest.trim_start().strip_prefix('"')
            && let Some(after) = after.trim_start().strip_prefix(':')
            && after.trim_start().starts_with("true")
        {
            return true;
        }
    }
}

/// What one Claude transcript's own records say about it.
pub(super) struct ClaudeNativeSummary {
    /// The resume picker's verdict: a sidechain, a team session, a daemon
    /// worker, a `/loop` record, or a non-interactive entrypoint. Importing a
    /// session the user named by id ignores this.
    pub filtered: bool,
    pub title: String,
    /// Absent when no record in the file carries a `cwd`.
    pub cwd: Option<PathBuf>,
    pub git_branch: String,
}

/// The picker's view of one transcript: `None` when the file is filtered out
/// of the resume list, and an error when it has no `cwd` to import from.
pub(super) fn claude_native_metadata(path: &Path) -> Result<Option<(String, PathBuf, String)>> {
    let summary = claude_native_summary(path)?;
    if summary.filtered {
        return Ok(None);
    }
    let cwd = summary
        .cwd
        .with_context(|| format!("Claude session {} has no cwd", path.display()))?;
    Ok(Some((summary.title, cwd, summary.git_branch)))
}

pub(super) fn claude_native_summary(path: &Path) -> Result<ClaudeNativeSummary> {
    let mut custom_title = None;
    let mut agent_name = None;
    let mut ai_title = None;
    let mut cwd = None;
    let mut git_branch = None;
    let mut entrypoint = None;
    let mut filtered = false;
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let line = line?;
        // Parsing every record of every transcript is what made the import
        // scan slow: a large session is megabytes of JSON, and only a handful
        // of records can change the answer. A line that cannot contain any of
        // the keys read below is skipped without being parsed. The position
        // keys drop out of the test as soon as their value is known, which is
        // usually on the first line.
        if !claude_line_may_matter(
            &line,
            cwd.is_none(),
            git_branch.is_none(),
            entrypoint.is_none(),
        ) {
            continue;
        }
        let record: Value = serde_json::from_str(&line)?;
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
    // by `claude -p /usage`, which must not displace real sessions there.
    Ok(ClaudeNativeSummary {
        filtered: filtered || entrypoint.as_deref().is_some_and(|value| value != "cli"),
        title: custom_title
            .or(agent_name)
            .or(ai_title)
            .unwrap_or_else(|| "Untitled session".into()),
        cwd,
        git_branch: git_branch.unwrap_or_else(|| "HEAD".into()),
    })
}

pub(super) fn git_branch_or_head(cwd: &Path) -> String {
    if cwd.as_os_str().is_empty() {
        return "HEAD".into();
    }
    git_optional_text(cwd, ["branch", "--show-current"])
        .ok()
        .flatten()
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "HEAD".into())
}

pub(crate) fn directory_size(path: &Path) -> Result<u64> {
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
