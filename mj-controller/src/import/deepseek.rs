//! Native DeepSeek Harness session discovery and transcript projection.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

const ACTIVE_SESSION_REASON: &str = "DeepSeek session is active or contains an incomplete turn";
const NATIVE_LOCK_REASON: &str = "DeepSeek session is locked by a running harness";

#[derive(Debug)]
struct Candidate {
    native_session_id: String,
    path: PathBuf,
    modified_at: SystemTime,
    size_bytes: u64,
    cwd: PathBuf,
    title: String,
    unavailable_reason: Option<&'static str>,
}

/// Locate one DSH session by native id or newest modified time.
pub(super) fn locate(
    home: &Path,
    selection: &ClaudeSessionSelection,
) -> Result<LocatedNativeSession> {
    let candidates = candidates(home)?;
    let candidate = match selection {
        ClaudeSessionSelection::NativeSessionId(id) => candidates
            .into_iter()
            .find(|candidate| candidate.native_session_id == *id)
            .with_context(|| {
                format!(
                    "DeepSeek session {id:?} was not found under {}",
                    home.display()
                )
            })?,
        ClaudeSessionSelection::Latest => candidates
            .into_iter()
            .next()
            .context("no DeepSeek session logs were found")?,
    };
    if let Some(reason) = candidate.unavailable_reason {
        bail!(
            "DeepSeek session {:?} cannot be imported: {}",
            candidate.native_session_id,
            reason
        );
    }
    Ok(LocatedNativeSession {
        native_session_id: candidate.native_session_id,
        source_path: candidate.path,
    })
}

/// Scan DSH v0 sessions newest first.
pub(super) fn scan(
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<NativeSessionListing>),
) -> Result<()> {
    let candidates = candidates(home)?;
    let total = candidates.len();
    report(SessionScanProgress {
        scanned: 0,
        total,
        session: None,
    });
    for (index, candidate) in candidates.into_iter().enumerate() {
        report(SessionScanProgress {
            scanned: index + 1,
            total,
            session: Some(NativeSessionListing {
                native_session_id: candidate.native_session_id,
                title: candidate.title,
                modified_at: candidate.modified_at,
                git_branch: git_branch_or_head(&candidate.cwd),
                size_bytes: candidate.size_bytes,
                cwd: candidate.cwd,
                unavailable_reason: candidate.unavailable_reason,
                natively_archived: false,
            }),
        });
    }
    Ok(())
}

