use super::*;

/// Clock-skew slack subtracted from a Codex session's own creation time before
/// it is used as an mtime floor for content probes.
pub(super) const CODEX_PROBE_FLOOR_SLACK_MS: i64 = 48 * 3600 * 1000;

pub(super) const CODEX_SCAN_CACHE_FILE: &str = "codex-scan-cache.json";

/// How a checkpoint export fails when the harness saved nothing for a native
/// session that received a prompt. The controller recognizes it in the
/// export's stderr to tell the caller what happened.
pub const NO_SESSION_ARTIFACTS: &str = "no session artifacts found";

/// Rollouts whose `session_meta` header named a different resumable thread.
/// Codex writes that header once, when it creates the file, so a negative
/// verdict never turns positive and is safe to remember across exports.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CodexScanCache {
    pub(super) session_id: String,
    pub(super) not_ours: BTreeSet<PathBuf>,
}

impl CodexScanCache {
    pub(super) fn empty(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            not_ours: BTreeSet::new(),
        }
    }
}

/// Everything that lets the Codex walk skip a content probe.
#[derive(Default)]
pub(super) struct CodexProbeContext<'a> {
    /// Rollouts modified before this unix-ms instant cannot belong to the
    /// session, so they are never opened. `None` disables the gate.
    floor_ms: Option<i64>,
    cache: Option<&'a mut CodexScanCache>,
}

/// A missing, unreadable, corrupt, or foreign-session cache is not an error:
/// the cache only ever saves work, so falling back to an empty one is correct.
pub(super) fn load_codex_scan_cache(relay_root: &Path, session_id: &str) -> CodexScanCache {
    fs::read(relay_root.join(CODEX_SCAN_CACHE_FILE))
        .ok()
        .and_then(|body| serde_json::from_slice::<CodexScanCache>(&body).ok())
        .filter(|cache| cache.session_id == session_id)
        .unwrap_or_else(|| CodexScanCache::empty(session_id))
}

pub(super) fn save_codex_scan_cache(relay_root: &Path, cache: &CodexScanCache) -> Result<()> {
    let relative = Path::new(CODEX_SCAN_CACHE_FILE);
    write_private_file(relay_root, relative, &serde_json::to_vec(cache)?, 0o600).with_context(
        || {
            format!(
                "write Codex scan cache {}",
                relay_root.join(relative).display()
            )
        },
    )
}

/// Codex native session IDs are UUIDv7, whose leading 48 bits hold the
/// session's creation time in unix milliseconds.
pub(super) fn uuid_v7_timestamp_ms(id: &str) -> Option<i64> {
    let groups = id.split('-').collect::<Vec<_>>();
    let [first, second, third, _, _] = groups.as_slice() else {
        return None;
    };
    let shaped = groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, width)| {
        group.len() == width
            && group
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    });
    if !shaped || !third.starts_with('7') {
        return None;
    }
    i64::from_str_radix(&format!("{first}{second}"), 16).ok()
}

pub(super) fn unix_millis(time: SystemTime) -> Option<i64> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since_epoch) => i64::try_from(since_epoch.as_millis()).ok(),
        Err(before_epoch) => i64::try_from(before_epoch.duration().as_millis())
            .ok()
            .map(|millis| -millis),
    }
}

pub fn collect_native_artifacts(
    harness: HarnessKind,
    home: &Path,
    session_id: &str,
    allow_empty: bool,
) -> Result<Vec<NativeArtifact>> {
    collect_native_artifacts_cached(harness, home, session_id, allow_empty, None)
}

pub(super) fn collect_native_artifacts_cached(
    harness: HarnessKind,
    home: &Path,
    session_id: &str,
    allow_empty: bool,
    cache: Option<&mut CodexScanCache>,
) -> Result<Vec<NativeArtifact>> {
    validate_component(session_id, "native session ID")?;
    let roots = harness.native_session_dirs();
    let mut probe = match harness {
        HarnessKind::Codex => CodexProbeContext {
            floor_ms: uuid_v7_timestamp_ms(session_id)
                .map(|created_ms| created_ms - CODEX_PROBE_FLOOR_SLACK_MS),
            cache,
        },
        _ => CodexProbeContext::default(),
    };
    let mut output = Vec::new();
    for relative in roots {
        let root = home.join(relative);
        if root.is_dir() {
            collect_native_tree(
                harness,
                home,
                &root,
                session_id,
                false,
                &mut probe,
                &mut output,
            )?;
        }
    }
    if harness == HarnessKind::Kimi && !output.is_empty() {
        collect_kimi_registry_artifacts(home, session_id, &mut output)?;
    }
    if harness == HarnessKind::Claude {
        collect_claude_memory_artifacts(home, session_id, &mut output)?;
    }
    output.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    ensure!(
        allow_empty
            || native_artifacts_hold_conversation(
                harness,
                output
                    .iter()
                    .map(|artifact| artifact.relative_path.as_path())
            ),
        NO_SESSION_ARTIFACTS
    );
    Ok(output)
}

