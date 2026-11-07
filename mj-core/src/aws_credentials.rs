//! Selected AWS credentials transported privately from controller to worker.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const SHARED_CREDENTIALS_FILE: &str = "aws-credentials";
pub const CONFIG_FILE: &str = "aws-config";
pub const EMPTY_CREDENTIALS_FILE: &str = "aws-empty-credentials";
/// Beside the secret: which role and session Mjolnir issued it for.
pub const SCOPE_FILE: &str = "aws-scope.json";
/// Every file the AWS credential provider keeps in a session's home. One list,
/// for staging, refresh and removal alike.
pub const FILES: [&str; 4] = [
    SHARED_CREDENTIALS_FILE,
    CONFIG_FILE,
    EMPTY_CREDENTIALS_FILE,
    SCOPE_FILE,
];

/// Which credentials Mjolnir issued, for what, until when. The worker keeps it
/// beside the secret, so a controller that has lost its memory (a restart) can
/// still tell credentials it issued for a session's role from anything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsCredentialScope {
    pub role_arn: String,
    /// The STS role session name, `mj-<session id>`.
    pub role_session_name: String,
    /// Fingerprint of the effective session policy the credentials were
    /// issued under. A narrower policy configured since makes them broader
    /// than configured.
    pub session_policy_fingerprint: String,
    /// The AWS `SourceIdentity` set when the credentials were issued: the
    /// role session name, or `None` when `source_identity` was off. Credentials
    /// issued before it was enabled lack the attribution configured now.
    #[serde(default)]
    pub source_identity: Option<String>,
    pub expires_at_epoch_millis: i64,
    /// Keyed MAC over the other fields and the secret's fingerprint, made by
    /// the controller with a key only it holds. The worker's home is writable
    /// by the session, so without it the session could write a record that
    /// vouches for any credentials; the controller trusts a record only when
    /// this verifies.
    #[serde(default)]
    pub tag: String,
}

impl AwsCredentialScope {
    /// The bytes the tag authenticates: every field but the tag, and the
    /// fingerprint of the secret the record describes, so a tag cannot be
    /// moved to other credentials.
    pub fn signed_payload(&self, secret_fingerprint: &str) -> Vec<u8> {
        serde_json::to_vec(&(
            "mjolnir-aws-scope-v1",
            secret_fingerprint,
            &self.role_arn,
            &self.role_session_name,
            &self.session_policy_fingerprint,
            &self.source_identity,
            self.expires_at_epoch_millis,
        ))
        .expect("a tuple of strings and numbers serializes")
    }
}

/// What the worker stores: the scope, bound to the secret it describes.
#[derive(Serialize, Deserialize)]
struct StoredScope {
    scope: AwsCredentialScope,
    secret_fingerprint: String,
}

/// The controller environment used by the AWS CLI to resolve one profile.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentialSource {
    pub environment: BTreeMap<String, String>,
    /// The role each session assumes instead of receiving the host's own
    /// credentials. Part of the source, and so of the sync target's identity:
    /// a configuration change makes obsolete queued work compare unequal.
    pub role: Option<crate::config::AwsRoleConfig>,
}

