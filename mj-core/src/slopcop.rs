//! The slopcop login a session borrows from the person running Mjolnir.
//!
//! slopcop is the CLI for the hosted SlopCop check service. A session that
//! cannot read the host's own login (see
//! [`crate::credentials::CredentialSyncTarget::syncs_host_logins`]) gets a copy
//! in its staged harness home as [`LOGIN_FILE`]: written at launch, kept
//! current by credential sync, and exported by the session's `slopcop` wrapper
//! on every invocation. Mjolnir reads slopcop's own configuration and never
//! writes it.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::credentials::{CredentialSnapshot, credential_fingerprint};

/// A session's copy of the login, relative to its harness home.
pub const LOGIN_FILE: &str = "slopcop-login";
/// The variables slopcop reads ahead of its stored configuration.
pub const TOKEN_ENV: &str = "SLOPCOP_TOKEN";
pub const SERVICE_ENV: &str = "SLOPCOP_SERVICE";

const MAX_TOKEN_BYTES: usize = 4 * 1024;
const MAX_SERVICE_URL_BYTES: usize = 2 * 1024;
/// slopcop's stored configuration holds a URL and a token; anything larger is
/// not that file.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// The encoded login: the token line, the service URL line, and newlines.
pub const MAX_LOGIN_BYTES: usize = MAX_TOKEN_BYTES + MAX_SERVICE_URL_BYTES + 2;

/// The token and, when it is not slopcop's default, the service URL.
#[derive(Clone, PartialEq, Eq)]
pub struct SlopcopLogin {
    token: String,
    service_url: Option<String>,
}

impl std::fmt::Debug for SlopcopLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlopcopLogin")
            .field("token", &"<redacted>")
            .field("service_url", &self.service_url)
            .finish()
    }
}

impl SlopcopLogin {
    pub fn new(token: &str, service_url: Option<&str>) -> Result<Self> {
        let token = crate::credentials::validate_opaque_token(
            "slopcop token",
            MAX_TOKEN_BYTES,
            token.as_bytes(),
        )?;
        let service_url = service_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(validate_service_url)
            .transpose()?;
        Ok(Self {
            token: token.to_owned(),
            service_url: service_url.map(ToOwned::to_owned),
        })
    }

    /// The two-line form the session wrapper reads with `read -r`: the token,
    /// then the service URL, which is empty for slopcop's default.
    pub fn encode(&self) -> Vec<u8> {
        format!(
            "{}\n{}\n",
            self.token,
            self.service_url.as_deref().unwrap_or_default()
        )
        .into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_LOGIN_BYTES {
            bail!("slopcop login is above the {MAX_LOGIN_BYTES} byte limit");
        }
        let text = std::str::from_utf8(bytes).context("slopcop login is not valid UTF-8")?;
        let mut lines = text.lines();
        let token = lines.next().unwrap_or_default();
        let service_url = lines.next();
        if lines.next().is_some() {
            bail!("slopcop login has more than a token and a service URL");
        }
        Self::new(token, service_url)
    }

    pub fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            present: true,
            fingerprint: credential_fingerprint(&self.encode()),
            freshness_epoch_ms: None,
        }
    }
}

/// The URL must fit on one line of the login file and in one environment
/// variable. Whether it names a usable service is slopcop's decision, as it
/// is for the host's own `--service`, so Mjolnir carries it unchanged.
fn validate_service_url(url: &str) -> Result<&str> {
    if url.len() > MAX_SERVICE_URL_BYTES {
        bail!("slopcop service URL is above the {MAX_SERVICE_URL_BYTES} byte limit");
    }
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("slopcop service URL contains whitespace or control characters");
    }
    Ok(url)
}

/// The host's login, resolved from this process's environment and slopcop's
/// stored configuration under the user configuration directory (Go's
/// `os.UserConfigDir`, which `dirs::config_dir` matches on every platform).
pub fn host_login() -> Result<Option<SlopcopLogin>> {
    resolve_login(
        |name| std::env::var(name).ok(),
        dirs::config_dir().as_deref(),
    )
}