/// Whether native artifacts, named by their paths in the harness home, hold
/// a conversation the harness can load. The one rule for both sides: a
/// capture refuses a prompted session without one, and a restore asks the
/// harness to load the archived session only when its archive has one.
///
/// Only the harness's own session files count. A checkpoint also carries
/// Mjolnir's attachments, goals and sub-agent reports, and project memory,
/// none of which is a conversation.
pub fn native_artifacts_hold_conversation<'a>(
    harness: HarnessKind,
    relative_paths: impl IntoIterator<Item = &'a Path>,
) -> bool {
    let mut session_files = relative_paths.into_iter().filter(|path| {
        harness
            .native_session_dirs()
            .iter()
            .any(|root| path.starts_with(root))
            && !is_project_memory_artifact(path)
    });
    match harness {
        // agy's `brain` files only support the conversation, which lives in
        // its database; agy loads a conversation without one and fails at the
        // next prompt, so the files alone are no history.
        HarnessKind::Antigravity => session_files.any(|path| {
            is_sqlite_native_database(harness, path) || is_antigravity_legacy_conversation(path)
        }),
        _ => session_files.next().is_some(),
    }
}

/// Whether the archive holds the conversation of the native session it
/// names. A session that never received a prompt can have none: Codex writes
/// a thread's rollout only at its first turn, so its archive names a thread
/// that no harness can load.
pub fn archive_holds_native_conversation(manifest: &crate::archive::ArchiveManifest) -> bool {
    native_artifacts_hold_conversation(
        manifest.session.harness_kind,
        manifest
            .payloads
            .iter()
            .filter_map(|payload| match &payload.role {
                PayloadRole::NativeArtifact { relative_path } => Some(relative_path.as_path()),
                _ => None,
            }),
    )
}

/// `projects/<slug>/memory/...`: Claude's project memory, and Mjolnir's
/// project-memory replica in every harness home, both live there. Claude
/// keeps its transcripts under `projects` too, so the memory has to be told
/// apart from them.
fn is_project_memory_artifact(relative_path: &Path) -> bool {
    let mut components = relative_path.components();
    components.next() == Some(Component::Normal("projects".as_ref()))
        && components.next().is_some()
        && components.next() == Some(Component::Normal("memory".as_ref()))
}

/// Collect native artifacts for an import whose locator already resolved the
/// exact source artifact. Codex rollouts are standalone JSONL files, so this
/// avoids probing every historical rollout (and any unrelated corrupt one).
pub fn collect_import_native_artifacts(
    harness: HarnessKind,
    home: &Path,
    session_id: &str,
    source_path: &Path,
) -> Result<Vec<NativeArtifact>> {
    if harness == HarnessKind::OpenCode {
        use crate::native::opencode;
        let (database, id) = opencode::database_key(source_path)
            .context("OpenCode import requires a migrated SQLite session")?;
        ensure!(
            id == session_id,
            "OpenCode native ID does not match its source"
        );
        let metadata = fs::symlink_metadata(database)?;
        ensure!(
            metadata.is_file(),
            "OpenCode database is not a regular file"
        );
        let data = sqlite_snapshot(database, "OpenCode", |copy| {
            opencode::retain_session(copy, id)
        })?;
        return Ok(vec![NativeArtifact {
            relative_path: PathBuf::from(OPENCODE_DATABASE),
            data: data.into(),
            mode: 0o600,
        }]);
    }
    if harness == HarnessKind::Muse {
        return collect_muse_import_artifacts(home, session_id, source_path);
    }
    if harness != HarnessKind::Codex {
        return collect_native_artifacts(harness, home, session_id, false);
    }
    validate_component(session_id, "native session ID")?;
    let relative = source_path.strip_prefix(home).with_context(|| {
        format!(
            "Codex rollout {} is outside {}",
            source_path.display(),
            home.display()
        )
    })?;
    validate_relative_path(relative)?;
    ensure!(
        matches!(relative.components().next(), Some(Component::Normal(component)) if component == "sessions" || component == "archived_sessions"),
        "Codex rollout '{}' is outside a session root",
        source_path.display()
    );
    let name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    ensure!(
        name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"),
        "Codex rollout '{}' is not a JSONL artifact",
        source_path.display()
    );
    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("stat Codex rollout {}", source_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Codex rollout '{}' is not a regular file",
        source_path.display()
    );
    Ok(vec![NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: mj_core::archive::NativeArtifactData::File(source_path.to_path_buf()),
        mode: file_mode(&metadata),
    }])
}

