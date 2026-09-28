//! One Kimi OAuth refresher per profile, shared by quota and inference.
//!
//! Anvil's inference client used to refresh the profile's OAuth credential
//! on its own, racing this daemon's quota poller and the Kimi CLI for the
//! single-use refresh token. Inference now borrows the poller's refresher
//! through Anvil's token-provider hook: every refresh in this process takes
//! the vendor's lock, re-reads the credential once the lock is held, and only
//! then spends the refresh token. Profiles with `KIMI_API_KEY` never touch
//! the credential file.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anvil_client::llm_client::BearerTokenProvider;
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use mj_core::config::{HarnessHost, HarnessKind};

pub(crate) struct KimiAuth {
    home: PathBuf,
    credentials_path: PathBuf,
    environment: HashMap<String, String>,
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
            home: home.to_path_buf(),
            credentials_path: home.join("credentials/kimi-code.json"),
            environment: environment.into_iter().collect(),
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
        crate::quota::ensure_fresh_kimi_token(
            &self.http,
            &self.home,
            &self.credentials_path,
            &self.environment,
            force,
            rejected,
        )
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
