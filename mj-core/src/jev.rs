//! Bounded, local Jev diagnostics. No transcript or activity state is mutated here.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};

const SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
const SEGMENTS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub version: u32,
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub status: String,
    pub checked: String,
    pub answer: String,
    pub action: String,
    pub scope: String,
    pub owner: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub technical: Option<Value>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionPage {
    pub decisions: Vec<Decision>,
    pub warnings: Vec<String>,
}

impl DecisionPage {
    pub fn merge(&mut self, other: Self) {
        self.decisions.extend(other.decisions);
        self.warnings.extend(other.warnings);
        self.decisions
            .sort_by_key(|d| std::cmp::Reverse((d.started_at_ms, d.id.clone())));
        self.decisions.truncate(100);
    }
}

enum Message {
    Append(Box<Decision>),
    Read(String, Option<String>, mpsc::Sender<Result<DecisionPage>>),
}

#[derive(Debug)]
struct Writer {
    tx: mpsc::Sender<Message>,
}

#[derive(Clone, Debug)]
pub struct DecisionLog(Arc<Writer>);

type Registry = Mutex<HashMap<PathBuf, Weak<Writer>>>;
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}
fn owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| format!("{}-{}", std::process::id(), crate::clock::epoch_millis()))
}

pub fn controller_log_dir() -> PathBuf {
    crate::config::data_dir().join("jev-decisions")
}

