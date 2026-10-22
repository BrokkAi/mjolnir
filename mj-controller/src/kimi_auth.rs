//! One Kimi OAuth refresher per profile, shared by quota and inference.
//!
//! Anvil's inference client used to refresh the profile's OAuth credential
//! on its own, racing this daemon's quota poller and the Kimi CLI for the
//! single-use refresh token. Inference now borrows the poller's refresher
//! through Anvil's token-provider hook: every refresh in this process takes
//! the vendor's lock, re-reads the credential once the lock is held, and only
//! then spends the refresh token. Profiles with `KIMI_API_KEY` never touch
//! the credential file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anvil_client::llm_client::BearerTokenProvider;
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use mj_core::config::{HarnessHost, HarnessKind};
use sha2::{Digest, Sha256};

const DEFAULT_KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding/v1";
const DEFAULT_KIMI_CODE_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEFAULT_OAUTH_STORAGE_NAME: &str = "kimi-code";

/// The endpoint and credential slot Kimi Code itself resolved from `config.toml`
/// and the environment. Keeping this in one place makes quota probes and
/// inference refresh the same OAuth token at the same host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KimiRuntimeAuth {
    pub(crate) base_url: String,
    pub(crate) oauth_host: String,
    pub(crate) credentials_path: PathBuf,
}

impl KimiRuntimeAuth {
    pub(crate) fn resolve(home: &Path, environment: &BTreeMap<String, String>) -> Result<Self> {
        let configured = read_managed_provider(&home.join("config.toml"))?;
        let env_base_url = nonempty(environment.get("KIMI_CODE_BASE_URL"));
        let env_oauth_host = nonempty(environment.get("KIMI_CODE_OAUTH_HOST"))
            .or_else(|| nonempty(environment.get("KIMI_OAUTH_HOST")));
        let base_url = normalize_endpoint(
            env_base_url
                .or(configured.base_url.as_deref())
                .unwrap_or(DEFAULT_KIMI_CODE_BASE_URL),
        );
        let oauth_host = normalize_endpoint(
            env_oauth_host
                .or(configured.oauth_host.as_deref())
                .unwrap_or(DEFAULT_KIMI_CODE_OAUTH_HOST),
        );
        let storage_name = oauth_storage_name(&base_url, &oauth_host);
        Ok(Self {
            base_url,
            oauth_host,
            credentials_path: home
                .join("credentials")
                .join(format!("{storage_name}.json")),
        })
    }
}

#[derive(Default)]
struct ManagedProviderConfig {
    base_url: Option<String>,
    oauth_host: Option<String>,
}

