//! GitHub credentials configured for the controller.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Optional GitHub integration settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    /// GitHub App authentication. When absent, the controller uses its
    /// existing `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth token` lookup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<GithubAppConfig>,
}

impl GithubConfig {
    pub fn is_default(&self) -> bool {
        self.app.is_none()
    }
}

/// Controller-host credentials for one GitHub App and its installations.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAppConfig {
    /// Numeric identifier shown in the GitHub App settings page.
    pub app_id: u64,
    /// PEM private key on the controller host. Never copied to a worker.
    pub private_key_path: PathBuf,
    /// Optional GitHub owner login to installation ID overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub installations: BTreeMap<String, u64>,
}

impl GithubAppConfig {
    pub fn validate(&self) -> Result<()> {
        if self.app_id == 0 {
            bail!("[github.app] app_id must be a positive integer");
        }
        if self.private_key_path.as_os_str().is_empty() {
            bail!("[github.app] private_key_path must not be empty");
        }
        let mut owners = BTreeSet::new();
        for (owner, installation_id) in &self.installations {
            if !valid_github_owner_login(owner) {
                bail!("[github.app.installations] key {owner:?} is not a valid GitHub owner login");
            }
            if !owners.insert(owner.to_ascii_lowercase()) {
                bail!(
                    "[github.app.installations] owner {owner:?} is configured more than once with different letter casing"
                );
            }
            if *installation_id == 0 {
                bail!(
                    "[github.app.installations.{owner}] installation ID must be a positive integer"
                );
            }
        }
        Ok(())
    }
}

pub fn valid_github_owner_login(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !owner.starts_with('-')
        && !owner.ends_with('-')
        && !owner.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_github_config_is_default_and_serializes_away() {
        let value: GithubConfig = toml::from_str("").unwrap();
        assert_eq!(value, GithubConfig::default());
        assert_eq!(toml::to_string(&value).unwrap(), "");
    }

    #[test]
    fn app_config_rejects_invalid_owner_and_installation_ids() {
        let valid = GithubAppConfig {
            app_id: 42,
            private_key_path: PathBuf::from("app.pem"),
            installations: BTreeMap::new(),
        };
        assert!(valid.validate().is_ok());

        let mut invalid = valid.clone();
        invalid.installations.insert("two--hyphens".into(), 1);
        assert!(invalid.validate().is_err());

        let mut invalid = valid;
        invalid.installations.insert("acme".into(), 0);
        assert!(invalid.validate().is_err());

        let ambiguous = GithubAppConfig {
            app_id: 42,
            private_key_path: PathBuf::from("app.pem"),
            installations: BTreeMap::from([("Acme".into(), 1), ("acme".into(), 2)]),
        };
        assert!(ambiguous.validate().is_err());
    }
}