/// Read a native DSH log into Hel's lossy chat projection.
pub(super) fn read_transcript(path: &Path) -> Result<ClaudeTranscript> {
    ensure!(
        mj_core::native::deepseek::is_session_log(path),
        "unsupported DeepSeek session artifact path {}",
        path.display()
    );
    if let Some(generation) = mj_core::native::deepseek::session_generation(path)
        && generation != "0"
    {
        bail!(
            "DeepSeek session log {} uses unsupported format generation v{}; upgrade the harness",
            path.display(),
            generation
        );
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat DeepSeek session {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "DeepSeek session is not a regular file: {}",
        path.display()
    );
    if native_lock_present(path)? {
        bail!(
            "DeepSeek session {} cannot be imported: {}",
            path.display(),
            NATIVE_LOCK_REASON
        );
    }
    let data =
        fs::read(path).with_context(|| format!("read DeepSeek session {}", path.display()))?;
    let log = mj_core::native::deepseek::read(path, &data)?;
    validate_path_identity(path, &log.header)?;
    ensure_top_level(&log.header)?;
    ensure!(
        !has_open_turn(&log.events),
        "DeepSeek session {} cannot be imported: {}",
        path.display(),
        ACTIVE_SESSION_REASON
    );

    let cwd = header_cwd(&log.header)?;
    let mut events = Vec::new();
    let mut edited_calls = BTreeMap::<String, PathBuf>::new();
    let mut completed_calls = BTreeSet::new();
    let mut assistant_ids = BTreeSet::new();
    let mut saw_user = false;

    for record in &log.events {
        let recorded_at_ms = dsh_event_time(record);
        match record.get("type").and_then(Value::as_str) {
            Some("user/message") if is_human_message(record) => {
                let text = record
                    .get("data")
                    .map(message_text)
                    .unwrap_or_else(|| message_text(record));
                let text = strip_hidden_prompt_context(&text);
                if text.trim().is_empty() {
                    continue;
                }
                finish_imported_turn(&mut events, None);
                let request_id = format!("import-{}", events.len() + 1);
                push_event(
                    &mut events,
                    recorded_at_ms,
                    WorkerEvent::PromptAccepted {
                        request_id,
                        text: text.to_owned(),
                        attachments: Vec::new(),
                    },
                );
                saw_user = true;
            }
            Some("assistant/message") => {
                let Some(message) = record.pointer("/data/message") else {
                    continue;
                };
                let Some(id) = message.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // Packed assistant chunks and their assembled message both
                // occur in normal DSH logs.  Only project an assembled
                // message, and guard against duplicate copies of one id.
                if !assistant_ids.insert(id.to_owned()) {
                    continue;
                }
                for text in message_text_parts(message) {
                    push_event(
                        &mut events,
                        recorded_at_ms,
                        WorkerEvent::Adapter {
                            kind: "session_update".into(),
                            payload: serde_json::json!({
                                "type": "session_update",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": text},
                                },
                            }),
                        },
                    );
                }
            }
            Some("tool/call") => {
                if let (Some(call_id), Some(name), Some(arguments)) = (
                    record.pointer("/data/callId").and_then(Value::as_str),
                    record.pointer("/data/name").and_then(Value::as_str),
                    record.pointer("/data/arguments").and_then(Value::as_str),
                ) && is_file_edit_tool(name)
                    && let Some(path) = edited_path(arguments)
                {
                    edited_calls.insert(call_id.to_owned(), path);
                }
            }
            Some("tool/result") => {
                let Some(message) = record.pointer("/data/message") else {
                    continue;
                };
                let Some(call_id) = message.pointer("/source/callId").and_then(Value::as_str)
                else {
                    continue;
                };
                if successful_tool_result(message) {
                    completed_calls.insert(call_id.to_owned());
                }
            }
            Some("turn/end") => finish_imported_turn(&mut events, recorded_at_ms),
            _ => {}
        }
    }

    ensure!(
        saw_user,
        "DeepSeek session {} contains no importable user messages",
        path.display()
    );
    finish_imported_turn(&mut events, None);
    finalize_import_event_times(&mut events, path)?;
    let edited_paths = edited_calls
        .into_iter()
        .filter(|(call_id, _)| completed_calls.contains(call_id))
        .map(|(_, path)| path)
        .collect();
    Ok(ClaudeTranscript {
        cwd,
        edited_paths,
        events,
    })
}

fn candidates(home: &Path) -> Result<Vec<Candidate>> {
    let sessions = home.join("sessions");
    ensure!(
        sessions.is_dir(),
        "DeepSeek sessions directory is missing: {}",
        sessions.display()
    );
    let mut output = Vec::new();
    let mut ids = BTreeSet::new();
    for project_entry in fs::read_dir(&sessions)
        .with_context(|| format!("read DeepSeek sessions directory {}", sessions.display()))?
    {
        let project = project_entry?.path();
        let project_metadata = fs::symlink_metadata(&project)?;
        if project_metadata.file_type().is_symlink() || !project_metadata.is_dir() {
            continue;
        }
        for session_entry in fs::read_dir(&project)? {
            let directory = session_entry?.path();
            let directory_metadata = fs::symlink_metadata(&directory)?;
            if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
                continue;
            }
            let Some(path) = find_log(&directory)? else {
                continue;
            };
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                continue;
            }
            let data = fs::read(&path)
                .with_context(|| format!("read DeepSeek session {}", path.display()))?;
            let log = mj_core::native::deepseek::read(&path, &data)?;
            validate_path_identity(&path, &log.header)?;
            if is_child(&log.header) {
                continue;
            }
            let id = header_id(&log.header)?;
            ensure!(
                ids.insert(id.clone()),
                "duplicate DeepSeek session id {:?} appears in multiple project directories",
                id
            );
            let cwd = header_cwd(&log.header)?;
            let title = session_title(&log.events).unwrap_or_else(|| id.clone());
            let unavailable_reason = if native_lock_present(&path)? {
                Some(NATIVE_LOCK_REASON)
            } else if has_open_turn(&log.events) {
                Some(ACTIVE_SESSION_REASON)
            } else {
                None
            };
            output.push(Candidate {
                native_session_id: id,
                path,
                modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size_bytes: metadata.len(),
                cwd,
                title,
                unavailable_reason,
            });
        }
    }
    output.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.path.cmp(&left.path))
    });
    Ok(output)
}

fn native_lock_present(path: &Path) -> Result<bool> {
    let parent = path
        .parent()
        .context("DeepSeek session log has no parent directory")?;
    let filename = path
        .file_name()
        .context("DeepSeek session log has no file name")?
        .to_os_string();
    let file_lock = parent.join({
        let mut lock_name = filename;
        lock_name.push(".lock");
        lock_name
    });
    let markers = [file_lock, parent.join("session.lock")];
    for marker in markers {
        match fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("stat DeepSeek session lock {}", marker.display()));
            }
        }
    }
    Ok(false)
}

