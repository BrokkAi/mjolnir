//! Local adapter/model ownership for ACP session IDs.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    pub session_id: String,
    pub cwd: PathBuf,
    pub adapter_source_id: String,
    pub model: String,
    pub model_value: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    sessions: Vec<Record>,
}

static WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub fn default_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("session-provenance.json")
}

pub fn find(session_id: &str, cwd: &Path) -> Option<Record> {
    find_at(&default_path(), session_id, cwd)
}

fn find_at(path: &Path, session_id: &str, cwd: &Path) -> Option<Record> {
    load(path)
        .ok()?
        .sessions
        .into_iter()
        .rev()
        .find(|record| record.session_id == session_id && record.cwd == cwd)
}

pub fn record(record: Record) {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(error) = record_at(&default_path(), record) {
        tracing::warn!("persist session provenance: {error:#}");
    }
}

pub fn remove(session_id: &str, cwd: &Path, adapter_source_id: Option<&str>) {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = remove_at(&default_path(), session_id, cwd, adapter_source_id);
}

fn remove_at(
    path: &Path,
    session_id: &str,
    cwd: &Path,
    adapter_source_id: Option<&str>,
) -> Result<()> {
    let mut store = load(path)?;
    store.sessions.retain(|record| {
        record.session_id != session_id
            || record.cwd != cwd
            || adapter_source_id.is_some_and(|adapter| record.adapter_source_id != adapter)
    });
    save(path, &store)
}

fn record_at(path: &Path, record: Record) -> Result<()> {
    let mut store = load(path).unwrap_or_default();
    store.sessions.retain(|existing| {
        existing.session_id != record.session_id
            || existing.cwd != record.cwd
            || existing.adapter_source_id != record.adapter_source_id
    });
    store.sessions.push(record);
    save(path, &store)
}

fn load(path: &Path) -> Result<Store> {
    if !path.exists() {
        return Ok(Store::default());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn save(path: &Path, store: &Store) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(store)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(adapter: &str, model: &str, cwd: &str) -> Record {
        Record {
            session_id: "same".into(),
            cwd: PathBuf::from(cwd),
            adapter_source_id: adapter.into(),
            model: model.into(),
            model_value: model.into(),
        }
    }

    #[test]
    fn record_replaces_same_adapter_session_without_colliding_across_adapters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provenance.json");
        record_at(&path, record("codex-acp", "gpt-old", "/tmp/work")).unwrap();
        record_at(&path, record("codex-acp", "gpt-new", "/tmp/work")).unwrap();
        record_at(&path, record("opencode-acp", "gpt-other", "/tmp/work")).unwrap();
        let store = load(&path).unwrap();
        assert_eq!(store.sessions.len(), 2);
        assert!(store.sessions.iter().any(|entry| entry.model == "gpt-new"));
    }

    #[test]
    fn find_returns_newest_record_matching_session_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provenance.json");
        record_at(&path, record("codex-acp", "gpt-old", "/tmp/work")).unwrap();
        record_at(&path, record("codex-acp", "gpt-other", "/tmp/other")).unwrap();
        record_at(&path, record("opencode-acp", "gpt-newest", "/tmp/work")).unwrap();

        let found = find_at(&path, "same", Path::new("/tmp/work")).expect("matching record");
        assert_eq!(found.adapter_source_id, "opencode-acp");
        assert_eq!(found.model, "gpt-newest");
        assert!(find_at(&path, "missing", Path::new("/tmp/work")).is_none());
        assert!(find_at(&path, "same", Path::new("/tmp/missing")).is_none());
    }

    #[test]
    fn remove_can_scope_one_adapter_or_all_adapters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provenance.json");
        record_at(&path, record("codex-acp", "gpt-codex", "/tmp/work")).unwrap();
        record_at(&path, record("opencode-acp", "gpt-opencode", "/tmp/work")).unwrap();
        record_at(&path, record("codex-acp", "gpt-other", "/tmp/other")).unwrap();

        remove_at(&path, "same", Path::new("/tmp/work"), Some("codex-acp")).unwrap();
        let store = load(&path).unwrap();
        assert_eq!(store.sessions.len(), 2);
        assert!(
            store
                .sessions
                .iter()
                .any(|entry| entry.adapter_source_id == "opencode-acp")
        );

        remove_at(&path, "same", Path::new("/tmp/work"), None).unwrap();
        let store = load(&path).unwrap();
        assert_eq!(store.sessions.len(), 1);
        assert_eq!(store.sessions[0].cwd, PathBuf::from("/tmp/other"));
    }

    #[test]
    fn load_handles_missing_files_and_reports_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        assert!(load(&missing).unwrap().sessions.is_empty());

        let malformed = dir.path().join("malformed.json");
        std::fs::write(&malformed, "not json").unwrap();
        let error = match load(&malformed) {
            Ok(_) => panic!("malformed store unexpectedly loaded"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("parse"), "{message}");
        assert!(message.contains("malformed.json"), "{message}");
    }

    #[test]
    fn save_creates_parent_directories_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state/provenance.json");
        let store = Store {
            sessions: vec![record("codex-acp", "gpt-current", "/tmp/work")],
        };

        save(&path, &store).unwrap();
        assert_eq!(load(&path).unwrap().sessions, store.sessions);
        assert!(!path.with_extension("json.tmp").exists());
    }
}
