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

pub(super) fn locate_unlisted_claude_sessions(
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

pub(super) fn claude_native_metadata(path: &Path) -> Result<Option<(String, PathBuf, String)>> {
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

pub(super) fn directory_size(path: &Path) -> Result<u64> {
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