/// Resolve a login the way slopcop does: `SLOPCOP_TOKEN`, else the stored
/// token; `SLOPCOP_SERVICE`, else the stored service URL.
///
/// `Ok(None)` means there is no login. A stored configuration that exists but
/// cannot be read is an error: the login is unknown, and a caller must not
/// treat it as signed out.
pub fn resolve_login(
    environment: impl Fn(&str) -> Option<String>,
    config_dir: Option<&Path>,
) -> Result<Option<SlopcopLogin>> {
    let set = |name| {
        environment(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let token = set(TOKEN_ENV);
    let service_url = set(SERVICE_ENV);
    if token.is_some() && service_url.is_some() {
        return SlopcopLogin::new(token.as_deref().unwrap_or_default(), service_url.as_deref())
            .map(Some);
    }
    let stored = match config_dir {
        Some(directory) => read_stored_config(&directory.join("slopcop").join("config.json"))?,
        None => StoredConfig::default(),
    };
    let Some(token) = token.or(stored.token) else {
        return Ok(None);
    };
    SlopcopLogin::new(&token, service_url.or(stored.service_url).as_deref()).map(Some)
}

/// The fields of slopcop's `config.json` it resolves a login from.
#[derive(Debug, Default, serde::Deserialize)]
struct StoredConfig {
    #[serde(default, rename = "serviceUrl")]
    service_url: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

fn read_stored_config(path: &Path) -> Result<StoredConfig> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredConfig::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read slopcop login {}", path.display()));
        }
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_CONFIG_BYTES + 1),
        &mut bytes,
    )
    .with_context(|| format!("read slopcop login {}", path.display()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!(
            "slopcop login {} is above the {MAX_CONFIG_BYTES} byte limit",
            path.display()
        );
    }
    let mut stored: StoredConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse slopcop login {}", path.display()))?;
    for value in [&mut stored.token, &mut stored.service_url] {
        *value = value
            .take()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
    }
    Ok(stored)
}

/// The session copy's state and contents.
pub fn read_login(path: &Path) -> Result<(CredentialSnapshot, Option<SlopcopLogin>)> {
    crate::credentials::refuse_symlinked_token("slopcop login", path)?;
    match std::fs::read(path) {
        Ok(bytes) => {
            let login = SlopcopLogin::decode(&bytes)?;
            Ok((login.snapshot(), Some(login)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((CredentialSnapshot::absent(), None))
        }
        Err(error) => Err(error).with_context(|| format!("read slopcop login {}", path.display())),
    }
}

/// Install `bytes`, an encoded login, owner-only in an existing directory.
pub fn write_login(path: &Path, bytes: &[u8]) -> Result<CredentialSnapshot> {
    let login = SlopcopLogin::decode(bytes)?;
    crate::credentials::refuse_symlinked_token("slopcop login", path)?;
    crate::config::atomic_write_existing(path, &login.encode())?;
    Ok(login.snapshot())
}

pub fn remove_login(path: &Path) -> Result<()> {
    crate::credentials::refuse_symlinked_token("slopcop login", path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove slopcop login {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_login_follows_slopcop_and_reports_an_unreadable_configuration_as_unknown() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("slopcop").join("config.json");
        let unset = |_: &str| None;
        assert_eq!(resolve_login(unset, Some(home.path())).unwrap(), None);

        // The file `slopcop login` writes: indented JSON with a trailing newline.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "{\n  \"serviceUrl\": \"https://slopcop.example\",\n  \"token\": \"stored-token\"\n}\n",
        )
        .unwrap();
        let stored = SlopcopLogin::new("stored-token", Some("https://slopcop.example")).unwrap();
        assert_eq!(
            resolve_login(unset, Some(home.path())).unwrap(),
            Some(stored)
        );

        // SLOPCOP_TOKEN wins over the stored token; the stored URL still applies.
        let token = |name: &str| (name == TOKEN_ENV).then(|| " env-token\n".to_owned());
        assert_eq!(
            resolve_login(token, Some(home.path())).unwrap(),
            Some(SlopcopLogin::new("env-token", Some("https://slopcop.example")).unwrap())
        );

        // `slopcop logout` keeps the service URL and drops the token.
        std::fs::write(
            &file,
            "{\n  \"serviceUrl\": \"https://slopcop.example\"\n}\n",
        )
        .unwrap();
        assert_eq!(resolve_login(unset, Some(home.path())).unwrap(), None);

        // A configuration that cannot be read says nothing about the login.
        std::fs::write(&file, "{ \"token\": ").unwrap();
        assert!(resolve_login(unset, Some(home.path())).is_err());
    }
}