/// External Muse storage has a separate XDG root. Normalize the selected
/// subtree into the same private layout used by ordinary worker checkpoints.
pub(super) fn collect_muse_import_artifacts(
    home: &Path,
    session_id: &str,
    source_path: &Path,
) -> Result<Vec<NativeArtifact>> {
    validate_component(session_id, "Muse native session ID")?;
    let sessions_root = crate::native::muse_sessions_root(home)?;
    let relative = source_path
        .strip_prefix(&sessions_root)
        .context("Muse session log is outside its native session root")?;
    validate_relative_path(relative)?;
    ensure_no_symlink_ancestors(&sessions_root, relative)?;
    let directory = source_path
        .parent()
        .context("Muse session has no directory")?;
    ensure!(
        source_path
            .file_name()
            .is_some_and(|name| name == "session.jsonl")
            && directory.file_name().is_some_and(|name| name == session_id),
        "Muse native session ID does not match its source path"
    );
    let mut output = Vec::new();
    collect_native_tree(
        HarnessKind::Muse,
        &sessions_root,
        directory,
        session_id,
        false,
        &mut CodexProbeContext::default(),
        &mut output,
    )?;
    ensure!(
        !output.is_empty(),
        "Muse native session contains no durable artifacts"
    );
    for artifact in &mut output {
        artifact.relative_path = Path::new(".data/muse/sessions").join(&artifact.relative_path);
    }
    output.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(output)
}

pub(super) fn collect_kimi_registry_artifacts(
    home: &Path,
    session_id: &str,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let source_workspace = output
        .iter()
        .find_map(|artifact| kimi_source_workspace(&artifact.relative_path, session_id))
        .context("Kimi native session state artifact is missing")?;
    let workspaces_path = home.join("workspaces.json");
    match fs::symlink_metadata(&workspaces_path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "Kimi workspace registry is not a regular file"
            );
            let workspaces: Value = serde_json::from_slice(&fs::read(&workspaces_path)?)
                .context("parse Kimi workspace registry")?;
            let workspace = workspaces
                .get("workspaces")
                .and_then(|entries| entries.get(&source_workspace))
                .cloned()
                .with_context(|| {
                    format!("Kimi workspace {source_workspace:?} is missing from registry")
                })?;
            let mut selected_workspaces = serde_json::Map::new();
            selected_workspaces.insert(source_workspace, workspace);
            output.push(NativeArtifact {
                relative_path: PathBuf::from("workspaces.json"),
                data: (serde_json::to_vec(&json!({
                    "version": workspaces.get("version").cloned().unwrap_or(Value::Null),
                    "deleted_workspace_ids": [],
                    "workspaces": selected_workspaces,
                }))?)
                .into(),
                mode: file_mode(&metadata),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Kimi workspace registry {}", workspaces_path.display())
            });
        }
    }

    let index_path = home.join("session_index.jsonl");
    match fs::symlink_metadata(&index_path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_file(),
                "Kimi session index is not a regular file"
            );
            let mut selected = Vec::new();
            for (line_number, line) in BufReader::new(File::open(&index_path)?).lines().enumerate()
            {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry: Value = serde_json::from_str(&line).with_context(|| {
                    format!("parse Kimi session index line {}", line_number + 1)
                })?;
                if entry.get("sessionId").and_then(Value::as_str) == Some(session_id) {
                    serde_json::to_writer(&mut selected, &entry)?;
                    selected.push(b'\n');
                }
            }
            ensure!(
                !selected.is_empty(),
                "Kimi session index does not contain native session {session_id:?}"
            );
            output.push(NativeArtifact {
                relative_path: PathBuf::from("session_index.jsonl"),
                data: (selected).into(),
                mode: file_mode(&metadata),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read Kimi session index {}", index_path.display()));
        }
    }
    Ok(())
}

