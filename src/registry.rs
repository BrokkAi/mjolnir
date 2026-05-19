//! Agent registry: discover and launch agents from a local TOML config.
//!
//! The registry reads `~/.config/mj/agents.toml` (or the XDG equivalent)
//! and provides named agent presets so users can run `mj --agent local`
//! instead of typing the full command every time.
//!
//! Config file format (matching the example in `PLANS.md`):
//!
//! ```toml
//! [agents.anvil]
//! command = "anvil"
//!
//! [agents.local]
//! command = "/path/to/custom-agent --flag"
//! description = "My local dev agent"
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// One named agent preset from the config file.
#[derive(Debug, Clone)]
pub struct AgentPreset {
    /// The command string to spawn. Parsed with `shell_words::split` at
    /// launch time, so quoted arguments are honoured.
    pub command: String,
    /// Optional human-readable description shown by `--list-agents`.
    pub description: Option<String>,
}

/// Collection of agent presets loaded from disk.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    pub agents: HashMap<String, AgentPreset>,
}

impl AgentRegistry {
    /// Load the registry from the default config path.
    ///
    /// Returns an empty registry (no error) when the file does not exist.
    /// Returns an error when the file exists but cannot be parsed.
    pub fn load() -> Result<Self> {
        let path = default_config_path();
        Self::load_from(&path)
    }

    /// Load the registry from an explicit path.
    ///
    /// Returns an empty registry when the file does not exist.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::from_str(&content)
    }

    /// Parse a TOML string into a registry.
    pub fn from_str(toml: &str) -> Result<Self> {
        let raw: toml::Value = toml::from_str(toml).context("parse agents.toml")?;
        let agents_table = match raw.get("agents") {
            Some(toml::Value::Table(t)) => t,
            Some(_) => {
                anyhow::bail!("agents.toml: 'agents' must be a table");
            }
            None => return Ok(Self::default()),
        };
        let mut agents = HashMap::new();
        for (name, value) in agents_table {
            let table = match value {
                toml::Value::Table(t) => t,
                _ => {
                    anyhow::bail!("agents.toml: agents.{name} must be a table");
                }
            };
            let command = match table.get("command") {
                Some(toml::Value::String(s)) => s.clone(),
                Some(_) => {
                    anyhow::bail!("agents.toml: agents.{name}.command must be a string");
                }
                None => {
                    anyhow::bail!("agents.toml: agents.{name} missing required 'command'");
                }
            };
            let description = table
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            agents.insert(
                name.clone(),
                AgentPreset {
                    command,
                    description,
                },
            );
        }
        Ok(Self { agents })
    }

    /// Look up a named preset. Returns `None` when the name is not in
    /// the registry.
    pub fn get(&self, name: &str) -> Option<&AgentPreset> {
        self.agents.get(name)
    }

    /// Return the list of preset names sorted alphabetically.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.agents.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }
}

/// Return the default config file path: `$XDG_CONFIG_HOME/mj/agents.toml`
/// (or `~/.config/mj/agents.toml` when `XDG_CONFIG_HOME` is unset).
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("agents.toml")
}

/// Resolve the agent command from CLI flags and the registry.
///
/// Precedence:
/// 1. `--command` (explicit, highest priority)
/// 2. `--agent` (named preset from the registry)
/// 3. Default: `"anvil"`
///
/// Returns the (program, args) pair ready for `AcpRuntimeConfig`.
pub fn resolve_command(
    command: Option<&str>,
    agent: Option<&str>,
    registry: &AgentRegistry,
) -> Result<(PathBuf, Vec<String>)> {
    let command_str = if let Some(cmd) = command {
        // Explicit --command always wins.
        cmd.to_string()
    } else if let Some(name) = agent {
        // Look up the named preset.
        match registry.get(name) {
            Some(preset) => preset.command.clone(),
            None => {
                let available = registry.names();
                if available.is_empty() {
                    anyhow::bail!(
                        "unknown agent '{name}'; no agents configured (create ~/.config/mj/agents.toml)"
                    );
                }
                anyhow::bail!(
                    "unknown agent '{name}'; available: {}",
                    available.join(", ")
                );
            }
        }
    } else {
        "anvil".to_string()
    };

    let parts = shell_words::split(&command_str)
        .with_context(|| format!("split command string: {command_str:?}"))?;
    let mut iter = parts.into_iter();
    let program = iter.next().context("empty command string")?;
    Ok((PathBuf::from(program), iter.collect()))
}

