//! Native Kimi background-task tracking.
//!
//! Kimi does not report its detached background work through ACP.  The durable
//! task lifecycle records in the native session's main wire stream are the
//! source of truth instead.  Both detached agents (`kind: "agent"`) and
//! detached shell processes (`kind: "process"`) are tracked.  This module
//! deliberately contains only file parsing and state tracking; the caller
//! decides where a snapshot is published.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SESSION_INDEX_FILE: &str = "session_index.jsonl";
const STATE_FILE: &str = "state.json";
const MAX_WIRE_LINE_BYTES: usize = 1024 * 1024;
const FILE_ID_SAMPLE_BYTES: usize = 4096;

const TASK_STARTED: &str = "task.started";
const TASK_TERMINATED: &str = "task.terminated";
const LEGACY_TASK_STARTED: &str = "background.task.started";
const LEGACY_TASK_TERMINATED: &str = "background.task.terminated";

/// A detached Kimi agent or process which is active according to the durable
/// wire log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KimiBackgroundTask {
    pub task_id: String,
    pub description: String,
    pub started_at_ms: i64,
    pub parent_tool_call_id: Option<String>,
}

/// The materialized native task level and provider IDs observed in its event
/// history.  `provider_tool_ids` intentionally retains IDs after termination:
/// a relay may see the ACP tool call before it catches up with this stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KimiTaskSnapshot {
    pub tasks: Vec<KimiBackgroundTask>,
    pub provider_tool_ids: BTreeSet<String>,
}

impl KimiTaskSnapshot {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn task(&self, task_id: &str) -> Option<&KimiBackgroundTask> {
        self.tasks.iter().find(|task| task.task_id == task_id)
    }
}

/// Result of an append catch-up.  A replacement or truncation invalidates the
/// incremental offset and previously materialized level; the caller should
/// invoke [`KimiWireFollower::rescan`] before accepting another update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KimiWireRefresh {
    Updated(KimiTaskSnapshot),
    RescanRequired,
}

/// Incrementally follows one Kimi main-agent wire stream.
#[derive(Debug)]
pub struct KimiWireFollower {
    wire_path: PathBuf,
    offset: u64,
    next_line_number: u64,
    partial: Vec<u8>,
    tracker: TaskTracker,
    stamp: Option<FileStamp>,
}

impl KimiWireFollower {
    /// Construct an unopened follower.  The first [`refresh`](Self::refresh)
    /// performs an initial scan; [`open`](Self::open) is convenient when the
    /// caller wants that initial scan to be explicit.  A live trailing line
    /// without a newline is retained as partial input.
    pub fn new(wire_path: impl Into<PathBuf>) -> Self {
        Self {
            wire_path: wire_path.into(),
            offset: 0,
            next_line_number: 0,
            partial: Vec::new(),
            tracker: TaskTracker::default(),
            stamp: None,
        }
    }

    /// Open and fully scan a wire stream into a follower.
    pub fn open(wire_path: impl Into<PathBuf>) -> Result<Self> {
        let mut follower = Self::new(wire_path);
        follower.rescan()?;
        Ok(follower)
    }

    pub fn wire_path(&self) -> &Path {
        &self.wire_path
    }

    /// Return the latest materialized level without touching the filesystem.
    pub fn snapshot(&self) -> KimiTaskSnapshot {
        self.tracker.snapshot()
    }

    /// Read complete appended lines, retaining a trailing partial line.
    ///
    /// A malformed complete line is an error.  The follower commits no state
    /// from that refresh when parsing fails, so a caller can fail closed.
    pub fn refresh(&mut self) -> Result<KimiWireRefresh> {
        let current = file_stamp(&self.wire_path)?;
        if let Some(previous) = &self.stamp {
            if current.requires_rescan(previous, self.offset) {
                return Ok(KimiWireRefresh::RescanRequired);
            }
        } else {
            self.rescan()?;
            return Ok(KimiWireRefresh::Updated(self.snapshot()));
        }

        let mut file = open_wire_file(&self.wire_path)?;
        let opened = file_stamp_from_metadata(&self.wire_path, &file.metadata()?)?;
        if opened.requires_rescan(&current, self.offset)
            || opened.requires_rescan(self.stamp.as_ref().expect("stamp is set"), self.offset)
        {
            return Ok(KimiWireRefresh::RescanRequired);
        }
        file.seek(SeekFrom::Start(self.offset))
            .with_context(|| format!("seek Kimi wire stream {}", self.wire_path.display()))?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)
            .with_context(|| format!("read Kimi wire stream {}", self.wire_path.display()))?;

