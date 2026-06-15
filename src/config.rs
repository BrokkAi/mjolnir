//! Persistent user config for `mj`.
//!
//! Stores the default launch command and global picker preferences. Lives at
//! `~/.config/mj/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::expand_home_shortcut;

/// Environment variable carrying the OpenRouter API key. mjolnir injects
/// the active named key under this name when spawning the agent; anvil
/// reads it at startup (taking precedence over its on-disk credential).
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Label assigned to a single legacy key when migrating an older config
/// that stored `OPENROUTER_API_KEY` directly in the agent environment.
pub const DEFAULT_OPENROUTER_KEY_LABEL: &str = "default";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<SelectedAgent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorite_agents: Vec<String>,
    /// Named OpenRouter API keys the user can switch between. Labels are
    /// unique; the raw key is never shown in the UI (see [`OpenRouterKey::masked`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openrouter_keys: Vec<OpenRouterKey>,
    /// Label of the currently selected key in `openrouter_keys`. Persisted
    /// globally so the choice survives across sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_openrouter_key: Option<String>,
}

/// A single named OpenRouter API key. `label` is a human-friendly, unique
/// identifier shown in the picker/header; `key` is the secret and is never
/// rendered in full.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct OpenRouterKey {
    pub label: String,
    pub key: String,
}

impl OpenRouterKey {
    /// Masked rendering of the secret: at most the last four characters,
    /// prefixed with bullets. Never reveals more than the tail so it is
    /// safe to log or show on screen.
    pub fn masked(&self) -> String {
        mask_secret(&self.key)
    }
}

/// A label + masked-key pair safe to hand to the UI. Carries no secret
/// material so it can live in `AppState` and be rendered freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOption {
    pub label: String,
    pub masked: String,
}

/// Mask a secret to at most its last four characters, prefixed with
/// bullets. Strings of four characters or fewer are fully masked so a
/// short secret is never echoed back in the clear.
pub fn mask_secret(secret: &str) -> String {
    let count = secret.chars().count();
    if count <= 4 {
        return "•".repeat(count);
    }
    let last4: String = {
        let mut chars: Vec<char> = secret.chars().collect();
        chars.drain(..chars.len() - 4);
        chars.into_iter().collect()
    };
    format!("••••{last4}")
}

/// Launch command resolved by the picker. `source_id` identifies where
/// the choice came from so the picker can highlight the default row.
/// `"anvil"` and `"custom"` are reserved; everything else is a registry
/// agent id.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectedAgent {
    pub source_id: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

impl Config {
    /// Read the config from `path`. Returns `Config::default()` when the
    /// file does not exist; surfaces a parse error otherwise.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let s =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?;
        cfg.normalize();
        Ok(cfg)
    }

    /// Atomic-ish save: write to a tmp sibling then rename. Creates the
    /// parent directory on demand.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialize config")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    fn normalize(&mut self) {
        if let Some(agent) = self.agent.as_mut() {
            if agent.source_id == "anvil" {
                agent.program = PathBuf::from("uvx");
                agent.args = vec!["brokk".to_string(), "acp".to_string()];
            } else if agent.source_id == "custom" {
                agent.program = expand_home_shortcut(&agent.program.to_string_lossy());
                agent.args = agent
                    .args
                    .iter()
                    .map(|arg| expand_home_shortcut(arg).to_string_lossy().into_owned())
                    .collect();
            }
        }
        self.migrate_openrouter_keys();
        self.normalize_openrouter_keys();
    }

    /// Backward-compat migration: an older config kept the OpenRouter key
    /// directly in the agent environment as `OPENROUTER_API_KEY`. Lift it
    /// into a single labeled entry so existing setups keep working and the
    /// key becomes switchable. Only runs when no named keys exist yet, so
    /// it never clobbers a config that already opted into the new model.
    fn migrate_openrouter_keys(&mut self) {
        if !self.openrouter_keys.is_empty() {
            return;
        }
        let Some(agent) = self.agent.as_mut() else {
            return;
        };
        if let Some(key) = agent.env.remove(OPENROUTER_API_KEY_ENV) {
            self.openrouter_keys.push(OpenRouterKey {
                label: DEFAULT_OPENROUTER_KEY_LABEL.to_string(),
                key,
            });
            self.active_openrouter_key = Some(DEFAULT_OPENROUTER_KEY_LABEL.to_string());
        }
    }

    /// Enforce the invariants the rest of the code relies on: unique labels
    /// (first occurrence wins) and an `active_openrouter_key` that always
    /// points at an existing entry (defaulting to the first key, or `None`
    /// when the list is empty).
    fn normalize_openrouter_keys(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.openrouter_keys
            .retain(|k| seen.insert(k.label.clone()));

        let active_valid = self
            .active_openrouter_key
            .as_ref()
            .is_some_and(|label| self.openrouter_keys.iter().any(|k| &k.label == label));
        if !active_valid {
            self.active_openrouter_key = self.openrouter_keys.first().map(|k| k.label.clone());
        }
    }

    /// The currently selected key, if one is configured.
    pub fn active_openrouter_key(&self) -> Option<&OpenRouterKey> {
        let label = self.active_openrouter_key.as_ref()?;
        self.openrouter_keys.iter().find(|k| &k.label == label)
    }

    /// Select the key with `label` as active. Returns `false` (leaving the
    /// previous selection untouched) when no key carries that label.
    pub fn set_active_openrouter_key(&mut self, label: &str) -> bool {
        if self.openrouter_keys.iter().any(|k| k.label == label) {
            self.active_openrouter_key = Some(label.to_string());
            true
        } else {
            false
        }
    }

    /// Label + masked-key pairs for every configured key, in order. Safe to
    /// hand to the UI — carries no secret material.
    pub fn openrouter_key_options(&self) -> Vec<KeyOption> {
        self.openrouter_keys
            .iter()
            .map(|k| KeyOption {
                label: k.label.clone(),
                masked: k.masked(),
            })
            .collect()
    }
}

