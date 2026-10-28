//! Selected AWS credentials transported privately from controller to worker.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const SHARED_CREDENTIALS_FILE: &str = "aws-credentials";

/// The controller environment used by the AWS CLI to resolve one profile.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentialSource {
    pub environment: BTreeMap<String, String>,
}

impl std::fmt::Debug for AwsCredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsCredentialSource")
            .field(
                "environment_names",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// AWS CLI `process` export plus the selected profile name.
/// Deliberately has no Debug implementation: all but the profile are secrets.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AwsCredentials {
    version: u32,
    access_key_id: String,
    secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
    #[serde(default = "default_profile")]
    profile: String,
}

fn default_profile() -> String {
    "default".into()
}

impl AwsCredentials {
    pub fn from_export(profile: &str, bytes: &[u8]) -> Result<Self> {
        let mut credentials = Self::decode(bytes)?;
        credentials.profile = profile.to_owned();
        credentials.validate()?;
        Ok(credentials)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= crate::credentials::MAX_CREDENTIAL_BYTES,
            "AWS credential export is too large"
        );
        // Serde's detailed errors can include an offending secret value.
        let credentials: Self = serde_json::from_slice(bytes).map_err(|_| {
            anyhow::anyhow!("AWS credential export is not valid process-format JSON")
        })?;
        credentials.validate()?;
        Ok(credentials)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).context("encode AWS credentials")
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported AWS credential export version"
        );
        ensure!(
            !self.profile.is_empty()
                && !self
                    .profile
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '[' | ']')),
            "AWS profile name cannot be represented in a shared credentials file"
        );
        let valid_secret =
            |value: &str| !value.is_empty() && !value.chars().any(char::is_whitespace);
        ensure!(
            valid_secret(&self.access_key_id)
                && valid_secret(&self.secret_access_key)
                && self.session_token.as_deref().is_none_or(valid_secret),
            "AWS credential export contains missing or invalid credentials"
        );
        if let Some(expiration) = &self.expiration {
            let expiry = chrono::DateTime::parse_from_rfc3339(expiration)
                .map_err(|_| anyhow::anyhow!("AWS credential export has an invalid expiration"))?;
            ensure!(
                expiry.timestamp_millis() > crate::clock::epoch_millis(),
                "AWS credential export has expired; renew the selected AWS login"
            );
        }
        Ok(())
    }

    /// The native process provider honors expiration and reloads renewed
    /// credentials. A static INI entry can be cached forever by an AWS SDK.
    pub fn configuration(&self, target_home: &Path) -> Result<Vec<u8>> {
        self.validate()?;
        let path = crate::target_path::join(target_home, Path::new(SHARED_CREDENTIALS_FILE));
        let quoted = crate::targets::posix_quote(&crate::target_path::text(&path));
        let command = if crate::target_path::is_absolute(&path) {
            format!("cat {quoted}")
        } else {
            // SSH profile roots are relative to the target user's home;
            // credential_process itself runs in the project directory.
            let script = format!("exec cat \"$HOME\"/{quoted}");
            format!("sh -c {}", crate::targets::posix_quote(&script))
        };
        Ok(format!(
            "[profile {}]\ncredential_process = {command}\n",
            self.profile
        )
        .into_bytes())
    }

    pub fn install(&self, home: &Path) -> Result<crate::credentials::CredentialSnapshot> {
        let bytes = self.encode()?;
        crate::config::atomic_write(&home.join(SHARED_CREDENTIALS_FILE), &bytes)?;
        Ok(snapshot(&bytes))
    }
}

pub fn snapshot(bytes: &[u8]) -> crate::credentials::CredentialSnapshot {
    crate::credentials::CredentialSnapshot {
        present: true,
        fingerprint: crate::credentials::credential_fingerprint(bytes),
        freshness_epoch_ms: None,
    }
}

pub fn state(home: &Path) -> Result<crate::credentials::CredentialSnapshot> {
    match std::fs::read(home.join(SHARED_CREDENTIALS_FILE)) {
        Ok(bytes) => Ok(snapshot(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(crate::credentials::CredentialSnapshot::absent())
        }
        Err(error) => Err(error).context("read session AWS credentials"),
    }
}
