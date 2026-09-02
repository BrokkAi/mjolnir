use super::*;

/// Locate a Grok Build session directory. Its name is the session UUID, which
/// is the native identifier `session/load` takes.
pub fn locate_grok_session(
    home: &Path,
    selection: &GrokSessionSelection,
) -> Result<LocatedGrokSession> {
    let candidates = list_grok_sessions(home)?;
    let sessions = home.join("sessions");
    match selection {
        GrokSessionSelection::NativeSessionId(native_session_id) => candidates
            .into_iter()
            .find(|candidate| candidate.native_session_id == *native_session_id)
            .with_context(|| {
                format!(
                    "Grok Build session {native_session_id:?} was not found under {}",
                    sessions.display()
                )
            }),
        GrokSessionSelection::Latest => candidates
            .into_iter()
            .next()
            .context("no Grok Build session directories were found"),
    }
}

/// List native Grok Build sessions newest first.
pub fn list_grok_sessions(home: &Path) -> Result<Vec<LocatedGrokSession>> {
    let mut sessions = Vec::new();
    scan_grok_sessions(home, |progress| {
        if let Some(session) = progress.session {
            sessions.push(session);
        }
    })?;
    Ok(sessions)
}

/// Scan native Grok Build sessions newest first, reporting after every
/// candidate directory.
pub fn scan_grok_sessions(
    home: &Path,
    mut report: impl FnMut(SessionScanProgress<LocatedGrokSession>),
) -> Result<()> {
    let sessions = home.join("sessions");
    ensure!(
        sessions.is_dir(),
        "Grok Build sessions directory is missing: {}",
        sessions.display()
    );
    let mut candidates = grok_candidates(&sessions)?;
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
        let session = LocatedGrokSession {
            title: candidate.title,
            native_session_id: candidate.native_session_id,
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

/// Walk `sessions/<encoded-cwd>/<session-uuid>`. The sessions root also holds
/// the shared search index and lock files, which are not sessions.
fn grok_candidates(sessions: &Path) -> Result<Vec<KimiScanCandidate>> {
    let mut candidates = Vec::new();
    for cwd_entry in fs::read_dir(sessions)? {
        let cwd_directory = cwd_entry?.path();
        if !cwd_directory.is_dir() {
            continue;
        }
        let decoded_cwd = grok_decode_cwd_dirname(&cwd_directory);
        for session_entry in fs::read_dir(&cwd_directory)? {
            let session_entry = session_entry?;
            let session_path = session_entry.path();
            let metadata = fs::symlink_metadata(&session_path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let Some(native_session_id) = session_path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| validate_id("Grok Build session", name).is_ok())
            else {
                continue;
            };
            let (title, summary_cwd) = grok_listing_metadata(&session_path);
            let Some(cwd) = summary_cwd.or_else(|| decoded_cwd.clone()) else {
                continue;
            };
            candidates.push(KimiScanCandidate {
                native_session_id: native_session_id.to_owned(),
                modified_at: grok_session_modified_at(&session_path, &metadata),
                title: title.unwrap_or_else(|| native_session_id.to_owned()),
                cwd,
                session_path,
            });
        }
    }
    Ok(candidates)
}

/// Grok Build's session-directory name is the URL-encoded working directory,
/// or a `{slug}-{hash}` form for a long path that keeps the real value in a
/// `.cwd` file. Mirrors `decode_cwd_from_dirname` in grok-build.
pub(super) fn grok_decode_cwd_dirname(directory: &Path) -> Option<PathBuf> {
    let name = directory.file_name()?.to_str()?;
    if let Some(decoded) = url_decode(name)
        && decoded.starts_with('/')
    {
        return Some(PathBuf::from(decoded));
    }
    let recorded = fs::read_to_string(directory.join(".cwd")).ok()?;
    let recorded = recorded.trim();
    recorded.starts_with('/').then(|| PathBuf::from(recorded))
}

/// Percent-decoding for the session-directory name. `None` when the text is
/// not valid percent-encoded UTF-8, which means it is not a cwd key.
fn url_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Listing title and cwd from `summary.json`. Never fails the whole scan: an
/// unreadable session is listed with what could be recovered.
fn grok_listing_metadata(session_path: &Path) -> (Option<String>, Option<PathBuf>) {
    let summary = fs::read(session_path.join("summary.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let title = summary
        .get("session_summary")
        .and_then(Value::as_str)
        .and_then(normalize_session_title);
    let cwd = summary
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_absolute());
    (title, cwd)
}

fn grok_session_modified_at(session_path: &Path, metadata: &fs::Metadata) -> SystemTime {
    let mut modified_at = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    for name in [CHAT_HISTORY, "events.jsonl", "summary.json"] {
        if let Ok(modified) = fs::metadata(session_path.join(name)).and_then(|file| file.modified())
        {
            modified_at = modified_at.max(modified);
        }
    }
    modified_at
}

/// Project a Grok Build session directory. `chat_history.jsonl` is the
/// conversation of record: one JSON object per line, internally tagged by
/// `type`.
///
/// * `system` — the rendered system prompt; not conversation.
/// * `user` — `content` is a list of `{type: "text"|"image", ...}` parts. A
///   `synthetic_reason` marks a message the runtime injected rather than one a
///   person typed.
/// * `assistant` — `content` is the response text, with any `tool_calls` as
///   `{id, name, arguments}` beside it.
/// * `reasoning` — an inlined Responses-API reasoning item whose `summary`
///   holds `{type: "summary_text", text}` parts.
/// * `tool_result` — `{tool_call_id, content}`.
///
/// Compaction rewrites the file in place, so a `compaction_meta` message with
/// no earlier real prompt means the raw history is already gone.
pub fn read_grok_transcript(session_path: &Path) -> Result<GrokTranscript> {
    let summary_path = session_path.join("summary.json");
    let summary: Value = serde_json::from_slice(&fs::read(&summary_path)?).with_context(|| {
        format!(
            "parse Grok Build session summary {}",
            summary_path.display()
        )
    })?;
    let cwd = summary
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| session_path.parent().and_then(grok_decode_cwd_dirname))
        .context("Grok Build session summary does not declare its cwd")?;
    ensure!(
        cwd.is_absolute(),
        "Grok Build session cwd is not absolute: {}",
        cwd.display()
    );

    let history_path = session_path.join(CHAT_HISTORY);
    let body = fs::read_to_string(&history_path)
        .with_context(|| format!("read Grok Build chat history {}", history_path.display()))?;
    let mut events = Vec::new();
    let mut saw_raw_user = false;
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "parse Grok Build chat history {} line {}",
                history_path.display(),
                index + 1
            )
        })?;
        let recorded_at_ms = native_recorded_at_ms(&record);
        let item_type = record.get("type").and_then(Value::as_str);
        if record.get("synthetic_reason").and_then(Value::as_str) == Some("compaction_meta") {
            ensure!(
                saw_raw_user,
                "Grok Build session contains a compaction artifact before recoverable raw history"
            );
            continue;
        }
        match item_type {
            Some("user") if grok_real_user_item(&record) => {
                let text = grok_user_text(&record);
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
                        text,
                        attachments: Vec::new(),
                    },
                );
                saw_raw_user = true;
            }
            Some("reasoning") => {
                let thought = record
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("summary_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !thought.is_empty() {
                    push_grok_chunk(&mut events, recorded_at_ms, "agent_thought_chunk", &thought);
                }
            }
            Some("assistant") => {
                if let Some(text) = record
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    push_grok_chunk(&mut events, recorded_at_ms, "agent_message_chunk", text);
                }
            }
            _ => {}
        }
    }
    finish_imported_turn(&mut events, None);
    finalize_import_event_times(&mut events, &history_path)?;
    let edited_paths = grok_edited_paths(&body)?;
    Ok(GrokTranscript {
        cwd,
        edited_paths,
        events,
    })
}