/// Read only the managed Kimi Code provider sizing that changes where quota
/// lives. The rest of Kimi's config schema is allowed to evolve without
/// breaking this probe.
fn read_managed_provider(path: &Path) -> Result<ManagedProviderConfig> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("decode Kimi Code configuration {}", path.display()))?;
    let document: toml::Value = toml::from_str(text)
        .with_context(|| format!("parse Kimi Code configuration {}", path.display()))?;
    let Some(provider) = document
        .get("providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get("managed:kimi-code"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(Default::default());
    };
    let oauth = provider.get("oauth").and_then(toml::Value::as_table);
    Ok(ManagedProviderConfig {
        base_url: provider
            .get("base_url")
            .or_else(|| provider.get("baseUrl"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
        oauth_host: oauth
            .and_then(|oauth| oauth.get("oauthHost").or_else(|| oauth.get("oauth_host")))
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
    })
}

/// Kimi Code scopes OAuth files by deployment. The mainland default keeps
/// the legacy `kimi-code.json`; every other endpoint pair uses a 16-hex
/// digest of the same JSON object the official CLI hashes.
fn oauth_storage_name(base_url: &str, oauth_host: &str) -> String {
    if base_url == DEFAULT_KIMI_CODE_BASE_URL && oauth_host == DEFAULT_KIMI_CODE_OAUTH_HOST {
        return DEFAULT_OAUTH_STORAGE_NAME.to_owned();
    }
    let identity = format!(
        r#"{{"oauthHost":{},"baseUrl":{}}}"#,
        serde_json::to_string(oauth_host).expect("serialize OAuth host"),
        serde_json::to_string(base_url).expect("serialize API base URL"),
    );
    let digest = Sha256::digest(identity);
    let prefix = mj_core::hex::lower_hex(digest);
    format!("kimi-code-env-{}", &prefix[..16])
}

fn normalize_endpoint(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}

fn nonempty(value: Option<&String>) -> Option<&str> {
    value
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) struct KimiAuth {
    runtime: KimiRuntimeAuth,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl KimiAuth {
    pub(crate) fn new(home: &Path, mut environment: BTreeMap<String, String>) -> Result<Self> {
        HarnessKind::Kimi.configure_profile_home_environment(
            home,
            HarnessHost::current(),
            &mut environment,
        );
        let api_key = environment
            .get("KIMI_API_KEY")
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty());
        Ok(Self {
            runtime: KimiRuntimeAuth::resolve(home, &environment)?,
            api_key,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .context("build Kimi OAuth client")?,
        })
    }

    async fn token(&self, force: bool, rejected: Option<String>) -> Result<Option<String>> {
        if let Some(api_key) = &self.api_key {
            return Ok(if force { None } else { Some(api_key.clone()) });
        }
        crate::quota::ensure_fresh_kimi_token(&self.http, &self.runtime, force, rejected)
            .await
            .map(Some)
    }
}

impl BearerTokenProvider for KimiAuth {
    fn bearer_token(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(self.token(false, None))
    }

    /// After an explicit 401, refresh under the vendor lock unless another
    /// process already replaced the rejected token; either way the caller
    /// retries only if the token it gets back differs from the rejected one.
    fn rejected_bearer_token<'a>(
        &'a self,
        rejected: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(self.token(true, Some(rejected.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_kimi_config_uses_the_cli_scoped_credential_slot() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            r#"[providers."managed:kimi-code"]
type = "kimi"
base_url = "https://api.kimi.ai/coding/v1"

[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code-env-0e4f99c69cc27850"
oauthHost = "https://auth.kimi.ai"
"#,
        )
        .unwrap();

        let runtime = KimiRuntimeAuth::resolve(home.path(), &BTreeMap::new()).unwrap();

        assert_eq!(runtime.base_url, "https://api.kimi.ai/coding/v1");
        assert_eq!(runtime.oauth_host, "https://auth.kimi.ai");
        assert_eq!(
            runtime.credentials_path,
            home.path()
                .join("credentials/kimi-code-env-0e4f99c69cc27850.json")
        );
    }

    #[tokio::test]
    async fn profile_api_key_does_not_touch_oauth_credentials() {
        let home = tempfile::tempdir().unwrap();
        let credentials = home.path().join("credentials/kimi-code.json");
        std::fs::create_dir(credentials.parent().unwrap()).unwrap();
        let original = br#"{"access_token":"expired","refresh_token":"single-use","expires_at":1}"#;
        std::fs::write(&credentials, original).unwrap();
        let auth = KimiAuth::new(
            home.path(),
            BTreeMap::from([("KIMI_API_KEY".into(), "profile-api-key".into())]),
        )
        .unwrap();
        assert_eq!(
            auth.bearer_token().await.unwrap().as_deref(),
            Some("profile-api-key")
        );
        assert_eq!(
            auth.rejected_bearer_token("profile-api-key").await.unwrap(),
            None,
            "a static key has no replacement to retry with"
        );
        assert_eq!(std::fs::read(credentials).unwrap(), original);
    }

    #[tokio::test]
    async fn a_fresh_oauth_token_is_read_without_refreshing() {
        let home = tempfile::tempdir().unwrap();
        let credentials = home.path().join("credentials/kimi-code.json");
        std::fs::create_dir(credentials.parent().unwrap()).unwrap();
        let far_future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86_400;
        let original = format!(
            r#"{{"access_token":"fresh","refresh_token":"single-use","expires_at":{far_future},"expires_in":86400}}"#
        );
        std::fs::write(&credentials, &original).unwrap();
        let auth = KimiAuth::new(home.path(), BTreeMap::new()).unwrap();
        assert_eq!(auth.bearer_token().await.unwrap().as_deref(), Some("fresh"));
        assert_eq!(std::fs::read_to_string(credentials).unwrap(), original);
    }
}
