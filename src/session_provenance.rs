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
    load(&default_path())
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
    let path = default_path();
    let Ok(mut store) = load(&path) else {
        return;
    };
    store.sessions.retain(|record| {
        record.session_id != session_id
            || record.cwd != cwd
            || adapter_source_id.is_some_and(|adapter| record.adapter_source_id != adapter)
    });
    let _ = save(&path, &store);
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

    #[test]
    fn record_replaces_same_adapter_session_without_colliding_across_adapters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provenance.json");
        let record = |adapter: &str, model: &str| Record {
            session_id: "same".into(),
            cwd: PathBuf::from("/tmp/work"),
            adapter_source_id: adapter.into(),
            model: model.into(),
            model_value: model.into(),
        };
        record_at(&path, record("codex-acp", "gpt-old")).unwrap();
        record_at(&path, record("codex-acp", "gpt-new")).unwrap();
        record_at(&path, record("opencode-acp", "gpt-other")).unwrap();
        let store = load(&path).unwrap();
        assert_eq!(store.sessions.len(), 2);
        assert!(store.sessions.iter().any(|entry| entry.model == "gpt-new"));
    }
}