/// Read-modify-write the active OpenRouter key on disk. Loads the config at
/// `path`, switches the active key, and saves it back, preserving every
/// other field. Returns `Ok(false)` when no key with `label` exists.
pub fn set_active_openrouter_key_on_disk(path: &Path, label: &str) -> Result<bool> {
    let mut cfg = Config::load(path)?;
    if !cfg.set_active_openrouter_key(label) {
        return Ok(false);
    }
    cfg.save(path)?;
    Ok(true)
}

/// Default config path: `$XDG_CONFIG_HOME/mj/config.toml` (or
/// `~/.config/mj/config.toml` when `XDG_CONFIG_HOME` is unset).
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("config.toml")
}

/// Path for the persisted prompt-history file (NUL-delimited format to
/// support multiline prompts): `$XDG_CONFIG_HOME/mj/history.txt`.
pub fn history_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("history.txt")
}

/// Maximum number of history entries kept on disk. Older entries are
/// trimmed when the limit is exceeded.
pub const HISTORY_MAX_ENTRIES: usize = 100;

/// Load the prompt history from a NUL-delimited file (supports multiline
/// prompts). Returns an empty `Vec` when the file does not exist or is
/// unreadable.
pub fn load_history(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path).map_err(|e| tracing::warn!("load_history {path:?}: {e}")) {
        Ok(body) => body
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Persist the prompt history to disk in NUL-delimited format, capped
/// at `HISTORY_MAX_ENTRIES`.
pub fn save_history(path: &Path, entries: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create history dir {}", parent.display()))?;
    }
    let tail = if entries.len() > HISTORY_MAX_ENTRIES {
        &entries[entries.len() - HISTORY_MAX_ENTRIES..]
    } else {
        entries
    };
    let body = tail.join("\0");
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_history_returns_empty_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries = load_history(&path);
        assert!(entries.is_empty());
    }

    #[test]
    fn load_save_history_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries: Vec<String> = (0..5).map(|i| format!("prompt {i}")).collect();
        save_history(&path, &entries).expect("save");
        let loaded = load_history(&path);
        assert_eq!(loaded, entries);
    }

    #[test]
    fn save_history_caps_at_max_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries: Vec<String> = (0..120).map(|i| format!("prompt {i}")).collect();
        save_history(&path, &entries).expect("save");
        let loaded = load_history(&path);
        assert_eq!(loaded.len(), HISTORY_MAX_ENTRIES);
        // Keeps the most recent entries (tail).
        assert_eq!(loaded[0], format!("prompt {}", 120 - HISTORY_MAX_ENTRIES));
        assert_eq!(loaded[loaded.len() - 1], "prompt 119");
    }

    #[test]
    fn save_history_creates_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("history.txt");
        save_history(&path, &["hi".to_string()]).expect("save");
        assert_eq!(load_history(&path), vec!["hi".to_string()]);
    }

    #[test]
    fn save_load_history_preserves_multiline_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries = vec![
            "single line".to_string(),
            "line one\nline two\nline three".to_string(),
            "another single".to_string(),
        ];
        save_history(&path, &entries).expect("save");
        let loaded = load_history(&path);
        assert_eq!(loaded, entries);
    }

    #[test]
    fn save_empty_history_writes_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        save_history(&path, &[]).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert_eq!(body, "");
        let loaded = load_history(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.toml");
        let cfg = Config::load(&path).expect("load");
        assert!(cfg.agent.is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            agent: Some(SelectedAgent {
                source_id: "claude-acp".to_string(),
                program: PathBuf::from("/usr/local/bin/claude-acp"),
                args: vec!["--quiet".to_string()],
                env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            }),
            favorite_agents: vec!["claude-acp".to_string(), "anvil".to_string()],
            ..Default::default()
        };
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.favorite_agents, vec!["claude-acp", "anvil"]);
        let agent = loaded.agent.expect("agent");
        assert_eq!(agent.source_id, "claude-acp");
        assert_eq!(agent.program, PathBuf::from("/usr/local/bin/claude-acp"));
        assert_eq!(agent.args, vec!["--quiet"]);
        assert_eq!(agent.env.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("config.toml");
        let cfg = Config {
            agent: Some(SelectedAgent {
                source_id: "anvil".to_string(),
                program: PathBuf::from("uvx"),
                args: vec!["brokk".to_string(), "acp".to_string()],
                env: HashMap::new(),
            }),
            favorite_agents: Vec::new(),
            ..Default::default()
        };
        cfg.save(&path).expect("save");
        assert!(path.exists());
    }

    #[test]
    fn load_normalizes_legacy_anvil_agent_to_uvx_brokk_acp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