        let mut candidate = self.tracker.clone();
        let mut candidate_partial = self.partial.clone();
        let mut candidate_line_number = self.next_line_number;
        parse_append(
            &mut candidate,
            &mut candidate_partial,
            &mut candidate_line_number,
            &appended,
            &self.wire_path,
        )?;

        let offset = self
            .offset
            .checked_add(appended.len() as u64)
            .context("Kimi wire stream offset overflow")?;
        let updated_stamp = file_stamp(&self.wire_path)?;
        // The writer can append while the read is in progress.  Keeping the
        // current offset, rather than the metadata length, lets the next
        // refresh catch up with those bytes.
        self.tracker = candidate;
        self.partial = candidate_partial;
        self.next_line_number = candidate_line_number;
        self.offset = offset;
        self.stamp = Some(updated_stamp);
        Ok(KimiWireRefresh::Updated(self.snapshot()))
    }

    /// Discard incremental state and parse the complete stream again.
    pub fn rescan(&mut self) -> Result<KimiTaskSnapshot> {
        let bytes = fs::read(&self.wire_path).with_context(|| {
            format!(
                "read Kimi wire stream for full scan {}",
                self.wire_path.display()
            )
        })?;
        let mut tracker = TaskTracker::default();
        let mut partial = Vec::new();
        let mut line_count = 0;
        parse_append(
            &mut tracker,
            &mut partial,
            &mut line_count,
            &bytes,
            &self.wire_path,
        )?;
        let stamp = file_stamp(&self.wire_path)?;
        self.tracker = tracker;
        self.next_line_number = line_count;
        self.partial = partial;
        self.offset = bytes.len() as u64;
        self.stamp = Some(stamp);
        Ok(self.snapshot())
    }
}

/// Resolve the final index entry for `session_id`, then validate its native
/// directory and state identity before returning its canonical path.
pub fn resolve_session_dir(kimi_home: &Path, session_id: &str) -> Result<PathBuf> {
    ensure!(!session_id.is_empty(), "Kimi session id must not be empty");
    let canonical_home = fs::canonicalize(kimi_home).with_context(|| {
        format!(
            "canonicalize Kimi home while resolving session {session_id:?}: {}",
            kimi_home.display()
        )
    })?;
    ensure!(canonical_home.is_dir(), "Kimi home is not a directory");

    let index_path = canonical_home.join(SESSION_INDEX_FILE);
    let index = fs::read(&index_path)
        .with_context(|| format!("read Kimi session index {}", index_path.display()))?;
    let mut selected_session_dir = None;
    for (line_number, line) in complete_lines(&index, &index_path)? {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let entry: Value = serde_json::from_slice(line).with_context(|| {
            format!(
                "parse Kimi session index {} line {}",
                index_path.display(),
                line_number + 1
            )
        })?;
        let Some(entry) = entry.as_object() else {
            bail!(
                "Kimi session index {} line {} is not an object",
                index_path.display(),
                line_number + 1
            );
        };
        if entry.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        let session_dir = entry
            .get("sessionDir")
            .and_then(Value::as_str)
            .with_context(|| {
                format!(
                    "Kimi session index {} line {} matching {session_id:?} lacks string sessionDir",
                    index_path.display(),
                    line_number + 1
                )
            })?;
        selected_session_dir = Some(PathBuf::from(session_dir));
    }

    let session_dir = selected_session_dir.with_context(|| {
        format!(
            "Kimi session index {} has no session {session_id:?}",
            index_path.display()
        )
    })?;
    ensure!(
        session_dir.is_absolute(),
        "Kimi session directory is not absolute: {}",
        session_dir.display()
    );
    let canonical_session = fs::canonicalize(&session_dir).with_context(|| {
        format!(
            "canonicalize Kimi session directory {}",
            session_dir.display()
        )
    })?;
    ensure!(
        canonical_session != canonical_home && canonical_session.starts_with(&canonical_home),
        "Kimi session directory escapes Kimi home: {}",
        canonical_session.display()
    );
    ensure!(
        canonical_session.is_dir(),
        "Kimi session directory is not a directory: {}",
        canonical_session.display()
    );

    let state_path = canonical_session.join(STATE_FILE);
    let canonical_state = fs::canonicalize(&state_path)
        .with_context(|| format!("canonicalize Kimi session state {}", state_path.display()))?;
    ensure!(
        canonical_state.starts_with(&canonical_session),
        "Kimi session state escapes session directory: {}",
        canonical_state.display()
    );
    let state: Value = serde_json::from_slice(
        &fs::read(&canonical_state)
            .with_context(|| format!("read Kimi session state {}", canonical_state.display()))?,
    )
    .with_context(|| format!("parse Kimi session state {}", canonical_state.display()))?;
    let state_id = state
        .get("id")
        .or_else(|| state.get("sessionId"))
        .and_then(Value::as_str)
        .with_context(|| {
            format!(
                "Kimi session state {} lacks string id",
                canonical_state.display()
            )
        })?;
    ensure!(
        state_id == session_id,
        "Kimi session state id {state_id:?} does not match requested session {session_id:?}"
    );
    Ok(canonical_session)
}

