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
    ensure!(allow_empty || !output.is_empty(), NO_SESSION_ARTIFACTS);
    let total = output
        .iter()
        .try_fold(0_u64, |total, artifact| {
            total.checked_add(artifact.data.len() as u64)
        })
        .context("native artifact size overflow")?;
    ensure!(
        total <= MAX_NATIVE_TOTAL,
        "native session artifacts are too large"
    );
    Ok(output)
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
    ensure!(
        !is_secret_like_path(relative),
        "Codex rollout '{}' has a forbidden path",
        source_path.display()
    );
    let metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("stat Codex rollout {}", source_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Codex rollout '{}' is not a regular file",
        source_path.display()
    );
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "Codex rollout is too large"
    );
    Ok(vec![NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: fs::read(source_path)
            .with_context(|| format!("read Codex rollout {}", source_path.display()))?,
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
    let total = output
        .iter()
        .try_fold(0u64, |total, artifact| {
            total.checked_add(artifact.data.len() as u64)
        })
        .context("Muse native artifact size overflow")?;
    ensure!(
        total <= MAX_NATIVE_TOTAL,
        "Muse native session artifacts are too large"
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
    let metadata = fs::symlink_metadata(&workspaces_path)
        .with_context(|| format!("read Kimi workspace registry {}", workspaces_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Kimi workspace registry is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "Kimi workspace registry is too large"
    );
    let workspaces: Value = serde_json::from_slice(&fs::read(&workspaces_path)?)
        .context("parse Kimi workspace registry")?;
    let workspace = workspaces
        .pointer(&format!("/workspaces/{source_workspace}"))
        .cloned()
        .with_context(|| format!("Kimi workspace {source_workspace:?} is missing from registry"))?;
    let mut selected_workspaces = serde_json::Map::new();
    selected_workspaces.insert(source_workspace, workspace);
    output.push(NativeArtifact {
        relative_path: PathBuf::from("workspaces.json"),
        data: serde_json::to_vec(&json!({
            "version": workspaces.get("version").cloned().unwrap_or(Value::Null),
            "deleted_workspace_ids": [],
            "workspaces": selected_workspaces,
        }))?,
        mode: file_mode(&metadata),
    });

    let index_path = home.join("session_index.jsonl");
    let metadata = fs::symlink_metadata(&index_path)
        .with_context(|| format!("read Kimi session index {}", index_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "Kimi session index is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "Kimi session index is too large"
    );
    let mut selected = Vec::new();
    for (line_number, line) in fs::read_to_string(&index_path)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line)
            .with_context(|| format!("parse Kimi session index line {}", line_number + 1))?;
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
        data: selected,
        mode: file_mode(&metadata),
    });
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
    if is_secret_like_path(relative) {
        return Ok(());
    }
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "native artifact is too large"
    );
    validate_relative_path(relative)?;
    output.push(NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: fs::read(path)?,
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
    };
    if !selected || is_secret_like_path(relative) {
        return Ok(());
    }
    ensure!(
        metadata.len() <= MAX_NATIVE_FILE,
        "native artifact is too large"
    );
    validate_relative_path(relative)?;
    output.push(NativeArtifact {
        relative_path: relative.to_path_buf(),
        data: fs::read(path)?,
        mode: file_mode(&metadata),
    });
    Ok(())
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