source_id = "anvil"
program = "anvil"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        let agent = cfg.agent.expect("agent");
        assert_eq!(agent.source_id, "anvil");
        assert_eq!(agent.program, PathBuf::from("uvx"));
        assert_eq!(agent.args, vec!["brokk", "acp"]);
    }

    #[test]
    fn load_expands_custom_agent_home_shortcuts() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
source_id = "custom"
program = "~/bin/agent"
args = ["--config", "$HOME/.config/agent.toml", "${HOME}/literal"]
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        let agent = cfg.agent.expect("agent");
        assert_eq!(agent.source_id, "custom");
        assert_eq!(agent.program, home.join("bin/agent"));
        assert_eq!(
            agent.args,
            vec![
                "--config".to_string(),
                home.join(".config/agent.toml").display().to_string(),
                "${HOME}/literal".to_string(),
            ]
        );
    }

    #[test]
    fn load_parse_error_is_surfaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"not = valid = toml = @@@").expect("write");
        let err = Config::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"), "error mentions parse: {msg}");
    }

    #[test]
    fn mask_secret_shows_at_most_last_four_chars() {
        assert_eq!(mask_secret("sk-or-v1-abcdef1234"), "••••1234");
        // Exactly four or fewer is fully masked — never echoed.
        assert_eq!(mask_secret("1234"), "••••");
        assert_eq!(mask_secret("ab"), "••");
        assert_eq!(mask_secret(""), "");
    }

    #[test]
    fn masked_key_never_reveals_more_than_tail() {
        let key = OpenRouterKey {
            label: "work".to_string(),
            key: "sk-or-v1-supersecretvalue9876".to_string(),
        };
        let masked = key.masked();
        assert_eq!(masked, "••••9876");
        assert!(!masked.contains("supersecret"));
    }

    #[test]
    fn legacy_env_key_migrates_to_named_default_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
source_id = "anvil"
program = "uvx"

[agent.env]
OPENROUTER_API_KEY = "sk-or-legacy-key-0001"
OTHER = "keepme"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        // The legacy key is lifted into a single labeled, active entry.
        assert_eq!(cfg.openrouter_keys.len(), 1);
        assert_eq!(cfg.openrouter_keys[0].label, "default");
        assert_eq!(cfg.openrouter_keys[0].key, "sk-or-legacy-key-0001");
        assert_eq!(cfg.active_openrouter_key.as_deref(), Some("default"));
        assert_eq!(
            cfg.active_openrouter_key().map(|k| k.key.as_str()),
            Some("sk-or-legacy-key-0001")
        );
        // It is removed from the agent env (single source of truth) but
        // unrelated env vars are preserved.
        let agent = cfg.agent.expect("agent");
        assert!(!agent.env.contains_key("OPENROUTER_API_KEY"));
        assert_eq!(agent.env.get("OTHER"), Some(&"keepme".to_string()));
    }

    #[test]
    fn migration_skipped_when_named_keys_already_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