/// Parse the complete main-agent wire stream.
#[cfg(test)]
pub fn full_scan(wire_path: &Path) -> Result<KimiTaskSnapshot> {
    let bytes = fs::read(wire_path)
        .with_context(|| format!("read Kimi wire stream {}", wire_path.display()))?;
    let (tracker, _) = parse_complete(&bytes, wire_path)?;
    Ok(tracker.snapshot())
}

#[derive(Debug, Clone, Default)]
struct TaskTracker {
    active: BTreeMap<String, KimiBackgroundTask>,
    provider_tool_ids: BTreeSet<String>,
}

impl TaskTracker {
    fn snapshot(&self) -> KimiTaskSnapshot {
        KimiTaskSnapshot {
            tasks: self.active.values().cloned().collect(),
            provider_tool_ids: self.provider_tool_ids.clone(),
        }
    }

    fn apply(&mut self, event: TaskEvent) {
        if let Some(parent_tool_call_id) = event.parent_tool_call_id.as_ref() {
            self.provider_tool_ids.insert(parent_tool_call_id.clone());
        }
        match event.kind {
            TaskEventKind::Started(task) => {
                self.active.insert(task.task_id.clone(), task);
            }
            TaskEventKind::Terminated { task_id } => {
                self.active.remove(&task_id);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct TaskEvent {
    kind: TaskEventKind,
    parent_tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
enum TaskEventKind {
    Started(KimiBackgroundTask),
    Terminated { task_id: String },
}

#[derive(Debug, Clone)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    prefix: Vec<u8>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileStamp {
    fn requires_rescan(&self, previous: &Self, offset: u64) -> bool {
        if self.len < offset {
            return true;
        }
        #[cfg(unix)]
        if (self.device, self.inode) != (previous.device, previous.inode) {
            return true;
        }
        let common_prefix_len = self.prefix.len().min(previous.prefix.len());
        if self.prefix[..common_prefix_len] != previous.prefix[..common_prefix_len] {
            return true;
        }
        // On platforms without a file identity API, a same-length rewrite is
        // still detectable when its modification time changes.
        self.len == offset && self.modified != previous.modified
    }
}

fn open_wire_file(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat Kimi wire stream {}", path.display()))?;
    ensure_regular_file(&metadata, path, "Kimi wire stream")?;
    OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("open Kimi wire stream {}", path.display()))
}

fn ensure_regular_file(metadata: &Metadata, path: &Path, label: &str) -> Result<()> {
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "{label} is not a regular file: {}",
        path.display()
    );
    Ok(())
}

fn file_stamp(path: &Path) -> Result<FileStamp> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat Kimi wire stream {}", path.display()))?;
    ensure_regular_file(&metadata, path, "Kimi wire stream")?;
    file_stamp_from_metadata(path, &metadata)
}

fn file_stamp_from_metadata(path: &Path, metadata: &Metadata) -> Result<FileStamp> {
    let mut file = File::open(path)
        .with_context(|| format!("open Kimi wire stream for fingerprint {}", path.display()))?;
    let mut prefix = vec![0; FILE_ID_SAMPLE_BYTES.min(metadata.len() as usize)];
    let mut read = 0;
    while read < prefix.len() {
        let count = file
            .read(&mut prefix[read..])
            .with_context(|| format!("read Kimi wire stream fingerprint {}", path.display()))?;
        if count == 0 {
            prefix.truncate(read);
            break;
        }
        read += count;
    }
    Ok(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        prefix,
        #[cfg(unix)]
        device: std::os::unix::fs::MetadataExt::dev(metadata),
        #[cfg(unix)]
        inode: std::os::unix::fs::MetadataExt::ino(metadata),
    })
}

