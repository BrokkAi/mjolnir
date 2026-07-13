//! Persistent user config for `mj`.
//!
//! Stores role-owned Council preferences and custom ACP launches. Lives at
//! `~/.config/mj/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{expand_home_shortcut, normalize_spawn_program};
use crate::spinner::SpinnerStyle;
use crate::theme::TerminalThemeKind;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "TerminalThemeKind::is_default")]
    pub theme: TerminalThemeKind,
    #[serde(default, skip_serializing_if = "SpinnerStyle::is_default")]
    pub spinner: SpinnerStyle,
    /// Legacy custom-agent entries migrated into `acp.servers` on load.
    #[serde(default, skip_serializing)]
    pub custom_agents: Vec<CustomAgent>,
    /// Legacy quick-start Council model table. Values migrate into the role
    /// tables below and this field is never serialized.
    #[serde(default, skip_serializing)]
    pub models: ModelsConfig,
    /// Thor's coordination and review behavior.
    #[serde(default, skip_serializing_if = "ThorConfig::is_default")]
    pub thor: ThorConfig,
    /// Loki's streaming review behavior.
    #[serde(default, skip_serializing_if = "LokiConfig::is_default")]
    pub loki: LokiConfig,
    /// Eitri's model preference.
    #[serde(default, skip_serializing_if = "EitriConfig::is_default")]
    pub eitri: EitriConfig,
    /// Explicit user-provisioned ACP servers.
    #[serde(default, skip_serializing_if = "AcpConfig::is_default")]
    pub acp: AcpConfig,
    /// `/ragnarok` battle knobs.
    #[serde(default, skip_serializing_if = "RagnarokConfig::is_default")]
    pub ragnarok: RagnarokConfig,
}