fn find_log(directory: &Path) -> Result<Option<PathBuf>> {
    let mut current = None;
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        if !mj_core::native::deepseek::is_session_log(&path) {
            continue;
        }
        let generation = mj_core::native::deepseek::session_generation(&path)
            .context("DeepSeek session log generation is invalid")?;
        if generation != "0" {
            bail!(
                "DeepSeek session log {} uses unsupported format generation v{}; upgrade the harness",
                path.display(),
                generation
            );
        }
        if current.is_some() {
            bail!(
                "DeepSeek session directory {} contains both plaintext and Zstandard logs",
                directory.display()
            );
        }
        current = Some(path);
    }
    Ok(current)
}

fn validate_path_identity(path: &Path, header: &Value) -> Result<()> {
    let id = header_id(header)?;
    let cwd = header_cwd(header)?;
    let session_dir = path
        .parent()
        .context("DeepSeek session log has no session directory")?;
    let encoded_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("DeepSeek session directory name is not UTF-8")?;
    ensure!(
        mj_core::native::deepseek::decode_segment(encoded_id).as_deref() == Some(id.as_str()),
        "corrupt DeepSeek session log {}: header id {:?} does not match its directory",
        path.display(),
        id
    );
    let project_dir = session_dir
        .parent()
        .context("DeepSeek session directory has no project directory")?;
    let expected_project = mj_core::native::deepseek::project_key(&cwd)?;
    ensure!(
        project_dir.file_name().and_then(|name| name.to_str()) == Some(expected_project.as_str()),
        "corrupt DeepSeek session log {}: header cwd {} does not match its project directory",
        path.display(),
        cwd.display()
    );
    Ok(())
}

fn header_id(header: &Value) -> Result<String> {
    header
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .context("DeepSeek session header id is missing")
}

fn header_cwd(header: &Value) -> Result<PathBuf> {
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
        .map(PathBuf::from)
        .context("DeepSeek session header cwd is missing")?;
    ensure!(
        cwd.is_absolute(),
        "DeepSeek session cwd is not absolute: {}",
        cwd.display()
    );
    Ok(cwd)
}

fn ensure_top_level(header: &Value) -> Result<()> {
    ensure!(
        !is_child(header),
        "DeepSeek subagent sessions cannot be imported as top-level sessions"
    );
    Ok(())
}

fn is_child(header: &Value) -> bool {
    header.get("parentSession").is_some()
        || header.get("origin").and_then(Value::as_str) == Some("subagent")
        || header
            .get("delegationDepth")
            .and_then(Value::as_u64)
            .is_some_and(|depth| depth > 0)
}

fn has_open_turn(events: &[Value]) -> bool {
    let mut open = false;
    for event in events {
        match event.get("type").and_then(Value::as_str) {
            Some("turn/start") => open = true,
            Some("turn/end") => open = false,
            _ => {}
        }
    }
    open
}

fn session_title(events: &[Value]) -> Option<String> {
    events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("session/title"))
        .filter_map(|event| event.pointer("/data/title").and_then(Value::as_str))
        .filter_map(normalize_session_title)
        .next_back()
}

fn is_human_message(record: &Value) -> bool {
    let Some(source) = record.pointer("/data/source").and_then(Value::as_object) else {
        return false;
    };
    source.get("kind").and_then(Value::as_str) == Some("user") && !source.contains_key("form")
}

fn message_text(record: &Value) -> String {
    message_text_parts(record).collect::<Vec<_>>().join("\n")
}

fn message_text_parts(record: &Value) -> impl Iterator<Item = &str> {
    record
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
}

fn successful_tool_result(message: &Value) -> bool {
    if message.get("isError").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return false;
    };
    !content
        .iter()
        .any(|block| block.get("isError").and_then(Value::as_bool) == Some(true))
}

fn is_file_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "write" | "edit" | "apply_patch" | "Write" | "Edit" | "ApplyPatch"
    )
}

fn edited_path(arguments: &str) -> Option<PathBuf> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    ["file_path", "path", "filePath", "filename"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(PathBuf::from))
}

fn dsh_event_time(record: &Value) -> Option<i64> {
    record.get("time").and_then(Value::as_i64).or_else(|| {
        record
            .get("time")
            .and_then(Value::as_u64)
            .and_then(|time| i64::try_from(time).ok())
    })
}