#[cfg(test)]
fn parse_complete(bytes: &[u8], path: &Path) -> Result<(TaskTracker, u64)> {
    let mut tracker = TaskTracker::default();
    let mut line_count = 0;
    for (line_number, line) in complete_lines(bytes, path)? {
        parse_line(&mut tracker, line, line_number, path)?;
        line_count = line_number + 1;
    }
    Ok((tracker, line_count))
}

fn complete_lines<'a>(bytes: &'a [u8], path: &Path) -> Result<Vec<(u64, &'a [u8])>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut line_number = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = &bytes[start..index];
        ensure_line_size(line, path, line_number)?;
        lines.push((line_number, trim_cr(line)));
        line_number += 1;
        start = index + 1;
    }
    if start < bytes.len() {
        let line = &bytes[start..];
        ensure_line_size(line, path, line_number)?;
        lines.push((line_number, trim_cr(line)));
    }
    Ok(lines)
}

fn parse_append(
    tracker: &mut TaskTracker,
    partial: &mut Vec<u8>,
    next_line_number: &mut u64,
    appended: &[u8],
    path: &Path,
) -> Result<()> {
    partial.extend_from_slice(appended);
    let mut start = 0;
    let mut line_number = *next_line_number;
    for (index, byte) in partial.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line = &partial[start..index];
        ensure_line_size(line, path, line_number)?;
        let line = trim_cr(line);
        parse_line(tracker, line, line_number, path)?;
        line_number += 1;
        start = index + 1;
    }
    if start == 0 {
        ensure_line_size(partial, path, line_number)?;
    } else {
        let tail = partial[start..].to_vec();
        ensure_line_size(&tail, path, line_number)?;
        *partial = tail;
    }
    *next_line_number = line_number;
    Ok(())
}