pub(super) fn kimi_source_workspace(relative_path: &Path, session_id: &str) -> Option<String> {
    let mut components = relative_path.components();
    (components.next() == Some(Component::Normal("sessions".as_ref()))).then_some(())?;
    let workspace = components.next()?.as_os_str().to_str()?.to_owned();
    let session = components.next()?.as_os_str().to_str()?;
    let file = components.next()?.as_os_str().to_str()?;
    (components.next().is_none()
        && file == "state.json"
        && (session == session_id || session == format!("session_{session_id}")))
    .then_some(workspace)
}

/// Claude keeps per-project memory next to the transcripts, outside the
/// session-id subtree the main pass walks, so capture it in a post-pass.
///
/// The memory directory is scoped to the slug that owns this session's
/// transcript. On a LocalBare target the harness home is the user's real
/// `~/.claude`, which holds memory for every unrelated project; only the
/// session's own project memory may leave the machine.
pub(super) fn collect_claude_memory_artifacts(
    home: &Path,
    session_id: &str,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let mut slugs: Vec<String> = output
        .iter()
        .filter_map(|artifact| claude_session_project_slug(&artifact.relative_path, session_id))
        .collect();
    slugs.sort();
    slugs.dedup();
    // An unprompted session exported with `allow_empty` has no transcript, so
    // there is no project to scope memory to.
    for slug in slugs {
        let root = home.join("projects").join(&slug).join("memory");
        if root.is_dir() {
            collect_claude_memory_tree(home, &root, output)?;
        }
    }
    Ok(())
}

/// Return the project slug when `relative_path` is this session's transcript
/// (`projects/<slug>/<session_id>.jsonl`) or lives in its session subtree
/// (`projects/<slug>/<session_id>/...`).
pub(super) fn claude_session_project_slug(
    relative_path: &Path,
    session_id: &str,
) -> Option<String> {
    let mut components = relative_path.components();
    (components.next() == Some(Component::Normal("projects".as_ref()))).then_some(())?;
    let slug = components.next()?.as_os_str().to_str()?.to_owned();
    let entry = components.next()?.as_os_str().to_str()?;
    let is_transcript = entry == format!("{session_id}.jsonl") && components.next().is_none();
    (is_transcript || entry == session_id).then_some(slug)
}

pub(super) fn collect_claude_memory_tree(
    home: &Path,
    path: &Path,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_claude_memory_tree(home, &entry?.path(), output)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    let relative = path.strip_prefix(home)?;
    validate_relative_path(relative)?;
    output.push(NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: mj_core::archive::NativeArtifactData::File(path.to_path_buf()),
        mode: file_mode(&metadata),
    });
    Ok(())
}

pub(super) fn collect_native_tree(
    harness: HarnessKind,
    home: &Path,
    path: &Path,
    session_id: &str,
    inside_session: bool,
    probe: &mut CodexProbeContext<'_>,
    output: &mut Vec<NativeArtifact>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let inside = inside_session
        || path.file_name().is_some_and(|name| {
            name == session_id
                || (harness == HarnessKind::Kimi
                    && name.to_str() == Some(&format!("session_{session_id}")))
        });
    if metadata.is_dir() {
        if harness == HarnessKind::Antigravity
            && path.strip_prefix(home).ok() == Some(antigravity_scratch(session_id).as_path())
        {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            collect_native_tree(
                harness,
                home,
                &entry?.path(),
                session_id,
                inside,
                probe,
                output,
            )?;
        }
        return Ok(());
    }
    ensure!(metadata.is_file(), "native artifact is not a regular file");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let relative = path.strip_prefix(home)?;
    let selected = match harness {
        HarnessKind::Codex => {
            (name.contains(session_id)
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")))
                || (name.ends_with(".jsonl")
                    && codex_probe_selects(probe, path, relative, &metadata, session_id))
        }
        HarnessKind::Claude => inside || name == format!("{session_id}.jsonl"),
        HarnessKind::Kimi => inside && kimi_session_artifact(relative, session_id),
        HarnessKind::Grok => inside && grok_session_artifact(relative, session_id),
        HarnessKind::Muse => inside && name == "session.jsonl",
        HarnessKind::OpenCode => is_opencode_native_artifact(relative),
        HarnessKind::Antigravity => is_antigravity_native_artifact(relative, session_id),
        HarnessKind::Dsh => {
            is_dsh_session_record(relative, session_id) || is_dsh_native_log(relative, session_id)
        }
    };
    if !selected {
        return Ok(());
    }
    validate_relative_path(relative)?;
    // SQLite snapshots use the backup API; their database representation stays in memory.
    let database = is_sqlite_native_database(harness, relative);
    let data = if database {
        sqlite_native_database_snapshot(harness, path)?
    } else {
        Vec::new()
    };
    output.push(NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: if database {
            data.into()
        } else {
            mj_core::archive::NativeArtifactData::File(path.to_path_buf())
        },
        mode: file_mode(&metadata),
    });
    Ok(())
}

