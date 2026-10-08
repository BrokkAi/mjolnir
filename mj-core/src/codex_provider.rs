//! Reads the selected model provider out of a Codex profile home.
//!
//! Codex keeps its configuration in `config.toml` inside `CODEX_HOME`. A
//! `model_provider` can select one of Codex's built-in providers, or a custom
//! provider described by `[model_providers.<id>]`. Mjolnir copies the file
//! verbatim into the staged profile home; it reads only the provider identity
//! and the custom-provider fields it needs for authentication and catalog
//! discovery.
//!
//! A profile with no `config.toml`, or one that names no `model_provider`,
//! uses Codex's default OpenAI provider and reports `None` here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The selected Codex provider, represented according to who defines it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexProvider {
    pub definition: CodexProviderDefinition,
    /// The top-level `model_catalog_json` path, when the profile's Codex
    /// `config.toml` names one. A relative path resolves against `CODEX_HOME`.
    pub model_catalog_json: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexProviderDefinition {
    /// A provider Codex ships and configures itself. Its optional provider
    /// table is left to Codex, which owns the built-in provider schema.
    BuiltIn(BuiltInCodexProvider),
    /// A provider described by the profile's `[model_providers.<id>]` table.
    Custom(CustomCodexProvider),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInCodexProvider {
    OpenAi,
    AmazonBedrock,
    AmazonBedrockRuntime,
    Ollama,
    LmStudio,
}

impl BuiltInCodexProvider {
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "openai" => Self::OpenAi,
            "amazon-bedrock" => Self::AmazonBedrock,
            "amazon-bedrock-runtime" => Self::AmazonBedrockRuntime,
            "ollama" => Self::Ollama,
            "lmstudio" => Self::LmStudio,
            _ => return None,
        })
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::AmazonBedrock => "amazon-bedrock",
            Self::AmazonBedrockRuntime => "amazon-bedrock-runtime",
            Self::Ollama => "ollama",
            Self::LmStudio => "lmstudio",
        }
    }

    pub const fn uses_aws_credentials(self) -> bool {
        matches!(self, Self::AmazonBedrock | Self::AmazonBedrockRuntime)
    }

    /// Only the built-in OpenAI provider uses Codex's own login file.
    pub const fn uses_codex_login(self) -> bool {
        matches!(self, Self::OpenAi)
    }

    /// These local providers do not need a login file or an API key.
    pub const fn needs_no_authentication(self) -> bool {
        matches!(self, Self::Ollama | Self::LmStudio)
    }
}

// `Debug` is written by hand below so an inline key is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomCodexProvider {
    pub id: String,
    pub base_url: String,
    /// Environment variable that carries the API key, when the provider uses
    /// `env_key`.
    pub env_key: Option<String>,
    /// The key itself, when the provider inlines it as
    /// `experimental_bearer_token`. It already lives in the profile's
    /// `config.toml`, which Mjolnir copies into every staged home, so Mjolnir
    /// may use it for its own provider calls but never treats it as a secret
    /// of its own.
    pub bearer_token: Option<String>,
}

/// A custom provider's API key source, as its own table names it.
///
/// `parse_config` rejects a table that names neither, so a custom provider
/// always resolves to one of these.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CodexProviderKey<'a> {
    /// The profile supplies the key in this environment variable.
    EnvKey(&'a str),
    /// The key is written into the provider table itself.
    Inline(&'a str),
}

impl std::fmt::Debug for CodexProviderKey<'_> {
    /// An inline key must never reach a log or an error, so the derived
    /// `Debug` is replaced with one that redacts it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvKey(env_key) => f.debug_tuple("EnvKey").field(env_key).finish(),
            Self::Inline(_) => f.debug_tuple("Inline").field(&"<redacted>").finish(),
        }
    }
}

/// A provider's inline key must never reach a log or an error, so the derived
/// `Debug` is replaced with one that redacts it.
impl std::fmt::Debug for CustomCodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomCodexProvider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("env_key", &self.env_key)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl CustomCodexProvider {
    /// Which key source this provider names. When a table names both, `env_key`
    /// wins, matching the documented preference for the form that keeps the key
    /// out of the staged configuration.
    pub fn key(&self) -> Option<CodexProviderKey<'_>> {
        Some(
            match (self.env_key.as_deref(), self.bearer_token.as_deref()) {
                (Some(env_key), _) => CodexProviderKey::EnvKey(env_key),
                (None, Some(token)) => CodexProviderKey::Inline(token),
                (None, None) => return None,
            },
        )
    }
}

impl CodexProvider {
    pub fn id(&self) -> &str {
        match &self.definition {
            CodexProviderDefinition::BuiltIn(provider) => provider.id(),
            CodexProviderDefinition::Custom(provider) => &provider.id,
        }
    }

    pub fn built_in(&self) -> Option<BuiltInCodexProvider> {
        match &self.definition {
            CodexProviderDefinition::BuiltIn(provider) => Some(*provider),
            CodexProviderDefinition::Custom(_) => None,
        }
    }

    pub fn custom(&self) -> Option<&CustomCodexProvider> {
        match &self.definition {
            CodexProviderDefinition::BuiltIn(_) => None,
            CodexProviderDefinition::Custom(provider) => Some(provider),
        }
    }

    pub fn uses_aws_credentials(&self) -> bool {
        self.built_in()
            .is_some_and(BuiltInCodexProvider::uses_aws_credentials)
    }

    /// Whether this provider uses Codex's native OpenAI login file. The
    /// built-in `openai` provider is equivalent to leaving `model_provider`
    /// unset; every other explicit provider has its own authentication path.
    pub fn uses_codex_login(&self) -> bool {
        self.built_in()
            .is_some_and(BuiltInCodexProvider::uses_codex_login)
    }

