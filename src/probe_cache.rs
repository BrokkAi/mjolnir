//! Disk-persisted ACP adapter probe results. Warm startups bind the roster
//! from this cache instead of relaunching every adapter; entries are keyed by
//! the launch identity and invalidated by TTL or a changed adapter binary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::probe::{AdapterCapabilities, ModelOption};
use agent_client_protocol::schema::v1::SessionConfigOption;

pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_FORMAT_VERSION: u32 = 2;

pub fn default_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("mj")
        .join("acp-probes-v1.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    captured_at_unix: u64,
    fingerprint: Option<Fingerprint>,
    http_mcp: bool,
    models: Vec<ModelOption>,
    #[serde(default)]
    session_config: Vec<SessionConfigOption>,
    #[serde(default)]
    session_config_known: bool,
}

/// Identity of the adapter binary the entry was captured from. `None` when
/// the command cannot be stat'ed (e.g. resolved through an interpreter); such
/// entries are valid on TTL alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    modified_unix: u64,
    len: u64,
}

fn command_fingerprint(command: &Path) -> Option<Fingerprint> {
    let metadata = std::fs::metadata(command).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Fingerprint {
        modified_unix: modified,
        len: metadata.len(),
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn read(path: &Path) -> CacheFile {
    let file: CacheFile = std::fs::read(path)
        .ok()
        .and_then(|contents| serde_json::from_slice(&contents).ok())
        .unwrap_or_default();
    if file.version == CACHE_FORMAT_VERSION {
        file
    } else {
        CacheFile {
            version: CACHE_FORMAT_VERSION,
            ..CacheFile::default()
        }
    }
}

/// Fresh cached capabilities for `key`, or `None` when the entry is missing,
/// older than `ttl`, or captured from a different adapter binary.
pub fn load(path: &Path, key: &str, command: &Path, ttl: Duration) -> Option<AdapterCapabilities> {
    let entry = read(path).entries.remove(key)?;
    let age = now_unix().saturating_sub(entry.captured_at_unix);
    if age >= ttl.as_secs() {
        return None;
    }
    if entry.fingerprint != command_fingerprint(command) {
        return None;
    }
    Some(AdapterCapabilities {
        http_mcp: entry.http_mcp,
        models: entry.models,
        session_config: entry.session_config,
        session_config_known: entry.session_config_known,
    })
}

/// Record freshly probed capabilities. Failures are never cached, so a broken
/// adapter is re-probed on the next resolution instead of staying broken for
/// a full TTL. Best-effort: cache write errors are ignored.
pub fn store(path: &Path, key: &str, command: &Path, capabilities: &AdapterCapabilities) {
    let mut file = read(path);
    file.version = CACHE_FORMAT_VERSION;
    file.entries.insert(
        key.to_string(),
        Entry {
            captured_at_unix: now_unix(),
            fingerprint: command_fingerprint(command),
            http_mcp: capabilities.http_mcp,
            models: capabilities.models.clone(),
            session_config: capabilities.session_config.clone(),
            session_config_known: capabilities.session_config_known,
        },
    );
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(serialized) = serde_json::to_vec_pretty(&file) else {
        return;
    };
    // Atomic replace so concurrent mj processes never observe a torn file.
    let Ok(temp) = tempfile::NamedTempFile::new_in(parent) else {
        return;
    };
    if std::io::Write::write_all(&mut temp.as_file(), &serialized).is_ok() {
        let _ = temp.persist(path);
    }
}

/// Remove every persisted adapter capability entry.
///
/// Returns whether a cache file existed. A missing cache is already clear and
/// therefore succeeds.
pub fn clear(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionConfigOption, SessionConfigSelectOption};

    fn capabilities(model: &str) -> AdapterCapabilities {
        AdapterCapabilities {
            http_mcp: true,
            models: vec![ModelOption {
                value: model.to_string(),
                name: model.to_string(),
                description: None,
            }],
            session_config: Vec::new(),
            session_config_known: true,
        }
    }

    #[test]
    fn cached_probe_roundtrips_for_an_unchanged_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("probes.json");
        let command = dir.path().join("agent");
        std::fs::write(&command, b"binary").expect("command");

        let mut capabilities = capabilities("m1");
        capabilities.session_config = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![SessionConfigSelectOption::new("default", "Default")],
        )];
        store(&cache, "custom:company", &command, &capabilities);
        let loaded =
            load(&cache, "custom:company", &command, CACHE_TTL).expect("fresh cache entry");
        assert!(loaded.http_mcp);
        assert_eq!(loaded.models[0].value, "m1");
        assert!(loaded.session_config_known);
        assert_eq!(loaded.session_config[0].id.to_string(), "service_tier");

        assert!(load(&cache, "other-key", &command, CACHE_TTL).is_none());
    }

    #[test]
    fn expired_entries_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("probes.json");
        let command = dir.path().join("agent");
        std::fs::write(&command, b"binary").expect("command");

        store(&cache, "kimi", &command, &capabilities("m1"));
        assert!(load(&cache, "kimi", &command, Duration::ZERO).is_none());
    }

    #[test]
    fn older_cache_formats_are_invalidated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("probes.json");
        let command = dir.path().join("agent");
        std::fs::write(&command, b"binary").expect("command");
        let contents = serde_json::json!({
            "entries": {
                "codex-acp": {
                    "captured_at_unix": now_unix(),
                    "fingerprint": command_fingerprint(&command),
                    "http_mcp": true,
                    "models": []
                }
            }
        });
        std::fs::write(&cache, serde_json::to_vec(&contents).expect("serialize"))
            .expect("write old cache");

        assert!(load(&cache, "codex-acp", &command, CACHE_TTL).is_none());
    }

    #[test]
    fn changed_binary_invalidates_the_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("probes.json");
        let command = dir.path().join("agent");
        std::fs::write(&command, b"binary").expect("command");

        store(&cache, "kimi", &command, &capabilities("m1"));
        std::fs::write(&command, b"binary-upgraded").expect("replace command");
        assert!(load(&cache, "kimi", &command, CACHE_TTL).is_none());
    }

    #[test]
    fn unstatable_commands_cache_on_ttl_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("probes.json");
        let command = dir.path().join("missing-binary");

        store(&cache, "custom:npx", &command, &capabilities("m1"));
        assert!(load(&cache, "custom:npx", &command, CACHE_TTL).is_some());
    }

    #[test]
    fn clearing_removes_every_entry_and_missing_cache_is_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("probes.json");
        let command = dir.path().join("agent");
        std::fs::write(&command, b"binary").expect("command");

        store(&cache, "codex-acp", &command, &capabilities("gpt"));
        store(&cache, "kimi", &command, &capabilities("kimi"));

        assert!(clear(&cache).expect("clear populated cache"));
        assert!(load(&cache, "codex-acp", &command, CACHE_TTL).is_none());
        assert!(load(&cache, "kimi", &command, CACHE_TTL).is_none());
        assert!(!clear(&cache).expect("clear missing cache"));
    }
}