/// dsh-acp writes each session to `sessions/<id>.json` by renaming a
/// `.<id>.<suffix>.tmp` file over it, so the record itself is complete and an
/// interrupted write's temporary file is never captured.
pub(super) fn is_dsh_session_record(relative_path: &Path, session_id: &str) -> bool {
    relative_path == Path::new("sessions").join(format!("{session_id}.json"))
}

/// A DSH native session log lives at
/// `dsh-home/sessions/<project-key>/<session-id>/session.v4.jsonl.zstd`.
pub(super) fn is_dsh_native_log(relative_path: &Path, session_id: &str) -> bool {
    dsh_native_log_parts(relative_path).is_some_and(|session| session == session_id)
}

pub(super) fn is_dsh_native_log_path(relative_path: &Path) -> bool {
    dsh_native_log_parts(relative_path).is_some()
}

fn dsh_native_log_parts(relative_path: &Path) -> Option<&str> {
    let mut components = relative_path.components();
    if components.next() != Some(Component::Normal("dsh-home".as_ref())) {
        return None;
    }
    if components.next() != Some(Component::Normal("sessions".as_ref())) {
        return None;
    }
    components.next()?;
    let session = components.next()?.as_os_str().to_str()?;
    let log = components.next()?.as_os_str();
    if components.next().is_some() || log != "session.v4.jsonl.zstd" {
        return None;
    }
    Some(session)
}

/// OpenCode's database within a session home.
pub(super) use crate::native::opencode::DATABASE as OPENCODE_DATABASE;

/// OpenCode keeps every session of a data directory in one SQLite database,
/// [`OPENCODE_DATABASE`]. A session home has exactly one data directory, so
/// the database is the session's native state.
pub(super) fn is_opencode_native_artifact(relative_path: &Path) -> bool {
    relative_path == Path::new(OPENCODE_DATABASE)
}

/// Where agy keeps one SQLite database per conversation, named by the ACP
/// session ID, and the directory per conversation beside it.
pub(super) const ANTIGRAVITY_CONVERSATIONS: &str = "antigravity-cli/conversations";
pub(super) const ANTIGRAVITY_BRAIN: &str = "antigravity-cli/brain";

/// A native session's own agy files: its conversation database, and
/// everything under its `brain` directory (the full text of long tool output,
/// which the conversation points the model at, and agy's artifacts). Never
/// the login, the logs, the indexes shared by all conversations, or another
/// conversation's files. The walk never enters the conversation's
/// [`antigravity_scratch`] directory. The database's `-wal` and `-shm` files
/// are not selected: the snapshot taken of the database already holds their
/// pages.
pub(super) fn is_antigravity_native_artifact(relative_path: &Path, session_id: &str) -> bool {
    relative_path == Path::new(ANTIGRAVITY_CONVERSATIONS).join(format!("{session_id}.db"))
        || relative_path == Path::new(ANTIGRAVITY_CONVERSATIONS).join(format!("{session_id}.pb"))
        || relative_path.starts_with(Path::new(ANTIGRAVITY_BRAIN).join(session_id))
}

/// The agent's own working directory inside a conversation's `brain`, kept
/// out of checkpoints like `/tmp`, so `collect_native_tree` does not descend
/// into it. Agents fill it with throwaway copies: one held 101 copies of a
/// project tree, 260,000 files in 11 GB.
fn antigravity_scratch(session_id: &str) -> PathBuf {
    Path::new(ANTIGRAVITY_BRAIN)
        .join(session_id)
        .join("scratch")
}

fn is_antigravity_legacy_conversation(path: &Path) -> bool {
    path.parent() == Some(Path::new(ANTIGRAVITY_CONVERSATIONS))
        && path.extension().is_some_and(|ext| ext == "pb")
}