    /// Whether this provider does not require credentials at all.
    pub fn needs_no_authentication(&self) -> bool {
        self.built_in()
            .is_some_and(BuiltInCodexProvider::needs_no_authentication)
    }

    /// Whether the provider has no harness login file to synchronize. This
    /// includes providers that use an external credential chain, local
    /// providers without authentication, and custom API-key providers.
    pub fn skips_login_file_sync(&self) -> bool {
        match &self.definition {
            CodexProviderDefinition::BuiltIn(provider) => !provider.uses_codex_login(),
            CodexProviderDefinition::Custom(provider) => provider.env_key.is_some(),
        }
    }

    /// Host component of a custom provider's `base_url`, lowercased, when the
    /// URL parses. Built-in providers do not expose a custom base URL here.
    pub fn host(&self) -> Option<String> {
        self.custom().and_then(|provider| {
            url::Url::parse(&provider.base_url)
                .ok()
                .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        })
    }

    /// Which custom service this provider points at. Built-ins and unrecognized
    /// hosts are `Other` for Mjolnir's provider-specific quota and utility work.
    pub fn kind(&self) -> CodexProviderKind {
        self.host()
            .as_deref()
            .map_or(CodexProviderKind::Other, CodexProviderKind::from_host)
    }
}

/// Which service a custom provider points at, as far as Mjolnir needs to
/// treat it differently: Z.ai publishes a Coding Plan quota endpoint, DeepSeek
/// serves usage-priced chat completions under the same `/v1` base URL, and
/// everything else is an unknown usage-priced service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexProviderKind {
    Zai,
    DeepSeek,
    Other,
}

impl CodexProviderKind {
    /// Classifies a lowercased host name. An unrecognized host is `Other`.
    pub fn from_host(host: &str) -> Self {
        match host.to_ascii_lowercase().as_str() {
            "api.z.ai" | "open.bigmodel.cn" => Self::Zai,
            "api.deepseek.com" => Self::DeepSeek,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CodexConfigFile {
    model_provider: Option<String>,
    /// Top-level key naming the JSON file Codex advertises models from.
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
    let model_catalog_json = file.model_catalog_json.clone();
    if let Some(provider) = BuiltInCodexProvider::from_id(&id) {
        return Ok(Some(CodexProvider {
            definition: CodexProviderDefinition::BuiltIn(provider),
            model_catalog_json,
        }));
    }
    let Some(table) = file.model_providers.get(&id) else {
        bail!(
            "{} names model_provider {id:?}, which is not a Codex built-in provider, and has no [model_providers.{id}] custom-provider table",
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
    let bearer_token = table
        .experimental_bearer_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    let env_key = table
        .env_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    if env_key.is_none() && bearer_token.is_none() {
        bail!(
            "{} declares model provider {id:?} with neither env_key nor experimental_bearer_token",
            path.display()
        );
    }
    Ok(Some(CodexProvider {
        definition: CodexProviderDefinition::Custom(CustomCodexProvider {
            id,
            base_url,
            env_key,
            bearer_token,
        }),
        model_catalog_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Option<CodexProvider>> {
        parse_config(text, Path::new("config.toml"))
    }

    #[test]
    fn config_without_model_provider_reports_no_selected_provider() {
        let home = tempfile::tempdir().expect("temporary home");
        std::fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").expect("write");
        assert_eq!(codex_provider(home.path()).expect("read"), None);
    }

    #[test]
    fn built_in_bedrock_runtime_accepts_aws_overrides_without_a_custom_provider_definition() {
        let provider = parse(
            "model = \"global.openai.gpt-6-luna\"\n\
             model_provider = \"amazon-bedrock-runtime\"\n\
             [model_providers.amazon-bedrock-runtime.aws]\n\
             region = \"us-east-1\"\n",
        )
        .expect("parse")
        .expect("built-in provider");
        assert_eq!(
            provider.definition,
            CodexProviderDefinition::BuiltIn(BuiltInCodexProvider::AmazonBedrockRuntime)
        );
        assert!(provider.uses_aws_credentials());
        assert_eq!(provider.id(), "amazon-bedrock-runtime");
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
        let custom = provider.custom().expect("custom provider");
        assert_eq!(custom.id, "zai");
        assert_eq!(custom.base_url, "https://api.z.ai/api/v1");
        assert_eq!(custom.env_key.as_deref(), Some("ZAI_API_KEY"));
        assert_eq!(custom.bearer_token, None);
        assert_eq!(custom.key(), Some(CodexProviderKey::EnvKey("ZAI_API_KEY")));
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
        let custom = provider.custom().expect("custom provider");
        assert_eq!(custom.env_key, None);
        assert_eq!(custom.bearer_token.as_deref(), Some("secret"));
        assert_eq!(custom.key(), Some(CodexProviderKey::Inline("secret")));
    }

    #[test]
    fn an_inline_bearer_token_never_appears_in_debug_output() {
        let provider = parse(
            "model_provider = \"zai\"\n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             experimental_bearer_token = \"super-secret-token\"\n\
             wire_api = \"responses\"\n",
        )
        .expect("parse")
        .expect("provider");
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        let custom = provider.custom().expect("custom provider");
        let rendered = format!("{:?}", custom.key());
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
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
    fn unknown_provider_without_a_table_names_the_missing_custom_provider_table() {
        let error = parse("model_provider = \"unknown-service\"\n")
            .expect_err("unknown provider without a table")
            .to_string();
        assert!(error.contains("not a Codex built-in provider"), "{error}");
        assert!(error.contains("model_providers.unknown-service"), "{error}");
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