/// Format the registry for `--list-agents` output.
pub fn format_agent_list(registry: &AgentRegistry) -> String {
    if registry.agents.is_empty() {
        return "No agents configured. Create ~/.config/mj/agents.toml to add presets.".to_string();
    }
    let names = registry.names();
    let max_name = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for name in &names {
        let preset = &registry.agents[*name];
        let desc = preset.description.as_deref().unwrap_or("");
        if desc.is_empty() {
            lines.push(format!("  {name:max_name$}  {}", preset.command));
        } else {
            lines.push(format!("  {name:max_name$}  {}  -- {desc}", preset.command));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_TOML: &str = r#"
[agents.anvil]
command = "anvil"
"#;

    const MULTI_TOML: &str = r#"
[agents.anvil]
command = "anvil"

[agents.local]
command = "/path/to/custom-agent --flag"
description = "My local dev agent"
"#;

    const NO_COMMAND_TOML: &str = r#"
[agents.broken]
description = "missing command field"
"#;

    const NON_TABLE_AGENTS: &str = r#"
agents = "not a table"
"#;

    const NON_TABLE_AGENT: &str = r#"
[agents]
broken = "not a table"
"#;

    #[test]
    fn parse_minimal_config() {
        let reg = AgentRegistry::from_str(MINIMAL_TOML).expect("parse");
        assert_eq!(reg.agents.len(), 1);
        let preset = reg.get("anvil").expect("anvil");
        assert_eq!(preset.command, "anvil");
        assert!(preset.description.is_none());
    }

    #[test]
    fn parse_multi_agent_config() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        assert_eq!(reg.agents.len(), 2);

        let anvil = reg.get("anvil").expect("anvil");
        assert_eq!(anvil.command, "anvil");
        assert!(anvil.description.is_none());

        let local = reg.get("local").expect("local");
        assert_eq!(local.command, "/path/to/custom-agent --flag");
        assert_eq!(local.description.as_deref(), Some("My local dev agent"));
    }

    #[test]
    fn empty_toml_yields_empty_registry() {
        let reg = AgentRegistry::from_str("").expect("empty");
        assert!(reg.agents.is_empty());
    }

    #[test]
    fn missing_agents_key_yields_empty_registry() {
        let reg = AgentRegistry::from_str("[other]\nkey = 1\n").expect("no agents");
        assert!(reg.agents.is_empty());
    }

    #[test]
    fn missing_command_is_an_error() {
        let err = AgentRegistry::from_str(NO_COMMAND_TOML).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("broken"), "error mentions agent name: {msg}");
        assert!(
            msg.contains("missing required 'command'"),
            "error mentions missing field: {msg}"
        );
    }

    #[test]
    fn non_table_agents_key_is_an_error() {
        let err = AgentRegistry::from_str(NON_TABLE_AGENTS).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("'agents' must be a table"),
            "error is clear: {msg}"
        );
    }

    #[test]
    fn non_table_agent_entry_is_an_error() {
        let err = AgentRegistry::from_str(NON_TABLE_AGENT).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("must be a table"), "error is clear: {msg}");
    }

    #[test]
    fn names_returns_sorted_list() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        assert_eq!(reg.names(), vec!["anvil", "local"]);
    }

    #[test]
    fn resolve_command_with_explicit_flag() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        let (program, args) =
            resolve_command(Some("my-agent --verbose"), None, &reg).expect("resolve");
        assert_eq!(program, PathBuf::from("my-agent"));
        assert_eq!(args, vec!["--verbose"]);
    }

    #[test]
    fn resolve_command_from_agent_preset() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        let (program, args) = resolve_command(None, Some("local"), &reg).expect("resolve");
        assert_eq!(program, PathBuf::from("/path/to/custom-agent"));
        assert_eq!(args, vec!["--flag"]);
    }

    #[test]
    fn resolve_command_defaults_to_anvil() {
        let reg = AgentRegistry::default();
        let (program, args) = resolve_command(None, None, &reg).expect("resolve");
        assert_eq!(program, PathBuf::from("anvil"));
        assert!(args.is_empty());
    }

    #[test]
    fn resolve_command_explicit_overrides_agent() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        let (program, args) =
            resolve_command(Some("override"), Some("local"), &reg).expect("resolve");
        assert_eq!(program, PathBuf::from("override"));
        assert!(args.is_empty());
    }

    #[test]
    fn resolve_command_unknown_agent_is_an_error() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        let err = resolve_command(None, Some("nonexistent"), &reg).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown agent 'nonexistent'"),
            "error mentions name: {msg}"
        );
        assert!(
            msg.contains("available: anvil, local"),
            "error lists available: {msg}"
        );
    }

    #[test]
    fn resolve_command_unknown_agent_with_empty_registry_suggests_config() {
        let reg = AgentRegistry::default();
        let err = resolve_command(None, Some("anything"), &reg).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no agents configured"),
            "error suggests config: {msg}"
        );
    }

    #[test]
    fn format_agent_list_with_presets() {
        let reg = AgentRegistry::from_str(MULTI_TOML).expect("parse");
        let output = format_agent_list(&reg);
        assert!(output.contains("anvil"));
        assert!(output.contains("local"));
        assert!(output.contains("My local dev agent"));
    }

    #[test]
    fn format_agent_list_empty() {
        let reg = AgentRegistry::default();
        let output = format_agent_list(&reg);
        assert!(output.contains("No agents configured"));
    }

    #[test]
    fn load_from_missing_file_returns_empty() {
        let dir = std::env::temp_dir().join("mjolnir_test_nonexistent");
        let path = dir.join("agents.toml");
        // Ensure the file does not exist.
        let _ = std::fs::remove_file(&path);
        let reg = AgentRegistry::load_from(&path).expect("missing file is ok");
        assert!(reg.agents.is_empty());
    }

    #[test]
    fn load_from_existing_file() {
        let dir = std::env::temp_dir().join("mjolnir_test_registry");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("agents.toml");
        std::fs::write(&path, MINIMAL_TOML).expect("write");
        let reg = AgentRegistry::load_from(&path).expect("load");
        assert_eq!(reg.agents.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_config_path_ends_with_mj_agents_toml() {
        let path = default_config_path();
        assert!(path.ends_with("mj/agents.toml"), "path: {}", path.display());
    }
}