/// Whether `relative_path` is a SQLite database a checkpoint carries as a
/// snapshot, which is also the one rule for what a restore must not leave
/// beside the database it replaces ([`sqlite_sidecars`]).
pub(super) fn is_sqlite_native_database(harness: HarnessKind, relative_path: &Path) -> bool {
    match harness {
        HarnessKind::OpenCode => is_opencode_native_artifact(relative_path),
        HarnessKind::Antigravity => {
            relative_path.parent() == Some(Path::new(ANTIGRAVITY_CONVERSATIONS))
                && relative_path.extension().is_some_and(|ext| ext == "db")
        }
        _ => false,
    }
}

/// The files SQLite keeps beside a database while it is open: the
/// write-ahead log, whose pages SQLite replays over the database when it next
/// opens it, and the shared-memory index of that log. A snapshot already
/// holds the log's pages, so a restore removes any it finds beside the
/// database it writes; an old log replayed over the restored pages would
/// corrupt them.
pub(super) fn sqlite_sidecars(database: &Path) -> [PathBuf; 2] {
    let beside = |suffix: &str| {
        let mut name = database.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [beside("-wal"), beside("-shm")]
}

/// The tables in which OpenCode keeps logins: provider and connector
/// credentials, and the tokens of OpenCode's own accounts.
const OPENCODE_LOGIN_TABLES: [&str; 3] = ["credential", "account", "control_account"];

/// A database as a checkpoint carries it. OpenCode 2 keeps its logins in the
/// same file as its sessions, and every launch imports the profile's logins
/// into the staged home afresh, so a checkpoint needs none of them and must
/// not carry their secrets. agy's logins are in another file, so its
/// conversation database needs nothing removed.
fn sqlite_native_database_snapshot(harness: HarnessKind, path: &Path) -> Result<Vec<u8>> {
    match harness {
        HarnessKind::OpenCode => sqlite_snapshot(path, "OpenCode", |copy| {
            for table in OPENCODE_LOGIN_TABLES {
                let exists: bool = copy.query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )?;
                if exists {
                    copy.execute(&format!("DELETE FROM \"{table}\""), [])?;
                }
            }
            // VACUUM rebuilds the file, so no freed page keeps a deleted login.
            copy.execute_batch("VACUUM;")?;
            Ok(())
        }),
        _ => sqlite_snapshot(path, harness.display_name(), |_| Ok(())),
    }
}

/// A consistent copy of a database a harness may be writing, write-ahead log
/// included, with `edit` applied to the copy. SQLite's backup API reads every
/// page in one transaction, so the copy holds exactly what a reader of the
/// live database sees; copying the file alone would leave out whatever is
/// still in the log.
pub(super) fn sqlite_snapshot(
    path: &Path,
    label: &str,
    edit: impl FnOnce(&rusqlite::Connection) -> Result<()>,
) -> Result<Vec<u8>> {
    let snapshot = sqlite_snapshot_file(path, label, edit)?;
    fs::read(snapshot.path()).with_context(|| {
        format!(
            "read {label} database snapshot {}",
            snapshot.path().display()
        )
    })
}

/// The same consistent copy, left in a private temporary file.
pub(super) fn sqlite_snapshot_file(
    path: &Path,
    label: &str,
    edit: impl FnOnce(&rusqlite::Connection) -> Result<()>,
) -> Result<tempfile::NamedTempFile> {
    let scratch = tempfile::Builder::new()
        .prefix("snapshot")
        .tempfile()
        .with_context(|| format!("create {label} snapshot file"))?;
    let snapshot_path = scratch.path().to_path_buf();
    let snapshot = || -> Result<()> {
        let source = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // The running harness may hold a brief write lock.
        source.busy_timeout(std::time::Duration::from_secs(5))?;
        // SQLite's backup takes the destination as a path, whatever its
        // encoding. Copying every page in one step reads them all in one
        // transaction, so the copy is consistent, write-ahead log included.
        let mut copy = rusqlite::Connection::open(&snapshot_path)?;
        {
            use rusqlite::backup::{Backup, StepResult};
            let backup = Backup::new(&source, &mut copy)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match backup.step(-1)? {
                    StepResult::Done => break,
                    // A writer held the database; wait it out, briefly.
                    _ if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    other => bail!("{label}'s database stayed busy ({other:?})"),
                }
            }
        }
        drop(source);
        edit(&copy)?;
        // A copy in WAL mode keeps its pages in a log until a checkpoint moves
        // them into the file itself, which is all the checkpoint carries.
        copy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    };
    snapshot().with_context(|| format!("snapshot {label} database {}", path.display()))?;
    Ok(scratch)
}

