//! Reads the custom model provider out of a Codex profile home.
//!
//! Codex keeps its own configuration in `config.toml` inside the directory
//! named by `CODEX_HOME`. That file decides which service Codex talks to:
//! `model_provider = "<id>"` selects an entry from the `[model_providers.<id>]`
//! table, and that entry carries the base URL, the wire protocol, and how the
//! API key is supplied. Mjolnir copies the file verbatim into the staged
//! profile home, so the file stays the single source of truth; this module only
//! reads the few keys Mjolnir needs to know how a profile authenticates and
//! where its model catalog and quota live.
//!
//! A profile with no `config.toml`, or one that names no `model_provider`, uses
//! Codex's built-in OpenAI provider and therefore reports `None` here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A custom model provider named in a Codex `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexProvider {
    pub id: String,
    pub base_url: String,
    /// Environment variable that carries the API key, when the provider uses
    /// `env_key`.
    pub env_key: Option<String>,
    /// True when the key is inline as `experimental_bearer_token`.
    pub inline_bearer_token: bool,
    /// The top-level `model_catalog_json` path, when the user wrote one
    /// themselves. Mjolnir generates this file for custom providers, so a
    /// user-supplied value is rejected by profile validation.
    pub model_catalog_json: Option<PathBuf>,
}

impl CodexProvider {
    /// Host component of `base_url`, lowercased, when the URL parses.
    pub fn host(&self) -> Option<String> {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
    }
}

#[derive(Debug, Deserialize)]
struct CodexConfigFile {
    model_provider: Option<String>,
    /// Top-level key naming the JSON file Codex advertises models from. A
    /// relative path resolves against `CODEX_HOME`.
    model_catalog_json: Option<PathBuf>,
    #[serde(default)]
    model_providers: std::collections::BTreeMap<String, ProviderTable>,
}

#[derive(Debug, Deserialize)]
struct ProviderTable {
    base_url: Option<String>,
    wire_api: Option<String>,
    env_key: Option<String>,
    experimental_bearer_token: Option<String>,
}

/// Reads `<home>/config.toml`. `Ok(None)` when the file is absent, unreadable,
/// or names no `model_provider`.
pub fn codex_provider(home: &Path) -> Result<Option<CodexProvider>> {
    let path = home.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // An absent or unreadable home is a profile whose harness has not been
        // set up yet. Discovery creates such profiles before their homes exist.
        Err(_) => return Ok(None),
    };
    parse_config(&text, &path)
}

/// Parses the text of a Codex `config.toml`. `path` appears in error messages.
pub fn parse_config(text: &str, path: &Path) -> Result<Option<CodexProvider>> {
    let file: CodexConfigFile = toml::from_str(text)
        .with_context(|| format!("parse Codex configuration {}", path.display()))?;
    let Some(id) = file.model_provider else {
        return Ok(None);
    };
    let Some(table) = file.model_providers.get(&id) else {
        bail!(
            "{} names model_provider {id:?} but has no [model_providers.{id}] table",
            path.display()
        );
    };
    let Some(base_url) = table.base_url.clone() else {
        bail!(
            "{} is missing base_url for model provider {id:?}",
            path.display()
        );
    };
    match table.wire_api.as_deref() {
        Some("responses") => {}
        Some(other) => bail!(
            "{} sets wire_api = {other:?} for model provider {id:?}; Codex only supports \"responses\"",
            path.display()
        ),
        None => bail!(
            "{} is missing wire_api = \"responses\" for model provider {id:?}",
            path.display()
        ),
    }
    let inline_bearer_token = table
        .experimental_bearer_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    let env_key = table
        .env_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    if env_key.is_none() && !inline_bearer_token {
        bail!(
            "{} declares model provider {id:?} with neither env_key nor experimental_bearer_token",
            path.display()
        );
    }
    Ok(Some(CodexProvider {
        id,
        base_url,
        env_key,
        inline_bearer_token,
        model_catalog_json: file.model_catalog_json.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Option<CodexProvider>> {
        parse_config(text, Path::new("config.toml"))
    }

    #[test]
    fn missing_config_file_reports_no_custom_provider() {
        let home = tempfile::tempdir().expect("temporary home");
        assert_eq!(codex_provider(home.path()).expect("read"), None);
    }

    #[test]
    fn config_without_model_provider_reports_no_custom_provider() {
        let home = tempfile::tempdir().expect("temporary home");
        std::fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").expect("write");
        assert_eq!(codex_provider(home.path()).expect("read"), None);
    }

    #[test]
    fn env_key_provider_reports_its_variable_and_base_url() {
        let provider = parse(
            "model = \"glm-5.3\"\n\
             model_provider = \"zai\"\n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             env_key = \"ZAI_API_KEY\"\n\
             wire_api = \"responses\"\n",
        )
        .expect("parse")
        .expect("provider");
        assert_eq!(provider.id, "zai");
        assert_eq!(provider.base_url, "https://api.z.ai/api/v1");
        assert_eq!(provider.env_key.as_deref(), Some("ZAI_API_KEY"));
        assert!(!provider.inline_bearer_token);
        assert_eq!(provider.host().as_deref(), Some("api.z.ai"));
    }

    #[test]
    fn inline_bearer_token_provider_is_accepted_without_an_env_key() {
        let provider = parse(
            "model_provider = \"zai\"\n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             experimental_bearer_token = \"secret\"\n\
             wire_api = \"responses\"\n",
        )
        .expect("parse")
        .expect("provider");
        assert_eq!(provider.env_key, None);
        assert!(provider.inline_bearer_token);
    }

    #[test]
    fn chat_wire_api_is_rejected_with_the_supported_value() {
        let error = parse(
            "model_provider = \"zai\"\n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             env_key = \"ZAI_API_KEY\"\n\
             wire_api = \"chat\"\n",
        )
        .expect_err("chat is rejected")
        .to_string();
        assert!(error.contains("responses"), "{error}");
    }

    #[test]
    fn missing_provider_table_is_rejected_by_name() {
        let error = parse("model_provider = \"zai\"\n")
            .expect_err("missing table")
            .to_string();
        assert!(error.contains("model_providers.zai"), "{error}");
    }

    #[test]
    fn provider_without_any_key_source_is_rejected() {
        let error = parse(
            "model_provider = \"zai\"\n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             wire_api = \"responses\"\n",
        )
        .expect_err("no key")
        .to_string();
        assert!(error.contains("env_key"), "{error}");
    }

    #[test]
    fn user_supplied_model_catalog_path_is_reported() {
        let provider = parse(
            "model_provider = \"zai\"\n\
             model_catalog_json = \"mine.json\"\n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             env_key = \"ZAI_API_KEY\"\n\
             wire_api = \"responses\"\n",
        )
        .expect("parse")
        .expect("provider");
        assert_eq!(
            provider.model_catalog_json.as_deref(),
            Some(Path::new("mine.json"))
        );
    }
}
