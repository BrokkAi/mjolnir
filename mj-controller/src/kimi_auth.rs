//! Profile-local access to the vendor-owned Kimi credential service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anvil_client::kimi_auth::{KimiService, KimiServiceConfig};
use anvil_client::llm_client::BearerTokenProvider;
use anyhow::Result;
use futures::future::BoxFuture;
use mj_core::config::{HarnessHost, HarnessKind};
use serde_json::Value;
use tokio::sync::OnceCell;

/// Both quota and inference select authentication through this adapter. Anvil
/// shares the vendor service by canonical home, including across daemon handoff.
pub(crate) struct KimiAuth {
    home: PathBuf,
    environment: BTreeMap<String, String>,
    initialized: OnceCell<InitializedService>,
}

struct InitializedService {
    service: Arc<KimiService>,
    // Keep the managed installation leased until the service inherits its lease.
    _lease: Option<std::fs::File>,
}

impl KimiAuth {
    pub(crate) fn new(home: &Path, mut environment: BTreeMap<String, String>) -> Self {
        HarnessKind::Kimi.configure_profile_home_environment(
            home,
            HarnessHost::current(),
            &mut environment,
        );
        Self {
            home: home.to_path_buf(),
            environment,
            initialized: OnceCell::new(),
        }
    }

    async fn service(&self) -> Result<&Arc<KimiService>> {
        let initialized = self
            .initialized
            .get_or_try_init(|| async {
                let mut config = KimiServiceConfig::from_home(&self.home);
                config.environment = self.environment.clone();
                let launch = if config.uses_api_key() {
                    None
                } else {
                    let (prepared, lease) = crate::controller::prepare_local_managed_harness(
                        HarnessKind::Kimi,
                        self.home.clone(),
                        self.environment.clone(),
                    )
                    .await?;
                    config.executable = prepared.command;
                    config.lease_path = Some(prepared.lease_path);
                    config.environment.extend(prepared.environment);
                    Some(lease)
                };
                Ok::<_, anyhow::Error>(InitializedService {
                    service: KimiService::new(config)?,
                    _lease: launch,
                })
            })
            .await?;
        Ok(&initialized.service)
    }

    pub(crate) async fn usage(&self) -> Result<Value> {
        self.service().await?.usage().await
    }
}

impl BearerTokenProvider for KimiAuth {
    fn bearer_token(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async { self.service().await?.bearer_token().await })
    }

    fn rejected_bearer_token<'a>(
        &'a self,
        rejected: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move { self.service().await?.rejected_bearer_token(rejected).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profile_api_key_needs_no_runtime_and_does_not_touch_oauth_credentials() {
        let home = tempfile::tempdir().unwrap();
        let credentials = home.path().join("credentials/kimi-code.json");
        std::fs::create_dir(credentials.parent().unwrap()).unwrap();
        let original = br#"{"access_token":"expired","refresh_token":"single-use","expires_at":1}"#;
        std::fs::write(&credentials, original).unwrap();
        let auth = KimiAuth::new(
            home.path(),
            BTreeMap::from([
                ("KIMI_API_KEY".into(), "profile-api-key".into()),
                ("PATH".into(), "/missing-kimi-runtime".into()),
            ]),
        );
        assert_eq!(
            auth.bearer_token().await.unwrap().as_deref(),
            Some("profile-api-key")
        );
        assert_eq!(std::fs::read(credentials).unwrap(), original);
    }
}
