use super::*;

/// Read the native JSONL only far enough to recover a transcript suitable for
/// Hel's chat view. Full tool traffic and reasoning remain in the copied
/// native rollout, not in this lossy projection.
pub fn read_claude_transcript(path: &Path) -> Result<ClaudeTranscript> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("read Claude session {}", path.display()))?;
    let mut cwd = None;
    let mut events = Vec::new();
    let mut saw_raw_user = false;

    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!("parse Claude session {} line {}", path.display(), index + 1)
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        if cwd.is_none() {
            cwd = record
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.trim().is_empty())
                .map(PathBuf::from);
        }
        if record.get("isMeta").and_then(Value::as_bool) == Some(true)
            || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let compaction_boundary = record.get("type").and_then(Value::as_str) == Some("system")
            && matches!(
                record.get("subtype").and_then(Value::as_str),
                Some("compact_boundary" | "compaction")
            );
        let compaction_summary = record
            .get("isCompactSummary")
            .or_else(|| record.pointer("/message/isCompactSummary"))
            .and_then(Value::as_bool)
            == Some(true);
        if compaction_boundary || compaction_summary {
            ensure!(
                saw_raw_user,
                "Claude session contains a compaction artifact before recoverable raw history"
            );
            continue;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(text) = record
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .map(strip_hidden_prompt_context)
                    .filter(|text| !text.trim().is_empty())
                else {
                    continue;
                };
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
                saw_raw_user = true;
            }
            Some("assistant") => {
                let Some(content) = record.pointer("/message/content").and_then(Value::as_array)
                else {
                    continue;
                };
                for block in content {
                    let Some(text) = block
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    else {
                        continue;
                    };
                    if block.get("type").and_then(Value::as_str) != Some("text") {
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
                // Claude marks a completed model response independently of
                // its text/tool blocks. Preserve that lifecycle boundary so
                // the restored durable worker is idle and accepts the next
                // user prompt instead of treating the imported turn as live.
                if matches!(
                    record
                        .pointer("/message/stop_reason")
                        .and_then(Value::as_str),
                    Some("end_turn" | "stop_sequence")
                ) {
                    push_event(&mut events, recorded_at_ms, WorkerEvent::TurnCompleted);
                }
            }
            _ => {}
        }
    }

    let cwd = cwd.context("Claude session does not declare its original cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Claude session cwd is not absolute: {}",
        cwd.display()
    );
    finalize_import_event_times(&mut events, path)?;
    let edited_paths = claude_edited_paths(path)?;
    Ok(ClaudeTranscript {
        cwd,
        edited_paths,
        events,
    })
}

/// Project a Codex rollout into the canonical transcript used by Hel chat.
pub fn read_codex_transcript(path: &Path) -> Result<CodexTranscript> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("read Codex session {}", path.display()))?;
    let mut cwd = None;
    let mut history_mode = None;
    let mut events = Vec::new();
    let mut edited_paths = BTreeSet::new();
    let mut saw_user = false;
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!("parse Codex session {} line {}", path.display(), index + 1)
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            if cwd.is_none() {
                cwd = record
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .filter(|cwd| !cwd.trim().is_empty())
                    .map(PathBuf::from);
            }
            if history_mode.is_none() {
                history_mode = Some(
                    record
                        .pointer("/payload/history_mode")
                        .and_then(Value::as_str)
                        .map(parse_codex_history_mode)
                        .transpose()?
                        .unwrap_or(CodexHistoryMode::Legacy),
                );
            }
            continue;
        }
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        if record.pointer("/payload/type").and_then(Value::as_str) == Some("item_completed")
            && record.pointer("/payload/item/type").and_then(Value::as_str) == Some("FileChange")
            && record
                .pointer("/payload/item/status")
                .and_then(Value::as_str)
                == Some("completed")
            && let Some(changes) = record
                .pointer("/payload/item/changes")
                .and_then(Value::as_object)
        {
            edited_paths.extend(changes.keys().map(PathBuf::from));
        }
        match record.pointer("/payload/type").and_then(Value::as_str) {
            Some("item_completed")
                if record.pointer("/payload/item/type").and_then(Value::as_str)
                    == Some("UserMessage") =>
            {
                let Some(text) = codex_completed_item_text(&record) else {
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
                saw_user = true;
            }
            Some("item_completed")
                if record.pointer("/payload/item/type").and_then(Value::as_str)
                    == Some("AgentMessage") =>
            {
                let Some(text) = codex_completed_item_text(&record) else {
                    continue;
                };
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
            Some("turn_complete" | "turn_aborted") => {
                finish_imported_turn(&mut events, recorded_at_ms)
            }
            _ => {}
        }
    }
    ensure!(
        history_mode == Some(CodexHistoryMode::Paginated),
        "{CODEX_LEGACY_IMPORT_ISSUE}"
    );
    ensure!(
        saw_user,
        "Codex paginated session contains no importable user messages"
    );
    finish_imported_turn(&mut events, None);
    let cwd = cwd.context("Codex session does not declare its original cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Codex session cwd is not absolute: {}",
        cwd.display()
    );
    finalize_import_event_times(&mut events, path)?;
    Ok(CodexTranscript {
        cwd,
        edited_paths: edited_paths.into_iter().collect(),
        events,
    })
}

