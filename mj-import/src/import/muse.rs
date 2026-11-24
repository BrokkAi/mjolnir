use std::collections::{BTreeMap, BTreeSet};

use mj_checkpoint::native::muse::{MuseRecord, read_records};

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
    Ok(LocatedNativeSession {
        native_session_id: candidate.native_session_id.clone(),
        source_path: candidate.session_path.clone(),
        modified_at: candidate.modified_at,
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

/// Validate the selected stream's identity and lifecycle before importing it.
pub(super) fn validate_transcript(path: &Path, original_key: &Path) -> Result<PathBuf> {
    let summary = muse_summary(path)?;
    ensure!(
        original_key
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(&summary.native_session_id),
        "Muse session stream id does not match its directory"
    );
    ensure!(
        !summary.active,
        "Muse session {:?} has an active or incomplete turn; import requires a terminal lifecycle record",
        summary.native_session_id
    );
    Ok(summary.cwd)
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
    ensure!(
        mj_core::target_path::is_absolute_on_host_or_target(&cwd),
        "Muse workspace root is not absolute"
    );
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

pub(super) fn recent_directories(
    home: &Path,
    limit: usize,
    executor: &impl crate::targets::CommandExecutor,
) -> Result<NativeProjectSeed> {
    let mut candidates = muse_candidates(&mj_checkpoint::native::muse_sessions_root(home)?)?;
    candidates.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| b.session_path.cmp(&a.session_path))
    });
    let mut seed = NativeProjectSeed {
        directories: Vec::new(),
        errors: Vec::new(),
    };
    for candidate in candidates.into_iter().take(limit) {
        ensure!(
            !executor.cancellation_requested(),
            "project discovery cancelled"
        );
        match native_project_directory(&candidate.session_path, HarnessKind::Muse, executor) {
            Ok(directory) => seed.directories.push(directory),
            Err(error) => seed.errors.push((
                candidate.session_path.clone(),
                format!("{}: {error:#}", candidate.session_path.display()),
            )),
        }
    }
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn summary_rejects_relative_workspace_and_mixed_session_streams() {
        let mut records = read_records(&session_bytes(false)).unwrap();
        records[0].value["payload"]["record"]["workspace_root"] = json!("relative");
        assert!(
            muse_summary_from_records(Path::new("session.jsonl"), &records)
                .unwrap_err()
                .to_string()
                .contains("not absolute")
        );
        records[0].value["payload"]["record"]["workspace_root"] =
            json!(crate::import::test_fixtures::native_cwd("/work"));
        records[1].value["stream"]["id"] = json!("another-session");
        assert!(
            muse_summary_from_records(Path::new("session.jsonl"), &records)
                .unwrap_err()
                .to_string()
                .contains("different session stream")
        );
    }
}
