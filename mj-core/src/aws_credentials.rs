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
    /// Whether credentials are minted for the role. A role is configured for
    /// every session of the profile, but only a session that receives synced
    /// credentials gets any: for a profile that does not authenticate with the
    /// AWS credential chain nothing is minted, and credentials a session holds
    /// from before are still withdrawn.
    pub mints: bool,
    /// Where the harness's own configuration may name the AWS profile the
    /// role-scoped mint runs as (Codex's provider table). Only the pointer is
    /// part of the source and of the sync target's identity; the file is read
    /// each time a credential is minted, because an edit to it does not
    /// rebuild the target.
    pub selection: Option<AwsProfileSelection>,
}

/// The configuration file that may select the AWS profile.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AwsProfileSelection {
    pub home: std::path::PathBuf,
    /// The Codex profile overlay the profile runs with, if any.
    pub overlay: Option<String>,
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

    /// The AWS profile these credentials are installed under: the section name
    /// in the session's AWS configuration.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// These credentials, installed under another profile name. The name is
    /// part of the secret, so this is done before the scope is signed.
    pub fn with_profile(mut self, profile: &str) -> Result<Self> {
        ensure!(
            valid_profile_name(profile),
            "AWS profile name {profile:?} is not valid"
        );
        self.profile = profile.to_owned();
        Ok(self)
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

/// A profile name the controller will install role-scoped credentials under.
pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// The AWS profile this session was staged with: the section name in its AWS
/// configuration. Role-scoped credentials are installed under it, so a renewal
/// keeps the name the session's environment selects. The session can edit the
/// file, which is safe here only because what is installed under the name is
/// role-scoped: a changed name changes where those credentials are written, not
/// what they allow. `None` when nothing was staged or the name is not valid.
pub fn staged_profile(home: &Path) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(home.join(CONFIG_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read session AWS configuration"),
    };
    Ok(text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("[profile ")
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|name| valid_profile_name(name))
            .map(str::to_owned)
    }))
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
    state_for_user(home, user_home().as_deref())
}

/// The home directory of the user this process runs as, which a relative
/// profile path in the session's AWS configuration is relative to.
fn user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// [`state`] for a user whose home directory is `user_home`.
pub fn state_for_user(
    home: &Path,
    user_home: Option<&Path>,
) -> Result<crate::credentials::CredentialSnapshot> {
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
    let Ok(credentials) = AwsCredentials::decode(&bytes) else {
        return Ok(absent());
    };
    let read = |name: &str| match std::fs::read(home.join(name)) {
        Ok(actual) => Ok(Some(actual)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read session AWS {name}")),
    };
    let Some(configuration) = read(CONFIG_FILE)? else {
        return Ok(absent());
    };
    if !configuration_names(
        &configuration,
        credentials.profile(),
        &home.join(SHARED_CREDENTIALS_FILE),
        user_home,
    ) {
        return Ok(absent());
    }
    if !read(EMPTY_CREDENTIALS_FILE)?.is_some_and(|empty| empty.is_empty()) {
        return Ok(absent());
    }
    Ok(snapshot(&bytes))
}

/// Whether the AWS configuration `actual` is the one for `profile` and the
/// secret at `secret`. Launch writes the secret's path as the target will see
/// it, which for an SSH worker is relative to the user's home, while the
/// worker has resolved its own home to an absolute path: the two spellings of
/// one path are equal, so this resolves a relative one against `user_home` and
/// compares the paths, not their text. This is the one place the configured
/// path is compared.
fn configuration_names(
    actual: &[u8],
    profile: &str,
    secret: &Path,
    user_home: Option<&Path>,
) -> bool {
    let Ok(actual) = std::str::from_utf8(actual) else {
        return false;
    };
    let mut lines = actual.lines();
    let (Some(header), Some(command), None) = (lines.next(), lines.next(), lines.next()) else {
        return false;
    };
    if header != format!("[profile {profile}]") {
        return false;
    }
    let Some(configured) = command
        .strip_prefix("credential_process = ")
        .and_then(configured_secret_path)
    else {
        return false;
    };
    let configured = Path::new(&configured);
    let resolved = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        match user_home {
            Some(user_home) => user_home.join(configured),
            None => return false,
        }
    };
    same_path(&resolved, secret)
}

/// The path in a `credential_process` command written by
/// [`AwsCredentials::configuration`]: `cat '<absolute path>'`, or for a path
/// relative to the user's home, `sh -c 'exec cat "$HOME"/'<path>''`.
fn configured_secret_path(command: &str) -> Option<String> {
    let unquote = |text: &str| {
        text.strip_prefix('\'')
            .and_then(|text| text.strip_suffix('\''))
            .map(|text| text.replace("'\\''", "'"))
    };
    if let Some(path) = command.strip_prefix("cat ") {
        return unquote(path);
    }
    let script = unquote(command.strip_prefix("sh -c ")?)?;
    unquote(script.strip_prefix("exec cat \"$HOME\"/")?)
}

/// Two spellings of one path: equal once symlinks are resolved where the files
/// exist, and otherwise equal component by component.
fn same_path(left: &Path, right: &Path) -> bool {
    let resolved = |path: &Path| {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.components().collect::<std::path::PathBuf>())
    };
    resolved(left) == resolved(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> AwsCredentials {
        AwsCredentials::from_export(
            "bedrock",
            &serde_json::to_vec(&serde_json::json!({
                "Version": 1, "AccessKeyId": "key", "SecretAccessKey": "secret",
                "SessionToken": "token", "Expiration": "2099-01-01T00:00:00Z",
            }))
            .unwrap(),
        )
        .unwrap()
    }

    // Launch writes an SSH worker's profile path relative to the user's home;
    // the worker has resolved its own home to an absolute path before it
    // compares. Those are two spellings of one path, so valid credentials are
    // present, not absent: reading them as absent would have the next sync
    // withdraw them.
    #[test]
    fn a_relative_launch_path_and_an_absolute_worker_home_name_the_same_secret() {
        let user_home = tempfile::tempdir().unwrap();
        let relative = Path::new(".local/share/hel/workers/s/profile");
        let home = user_home.path().join(relative);
        std::fs::create_dir_all(&home).unwrap();

        credentials().install(&home, relative).unwrap();
        assert!(
            std::fs::read_to_string(home.join(CONFIG_FILE))
                .unwrap()
                .contains("\"$HOME\""),
            "launch wrote the relative spelling"
        );
        assert!(
            state_for_user(&home, Some(user_home.path()))
                .unwrap()
                .present
        );

        // The absolute spelling (a local worker) is the same secret too.
        credentials().install(&home, &home).unwrap();
        assert!(state_for_user(&home, None).unwrap().present);

        // A configuration naming another secret, or a relative one that cannot
        // be resolved, is not this session's.
        credentials()
            .install(&home, Path::new(".local/share/hel/workers/other/profile"))
            .unwrap();
        assert!(
            !state_for_user(&home, Some(user_home.path()))
                .unwrap()
                .present
        );
        credentials().install(&home, relative).unwrap();
        assert!(!state_for_user(&home, None).unwrap().present);
        // Nor is a configuration for another profile.
        std::fs::write(
            home.join(CONFIG_FILE),
            std::fs::read_to_string(home.join(CONFIG_FILE))
                .unwrap()
                .replace("[profile bedrock]", "[profile other]"),
        )
        .unwrap();
        assert!(
            !state_for_user(&home, Some(user_home.path()))
                .unwrap()
                .present
        );
    }
}