fn default_auto() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelsConfig {
    #[serde(default = "default_auto")]
    pub thor: String,
    #[serde(default = "default_auto")]
    pub loki: String,
    #[serde(default = "default_auto")]
    pub eitri: String,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            thor: default_auto(),
            loki: default_auto(),
            eitri: default_auto(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ThorConfig {
    #[serde(default = "default_auto")]
    pub model: String,
    #[serde(default = "default_true")]
    pub discrete_review: bool,
}

impl Default for ThorConfig {
    fn default() -> Self {
        Self {
            model: default_auto(),
            discrete_review: true,
        }
    }
}

impl ThorConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LokiConfig {
    #[serde(default = "default_auto")]
    pub model: String,
    #[serde(default = "default_true")]
    pub streaming_review: bool,
}

impl Default for LokiConfig {
    fn default() -> Self {
        Self {
            model: default_auto(),
            streaming_review: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EitriConfig {
    #[serde(default = "default_auto")]
    pub model: String,
}

impl Default for EitriConfig {
    fn default() -> Self {
        Self {
            model: default_auto(),
        }
    }
}

impl EitriConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcpConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<CustomAcpServer>,
}

impl AcpConfig {
    fn is_default(&self) -> bool {
        self.servers.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomAcpServer {
    pub name: String,
    pub command: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl LokiConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Knobs for `/ragnarok` battles.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RagnarokConfig {
    /// Hard cap on how many champions Thor may field (2-10). Thor still
    /// decides the count from task complexity; this caps the bill.
    #[serde(default = "default_max_competitors")]
    pub max_competitors: usize,
}

fn default_max_competitors() -> usize {
    10
}

impl Default for RagnarokConfig {
    fn default() -> Self {
        Self {
            max_competitors: default_max_competitors(),
        }
    }
}

impl RagnarokConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_true() -> bool {
    true
}

/// Concrete ACP launch selected by the model catalog for a session.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectedAgent {
    pub source_id: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// Legacy custom-agent shape retained solely for config migration.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CustomAgent {
    pub name: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl Config {
    pub fn role_models(&self) -> ModelsConfig {
        ModelsConfig {
            thor: self.thor.model.clone(),
            loki: self.loki.model.clone(),
            eitri: self.eitri.model.clone(),
        }
    }

    pub fn set_role_models(&mut self, models: &ModelsConfig) {
        self.thor.model.clone_from(&models.thor);
        self.loki.model.clone_from(&models.loki);
        self.eitri.model.clone_from(&models.eitri);
    }

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
        cfg.normalize()?;
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

    fn normalize(&mut self) -> Result<()> {
        if self.thor.model == "auto" && self.models.thor != "auto" {
            self.thor.model.clone_from(&self.models.thor);
        }
        if self.loki.model == "auto" && self.models.loki != "auto" {
            self.loki.model.clone_from(&self.models.loki);
        }
        if self.eitri.model == "auto" && self.models.eitri != "auto" {
            self.eitri.model.clone_from(&self.models.eitri);
        }

        for legacy in &self.custom_agents {
            if !self
                .acp
                .servers
                .iter()
                .any(|server| server.name == legacy.name)
            {
                self.acp.servers.push(CustomAcpServer {
                    name: legacy.name.clone(),
                    command: legacy.program.clone(),
                    args: legacy.args.clone(),
                });
            }
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
        let mut names = std::collections::HashSet::new();
        for server in &mut self.acp.servers {
            let valid_name = !server.name.is_empty()
                && server
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
            anyhow::ensure!(
                valid_name,
                "custom ACP server name '{}' must contain only letters, digits, '-' or '_'",
                server.name
            );
            anyhow::ensure!(
                names.insert(server.name.clone()),
                "duplicate custom ACP server name '{}'",
                server.name
            );
            anyhow::ensure!(
                !server.command.as_os_str().is_empty(),
                "custom ACP server '{}' has an empty command",
                server.name
            );
            server.command = expand_home_shortcut(&server.command.to_string_lossy());
            server.command = normalize_spawn_program(server.command.clone());
            server.args = server
                .args
                .iter()
                .map(|arg| expand_home_shortcut(arg).to_string_lossy().into_owned())
                .collect();
        }
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

/// Directory for exported conversation transcripts:
/// `$XDG_CONFIG_HOME/mj/transcripts`.
pub fn transcript_export_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("mj").join("transcripts"))
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
    fn ragnarok_max_competitors_roundtrips_and_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        // Default cap is omitted from the serialized form.
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            !body.contains("ragnarok"),
            "default ragnarok config should not be serialized: {body:?}"
        );
        assert_eq!(
            Config::load(&path).expect("load").ragnarok.max_competitors,
            10
        );

        // A custom cap survives the round trip.
        std::fs::write(&path, "[ragnarok]\nmax_competitors = 3\n").expect("write");
        let cfg = Config::load(&path).expect("load custom");
        assert_eq!(cfg.ragnarok.max_competitors, 3);
        cfg.save(&path).expect("save custom");
        let body = std::fs::read_to_string(&path).expect("read saved");
        assert!(body.contains("max_competitors = 3"), "body: {body:?}");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.toml");
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.theme, TerminalThemeKind::Dark);
        assert_eq!(cfg.role_models(), ModelsConfig::default());
        assert!(cfg.thor.discrete_review);
        assert!(cfg.loki.streaming_review);
    }

    #[test]
    fn council_reviews_default_on_and_can_be_disabled_independently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[thor]\ndiscrete_review = false\n\n[loki]\nstreaming_review = false\n",
        )
        .expect("write config");

        let cfg = Config::load(&path).expect("load config");
        assert!(!cfg.thor.discrete_review);
        assert!(!cfg.loki.streaming_review);

        cfg.save(&path).expect("save config");
        let saved = std::fs::read_to_string(&path).expect("read saved config");
        assert!(saved.contains("[thor]"), "saved: {saved}");
        assert!(saved.contains("discrete_review = false"), "saved: {saved}");
        assert!(saved.contains("[loki]"), "saved: {saved}");
        assert!(saved.contains("streaming_review = false"), "saved: {saved}");
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            theme: TerminalThemeKind::Light,
            thor: ThorConfig {
                model: "gpt-5-6-sol".to_string(),
                discrete_review: false,
            },
            acp: AcpConfig {
                servers: vec![CustomAcpServer {
                    name: "company".to_string(),
                    command: PathBuf::from("/usr/local/bin/company-acp"),
                    args: vec!["--stdio".to_string()],
                }],
            },
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.theme, TerminalThemeKind::Light);
        assert_eq!(loaded.thor.model, "gpt-5-6-sol");
        assert!(!loaded.thor.discrete_review);
        assert_eq!(loaded.acp.servers[0].name, "company");
        assert_eq!(loaded.acp.servers[0].args, vec!["--stdio"]);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("config.toml");
        let cfg = Config {
            theme: TerminalThemeKind::Dark,
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        assert!(path.exists());
    }

    #[test]
    fn legacy_models_migrate_into_role_owned_models() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[models]
thor = "gpt-5-6-sol"
loki = "claude-opus-4-8"
eitri = "gpt-5-6-luna"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.thor.model, "gpt-5-6-sol");
        assert_eq!(cfg.loki.model, "claude-opus-4-8");
        assert_eq!(cfg.eitri.model, "gpt-5-6-luna");
        cfg.save(&path).expect("save migrated config");
        let saved = std::fs::read_to_string(path).expect("read migrated config");
        assert!(!saved.contains("[models]"), "saved: {saved}");
        assert!(saved.contains("model = \"gpt-5-6-sol\""), "saved: {saved}");
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
    fn legacy_custom_agents_migrate_to_acp_servers_without_serializing_legacy_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            theme: TerminalThemeKind::AnsiDark,
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
            ..Config::default()
        };
        let mut migrated = cfg;
        migrated.normalize().expect("normalize");
        migrated.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.theme, TerminalThemeKind::AnsiDark);
        assert!(loaded.custom_agents.is_empty());
        assert_eq!(loaded.acp.servers.len(), 2);
        assert_eq!(loaded.acp.servers[0].name, "local-claude");
        assert_eq!(loaded.acp.servers[0].args, vec!["--debug"]);
        let body = std::fs::read_to_string(path).expect("read");
        assert!(!body.contains("custom_agents"), "saved: {body}");
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
        assert_eq!(cfg.acp.servers.len(), 1);
        let server = &cfg.acp.servers[0];
        assert_eq!(server.command, home.join("bin/agent"));
        assert_eq!(
            server.args,
            vec![
                "--config".to_string(),
                home.join(".config/agent.toml").display().to_string(),
            ]
        );
    }

    #[test]
    fn explicit_acp_server_wins_legacy_custom_agent_name_conflict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[acp.servers]]