impl std::fmt::Debug for AwsCredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsCredentialSource")
            .field(
                "environment_names",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("role", &self.role.as_ref().map(|role| &role.role_arn))
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
    /// Not part of the credential file the SDK reads; travels beside it.
    #[serde(skip)]
    scope: Option<AwsCredentialScope>,
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

    /// Credentials from `aws sts assume-role --output json`, whose output
    /// nests the same four fields under `Credentials`.
    pub fn from_assume_role(profile: &str, bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= crate::credentials::MAX_CREDENTIAL_BYTES,
            "AWS assume-role output is too large"
        );
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Output {
            credentials: Issued,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Issued {
            access_key_id: String,
            secret_access_key: String,
            session_token: String,
            expiration: String,
        }
        // Serde's detailed errors can include an offending secret value.
        let output: Output = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("AWS assume-role output is not the expected JSON"))?;
        let credentials = Self {
            version: 1,
            access_key_id: output.credentials.access_key_id,
            secret_access_key: output.credentials.secret_access_key,
            session_token: Some(output.credentials.session_token),
            expiration: Some(output.credentials.expiration),
            profile: profile.to_owned(),
            scope: None,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    /// These credentials, recorded as issued by Mjolnir for `role_arn` and
    /// the STS session `role_session_name`.
    pub fn issued_for(
        mut self,
        role_arn: &str,
        role_session_name: &str,
        session_policy_fingerprint: &str,
        source_identity: Option<&str>,
    ) -> Result<Self> {
        let expires_at_epoch_millis = self
            .expires_at_epoch_millis()
            .context("issued AWS credentials have no expiration")?;
        self.scope = Some(AwsCredentialScope {
            role_arn: role_arn.to_owned(),
            role_session_name: role_session_name.to_owned(),
            session_policy_fingerprint: session_policy_fingerprint.to_owned(),
            source_identity: source_identity.map(str::to_owned),
            expires_at_epoch_millis,
            tag: String::new(),
        });
        Ok(self)
    }

    /// Authenticate the scope with `sign`, given the fingerprint of the secret
    /// that will be installed and the scope's fields.
    pub fn sign_scope(
        mut self,
        sign: impl FnOnce(&AwsCredentialScope, &str) -> Result<String>,
    ) -> Result<Self> {
        let fingerprint = crate::credentials::credential_fingerprint(&self.encode()?);
        if let Some(scope) = &mut self.scope {
            scope.tag = sign(scope, &fingerprint)?;
        }
        Ok(self)
    }

    pub fn scope(&self) -> Option<&AwsCredentialScope> {
        self.scope.as_ref()
    }

    /// Attach a scope read from the wire.
    pub fn with_scope(mut self, scope: Option<AwsCredentialScope>) -> Self {
        self.scope = scope;
        self
    }

    /// When these credentials stop working, in milliseconds since the Unix
    /// epoch. `None` for credentials that do not expire.
    pub fn expires_at_epoch_millis(&self) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(self.expiration.as_deref()?)
            .ok()
            .map(|expiry| expiry.timestamp_millis())
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

    /// Install the complete provider, in staging or at its final home. The
    /// JSON is published last, after the files the SDK needs to resolve it.
    pub fn install(
        &self,
        home: &Path,
        target_home: &Path,
    ) -> Result<crate::credentials::CredentialSnapshot> {
        let bytes = self.encode()?;
        crate::config::atomic_write(&home.join(CONFIG_FILE), &self.configuration(target_home)?)?;
        crate::config::atomic_write(&home.join(EMPTY_CREDENTIALS_FILE), b"")?;
        // The scope record goes first, so the secret is never published
        // without the record of what it is; credentials Mjolnir did not issue
        // leave no record, and a stale one is removed.
        match &self.scope {
            Some(scope) => crate::config::atomic_write(
                &home.join(SCOPE_FILE),
                &serde_json::to_vec(&StoredScope {
                    scope: scope.clone(),
                    secret_fingerprint: crate::credentials::credential_fingerprint(&bytes),
                })
                .context("encode AWS credential scope")?,
            )?,
            None => remove_if_present(&home.join(SCOPE_FILE))?,
        }
        crate::config::atomic_write(&home.join(SHARED_CREDENTIALS_FILE), &bytes)?;
        Ok(snapshot(&bytes))
    }
}

/// Withdraw the session's AWS credentials. The credential file is the secret;
/// without it the native process provider finds nothing to use, and a later
/// install writes every file again.
pub fn remove(home: &Path) -> Result<crate::credentials::CredentialSnapshot> {
    remove_if_present(&home.join(SHARED_CREDENTIALS_FILE))?;
    remove_if_present(&home.join(SCOPE_FILE))?;
    state(home)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// The scope Mjolnir recorded for the credentials in `home`, when the record
/// describes the secret that is there now.
pub fn scope(home: &Path) -> Result<Option<AwsCredentialScope>> {
    let read = |name: &str| match std::fs::read(home.join(name)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read session AWS {name}")),
    };
    let (Some(record), Some(secret)) = (read(SCOPE_FILE)?, read(SHARED_CREDENTIALS_FILE)?) else {
        return Ok(None);
    };
    let Ok(stored) = serde_json::from_slice::<StoredScope>(&record) else {
        return Ok(None);
    };
    Ok(
        (stored.secret_fingerprint == crate::credentials::credential_fingerprint(&secret))
            .then_some(stored.scope),
    )
}

pub fn snapshot(bytes: &[u8]) -> crate::credentials::CredentialSnapshot {
    crate::credentials::CredentialSnapshot {
        present: true,
        fingerprint: crate::credentials::credential_fingerprint(bytes),
        freshness_epoch_ms: None,
    }
}

pub fn state(home: &Path) -> Result<crate::credentials::CredentialSnapshot> {
    let absent = crate::credentials::CredentialSnapshot::absent;
    let bytes = match std::fs::read(home.join(SHARED_CREDENTIALS_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(absent());
        }
        Err(error) => return Err(error).context("read session AWS credentials"),
    };
    // Matching JSON alone does not make the provider usable. Worker upgrades
    // can introduce these launch paths without restaging the profile. Invalid
    // or expired JSON must likewise allow reconciliation to replace it.
    let Ok(configuration) =
        AwsCredentials::decode(&bytes).and_then(|credentials| credentials.configuration(home))
    else {
        return Ok(absent());
    };
    for (name, expected) in [
        (CONFIG_FILE, configuration),
        (EMPTY_CREDENTIALS_FILE, Vec::new()),
    ] {
        match std::fs::read(home.join(name)) {
            Ok(actual) if actual == expected => {}
            Ok(_) => return Ok(absent()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(absent()),
            Err(error) => return Err(error).with_context(|| format!("read session AWS {name}")),
        }
    }
    Ok(snapshot(&bytes))
}