/// A `user` item a person actually typed. Everything the runtime injects
/// carries a `synthetic_reason`.
fn grok_real_user_item(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("user")
        && record.get("synthetic_reason").is_none()
}

fn grok_user_text(record: &Value) -> String {
    let text = record
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    strip_hidden_prompt_context(&text).to_owned()
}

fn push_grok_chunk(
    events: &mut Vec<SequencedEvent>,
    recorded_at_ms: Option<i64>,
    update: &str,
    text: &str,
) {
    push_event(
        events,
        recorded_at_ms,
        WorkerEvent::Adapter {
            kind: "session_update".into(),
            payload: json!({
                "type": "session_update",
                "update": {
                    "sessionUpdate": update,
                    "content": {"type": "text", "text": text},
                },
            }),
        },
    );
}

/// Files `search_replace` — Grok Build's only file-writing tool — was asked to
/// change, kept only when a matching `tool_result` proves the call ran. A tool
/// result carries no success flag, so a call that ran and failed is still
/// counted; over-reporting an edit only widens the set of repositories the
/// import inspects.
pub(super) fn grok_edited_paths(history: &str) -> Result<Vec<PathBuf>> {
    let mut calls = BTreeMap::<String, PathBuf>::new();
    let mut completed = BTreeSet::new();
    for line in history.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line)?;
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for call in record
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if call.get("name").and_then(Value::as_str) != Some("search_replace") {
                        continue;
                    }
                    let Some(id) = call.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let arguments = call
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                        .unwrap_or(Value::Null);
                    if let Some(path) = arguments.get("file_path").and_then(Value::as_str) {
                        calls.insert(id.to_owned(), PathBuf::from(path));
                    }
                }
            }
            Some("tool_result") => {
                if let Some(id) = record.get("tool_call_id").and_then(Value::as_str) {
                    completed.insert(id.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(calls
        .into_iter()
        .filter(|(id, _)| completed.contains(id))
        .map(|(_, path)| path)
        .collect())
}