pub(super) fn codex_completed_item_text(record: &Value) -> Option<String> {
    let parts = record
        .pointer("/payload/item/content")?
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Project a Kimi session directory. The main wire stream contains prompts and
/// generated text; tool traffic and thought blocks stay only in native files.
pub fn read_kimi_transcript(session_path: &Path) -> Result<KimiTranscript> {
    let state_path = session_path.join("state.json");
    let state: Value = serde_json::from_slice(&fs::read(&state_path)?)
        .with_context(|| format!("parse Kimi session state {}", state_path.display()))?;
    let cwd = state
        .get("workDir")
        .or_else(|| state.get("cwd"))
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
        .map(PathBuf::from)
        .context("Kimi session state does not declare workDir or cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Kimi session workDir is not absolute: {}",
        cwd.display()
    );
    let wire_path = session_path.join("agents/main/wire.jsonl");
    let body = fs::read_to_string(&wire_path)
        .with_context(|| format!("read Kimi wire stream {}", wire_path.display()))?;
    let mut events = Vec::new();
    let mut saw_raw_user = false;
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "parse Kimi wire stream {} line {}",
                wire_path.display(),
                index + 1
            )
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        if matches!(
            record.get("type").and_then(Value::as_str),
            Some("context.compaction" | "context.compacted" | "compaction")
        ) {
            ensure!(
                saw_raw_user,
                "Kimi session contains a compaction artifact before recoverable raw history"
            );
            continue;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("turn.prompt" | "turn.steer")
                if record.pointer("/origin/kind").and_then(Value::as_str) == Some("user") =>
            {
                finish_imported_turn(&mut events, None);
                let text = record
                    .pointer("/input")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = strip_hidden_prompt_context(&text);
                if !text.trim().is_empty() {
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
                    saw_raw_user = true;
                }
            }
            Some("context.append_loop_event")
                if record.pointer("/event/type").and_then(Value::as_str)
                    == Some("content.part")
                    && record.pointer("/event/part/type").and_then(Value::as_str)
                        == Some("text") =>
            {
                let Some(text) = record
                    .pointer("/event/part/text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
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
            _ => {}
        }
    }
    finish_imported_turn(&mut events, None);
    finalize_import_event_times(&mut events, &wire_path)?;
    let edited_paths = kimi_edited_paths(session_path)?;
    Ok(KimiTranscript {
        cwd,
        edited_paths,
        events,
    })
}

pub(super) fn claude_edited_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = vec![path.to_path_buf()];
    if let (Some(parent), Some(session_id)) = (
        path.parent(),
        path.file_stem().and_then(|value| value.to_str()),
    ) {
        let subagents = parent.join(session_id).join("subagents");
        if subagents.is_dir() {
            collect_files_named(&subagents, "jsonl", &mut files)?;
        }
    }
    let mut edited = BTreeSet::new();
    for file in files {
        let body = fs::read_to_string(&file)?;
        let mut calls = BTreeMap::<String, PathBuf>::new();
        let mut completed = BTreeSet::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let record: Value = serde_json::from_str(line)?;
            if record.get("type").and_then(Value::as_str) == Some("file-history-delta") {
                let Some(tracking) = record.get("trackingPath").and_then(Value::as_str) else {
                    continue;
                };
                let tracking = PathBuf::from(tracking);
                let path = if tracking.is_absolute() {
                    tracking
                } else if let Some(parent) = record
                    .pointer("/backup/realParentDir")
                    .and_then(Value::as_str)
                {
                    PathBuf::from(parent).join(
                        tracking
                            .file_name()
                            .expect("non-empty tracking path has a file name"),
                    )
                } else {
                    tracking
                };
                edited.insert(path);
            }
            if record.get("type").and_then(Value::as_str) == Some("assistant") {
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use")
                        || !matches!(
                            block.get("name").and_then(Value::as_str),
                            Some("Edit" | "Write" | "NotebookEdit")
                        )
                    {
                        continue;
                    }
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(path) = block
                        .pointer("/input/file_path")
                        .or_else(|| block.pointer("/input/notebook_path"))
                        .or_else(|| block.pointer("/input/path"))
                        .and_then(Value::as_str)
                    {
                        calls.insert(id.to_owned(), PathBuf::from(path));
                    }
                }
            }
            if record.get("type").and_then(Value::as_str) == Some("user") {
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("is_error").and_then(Value::as_bool) != Some(true)
                        && let Some(id) = block.get("tool_use_id").and_then(Value::as_str)
                    {
                        completed.insert(id.to_owned());
                    }
                }
            }
        }
        edited.extend(
            calls
                .into_iter()
                .filter(|(id, _)| completed.contains(id))
                .map(|(_, path)| path),
        );
    }
    Ok(edited.into_iter().collect())
}