pub(super) fn kimi_session_artifact(relative: &Path, session_id: &str) -> bool {
    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(session_index) = components.iter().position(|component| {
        *component == session_id || *component == format!("session_{session_id}")
    }) else {
        return false;
    };
    matches!(&components[session_index + 1..], ["state.json"])
        || matches!(
            &components[session_index + 1..],
            ["agents", _, "wire.jsonl"]
        )
}

/// Content-probe fallback for a root rollout whose filename no longer carries
/// its resumable thread ID, for example after Codex archives or renames it.
/// Opening every rollout costs gigabytes of reads on a busy `~/.codex`, so two
/// gates come first.
///
/// The mtime floor is filesystem truth: rollout filenames encode ambiguous
/// local time, historical rollouts are never rewritten, and hel's own restore
/// rewrites the files it installs with a fresh mtime. A rollout last modified
/// before the session was created cannot mention that session.
pub(super) fn codex_probe_selects(
    probe: &mut CodexProbeContext<'_>,
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    session_id: &str,
) -> bool {
    // An unreadable mtime or a non-UUIDv7 session ID fails open into the probe.
    if let Some(floor_ms) = probe.floor_ms
        && let Ok(modified) = metadata.modified()
        && unix_millis(modified).is_some_and(|modified_ms| modified_ms < floor_ms)
    {
        return false;
    }
    if probe
        .cache
        .as_ref()
        .is_some_and(|cache| cache.not_ours.contains(relative))
    {
        return false;
    }
    if codex_rollout_has_thread_id(path, session_id) {
        return true;
    }
    if let Some(cache) = probe.cache.as_mut() {
        cache.not_ours.insert(relative.to_path_buf());
    }
    false
}

pub(super) fn codex_rollout_has_thread_id(path: &Path, thread_id: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            return false;
        };
        if read == 0 {
            break;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            return false;
        };
        // A rollout carries exactly one `session_meta` header, so the first one
        // settles the question without parsing the rest of the file.
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            let Some(payload) = record.get("payload") else {
                return false;
            };
            // Modern Codex stores the resumable thread ID in `id` and a shared
            // session-tree ID in `session_id`. Child agents therefore have the
            // same `session_id` as the root but must not be archived as roots.
            // Only old records with no `id` field use the legacy fallback.
            if let Some(id) = payload.get("id") {
                return id.as_str() == Some(thread_id);
            }
            return payload.get("session_id").and_then(Value::as_str) == Some(thread_id);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_capture_requires_its_own_conversation_and_keeps_legacy_payload() {
        let home = tempfile::tempdir().unwrap();
        let id = "literal-id";
        let brain = Path::new(ANTIGRAVITY_BRAIN).join(id);
        let log = brain.join("transcript.jsonl");
        let other = Path::new(ANTIGRAVITY_CONVERSATIONS).join("other.pb");
        fs::create_dir_all(home.path().join(brain)).unwrap();
        fs::create_dir_all(home.path().join(ANTIGRAVITY_CONVERSATIONS)).unwrap();
        fs::write(home.path().join(&log), b"transcript").unwrap();
        fs::write(home.path().join(other), b"unrelated conversation").unwrap();
        let error =
            collect_native_artifacts(HarnessKind::Antigravity, home.path(), id, false).unwrap_err();
        assert!(error.to_string().contains(NO_SESSION_ARTIFACTS));

        let conversation = Path::new(ANTIGRAVITY_CONVERSATIONS).join(format!("{id}.pb"));
        fs::write(home.path().join(&conversation), b"legacy conversation").unwrap();
        let scratch = home.path().join(antigravity_scratch(id)).join("copy");
        fs::create_dir_all(&scratch).unwrap();
        // Hard-won: suspend failed with "archive manifest is too large" when
        // an agent's scratch held 260,000 files, each captured as a payload.
        fs::write(scratch.join("Cargo.lock"), b"throwaway copy").unwrap();
        let artifacts =
            collect_native_artifacts(HarnessKind::Antigravity, home.path(), id, false).unwrap();
        assert_eq!(
            artifacts
                .into_iter()
                .map(|artifact| artifact.relative_path)
                .collect::<Vec<_>>(),
            [log, conversation],
        );
    }
}