impl DecisionLog {
    /// Starts a dedicated writer. Directory creation and all file I/O happen there.
    pub fn open(directory: PathBuf) -> Result<Self> {
        let mut registry = registry().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(writer) = registry.get(&directory).and_then(Weak::upgrade) {
            return Ok(Self(writer));
        }
        let (tx, rx) = mpsc::channel();
        let writer = Arc::new(Writer { tx });
        let path = directory.clone();
        std::thread::Builder::new()
            .name("jev-diagnostics".into())
            .spawn(move || {
                let mut failure = None;
                while let Ok(message) = rx.recv() {
                    match message {
                        Message::Append(record) => {
                            if let Err(error) = append(&path, &record, SEGMENT_BYTES) {
                                tracing::warn!(%error, "Jev diagnostic write failed");
                                failure = Some(format!("Jev diagnostic write failed: {error:#}"));
                            }
                        }
                        Message::Read(session, id, reply) => {
                            let result =
                                read_files(&path, &session, id.as_deref()).map(|mut page| {
                                    if let Some(error) = &failure {
                                        page.warnings.push(error.clone());
                                    }
                                    page
                                });
                            // A closed inspector may have cancelled its read.
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .context("start Jev diagnostic writer")?;
        registry.insert(directory, Arc::downgrade(&writer));
        Ok(Self(writer))
    }

    fn append(&self, decision: &Decision) {
        if self
            .0
            .tx
            .send(Message::Append(Box::new(decision.clone())))
            .is_err()
        {
            tracing::warn!(decision_id = %decision.id, "Jev diagnostic writer stopped");
        }
    }

    pub fn start(&self, session: &str, kind: &str, checked: &str, scope: &str) -> Attempt {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let now = crate::clock::epoch_millis();
        let record = Decision {
            version: 1,
            id: format!("{}-{:020}", owner(), NEXT.fetch_add(1, Ordering::Relaxed)),
            session_id: session.into(),
            kind: kind.into(),
            started_at_ms: now,
            updated_at_ms: now,
            status: "pending".into(),
            checked: checked.into(),
            scope: scope.into(),
            answer: "Waiting for Jev.".into(),
            action: "No action yet.".into(),
            owner: owner().into(),
            technical: Some(serde_json::json!({})),
        };
        self.append(&record);
        Attempt(Arc::new(Mutex::new(AttemptState {
            log: self.clone(),
            record,
            finished: false,
        })))
    }
}

/// Shared across classification and application; the last owner records cancellation.
#[derive(Clone, Debug)]
pub struct Attempt(Arc<Mutex<AttemptState>>);
#[derive(Debug)]
struct AttemptState {
    log: DecisionLog,
    record: Decision,
    finished: bool,
}
impl Drop for AttemptState {
    fn drop(&mut self) {
        if !self.finished {
            self.record.status = "cancelled".into();
            self.record.action = "Check cancelled before its outcome was confirmed.".into();
            self.record.updated_at_ms = crate::clock::epoch_millis();
            self.log.append(&self.record);
        }
    }
}
impl Attempt {
    pub fn id(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record
            .id
            .clone()
    }
    pub fn update(&self, answer: Option<&str>, technical: Value) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.finished {
            return;
        }
        if let Some(answer) = answer {
            state.record.answer = answer.into();
        }
        if let (Some(existing), Some(fields)) = (
            state
                .record
                .technical
                .as_mut()
                .and_then(Value::as_object_mut),
            technical.as_object(),
        ) {
            existing.extend(fields.clone());
        }
        state.record.updated_at_ms = crate::clock::epoch_millis();
        state.log.append(&state.record);
    }
    pub fn finish(&self, status: &str, action: &str) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.finished {
            return;
        }
        state.finished = true;
        state.record.status = status.into();
        state.record.action = action.into();
        state.record.updated_at_ms = crate::clock::epoch_millis();
        state.log.append(&state.record);
    }
}

fn segment(directory: &Path, n: usize) -> PathBuf {
    directory.join(format!("decisions.{n}.jsonl"))
}
fn append(directory: &Path, record: &Decision, limit: u64) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let current = segment(directory, 0);
    if std::fs::metadata(&current).map(|m| m.len()).unwrap_or(0) + bytes.len() as u64 > limit {
        for n in (1..SEGMENTS).rev() {
            let previous = segment(directory, n - 1);
            let dest = segment(directory, n);
            if dest.exists() {
                std::fs::remove_file(&dest)?;
            }
            if previous.exists() {
                std::fs::rename(previous, dest)?;
            }
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(current)?.write_all(&bytes)?;
    Ok(())
}

/// Blocking read; callers must use a background task. Live reads are ordered after writes.
pub fn read(directory: &Path, session: &str, id: Option<&str>) -> Result<DecisionPage> {
    let writer = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(directory)
        .and_then(Weak::upgrade);
    if let Some(writer) = writer {
        let (tx, rx) = mpsc::channel();
        writer
            .tx
            .send(Message::Read(session.into(), id.map(str::to_owned), tx))
            .context("Jev diagnostic writer stopped")?;
        return rx.recv().context("Jev diagnostic reader stopped")?;
    }
    read_files(directory, session, id)
}
fn read_files(directory: &Path, session: &str, id: Option<&str>) -> Result<DecisionPage> {
    let mut records = BTreeMap::new();
    let mut page = DecisionPage::default();
    for n in (0..SEGMENTS).rev() {
        let file = match std::fs::File::open(segment(directory, n)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("read Jev diagnostics"),
        };
        let mut reader = std::io::BufReader::new(file.take(SEGMENT_BYTES + 1024 * 1024));
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if !line.ends_with('\n') {
                page.warnings
                    .push("An incomplete diagnostic write was skipped.".into());
                break;
            }
            match serde_json::from_str::<Decision>(&line) {
                Ok(mut record)
                    if record.version == 1
                        && record.session_id == session
                        && id.is_none_or(|id| record.id == id) =>
                {
                    if record.status == "pending" && record.owner != owner() {
                        record.status = "interrupted".into();
                        record.action =
                            "The previous process ended before recording an outcome.".into();
                    }
                    if id.is_none() {
                        record.technical = None;
                    }
                    records.insert(record.id.clone(), record);
                }
                Ok(_) => {}
                Err(_) => {
                    if !page
                        .warnings
                        .iter()
                        .any(|w| w == "An unreadable diagnostic record was skipped.")
                    {
                        page.warnings
                            .push("An unreadable diagnostic record was skipped.".into());
                    }
                }
            }
        }
    }
    page.decisions = records.into_values().collect();
    page.decisions
        .sort_by_key(|d| std::cmp::Reverse((d.started_at_ms, d.id.clone())));
    page.decisions.truncate(100);
    Ok(page)
}
use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_input_and_final_action_are_correlated_without_exposing_inputs_in_lists() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path().into()).unwrap();
        let attempt = log.start(
            "s",
            "activity",
            "Who needs to act?",
            "Current conversation and live facts",
        );
        let input =
            serde_json::json!({"text":"résumé 🛠", "tools_in_flight":["shell"], "omitted":true});
        attempt.update(Some("Finished"), serde_json::json!({"request":input}));
        attempt.finish("stale", "New output arrived; mj kept the runtime status.");
        let list = read(dir.path(), "s", None).unwrap();
        assert_eq!(list.decisions.len(), 1);
        assert!(list.decisions[0].technical.is_none());
        let detail = read(dir.path(), "s", Some(&attempt.id())).unwrap();
        assert_eq!(
            detail.decisions[0].technical.as_ref().unwrap()["request"],
            input
        );
        assert_eq!(detail.decisions[0].status, "stale");
        assert!(
            read(dir.path(), "other", None)
                .unwrap()
                .decisions
                .is_empty()
        );
    }
    #[test]
    fn cancellation_rotation_restart_and_partial_writes_are_honest() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path().into()).unwrap();
        let attempt = log.start("s", "continuation", "Work left?", "Earlier instructions");
        let id = attempt.id();
        drop(attempt);
        let mut record = read(dir.path(), "s", Some(&id))
            .unwrap()
            .decisions
            .remove(0);
        assert_eq!(record.status, "cancelled");
        drop(log);
        // Each record occupies its own segment with this deliberately small limit.
        for i in 0..8 {
            record.id = i.to_string();
            append(dir.path(), &record, 1).unwrap();
        }
        assert_eq!(
            read_files(dir.path(), "s", None).unwrap().decisions.len(),
            4
        );
        assert!(
            read_files(dir.path(), "s", Some(&id))
                .unwrap()
                .decisions
                .is_empty()
        );
        record.status = "pending".into();
        record.owner = "previous-process".into();
        append(dir.path(), &record, SEGMENT_BYTES).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(segment(dir.path(), 0))
            .unwrap()
            .write_all(b"{broken")
            .unwrap();
        let page = read_files(dir.path(), "s", Some(&record.id)).unwrap();
        assert_eq!(page.decisions[0].status, "interrupted");
        assert_eq!(page.warnings.len(), 1);
    }
}