name = "company"
command = "/new/acp"
args = ["--new"]

[[custom_agents]]
name = "company"
program = "/legacy/acp"
args = ["--legacy"]
description = "old entry"
"#,
        )
        .expect("write");

        let config = Config::load(&path).expect("load");
        assert_eq!(config.acp.servers.len(), 1);
        assert_eq!(config.acp.servers[0].command, PathBuf::from("/new/acp"));
        assert_eq!(config.acp.servers[0].args, vec!["--new"]);
    }

    #[test]
    fn custom_acp_servers_validate_names_commands_and_duplicates() {
        for (body, expected) in [
            (
                "[[acp.servers]]\nname = 'bad name'\ncommand = 'server'\n",
                "must contain only",
            ),
            (
                "[[acp.servers]]\nname = 'empty'\ncommand = ''\n",
                "empty command",
            ),
            (
                "[[acp.servers]]\nname = 'same'\ncommand = 'one'\n[[acp.servers]]\nname = 'same'\ncommand = 'two'\n",
                "duplicate",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("config.toml");
            std::fs::write(&path, body).expect("write");
            let error = Config::load(&path).expect_err("invalid custom server");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn saving_legacy_config_drops_obsolete_agent_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
agent = "legacy"
favorite_agents = ["old"]

[scores]
source = "arena"

[session_config.old]
mode = "ask"
"#,
        )
        .expect("write");
        let config = Config::load(&path).expect("load ignored legacy fields");
        config.save(&path).expect("save clean schema");
        let body = std::fs::read_to_string(path).expect("read");
        assert_eq!(body, "");
    }

    #[test]
    fn empty_config_serializes_as_blank() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Config::default().save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            !body.contains("models") && !body.contains("custom_agents"),
            "blank config should not write legacy fields: {body:?}"
        );
        assert!(
            !body.contains("theme"),
            "default theme should not be serialized: {body:?}"
        );
    }

    #[test]
    fn theme_config_defaulting_and_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write");
        let cfg = Config::load(&path).expect("load default");
        assert_eq!(cfg.theme, TerminalThemeKind::Dark);

        let cfg = Config {
            theme: TerminalThemeKind::AnsiLight,
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("theme = \"ansi-light\""));

        let loaded = Config::load(&path).expect("load saved");
        assert_eq!(loaded.theme, TerminalThemeKind::AnsiLight);
    }

    #[test]
    fn spinner_config_defaulting_and_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write");
        let cfg = Config::load(&path).expect("load default");
        assert_eq!(cfg.spinner, SpinnerStyle::default());

        // Default style is omitted from the serialized form.
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            !body.contains("spinner"),
            "default spinner should not be serialized: {body:?}"
        );

        let cfg = Config {
            spinner: SpinnerStyle::Bars,
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("spinner = \"bars\""));

        let loaded = Config::load(&path).expect("load saved");
        assert_eq!(loaded.spinner, SpinnerStyle::Bars);
    }
}
