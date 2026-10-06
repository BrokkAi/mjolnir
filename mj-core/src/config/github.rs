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
    /// Complete permission grant requested for session tokens. `None` keeps
    /// the installation's full permission grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_permissions: Option<GithubPermissionSet>,
    /// Complete permission grant requested by `mj github-token`. `None`
    /// keeps the installation's full permission grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_permissions: Option<GithubPermissionSet>,
}

/// A subset of GitHub App permissions requested for an installation token.
pub type GithubPermissionSet = BTreeMap<String, GithubPermissionLevel>;

/// Permission levels GitHub allows an installation access token to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubPermissionLevel {
    Read,
    Write,
}

impl GithubPermissionLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl GithubAppConfig {
    pub fn validate(&self) -> Result<()> {
        if self.app_id == 0 {
            bail!("[github.app] app_id must be a positive integer");
        }
        if self.private_key_path.as_os_str().is_empty() {
            bail!("[github.app] private_key_path must not be empty");
        }
        for (section, permissions) in [
            ("session_permissions", self.session_permissions.as_ref()),
            ("token_permissions", self.token_permissions.as_ref()),
        ] {
            if let Some(permissions) = permissions {
                for permission in permissions.keys() {
                    if !valid_github_permission_name(permission) {
                        bail!(
                            "[github.app.{section}] key {permission:?} is not a valid GitHub permission name"
                        );
                    }
                }
            }
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

fn valid_github_permission_name(permission: &str) -> bool {
    let mut bytes = permission.bytes();
    matches!(bytes.next(), Some(byte) if byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
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
            session_permissions: None,
            token_permissions: None,
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
            session_permissions: None,
            token_permissions: None,
        };
        assert!(ambiguous.validate().is_err());
    }

    #[test]
    fn permission_tables_distinguish_absent_from_present_empty_and_validate_names() {
        let absent: GithubAppConfig =
            toml::from_str("app_id = 42\nprivate_key_path = 'app.pem'\n").unwrap();
        assert_eq!(absent.session_permissions, None);
        assert_eq!(absent.token_permissions, None);

        let empty: GithubAppConfig =
            toml::from_str("app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\n")
                .unwrap();
        assert_eq!(empty.session_permissions, Some(BTreeMap::new()));

        let configured: GithubAppConfig = toml::from_str(
            "app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\ncontents = 'write'\nstatuses = 'read'\n",
        )
        .unwrap();
        assert_eq!(
            configured.session_permissions,
            Some(BTreeMap::from([
                ("contents".into(), GithubPermissionLevel::Write),
                ("statuses".into(), GithubPermissionLevel::Read),
            ]))
        );
        assert!(configured.validate().is_ok());

        let invalid_name: GithubAppConfig = toml::from_str(
            "app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\n'Contents' = 'read'\n",
        )
        .unwrap();
        assert!(invalid_name.validate().is_err());

        let invalid_level = toml::from_str::<GithubAppConfig>(
            "app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\ncontents = 'admin'\n",
        );
        assert!(invalid_level.is_err());
    }
}
