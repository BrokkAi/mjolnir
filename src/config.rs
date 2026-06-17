//! Persistent user config for `mj`.
//!
//! Stores the default launch command and global picker preferences. Lives at
//! `~/.config/mj/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{expand_home_shortcut, normalize_spawn_program};

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<SelectedAgent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorite_agents: Vec<String>,
    /// Named custom agents shown in the picker alongside registry entries.
    /// Their `source_id` in [`SelectedAgent`]/`PickerOutcome` is
    /// `custom:<name>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_agents: Vec<CustomAgent>,
}

/// Launch command resolved by the picker. `source_id` identifies where
/// the choice came from so the picker can highlight the default row.
/// `"anvil"` and `"custom"` are reserved (`"custom"` is a legacy shape
/// for the single ad-hoc custom command); named custom agents use
/// `custom:<name>`. Everything else is a registry agent id.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectedAgent {
    pub source_id: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// A named, persistent custom agent the user added through the picker.
/// Shown as a first-class entry in the picker so it never needs to be
/// re-typed and can be set as the default.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CustomAgent {
    pub name: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// Prefix marking a [`SelectedAgent::source_id`] as a named custom agent
/// (`custom:<name>`).
pub const CUSTOM_AGENT_SOURCE_PREFIX: &str = "custom:";

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
            } else if agent.source_id == "custom"
                || agent.source_id.starts_with(CUSTOM_AGENT_SOURCE_PREFIX)
            {
                agent.program = expand_home_shortcut(&agent.program.to_string_lossy());
                agent.args = agent
                    .args
                    .iter()
                    .map(|arg| expand_home_shortcut(arg).to_string_lossy().into_owned())
                    .collect();
            }
            agent.program = normalize_spawn_program(agent.program.clone());
        }
        for custom in self.custom_agents.iter_mut() {
            custom.program = expand_home_shortcut(&custom.program.to_string_lossy());
            custom.program = normalize_spawn_program(custom.program.clone());
            custom.args = custom
                .args
                .iter()
                .map(|arg| expand_home_shortcut(arg).to_string_lossy().into_owned())
                .collect();
        }
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
            custom_agents: Vec::new(),
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
            custom_agents: Vec::new(),
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
    fn custom_agents_roundtrip_through_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            agent: None,
            favorite_agents: Vec::new(),
            custom_agents: vec![
                CustomAgent {
                    name: "local-claude".to_string(),
                    program: PathBuf::from("/usr/local/bin/claude-acp"),
                    args: vec!["--debug".to_string()],
                    description: "claude with debug logging".to_string(),
                },
                CustomAgent {
                    name: "staging-agent".to_string(),
                    program: PathBuf::from("npx"),
                    args: vec!["-y".to_string(), "@example/staging".to_string()],
                    description: String::new(),
                },
            ],
        };
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.custom_agents.len(), 2);
        assert_eq!(loaded.custom_agents[0].name, "local-claude");
        assert_eq!(loaded.custom_agents[0].args, vec!["--debug"]);
        assert_eq!(loaded.custom_agents[1].name, "staging-agent");
        assert_eq!(loaded.custom_agents[1].description, "");
    }

    #[test]
    fn load_normalizes_npx_agent_program_for_spawn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
source_id = "claude-acp"
program = "npx"
args = ["-y", "@example/agent"]
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        let agent = cfg.agent.expect("agent");
        if cfg!(windows) {
            assert_eq!(agent.program, PathBuf::from("npx.cmd"));
        } else {
            assert_eq!(agent.program, PathBuf::from("npx"));
        }
    }

    #[test]
    fn load_expands_home_shortcuts_in_custom_agents() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[custom_agents]]
name = "my-agent"
program = "~/bin/agent"
args = ["--config", "$HOME/.config/agent.toml"]
description = "test"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.custom_agents.len(), 1);
        let agent = &cfg.custom_agents[0];
        assert_eq!(agent.program, home.join("bin/agent"));
        assert_eq!(
            agent.args,
            vec![
                "--config".to_string(),
                home.join(".config/agent.toml").display().to_string(),
            ]
        );
    }

    #[test]
    fn load_expands_home_shortcuts_for_named_custom_default_agent() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
source_id = "custom:my-agent"
program = "~/bin/agent"
args = ["--flag", "$HOME/data"]
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        let agent = cfg.agent.expect("agent");
        assert_eq!(agent.source_id, "custom:my-agent");
        assert_eq!(agent.program, home.join("bin/agent"));
        assert_eq!(
            agent.args,
            vec![
                "--flag".to_string(),
                home.join("data").display().to_string()
            ]
        );
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