fn ensure_line_size(line: &[u8], path: &Path, line_number: u64) -> Result<()> {
    ensure!(
        line.len() <= MAX_WIRE_LINE_BYTES,
        "Kimi wire stream {} line {} exceeds {} bytes",
        path.display(),
        line_number + 1,
        MAX_WIRE_LINE_BYTES
    );
    Ok(())
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn parse_line(tracker: &mut TaskTracker, line: &[u8], line_number: u64, path: &Path) -> Result<()> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let record: Value = serde_json::from_slice(line).with_context(|| {
        format!(
            "parse Kimi wire stream {} line {}",
            path.display(),
            line_number + 1
        )
    })?;
    let Some(event_type) = record.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(event_kind) = task_event_kind(event_type) else {
        return Ok(());
    };
    let event = parse_task_event(&record, event_kind).with_context(|| {
        format!(
            "parse Kimi task lifecycle record in {} line {}",
            path.display(),
            line_number + 1
        )
    })?;
    if let Some(event) = event {
        tracker.apply(event);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum TaskEventType {
    Started,
    Terminated,
}

fn task_event_kind(event_type: &str) -> Option<TaskEventType> {
    match event_type {
        TASK_STARTED | LEGACY_TASK_STARTED => Some(TaskEventType::Started),
        TASK_TERMINATED | LEGACY_TASK_TERMINATED => Some(TaskEventType::Terminated),
        _ => None,
    }
}

fn parse_task_event(record: &Value, event_kind: TaskEventType) -> Result<Option<TaskEvent>> {
    if let Some(agent_id) = record.get("agentId") {
        let agent_id = agent_id
            .as_str()
            .context("lifecycle record agentId is not a string")?;
        if agent_id != "main" {
            return Ok(None);
        }
    }
    let info = record
        .get("info")
        .and_then(Value::as_object)
        .context("lifecycle record lacks object info")?;
    if let Some(info_agent_id) = info.get("agentId") {
        info_agent_id
            .as_str()
            .context("lifecycle record info.agentId is not a string")?;
    } else if record.get("agentId").is_none() {
        bail!("lifecycle record lacks agentId");
    }

    // Kimi journals detached agents and detached shell processes the same
    // way; both are background work a turn is no longer waiting on.
    if let Some(kind) = info.get("kind") {
        let kind = kind
            .as_str()
            .context("lifecycle record kind is not a string")?;
        if kind != "agent" && kind != "process" {
            return Ok(None);
        }
    } else if matches!(event_kind, TaskEventType::Started) {
        bail!("task lifecycle record lacks kind");
    }
    if let Some(detached) = info.get("detached") {
        let detached = detached
            .as_bool()
            .context("lifecycle record detached is not a boolean")?;
        if !detached {
            return Ok(None);
        }
    } else if matches!(event_kind, TaskEventType::Started) {
        bail!("task lifecycle record lacks detached");
    }

    let task_id = info
        .get("taskId")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.is_empty())
        .context("task lifecycle record lacks non-empty taskId")?
        .to_owned();
    let parent_tool_call_id = optional_string(info, "parentToolCallId")?;
    match event_kind {
        TaskEventType::Started => {
            let description = info
                .get("description")
                .and_then(Value::as_str)
                .context("task start record lacks string description")?
                .to_owned();
            let started_at_ms = info
                .get("startedAt")
                .and_then(Value::as_i64)
                .context("task start record lacks integer startedAt")?;
            if let Some(status) = info.get("status") {
                status
                    .as_str()
                    .context("task lifecycle record status is not a string")?;
            }
            Ok(Some(TaskEvent {
                kind: TaskEventKind::Started(KimiBackgroundTask {
                    task_id,
                    description,
                    started_at_ms,
                    parent_tool_call_id: parent_tool_call_id.clone(),
                }),
                parent_tool_call_id,
            }))
        }
        TaskEventType::Terminated => {
            if let Some(status) = info.get("status") {
                status
                    .as_str()
                    .context("task lifecycle record status is not a string")?;
            }
            Ok(Some(TaskEvent {
                kind: TaskEventKind::Terminated { task_id },
                parent_tool_call_id,
            }))
        }
    }
}

fn optional_string(object: &serde_json::Map<String, Value>, field: &str) -> Result<Option<String>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(
            value
                .as_str()
                .with_context(|| format!("lifecycle record {field} is not a string"))?
                .to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;

    fn session_fixture() -> (TempDir, String, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "session-test".to_owned();
        let session_dir = temp.path().join("sessions/workspace").join(&session_id);
        fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        fs::write(
            session_dir.join(STATE_FILE),
            json!({"version": 2, "id": session_id}).to_string(),
        )
        .unwrap();
        (temp, session_id, session_dir)
    }

    fn write_index(home: &Path, rows: &[Value]) {
        let contents = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(home.join(SESSION_INDEX_FILE), contents).unwrap();
    }

    fn lifecycle(
        event_type: &str,
        task_id: &str,
        parent_tool_call_id: Option<&str>,
        detached: bool,
        kind: &str,
    ) -> Value {
        let mut info = json!({
            "taskId": task_id,
            "description": "background work",
            "status": if event_type.ends_with("started") { "running" } else { "completed" },
            "detached": detached,
            "startedAt": 1234,
            "endedAt": null,
            "kind": kind,
            "agentId": "main"
        });
        if let Some(parent_tool_call_id) = parent_tool_call_id {
            info["parentToolCallId"] = Value::String(parent_tool_call_id.to_owned());
        }
        json!({"type": event_type, "agentId": "main", "info": info})
    }

    fn append_jsonl(path: &Path, records: &[Value]) {
        let mut bytes = records
            .iter()
            .map(|record| record.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn resolve_uses_last_matching_index_row_and_validates_state() {
        let (temp, session_id, session_dir) = session_fixture();
        let second = temp.path().join("sessions/other").join(&session_id);
        fs::create_dir_all(&second).unwrap();
        fs::write(
            second.join(STATE_FILE),
            json!({"id": session_id}).to_string(),
        )
        .unwrap();
        write_index(
            temp.path(),
            &[
                json!({"sessionId": session_id, "sessionDir": session_dir}),
                json!({"sessionId": session_id, "sessionDir": second}),
            ],
        );
        assert_eq!(
            resolve_session_dir(temp.path(), &session_id).unwrap(),
            second.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_rejects_session_path_outside_home() {
        let (temp, session_id, _) = session_fixture();
        let outside = tempfile::tempdir().unwrap();
        let outside_session = outside.path().join("session");
        fs::create_dir_all(&outside_session).unwrap();
        fs::write(
            outside_session.join(STATE_FILE),
            json!({"id": session_id}).to_string(),
        )
        .unwrap();
        write_index(
            temp.path(),
            &[json!({"sessionId": session_id, "sessionDir": outside_session})],
        );
        let error = resolve_session_dir(temp.path(), &session_id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes Kimi home"), "{error}");
    }

    #[test]
    fn resolve_rejects_state_id_mismatch() {
        let (temp, session_id, session_dir) = session_fixture();
        fs::write(
            session_dir.join(STATE_FILE),
            json!({"id": "other"}).to_string(),
        )
        .unwrap();
        write_index(
            temp.path(),
            &[json!({"sessionId": session_id, "sessionDir": session_dir})],
        );
        let error = resolve_session_dir(temp.path(), &session_id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn full_scan_handles_modern_legacy_filters_and_termination() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let mut modern = lifecycle(TASK_STARTED, "modern", Some("call-modern"), true, "agent");
        modern["info"]["agentId"] = json!("agent-1");
        append_jsonl(
            &wire,
            &[
                json!({"type":"unrelated", "value": 1}),
                modern,
                lifecycle(
                    LEGACY_TASK_STARTED,
                    "legacy",
                    Some("call-legacy"),
                    true,
                    "agent",
                ),
                lifecycle(TASK_STARTED, "shell", None, false, "agent"),
                lifecycle(TASK_STARTED, "review", None, true, "tool"),
                lifecycle(TASK_STARTED, "other-agent", None, true, "agent").tap_agent("worker"),
                lifecycle(
                    TASK_TERMINATED,
                    "modern",
                    Some("call-modern"),
                    true,
                    "agent",
                ),
            ],
        );
        let snapshot = full_scan(&wire).unwrap();
        assert_eq!(
            snapshot
                .tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            ["legacy"]
        );
        assert!(snapshot.provider_tool_ids.contains("call-modern"));
        assert!(snapshot.provider_tool_ids.contains("call-legacy"));
    }

    #[test]
    fn duplicate_starts_are_idempotent_and_terminal_status_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let start = lifecycle(TASK_STARTED, "same", None, true, "agent");
        let mut terminate = lifecycle(TASK_TERMINATED, "same", None, true, "agent");
        terminate["info"]["status"] = json!("aborted");
        append_jsonl(&wire, &[start.clone(), start, terminate]);
        assert!(full_scan(&wire).unwrap().tasks.is_empty());
    }

    #[test]
    fn detached_process_task_is_tracked_until_it_terminates() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        // Kimi's process records carry no info.agentId and may omit timeoutMs.
        let mut start = lifecycle(
            TASK_STARTED,
            "bash-r5ae",
            Some("tool_bash"),
            true,
            "process",
        );
        start["info"].as_object_mut().unwrap().remove("agentId");
        let mut terminate = lifecycle(
            TASK_TERMINATED,
            "bash-r5ae",
            Some("tool_bash"),
            true,
            "process",
        );
        terminate["info"].as_object_mut().unwrap().remove("agentId");
        append_jsonl(&wire, &[start.clone()]);

        let snapshot = full_scan(&wire).unwrap();
        assert_eq!(
            snapshot
                .tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            ["bash-r5ae"]
        );
        assert_eq!(
            snapshot.task("bash-r5ae").unwrap().parent_tool_call_id,
            Some("tool_bash".to_owned())
        );

        append_jsonl(&wire, &[start, terminate]);
        assert!(full_scan(&wire).unwrap().tasks.is_empty());
    }

    #[test]
    fn legacy_termination_alias_removes_task() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        append_jsonl(
            &wire,
            &[
                lifecycle(LEGACY_TASK_STARTED, "legacy", None, true, "agent"),
                lifecycle(LEGACY_TASK_TERMINATED, "legacy", None, true, "agent"),
            ],
        );

        assert!(full_scan(&wire).unwrap().tasks.is_empty());
    }

    #[test]
    fn follower_retains_partial_line_until_completion() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let line = lifecycle(TASK_STARTED, "partial", None, true, "agent").to_string();
        fs::write(&wire, &line[..line.len() / 2]).unwrap();
        let mut follower = KimiWireFollower::open(&wire).unwrap();
        assert!(follower.snapshot().tasks.is_empty());
        let mut file = OpenOptions::new().append(true).open(&wire).unwrap();
        use std::io::Write;
        file.write_all(&line.as_bytes()[line.len() / 2..]).unwrap();
        file.write_all(b"\n").unwrap();
        assert!(matches!(
            follower.refresh().unwrap(),
            KimiWireRefresh::Updated(_)
        ));
        assert_eq!(follower.snapshot().tasks[0].task_id, "partial");
    }

    #[test]
    fn follower_retains_partial_tail_when_it_matches_consumed_prefix_length() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let complete = b"{\"type\":\"unknown\"}\n";
        let partial = lifecycle(TASK_STARTED, "equal-tail", None, true, "agent").to_string();
        assert!(partial.len() >= complete.len());
        let mut first_append = complete.to_vec();
        first_append.extend_from_slice(&partial.as_bytes()[..complete.len()]);
        fs::write(&wire, first_append).unwrap();

        let mut follower = KimiWireFollower::open(&wire).unwrap();
        assert!(follower.snapshot().tasks.is_empty());

        let mut file = OpenOptions::new().append(true).open(&wire).unwrap();
        use std::io::Write;
        file.write_all(&partial.as_bytes()[complete.len()..])
            .unwrap();
        file.write_all(b"\n").unwrap();
        follower.refresh().unwrap();

        assert_eq!(follower.snapshot().tasks[0].task_id, "equal-tail");
    }

    #[test]
    fn malformed_relevant_line_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        fs::write(&wire, b"{\"type\":\"task.started\",\"agentId\":\"main\"}\n").unwrap();
        let error = full_scan(&wire).unwrap_err().to_string();
        assert!(error.contains("task lifecycle"), "{error}");
    }

    #[test]
    fn follower_detects_truncation_and_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        append_jsonl(&wire, &[json!({"type":"unknown"})]);
        let mut follower = KimiWireFollower::open(&wire).unwrap();
        fs::write(&wire, b"{}\n").unwrap();
        // Ensure filesystems with coarse timestamp granularity still expose
        // the replacement through the Unix inode identity or prefix sample.
        thread::sleep(Duration::from_millis(2));
        assert!(matches!(
            follower.refresh().unwrap(),
            KimiWireRefresh::RescanRequired
        ));
    }

    trait AgentOverride {
        fn tap_agent(self, agent_id: &str) -> Self;
    }

    impl AgentOverride for Value {
        fn tap_agent(mut self, agent_id: &str) -> Self {
            self["agentId"] = Value::String(agent_id.to_owned());
            self
        }
    }
}
