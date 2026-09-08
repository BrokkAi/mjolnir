use std::collections::{BTreeMap, BTreeSet};

use hel::hel_native::muse::{MuseRecord, read_records};

use super::*;

const MUSE_ACTIVE_REASON: &str = "Muse session has an active or incomplete turn";
const MUSE_UNAVAILABLE_REASON: &str = "Muse session is corrupt or uses an unsupported format";
const MUSE_DUPLICATE_REASON: &str = "Muse session id occurs in multiple session directories";
const MAX_MUSE_SESSION_FILE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug)]
struct MuseCandidate {
    native_session_id: String,
    session_path: PathBuf,
    modified_at: SystemTime,
    size_bytes: u64,
}

#[derive(Debug)]
struct MuseSummary {
    native_session_id: String,
    cwd: PathBuf,
    title: String,
    edited_paths: Vec<PathBuf>,
    active: bool,
}

/// Locate a Muse session under its native date/session directory layout.
pub(super) fn locate(
    sessions_root: &Path,
    selection: &ClaudeSessionSelection,
) -> Result<LocatedNativeSession> {
    let mut candidates = muse_candidates(sessions_root)?;
    candidates.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.session_path.cmp(&left.session_path))
    });
    let candidate = match selection {
        ClaudeSessionSelection::NativeSessionId(native_session_id) => {
            validate_id("Muse session", native_session_id)?;
            let matches = candidates
                .iter()
                .filter(|candidate| candidate.native_session_id == *native_session_id)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => {
                    bail!(
                        "Muse session {native_session_id:?} was not found under {}",
                        sessions_root.display()
                    )
                }
                [_] => matches[0],
                _ => {
                    bail!(
                        "Muse session {native_session_id:?} occurs in multiple session directories under {}",
                        sessions_root.display()
                    )
                }
            }
        }
        ClaudeSessionSelection::Latest => candidates
            .first()
            .context("no Muse session JSONL files were found")?,
    };
    let summary = muse_summary(&candidate.session_path)
        .with_context(|| format!("read Muse session {}", candidate.session_path.display()))?;
    ensure!(
        summary.native_session_id == candidate.native_session_id,
        "Muse session stream id does not match its directory"
    );
    ensure!(
        !summary.active,
        "Muse session {:?} has an active or incomplete turn; wait for its native terminal event before importing",
        summary.native_session_id
    );
    // Projection performs the stricter importability checks (including a
    // recoverable user prompt and absolute workspace root) before selection
    // reaches archive publication.
    read_transcript(&candidate.session_path)
        .with_context(|| format!("validate Muse session {}", candidate.session_path.display()))?;
    Ok(LocatedNativeSession {
        native_session_id: candidate.native_session_id.clone(),
        source_path: candidate.session_path.clone(),
    })
}

/// Scan only `YYYY/MM/DD/<session-id>/session.jsonl` files directly below the
/// sessions root.  `subagent/` descendants are native child streams and are
/// captured with their parent rather than listed as independent sessions.
pub(super) fn scan(
    sessions_root: &Path,
    mut report: impl FnMut(SessionScanProgress<NativeSessionListing>),
) -> Result<()> {
    let mut candidates = muse_candidates(sessions_root)?;
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
    let mut occurrences = BTreeMap::<String, usize>::new();
    for candidate in &candidates {
        *occurrences
            .entry(candidate.native_session_id.clone())
            .or_default() += 1;
    }

    for (index, candidate) in candidates.into_iter().enumerate() {
        let duplicate = occurrences
            .get(&candidate.native_session_id)
            .copied()
            .unwrap_or_default()
            > 1;
        let session = match muse_summary(&candidate.session_path) {
            Ok(summary) => NativeSessionListing {
                native_session_id: summary.native_session_id.clone(),
                title: summary.title,
                modified_at: candidate.modified_at,
                git_branch: git_branch_or_head(&summary.cwd),
                size_bytes: candidate.size_bytes,
                cwd: summary.cwd,
                unavailable_reason: if summary.native_session_id != candidate.native_session_id {
                    Some(MUSE_UNAVAILABLE_REASON)
                } else if duplicate {
                    Some(MUSE_DUPLICATE_REASON)
                } else if summary.active {
                    Some(MUSE_ACTIVE_REASON)
                } else {
                    None
                },
                natively_archived: false,
            },
            Err(error) => {
                tracing::debug!(
                    path = %candidate.session_path.display(),
                    error = %error,
                    "Muse session is unavailable during discovery"
                );
                NativeSessionListing {
                    native_session_id: candidate.native_session_id,
                    title: candidate
                        .session_path
                        .parent()
                        .and_then(Path::file_name)
                        .and_then(|name| name.to_str())
                        .unwrap_or("Muse session")
                        .to_owned(),
                    modified_at: candidate.modified_at,
                    git_branch: "HEAD".to_owned(),
                    size_bytes: candidate.size_bytes,
                    cwd: PathBuf::new(),
                    unavailable_reason: Some(MUSE_UNAVAILABLE_REASON),
                    natively_archived: false,
                }
            }
        };
        report(SessionScanProgress {
            scanned: index + 1,
            total,
            session: Some(session),
        });
    }
    Ok(())
}