active_openrouter_key = "work"

[agent]
source_id = "anvil"
program = "uvx"

[agent.env]
OPENROUTER_API_KEY = "stale-env-key"

[[openrouter_keys]]
label = "work"
key = "sk-work-1111"

[[openrouter_keys]]
label = "personal"
key = "sk-personal-2222"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        // The new model wins; the stale env key is left untouched in env
        // (spawn-time injection overrides it with the active named key).
        assert_eq!(cfg.openrouter_keys.len(), 2);
        assert_eq!(cfg.active_openrouter_key.as_deref(), Some("work"));
        assert_eq!(
            cfg.active_openrouter_key().map(|k| k.key.as_str()),
            Some("sk-work-1111")
        );
    }

    #[test]
    fn config_with_no_openrouter_key_stays_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
source_id = "claude-acp"
program = "/usr/bin/claude-acp"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert!(cfg.openrouter_keys.is_empty());
        assert!(cfg.active_openrouter_key.is_none());
        assert!(cfg.active_openrouter_key().is_none());
    }

    #[test]
    fn set_active_openrouter_key_rejects_unknown_label() {
        let mut cfg = Config {
            openrouter_keys: vec![OpenRouterKey {
                label: "work".to_string(),
                key: "sk-work".to_string(),
            }],
            active_openrouter_key: Some("work".to_string()),
            ..Default::default()
        };
        assert!(!cfg.set_active_openrouter_key("nope"));
        assert_eq!(cfg.active_openrouter_key.as_deref(), Some("work"));
        assert!(cfg.set_active_openrouter_key("work"));
    }

    #[test]
    fn invalid_active_label_falls_back_to_first_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
active_openrouter_key = "ghost"

[[openrouter_keys]]
label = "alpha"
key = "sk-alpha"

[[openrouter_keys]]
label = "beta"
key = "sk-beta"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.active_openrouter_key.as_deref(), Some("alpha"));
    }

    #[test]
    fn duplicate_labels_are_deduped_keeping_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[openrouter_keys]]
label = "dup"
key = "first-wins"

[[openrouter_keys]]
label = "dup"
key = "second-loses"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.openrouter_keys.len(), 1);
        assert_eq!(cfg.openrouter_keys[0].key, "first-wins");
    }

    #[test]
    fn set_active_openrouter_key_on_disk_persists_and_preserves_other_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            agent: Some(SelectedAgent {
                source_id: "anvil".to_string(),
                program: PathBuf::from("uvx"),
                args: vec!["brokk".to_string(), "acp".to_string()],
                env: HashMap::from([("OTHER".to_string(), "keep".to_string())]),
            }),
            favorite_agents: vec!["anvil".to_string()],
            openrouter_keys: vec![
                OpenRouterKey {
                    label: "work".to_string(),
                    key: "sk-work".to_string(),
                },
                OpenRouterKey {
                    label: "personal".to_string(),
                    key: "sk-personal".to_string(),
                },
            ],
            active_openrouter_key: Some("work".to_string()),
        };
        cfg.save(&path).expect("save");

        // Unknown label is rejected and nothing is written.
        assert!(!set_active_openrouter_key_on_disk(&path, "ghost").expect("rmw"));
        assert_eq!(
            Config::load(&path).expect("load").active_openrouter_key,
            Some("work".to_string())
        );

        // Valid switch persists and leaves every other field intact.
        assert!(set_active_openrouter_key_on_disk(&path, "personal").expect("rmw"));
        let reloaded = Config::load(&path).expect("load");
        assert_eq!(reloaded.active_openrouter_key.as_deref(), Some("personal"));
        assert_eq!(reloaded.openrouter_keys.len(), 2);
        assert_eq!(reloaded.favorite_agents, vec!["anvil".to_string()]);
        let agent = reloaded.agent.expect("agent");
        assert_eq!(agent.env.get("OTHER"), Some(&"keep".to_string()));
    }

    #[test]
    fn openrouter_key_options_are_masked() {
        let cfg = Config {
            openrouter_keys: vec![OpenRouterKey {
                label: "work".to_string(),
                key: "sk-or-v1-abcd9999".to_string(),
            }],
            ..Default::default()
        };
        let options = cfg.openrouter_key_options();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].label, "work");
        assert_eq!(options[0].masked, "••••9999");
    }

    #[test]
    fn empty_config_serializes_as_blank() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Config::default().save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        // No agent key serialized when None.
        assert!(
            !body.contains("agent"),
            "blank config should not write agent: {body:?}"
        );
    }
}
