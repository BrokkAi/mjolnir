//! Persistent user config for `mj`.
//!
//! Stores the launch command of the agent selected in the picker so
//! subsequent startups skip the prompt. Lives at
//! `~/.config/mj/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    pub agent: Option<SelectedAgent>,
}

/// Launch command resolved by the picker. `source_id` identifies where
/// the choice came from so the picker can highlight the current row.
/// `"anvil"` and `"custom"` are reserved; everything else is a registry
/// agent id.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
        toml::from_str(&s).with_context(|| format!("parse {}", path.display()))
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
}

/// Default config path: `$XDG_CONFIG_HOME/mj/config.toml` (or
/// `~/.config/mj/config.toml` when `XDG_CONFIG_HOME` is unset).
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
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
                program: PathBuf::from("anvil"),
                args: vec![],
                env: HashMap::new(),
            }),
        };
        cfg.save(&path).expect("save");
        assert!(path.exists());
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