/// Project a Muse session's visible conversation into Hel's canonical event
/// transcript.  Tool details and retained permission records remain in the
/// copied native artifacts.
pub(super) fn read_transcript(path: &Path) -> Result<ClaudeTranscript> {
    let data = read_muse_session_file(path)?;
    let records =
        read_records(&data).with_context(|| format!("parse Muse session {}", path.display()))?;
    let summary = muse_summary_from_records(path, &records)?;
    ensure!(
        !summary.active,
        "Muse session {:?} has an active or incomplete turn; import requires a terminal lifecycle record",
        summary.native_session_id
    );

    let mut events = Vec::new();
    let mut seen_intents = BTreeSet::new();
    let mut seen_messages = BTreeSet::new();
    let mut saw_prompt = false;

    for record in &records {
        let value = &record.value;
        let payload_type = value.get("payload_type").and_then(Value::as_str);
        let recorded_at_ms = muse_recorded_at_ms(value);
        match payload_type {
            Some("runtime.user_intent.accepted") => {
                let intent_id = value
                    .pointer("/payload/intent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let deduplication_key = if intent_id.is_empty() {
                    format!(
                        "text:{}:{}",
                        muse_intent_text(value).unwrap_or_default(),
                        value
                            .get("sequence")
                            .and_then(Value::as_u64)
                            .unwrap_or_default()
                    )
                } else {
                    intent_id.to_owned()
                };
                if !seen_intents.insert(deduplication_key) {
                    continue;
                }
                let Some(text) = muse_intent_text(value) else {
                    continue;
                };
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
                saw_prompt = true;
            }
            Some("runtime.session") => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                if payload.get("kind").and_then(Value::as_str) != Some("run") {
                    continue;
                }
                let event = payload.get("event").unwrap_or(&Value::Null);
                match event.get("kind").and_then(Value::as_str) {
                    Some("assistant_message_committed") => {
                        let Some(text) = event.get("text").and_then(Value::as_str) else {
                            continue;
                        };
                        if text.is_empty() {
                            continue;
                        }
                        if let Some(message_id) = event.get("message_id").and_then(Value::as_str)
                            && !seen_messages.insert(message_id.to_owned())
                        {
                            continue;
                        }
                        push_event(
                            &mut events,
                            recorded_at_ms,
                            WorkerEvent::Adapter {
                                kind: "session_update".into(),
                                payload: json!({
                                    "type": "session_update",
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": {"type": "text", "text": text},
                                    },
                                }),
                            },
                        );
                    }
                    Some("terminal") => {
                        finish_imported_turn(&mut events, recorded_at_ms);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    ensure!(
        saw_prompt,
        "Muse session contains no importable user intent"
    );
    finish_imported_turn(&mut events, None);
    finalize_import_event_times(&mut events, path)?;
    Ok(ClaudeTranscript {
        cwd: summary.cwd,
        edited_paths: summary.edited_paths,
        events,
    })
}

fn muse_candidates(sessions_root: &Path) -> Result<Vec<MuseCandidate>> {
    ensure!(
        sessions_root.is_dir(),
        "Muse sessions directory is missing: {}",
        sessions_root.display()
    );
    let mut candidates = Vec::new();
    for year in read_directories(sessions_root)? {
        if !is_date_component(&year, 4) {
            continue;
        }
        for month in read_directories(&year)? {
            if !is_date_component(&month, 2) {
                continue;
            }
            for day in read_directories(&month)? {
                if !is_date_component(&day, 2) {
                    continue;
                }
                for session_dir in read_directories(&day)? {
                    let Some(native_session_id) = session_dir
                        .file_name()
                        .and_then(|name| name.to_str())
                        .filter(|id| validate_id("Muse session", id).is_ok())
                    else {
                        continue;
                    };
                    let session_path = session_dir.join("session.jsonl");
                    let metadata = match fs::symlink_metadata(&session_path) {
                        Ok(metadata)
                            if metadata.file_type().is_file()
                                && !metadata.file_type().is_symlink() =>
                        {
                            metadata
                        }
                        _ => continue,
                    };
                    candidates.push(MuseCandidate {
                        native_session_id: native_session_id.to_owned(),
                        session_path,
                        modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        size_bytes: metadata.len(),
                    });
                }
            }
        }
    }
    Ok(candidates)
}

fn read_directories(root: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in
        fs::read_dir(root).with_context(|| format!("read Muse directory {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            directories.push(path);
        }
    }
    Ok(directories)
}

fn is_date_component(path: &Path, width: usize) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.len() == width && name.bytes().all(|byte| byte.is_ascii_digit())
}

fn muse_summary(path: &Path) -> Result<MuseSummary> {
    let data = read_muse_session_file(path)?;
    let records =
        read_records(&data).with_context(|| format!("parse Muse session {}", path.display()))?;
    muse_summary_from_records(path, &records)
}

fn read_muse_session_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat Muse session {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Muse session is not a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_MUSE_SESSION_FILE_BYTES,
        "Muse session is too large: {} bytes (maximum is {} bytes)",
        metadata.len(),
        MAX_MUSE_SESSION_FILE_BYTES
    );
    let data = fs::read(path).with_context(|| format!("read Muse session {}", path.display()))?;
    ensure!(
        data.len() as u64 == metadata.len(),
        "Muse session changed while it was being read: {}",
        path.display()
    );
    Ok(data)
}

fn muse_summary_from_records(path: &Path, records: &[MuseRecord]) -> Result<MuseSummary> {
    let native_session_id = records
        .iter()
        .find_map(|record| {
            (!record.retained_frame)
                .then(|| record.value.pointer("/stream/id"))
                .flatten()
                .and_then(Value::as_str)
        })
        .or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
        })
        .context("Muse session does not declare a stream id")?
        .to_owned();

    let cwd = records
        .iter()
        .filter(|record| !record.retained_frame)
        .filter(|record| {
            record.value.get("payload_type").and_then(Value::as_str)
                == Some("runtime.session.metadata")
        })
        .filter_map(|record| {
            record
                .value
                .pointer("/payload/record/workspace_root")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.trim().is_empty())
                .map(PathBuf::from)
        })
        .next_back()
        .context("Muse session does not declare payload.record.workspace_root")?;
    validate_id("Muse session", &native_session_id)?;
    ensure!(cwd.is_absolute(), "Muse workspace root is not absolute");
    ensure!(
        records.iter().all(|record| {
            record.value.pointer("/stream/id").and_then(Value::as_str)
                == Some(native_session_id.as_str())
        }),
        "Muse session contains records from a different session stream"
    );

    let prompt_title = records
        .iter()
        .find_map(|record| {
            (record.value.get("payload_type").and_then(Value::as_str)
                == Some("runtime.user_intent.accepted"))
            .then(|| muse_intent_text(&record.value))
            .flatten()
            .and_then(|text| normalize_session_title(&text))
        })
        .unwrap_or_else(|| native_session_id.clone());
    let title = records
        .iter()
        .filter(|record| record.value["payload_type"] == "session.name.changed")
        .filter_map(|record| {
            record
                .value
                .pointer("/payload/new_name")
                .and_then(Value::as_str)
        })
        .filter_map(normalize_session_title)
        .next_back()
        .unwrap_or(prompt_title);

    let mut active_runs = BTreeSet::new();
    let mut pending_intents = BTreeSet::new();
    for record in records {
        let value = &record.value;
        match value.get("payload_type").and_then(Value::as_str) {
            Some("runtime.user_intent.accepted") => {
                if let Some(intent_id) = value.pointer("/payload/intent_id").and_then(Value::as_str)
                {
                    pending_intents.insert(intent_id.to_owned());
                }
            }
            Some("runtime.user_intent.materialized") => {
                if let (Some(intent_id), Some(run_id)) = (
                    value.pointer("/payload/intent_id").and_then(Value::as_str),
                    value
                        .pointer("/payload/outcome/run_id")
                        .and_then(Value::as_str),
                ) {
                    pending_intents.remove(intent_id);
                    active_runs.insert(run_id.to_owned());
                }
            }
            Some("runtime.session")
                if value.pointer("/payload/kind").and_then(Value::as_str) == Some("run") =>
            {
                let run_id = value.pointer("/payload/run_id").and_then(Value::as_str);
                match value.pointer("/payload/event/kind").and_then(Value::as_str) {
                    Some("started") => {
                        if let Some(run_id) = run_id {
                            active_runs.insert(run_id.to_owned());
                        }
                    }
                    Some("terminal") => {
                        if let Some(run_id) = run_id {
                            active_runs.remove(run_id);
                            pending_intents.remove(run_id);
                        } else if let Some(run_id) = active_runs.pop_last() {
                            pending_intents.remove(&run_id);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(MuseSummary {
        native_session_id,
        cwd,
        title,
        edited_paths: muse_edited_paths(records),
        active: !active_runs.is_empty() || !pending_intents.is_empty(),
    })
}

fn muse_intent_text(record: &Value) -> Option<String> {
    let text = |part: &Value| {
        let kind = part
            .get("kind")
            .or_else(|| part.get("type"))
            .and_then(Value::as_str);
        (kind == Some("text"))
            .then(|| part.get("text").and_then(Value::as_str).map(str::to_owned))
            .flatten()
    };
    let messages = record
        .pointer("/payload/model_messages")
        .and_then(Value::as_array);
    let parts = messages
        .into_iter()
        .flatten()
        .flat_map(|message| {
            message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
    if !parts.is_empty() {
        return Some(parts.join("\n"));
    }
    let parts = record
        .pointer("/payload/refill_blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
    if !parts.is_empty() {
        return Some(parts.join("\n"));
    }
    record
        .pointer("/payload/prompt")
        .or_else(|| record.pointer("/payload/text"))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn muse_recorded_at_ms(record: &Value) -> Option<i64> {
    record
        .get("recorded_at")
        .and_then(Value::as_i64)
        .map(|micros| micros / 1_000)
        .or_else(|| native_recorded_at_ms(record))
}

fn muse_edited_paths(records: &[MuseRecord]) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for record in records {
        let Some(event) = record.value.pointer("/payload/event") else {
            continue;
        };
        let event_kind = event
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_kind == "assistant_tool_calls_committed" {
            for call in event
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = call.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if !is_write_operation(name) {
                    continue;
                }
                if let Some(arguments) = call.get("args").and_then(Value::as_str)
                    && let Ok(arguments) = serde_json::from_str::<Value>(arguments)
                {
                    collect_path_values(&arguments, &mut paths);
                } else if let Some(arguments) = call.get("args") {
                    collect_path_values(arguments, &mut paths);
                }
            }
            continue;
        }
        if !event_kind.contains("tool_call") {
            continue;
        }
        let operation = event
            .get("name")
            .or_else(|| event.get("tool_name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_write_operation(operation) {
            continue;
        }
        for key in ["arguments", "args", "input"] {
            let Some(arguments) = event.get(key) else {
                continue;
            };
            if let Some(arguments) = arguments.as_str()
                && let Ok(arguments) = serde_json::from_str::<Value>(arguments)
            {
                collect_path_values(&arguments, &mut paths);
            } else {
                collect_path_values(arguments, &mut paths);
            }
        }
    }
    paths.into_iter().collect()
}

fn collect_path_values(value: &Value, paths: &mut BTreeSet<PathBuf>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if is_path_key(key)
                    && let Some(path) = value.as_str().filter(|path| !path.trim().is_empty())
                {
                    paths.insert(PathBuf::from(path));
                }
                collect_path_values(value, paths);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_path_values(value, paths);
            }
        }
        _ => {}
    }
}

fn is_write_operation(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    (value.contains("write")
        || value.contains("edit")
        || value.contains("patch")
        || value.contains("replace")
        || value.contains("rename")
        || value.contains("delete")
        || value.contains("move"))
        && !value.contains("read")
}

fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "path" | "file_path" | "filePath" | "target_path" | "targetPath" | "filename"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn record(id: &str, sequence: u64, payload_type: &str, payload: Value) -> Value {
        serde_json::json!({
            "schema_version": 1,
            "id": id,
            "stream": {"kind":"session","id":"muse-session-1"},
            "sequence": sequence,
            "recorded_at": 1_788_871_599_426_557_i64 + sequence as i64,
            "record_type": "event",
            "durability": "durable",
            "payload_type": payload_type,
            "payload_schema_version": 1,
            "payload": payload,
        })
    }

    fn session_bytes(active: bool) -> Vec<u8> {
        let mut records = vec![
            record(
                "metadata",
                1,
                "runtime.session.metadata",
                serde_json::json!({"kind":"metadata","record":{"workspace_root":"/work"}}),
            ),
            record(
                "intent",
                2,
                "runtime.user_intent.accepted",
                serde_json::json!({"intent_id":"run-1","model_messages":[{"content":[{"kind":"text","text":"hello Muse"}]}]}),
            ),
            record(
                "started",
                3,
                "runtime.session",
                serde_json::json!({"kind":"run","run_id":"run-1","event":{"kind":"started"}}),
            ),
            record(
                "assistant",
                4,
                "runtime.session",
                serde_json::json!({"kind":"run","run_id":"run-1","event":{"kind":"assistant_message_committed","message_id":"message-1","text":"hello back"}}),
            ),
        ];
        if !active {
            records.push(record(
                "terminal",
                5,
                "runtime.session",
                serde_json::json!({"kind":"run","run_id":"run-1","event":{"kind":"terminal","terminal":"completed"}}),
            ));
        }
        records
            .into_iter()
            .map(|record| serde_json::to_string(&record).unwrap() + "\n")
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn read_transcript_projects_muse_lifecycle_and_edit_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut data = session_bytes(false);
        let edit = record(
            "edit",
            6,
            "runtime.session",
            serde_json::json!({"kind":"run","event":{"kind":"tool_call","tool_name":"write_file","arguments":{"file_path":"src/lib.rs"}}}),
        );
        data.extend_from_slice((serde_json::to_string(&edit).unwrap() + "\n").as_bytes());
        fs::write(&path, data).unwrap();
        let transcript = read_transcript(&path).unwrap();
        assert_eq!(transcript.cwd, PathBuf::from("/work"));
        assert_eq!(transcript.edited_paths, [PathBuf::from("src/lib.rs")]);
        assert!(transcript.events.iter().any(|event| matches!(
            event.event,
            WorkerEvent::PromptAccepted { ref text, .. } if text == "hello Muse"
        )));
        assert!(
            transcript
                .events
                .iter()
                .any(|event| matches!(event.event, WorkerEvent::TurnCompleted))
        );
    }

    #[test]
    fn scan_excludes_subagent_logs_and_marks_active_turn() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("2026/09/08/muse-session-1");
        fs::create_dir_all(root.join("subagent/child")).unwrap();
        fs::write(root.join("session.jsonl"), session_bytes(true)).unwrap();
        fs::write(
            root.join("subagent/child/session.jsonl"),
            session_bytes(false),
        )
        .unwrap();
        let mut listed = Vec::new();
        scan(directory.path(), |progress| {
            if let Some(session) = progress.session {
                listed.push(session);
            }
        })
        .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].unavailable_reason, Some(MUSE_ACTIVE_REASON));
    }

    #[test]
    fn summary_rejects_relative_workspace_and_mixed_session_streams() {
        let mut records = read_records(&session_bytes(false)).unwrap();
        records[0].value["payload"]["record"]["workspace_root"] = json!("relative");
        assert!(
            muse_summary_from_records(Path::new("session.jsonl"), &records)
                .unwrap_err()
                .to_string()
                .contains("not absolute")
        );
        records[0].value["payload"]["record"]["workspace_root"] = json!("/work");
        records[1].value["stream"]["id"] = json!("another-session");
        assert!(
            muse_summary_from_records(Path::new("session.jsonl"), &records)
                .unwrap_err()
                .to_string()
                .contains("different session stream")
        );
    }

    #[test]
    fn materialized_intent_tracks_its_run_and_latest_native_name() {
        let mut rows = session_bytes(false)
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        rows[1]["payload"]["intent_id"] = json!("different-intent");
        rows.insert(3, record("materialized", 10, "runtime.user_intent.materialized",
            json!({"intent_id":"different-intent", "outcome":{"kind":"top_level_turn_started", "run_id":"run-1"}})));
        rows.push(record(
            "renamed",
            11,
            "session.name.changed",
            json!({"new_name":"Native title"}),
        ));
        let data = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let records = read_records(data.as_bytes()).unwrap();
        let summary = muse_summary_from_records(
            Path::new("/sessions/muse-session-1/session.jsonl"),
            &records,
        )
        .unwrap();
        assert!(!summary.active);
        assert_eq!(summary.title, "Native title");
        assert_eq!(
            muse_intent_text(
                &json!({"payload":{"refill_blocks":[{"kind":"text","text":"prompt"}]}})
            )
            .as_deref(),
            Some("prompt")
        );
    }
}