pub(super) fn kimi_edited_paths(session_path: &Path) -> Result<Vec<PathBuf>> {
    let agents = session_path.join("agents");
    if !agents.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_files_named(&agents, "jsonl", &mut files)?;
    let mut edited = BTreeSet::new();
    for file in files {
        let body = fs::read_to_string(file)?;
        let mut calls = BTreeMap::<String, PathBuf>::new();
        let mut completed = BTreeSet::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let record: Value = serde_json::from_str(line)?;
            if record.get("type").and_then(Value::as_str) != Some("context.append_loop_event") {
                continue;
            }
            let event = &record["event"];
            if event.get("type").and_then(Value::as_str) == Some("tool.call")
                && matches!(
                    event.get("name").and_then(Value::as_str),
                    Some("Edit" | "Write")
                )
                && let (Some(id), Some(path)) = (
                    event.get("toolCallId").and_then(Value::as_str),
                    event
                        .pointer("/args/path")
                        .or_else(|| event.pointer("/args/file_path"))
                        .and_then(Value::as_str),
                )
            {
                calls.insert(id.to_owned(), PathBuf::from(path));
            }
            if event.get("type").and_then(Value::as_str) == Some("tool.result")
                && event.pointer("/result/isError").and_then(Value::as_bool) != Some(true)
                && let Some(id) = event.get("toolCallId").and_then(Value::as_str)
            {
                completed.insert(id.to_owned());
            }
        }
        edited.extend(
            calls
                .into_iter()
                .filter(|(id, _)| completed.contains(id))
                .map(|(_, path)| path),
        );
    }
    Ok(edited.into_iter().collect())
}

pub(super) fn collect_files_named(
    root: &Path,
    extension: &str,
    output: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_files_named(&path, extension, output)?;
        } else if metadata.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some(extension)
        {
            output.push(path);
        }
    }
    Ok(())
}

pub(super) fn finish_imported_turn(events: &mut Vec<SequencedEvent>, recorded_at_ms: Option<i64>) {
    if !events.is_empty()
        && !matches!(
            events.last().map(|event| &event.event),
            Some(WorkerEvent::TurnCompleted)
        )
    {
        push_event(events, recorded_at_ms, WorkerEvent::TurnCompleted);
    }
}

pub(super) fn push_event(
    events: &mut Vec<SequencedEvent>,
    recorded_at_ms: Option<i64>,
    event: WorkerEvent,
) {
    events.push(SequencedEvent {
        seq: events.len() as u64 + 1,
        recorded_at_ms,
        request_id: None,
        event,
    });
}

pub(super) fn native_recorded_at_ms(record: &Value) -> Option<i64> {
    record
        .get("timestamp")
        .or_else(|| record.get("time"))
        .and_then(Value::as_str)
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp_millis())
}

/// Native streams predate Hel's durable event clock in some harness versions.
/// Preserve their record timestamps when available; otherwise use the source
/// artifact's modification time. Clamping regressions keeps the imported
/// sequence and its activity watermark monotonic even if the native clock
/// moved backwards while the session was being recorded.
pub(super) fn finalize_import_event_times(
    events: &mut [SequencedEvent],
    source_path: &Path,
) -> Result<()> {
    let Some(first) = events.first() else {
        return Ok(());
    };
    let mut last_recorded_at_ms = match events.iter().find_map(|event| event.recorded_at_ms) {
        Some(recorded_at_ms) => recorded_at_ms,
        None => DateTime::<Utc>::from(
            fs::metadata(source_path)
                .with_context(|| format!("stat import source {}", source_path.display()))?
                .modified()
                .with_context(|| format!("read import source mtime {}", source_path.display()))?,
        )
        .timestamp_millis(),
    };
    last_recorded_at_ms = first
        .recorded_at_ms
        .unwrap_or(last_recorded_at_ms)
        .max(last_recorded_at_ms);
    for event in events {
        last_recorded_at_ms = event
            .recorded_at_ms
            .unwrap_or(last_recorded_at_ms)
            .max(last_recorded_at_ms);
        event.recorded_at_ms = Some(last_recorded_at_ms);
    }
    Ok(())
}
