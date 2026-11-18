//! Harness credential interpretation and convergence.
//!
//! Hel clones a profile's harness credentials into every session's isolated
//! home. Rotating OAuth refresh tokens make those copies diverge: the first
//! copy to refresh invalidates the grant every other copy still holds. This
//! module owns the one interpretation of credential files Hel has, plus the
//! background service that reconciles the controller-side canonical copy with
//! each live session's copy in both directions.
//!
//! The same reconcile loop also pushes each profile's synced skills trees
//! (see `skills`) into live sessions. Skills are not secrets and do not
//! rotate, so they converge in one direction only: the canonical home wins.
//!
//! Credential bytes travel only in worker request and response frames. They
//! never enter the durable event stream or a checkpoint archive. Fingerprints
//! and freshness timestamps are not secret and may appear in logs.

mod locking;
pub use locking::{
    CLAUDE_REFRESH_LOCK_WAIT, CredentialInstall, CredentialWriteGuard,
    canonical_login_is_replaced_by, install_credentials_if_newer, install_session_credentials,
};

use crate::hex::lower_hex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::v1::schema::{ContentBlock, SessionUpdate};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::config::{AuthScheme, HarnessKind, HarnessProfile, harness_authentication_marker};
use crate::diagnostic::TurnDiagnostic;
use crate::relay::{RelayCommandOutcome, RelayEvent, RelayObservation};
use crate::targets::CommandSpec;

/// Credential files are small JSON or YAML documents. The cap keeps a hostile or
/// corrupt worker from making the controller buffer an arbitrary payload.
pub const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;

/// GitHub tokens are opaque but small. Keeping a separate, tight bound avoids
/// treating the live CLI credential as a general-purpose secret transport.
pub const MAX_GITHUB_TOKEN_BYTES: usize = 4 * 1024;

/// How often the coordinator reconciles every profile with its live sessions.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(60);

pub fn credential_fingerprint(bytes: &[u8]) -> String {
    lower_hex(Sha256::digest(bytes))
}

/// The staged OAuth login, excluding setup tokens and API-key/Bedrock launches.
pub fn claude_oauth_login_path(
    environment: &std::collections::BTreeMap<String, String>,
) -> Option<PathBuf> {
    if [
        CLAUDE_OAUTH_TOKEN_ENV,
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
    ]
    .iter()
    .any(|key| {
        environment
            .get(*key)
            .is_some_and(|value| !value.is_empty() && value != "0")
    }) {
        return None;
    }
    let home = Path::new(environment.get(HarnessKind::Claude.home_env())?);
    Some(crate::config::harness_authentication_marker(
        HarnessKind::Claude,
        home,
    ))
}

/// Identity of the access token itself. Stripping a refresh token or rewriting
/// the JSON cannot authorize recovery of a prompt that used the same login.
pub fn claude_access_token_fingerprint(bytes: &[u8]) -> Option<String> {
    let login: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let token = login.pointer("/claudeAiOauth/accessToken")?.as_str()?;
    (!token.is_empty()).then(|| credential_fingerprint(token.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubTokenSnapshot {
    pub present: bool,
    pub fingerprint: String,
}

impl GithubTokenSnapshot {
    pub fn absent() -> Self {
        Self {
            present: false,
            fingerprint: String::new(),
        }
    }

    pub fn of(token: &str) -> Self {
        Self {
            present: true,
            fingerprint: credential_fingerprint(token.as_bytes()),
        }
    }
}

/// One reading of an opaque single-line secret, shared by every token file Hel
/// stores so the limits and the rejections cannot drift apart.
pub(crate) fn validate_opaque_token<'a>(
    label: &str,
    limit: usize,
    bytes: &'a [u8],
) -> Result<&'a str> {
    if bytes.is_empty() {
        bail!("{label} is empty");
    }
    if bytes.len() > limit {
        bail!("{label} is above the {limit} byte limit");
    }
    let token = std::str::from_utf8(bytes)
        .with_context(|| format!("{label} is not valid UTF-8"))?
        .trim();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        bail!("{label} must be non-empty and contain no whitespace");
    }
    Ok(token)
}

/// A token file must be a real file. Following a symbolic link would let
/// whatever created it choose where the secret is read from or written to.
pub(crate) fn refuse_symlinked_token(label: &str, path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("{label} destination {} is a symbolic link", path.display());
    }
    Ok(())
}

pub fn validate_github_token(bytes: &[u8]) -> Result<&str> {
    validate_opaque_token("GitHub token", MAX_GITHUB_TOKEN_BYTES, bytes)
}

pub fn read_github_token(path: &Path) -> Result<(GithubTokenSnapshot, Option<String>)> {
    refuse_symlinked_token("GitHub token", path)?;
    match std::fs::read(path) {
        Ok(bytes) => {
            let token = validate_github_token(&bytes)?.to_owned();
            Ok((GithubTokenSnapshot::of(&token), Some(token)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((GithubTokenSnapshot::absent(), None))
        }
        Err(error) => Err(error).with_context(|| format!("read GitHub token {}", path.display())),
    }
}

pub fn write_github_token(path: &Path, bytes: &[u8]) -> Result<GithubTokenSnapshot> {
    let token = validate_github_token(bytes)?;
    refuse_symlinked_token("GitHub token", path)?;
    let mut body = token.as_bytes().to_vec();
    body.push(b'\n');
    crate::config::atomic_write_existing(path, &body)?;
    Ok(GithubTokenSnapshot::of(token))
}

pub fn remove_github_token(path: &Path) -> Result<()> {
    refuse_symlinked_token("GitHub token", path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove GitHub token {}", path.display())),
    }
}

/// Claude setup tokens are opaque and single-line, like the GitHub token.
pub const MAX_CLAUDE_OAUTH_TOKEN_BYTES: usize = 4 * 1024;

/// The variable Claude Code reads a long-lived OAuth token from. It takes
/// precedence over the `/login` credentials file and is honoured by the Agent
/// SDK, so a worker started with it never touches the rotating grant.
pub const CLAUDE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Where a profile's long-lived Claude setup token lives on the controller.
///
/// It sits beside the configuration rather than inside the profile home, so
/// profile staging never copies it into a session home or a container.
pub fn claude_oauth_token_path(profile_id: &str) -> PathBuf {
    crate::config::config_dir()
        .join("profiles")
        .join(profile_id)
        .join("claude-oauth-token")
}

pub fn validate_claude_oauth_token(bytes: &[u8]) -> Result<&str> {
    validate_opaque_token("Claude setup token", MAX_CLAUDE_OAUTH_TOKEN_BYTES, bytes)
}

/// The stored setup token, or `None` when the profile has none.
pub fn read_claude_oauth_token(path: &Path) -> Result<Option<String>> {
    refuse_symlinked_token("Claude setup token", path)?;
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(validate_claude_oauth_token(&bytes)?.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read Claude setup token {}", path.display()))
        }
    }
}

pub fn write_claude_oauth_token(path: &Path, bytes: &[u8]) -> Result<()> {
    let token = validate_claude_oauth_token(bytes)?;
    refuse_symlinked_token("Claude setup token", path)?;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("Claude setup token path has no directory")?;
    crate::config::create_owner_only_directory(directory)?;
    let mut body = token.as_bytes().to_vec();
    body.push(b'\n');
    crate::config::atomic_write_existing(path, &body)
}

/// Epoch milliseconds describing how current a credential copy is. Higher wins
/// when two copies of the same grant differ.
///
/// Key structure confirmed against real files on a developer machine:
/// * Claude `~/.claude/.credentials.json`: `{ "claudeAiOauth": { "accessToken",
///   "refreshToken", "expiresAt", "refreshTokenExpiresAt", "scopes",
///   "subscriptionType", "rateLimitTier" } }`. `expiresAt` is a 13-digit
///   number, so already epoch milliseconds.
/// * Codex `~/.codex/auth.json`: `{ "auth_mode", "tokens": { "access_token",
///   "refresh_token", "id_token", "account_id" }, "last_refresh" }`.
///   `last_refresh` is an RFC3339 string with fractional seconds and a `Z`
///   suffix.
/// * Kimi `~/.kimi-code/credentials/kimi-code.json`: `{ "access_token",
///   "refresh_token", "expires_at", "expires_in", "scope", "token_type" }`.
///   `expires_at` is a 10-digit number, so epoch seconds.
/// * Grok `~/.grok/auth.json`: an object keyed by `"<issuer>::<uuid>"`, each
///   value holding `{ "key", "refresh_token", "expires_at", ... }`.
///   `expires_at` is an RFC3339 string with nanosecond precision and a `Z`
///   suffix. A file may hold several grants, so the latest expiry wins.
///
/// Anything unparseable is `None` rather than a guess.
pub fn credential_freshness(kind: HarnessKind, bytes: &[u8]) -> Option<i64> {
    kind.credential_freshness_ms(&serde_json::from_slice(bytes).ok()?)
}

/// Epoch milliseconds at which the stored access token stops working, for the
/// harnesses Hel can refresh ahead of that deadline.
///
/// This is not [`credential_freshness`]. Freshness orders two copies of the
/// same grant; expiry says when the grant runs out. Claude states the same
/// number for both, but Codex orders copies by `last_refresh` and expires by
/// the `exp` claim of the access token in `tokens.access_token`. Grok has no
/// proactive-refresh path, so it reports nothing here.
///
/// Anything unparseable is `None` rather than a guess.
pub fn credential_expiry(kind: HarnessKind, bytes: &[u8]) -> Option<i64> {
    kind.credential_expiry_ms(&serde_json::from_slice(bytes).ok()?)
}

impl HarnessKind {
    /// Whether the credential copies this host hands to a harness process omit
    /// the refresh token, so the daemon stays the grant's only refresher.
    ///
    /// Claude Code refreshes five minutes before `expiresAt`, under a lock that
    /// covers only its own config directory. Every copy that carries the
    /// rotating single-use refresh token can therefore spend it, and a refresh
    /// inside a copy nobody pulls back leaves the host holding a spent token
    /// (#1490). Without the key, Claude Code never refreshes or writes the file
    /// and rereads a replaced one by modification time.
    ///
    /// The same on every host. Where the host's own login lives is
    /// [`HarnessKind::login_store`]'s business (on macOS it is the Keychain,
    /// which the sync reads and the refresher renews); the copies are files
    /// either way.
    pub fn copies_omit_refresh_token(self) -> bool {
        self == Self::Claude
    }
}

/// The bytes of a Claude credential copy without `claudeAiOauth.refreshToken`.
/// The key is removed, never set to `""`, which Claude Code reads as "login
/// ended"; an existing `""` is left alone. Callers decide whether a copy is
/// stripped ([`HarnessKind::copies_omit_refresh_token`]). Other harnesses, and
/// a document that is not Claude's JSON shape, are unchanged.
pub fn credential_copy_for_harness(kind: HarnessKind, bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    use std::borrow::Cow;

    if !credential_holds_refresh_token(kind, bytes) {
        return Cow::Borrowed(bytes);
    }
    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Cow::Borrowed(bytes);
    };
    let Some(oauth) = json
        .get_mut("claudeAiOauth")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Cow::Borrowed(bytes);
    };
    oauth.remove("refreshToken");
    match serde_json::to_vec(&json) {
        Ok(stripped) => Cow::Owned(stripped),
        Err(_) => Cow::Borrowed(bytes),
    }
}

/// Whether a Claude credential carries a refresh token its holder could
/// spend. An empty string is the "login ended" marker, not a token. Other
/// harnesses report none here.
pub fn credential_holds_refresh_token(kind: HarnessKind, bytes: &[u8]) -> bool {
    kind == HarnessKind::Claude
        && serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|json| {
                json.pointer("/claudeAiOauth/refreshToken")
                    .and_then(serde_json::Value::as_str)
                    .map(|token| !token.is_empty())
            })
            .unwrap_or(false)
}

/// Muse's Meta login records its access-token expiry in epoch seconds. A
/// refresh moves it forward, so it both orders copies and says when the login
/// runs out. A stored API key has none.
fn muse_login_expiry_ms(json: &serde_json::Value) -> Option<i64> {
    json.pointer("/providers/meta/expires_at")?
        .as_i64()?
        .checked_mul(1000)
}

impl HarnessKind {
    /// Read [`credential_freshness`] out of this harness's own credential JSON.
    pub fn credential_freshness_ms(self, json: &serde_json::Value) -> Option<i64> {
        match self {
            Self::Antigravity => {
                chrono::DateTime::parse_from_rfc3339(json.get("token")?.get("expiry")?.as_str()?)
                    .ok()
                    .map(|expiry| expiry.timestamp_millis())
            }
            Self::Claude => json.get("claudeAiOauth")?.get("expiresAt")?.as_i64(),
            Self::Codex => {
                let last_refresh = json.get("last_refresh")?.as_str()?;
                chrono::DateTime::parse_from_rfc3339(last_refresh)
                    .ok()
                    .map(|refreshed| refreshed.timestamp_millis())
            }
            Self::Kimi => json
                .get("expires_at")?
                .as_i64()
                .and_then(|seconds| seconds.checked_mul(1000)),
            Self::Muse => muse_login_expiry_ms(json),
            Self::Grok => json
                .as_object()?
                .values()
                .filter_map(|grant| {
                    let expires_at = grant.get("expires_at")?.as_str()?;
                    chrono::DateTime::parse_from_rfc3339(expires_at)
                        .ok()
                        .map(|expiry| expiry.timestamp_millis())
                })
                .max(),
            // OpenCode's logins live in its database, which no copy orders,
            // and DSH has no login.
            Self::OpenCode | Self::Dsh => None,
        }
    }

    /// Read [`credential_expiry`] out of this harness's own credential JSON.
    pub fn credential_expiry_ms(self, json: &serde_json::Value) -> Option<i64> {
        match self {
            Self::Claude => json.get("claudeAiOauth")?.get("expiresAt")?.as_i64(),
            Self::Codex => jwt_expiry_millis(json.get("tokens")?.get("access_token")?.as_str()?),
            Self::Kimi => json
                .get("expires_at")?
                .as_i64()
                .and_then(|seconds| seconds.checked_mul(1000)),
            Self::Muse => muse_login_expiry_ms(json),
            // None of these has a proactive-refresh path, so none reports an
            // expiry Hel would act on: each refreshes its own tokens on demand.
            Self::Grok | Self::OpenCode | Self::Dsh | Self::Antigravity => None,
        }
    }

    /// The harness CLI's own interactive login command.
    ///
    /// Verified against the locally installed CLIs with `--help`: `codex
    /// login`, `claude auth login` (there is no bare `claude login`), `kimi
    /// login`, `grok login`, `muse login`, and `opencode auth login`.
    pub fn native_login_command(self) -> (String, Vec<String>) {
        let arguments = match self {
            Self::Claude => vec!["auth".to_owned(), "login".to_owned()],
            Self::OpenCode => vec!["auth".to_owned(), "login".to_owned()],
            Self::Codex | Self::Kimi | Self::Grok | Self::Muse => vec!["login".to_owned()],
            // dsh-acp has no login; its profiles authenticate with an API
            // key, so `login_command` refuses before this is reached.
            Self::Dsh => Vec::new(),
            Self::Antigravity => vec!["acp".into(), "--login".into(), "--disable-telemetry".into()],
        };
        (self.cli_binary_name().to_owned(), arguments)
    }
}

/// The `exp` claim of a JWT, in epoch milliseconds.
///
/// Only the payload is read, and only for its expiry. The signature is the
/// issuer's business; Hel never accepts the token on anyone's behalf, so there
/// is nothing here for a forged claim to unlock beyond an early refresh.
fn jwt_expiry_millis(token: &str) -> Option<i64> {
    use base64::Engine as _;

    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp")?.as_i64()?.checked_mul(1000)
}

/// Reject anything that is not a plausible credential document before it
/// replaces a canonical file or lands in a session home.
pub fn validate_credential_payload(_kind: HarnessKind, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        bail!("credential payload is empty");
    }
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        bail!(
            "credential payload is {} bytes, above the {MAX_CREDENTIAL_BYTES} byte limit",
            bytes.len()
        );
    }
    let text = std::str::from_utf8(bytes).context("credential payload is not valid UTF-8")?;
    serde_json::from_str::<serde_json::Value>(text).context("credential payload is not JSON")?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LoginStatus {
    /// A credential the harness can start from is present.
    SignedIn,
    /// A credential is present but its recorded expiry has passed. The harness
    /// may still refresh it on the next turn.
    Expired,
    /// This profile uses a harness login and no credential is present.
    SignedOut,
    /// This profile authenticates without an interactive harness login.
    NotRequired(String),
    /// The status could not be read, or the harness keeps it somewhere this
    /// process cannot inspect.
    Unknown(String),
}

impl LoginStatus {
    /// Short text for a settings row, without a trailing period.
    pub fn label(&self) -> &str {
        match self {
            Self::SignedIn => "signed in",
            Self::Expired => "credential expired",
            Self::SignedOut => "signed out",
            Self::NotRequired(detail) | Self::Unknown(detail) => detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSnapshot {
    pub present: bool,
    pub fingerprint: String,
    pub freshness_epoch_ms: Option<i64>,
}

impl CredentialSnapshot {
    pub fn absent() -> Self {
        Self {
            present: false,
            fingerprint: String::new(),
            freshness_epoch_ms: None,
        }
    }

    pub fn of(kind: HarnessKind, bytes: &[u8]) -> Self {
        Self {
            present: true,
            fingerprint: credential_fingerprint(bytes),
            freshness_epoch_ms: credential_freshness(kind, bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Install the canonical copy into the session.
    Push,
    /// Adopt the session's copy as canonical.
    Pull,
    /// The copies already agree, or nothing can be decided.
    None,
}

/// Decide which way a profile's canonical copy and one session's copy converge.
///
/// When both sides are present, differ, and neither reports freshness, Hel
/// refuses to guess: file modification times are not a reliable proxy for grant
/// age across a container boundary.
pub fn reconcile(canonical: &CredentialSnapshot, session: &CredentialSnapshot) -> SyncAction {
    match (canonical.present, session.present) {
        (false, false) => SyncAction::None,
        (true, false) => SyncAction::Push,
        (false, true) => SyncAction::Pull,
        (true, true) => {
            if canonical.fingerprint == session.fingerprint {
                return SyncAction::None;
            }
            match (canonical.freshness_epoch_ms, session.freshness_epoch_ms) {
                (Some(canonical), Some(session)) if canonical > session => SyncAction::Push,
                (Some(canonical), Some(session)) if session > canonical => SyncAction::Pull,
                (Some(_), Some(_)) => SyncAction::None,
                (Some(_), None) => SyncAction::Push,
                (None, Some(_)) => SyncAction::Pull,
                (None, None) => SyncAction::None,
            }
        }
    }
}

/// Read a credential file, treating "not there" as a snapshot rather than an
/// error. A directory or unreadable file is an error worth surfacing.
pub fn read_credential_file(
    kind: HarnessKind,
    path: &Path,
) -> Result<(CredentialSnapshot, Vec<u8>)> {
    match std::fs::read(path) {
        Ok(bytes) => Ok((CredentialSnapshot::of(kind, &bytes), bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((CredentialSnapshot::absent(), Vec::new()))
        }
        Err(error) => Err(error).with_context(|| format!("read credentials {}", path.display())),
    }
}

/// Replace a credential file without exposing a partial write or widening its
/// permissions. Refuses a symlinked destination so a compromised session home
/// cannot redirect the write.
pub fn write_credential_file(kind: HarnessKind, path: &Path, bytes: &[u8]) -> Result<()> {
    validate_credential_payload(kind, bytes)?;
    refuse_symlinked_token("credential", path)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("restrict permissions on {}", parent.display()))?;
        }
    }
    // `atomic_write` creates the temporary file with mode 0600 and renames it
    // over the destination, so the installed file is never world-readable.
    crate::config::atomic_write(path, bytes)
}

/// Full-phrase markers that a harness rejected the session's credentials.
/// Kept tight on purpose: a false positive costs one redundant sync and one
/// notice, but a noisy list would train operators to ignore both.
const AUTH_FAILURE_PHRASES: [&str; 9] = [
    "oauth session expired and could not be refreshed",
    "please run /login",
    "authorization grant is invalid",
    // Codex, when its refresh token is rejected (R14-1): "Your access token
    // could not be refreshed. Please log out and sign in again."
    "access token could not be refreshed",
    "log out and sign in again",
    // OpenAI, when it refuses an API key (#1160): "401 Unauthorized:
    // Incorrect API key provided: sk-...".
    "incorrect api key provided",
    // Kimi, when its stored OAuth token is refused (#1132): "Stored token for
    // \"kimi-code\" was rejected; re-login required." Its error name,
    // `OAuthUnauthorizedError`, is an identifier below.
    "re-login required",
    // Hel's own marker for a turn the bridge failed with ACP `auth_required`.
    // The bridge's wording ("Authentication required") is too generic to match.
    "acp auth_required",
    // Codex ACP can refuse workspace routing while loading a parked session.
    "workspace routing discovery unauthorized (401)",
];

/// Machine-readable authentication errors must be complete identifiers. This
/// avoids treating paths such as `authentication_error_status_code` as auth
/// failures while still recognizing the codes in JSON and diagnostics.
const AUTH_FAILURE_IDENTIFIERS: [&str; 3] = [
    "authentication_error",
    "invalid_grant",
    "oauthunauthorizederror",
];

fn contains_ascii_identifier(text: &str, identifier: &str) -> bool {
    text.match_indices(identifier).any(|(start, _)| {
        let end = start + identifier.len();
        let is_identifier_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
        let starts_at_boundary = start == 0 || !is_identifier_byte(text.as_bytes()[start - 1]);
        let ends_at_boundary = end == text.len() || !is_identifier_byte(text.as_bytes()[end]);
        starts_at_boundary && ends_at_boundary
    })
}

fn contains_auth_failure_signature(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    AUTH_FAILURE_PHRASES
        .iter()
        .any(|phrase| normalized.contains(phrase))
        || AUTH_FAILURE_IDENTIFIERS
            .iter()
            .any(|identifier| contains_ascii_identifier(&normalized, identifier))
}

pub fn auth_failure_signature(_kind: HarnessKind, text: &str) -> bool {
    contains_auth_failure_signature(text)
}

/// [`auth_failure_signature`] for text whose harness is not known, such as a
/// failed turn's recorded reason.
pub fn text_reports_auth_failure(text: &str) -> bool {
    contains_auth_failure_signature(text)
}

/// Error kinds a harness names beside a failed turn when the provider
/// rejected its login. Codex sends `codexErrorInfo: "unauthorized"` (R14-1),
/// and a turn's diagnostic keeps that kind as its code. Matched against the
/// code only, never against free text, where "401 Unauthorized" is common.
const AUTH_FAILURE_ERROR_KINDS: [&str; 1] = [crate::diagnostic::AUTH_FAILURE_KIND];

/// Whether a failed turn's diagnostic says the provider rejected the
/// session's login: by its error kind, or by an auth failure phrase in its
/// message. A usage limit is never one, as in the worker's warning label.
pub fn turn_diagnostic_reports_auth_failure(diagnostic: &TurnDiagnostic) -> bool {
    if diagnostic.is_usage_limit() {
        return false;
    }
    diagnostic.code.as_deref().is_some_and(|code| {
        code == agent_client_protocol::ErrorCode::AUTH_REQUIRED.to_string()
            || AUTH_FAILURE_ERROR_KINDS
                .iter()
                .any(|kind| code.eq_ignore_ascii_case(kind))
    }) || contains_auth_failure_signature(&diagnostic.message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSyncReason {
    AuthenticationFailure,
    EmptyPromptResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSyncSignal {
    pub ordinal: u64,
    pub reason: CredentialSyncReason,
    /// Hash of the access token the failed turn used (the turn diagnostic's
    /// `login_fingerprint`). `None` when the failure carries none: a warning,
    /// a worker that predates the field, or a harness without a token hash.
    // Never serialized: an older client's copy of this struct denies unknown
    // fields and would fail to decode the session view. The daemon passes it
    // in-process only.
    #[serde(skip)]
    pub login_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncCause {
    pub session_id: String,
    pub ordinal: u64,
    pub reason: CredentialSyncReason,
    /// The failed turn's access-token hash, as in [`CredentialSyncSignal`].
    pub login_fingerprint: Option<String>,
}

impl CredentialSyncCause {
    pub fn for_failed_turn(
        session_id: &str,
        turn: &crate::state::MaterializedTurnOutcome,
    ) -> Option<Self> {
        crate::subagent::turn_failed_on_login(turn).then(|| Self {
            session_id: session_id.to_owned(),
            ordinal: turn.completed_ordinal,
            reason: CredentialSyncReason::AuthenticationFailure,
            login_fingerprint: turn
                .diagnostic
                .as_ref()
                .and_then(|diagnostic| diagnostic.login_fingerprint.clone()),
        })
    }
}

/// What an authentication failure means, decided once against the profile's
/// current login (#1547). Every consumer (the push, the refusal, the notice)
/// reads this answer instead of comparing logins on its own snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationVerdict {
    /// The failed token is no longer the profile's: the login was renewed
    /// under the session. The current login is pushed and the turn retries.
    LoginRenewed,
    /// The failed token is still the profile's current one: the provider
    /// refused the current login.
    CurrentLoginRefused,
    /// Either side's token identity is missing. Never grounds for a refusal.
    Unknown,
}

/// Is the access token that failed still the profile's current access token?
pub fn classify_authentication_failure(
    failed_token: Option<&str>,
    current_token: Option<&str>,
) -> AuthenticationVerdict {
    match (failed_token, current_token) {
        (Some(failed), Some(current)) if failed == current => {
            AuthenticationVerdict::CurrentLoginRefused
        }
        (Some(_), Some(_)) => AuthenticationVerdict::LoginRenewed,
        _ => AuthenticationVerdict::Unknown,
    }
}

/// The failed turn's access-token hash, when this event is a failed prompt.
pub fn relay_event_login_fingerprint(event: &RelayEvent) -> Option<String> {
    match &event.observation {
        RelayObservation::CommandCompleted {
            outcome:
                RelayCommandOutcome::Prompt {
                    diagnostic: Some(diagnostic),
                    ..
                },
            ..
        } if turn_diagnostic_reports_auth_failure(diagnostic) => {
            diagnostic.login_fingerprint.clone()
        }
        _ => None,
    }
}

/// Detect a reason for immediate credential reconciliation only in relay
/// observations originating from the harness: its warnings, its agent text,
/// and the diagnostic of a turn it failed. Durable prompt commands are
/// deliberately excluded, so user text cannot trigger a sync.
///
/// The failed turn counts on its own because the worker leaves out a warning
/// that only repeats the harness's last agent message (R14-1).
pub fn relay_event_credential_sync_reason(event: &RelayEvent) -> Option<CredentialSyncReason> {
    match &event.observation {
        RelayObservation::CommandCompleted {
            outcome:
                RelayCommandOutcome::Prompt {
                    diagnostic: Some(diagnostic),
                    ..
                },
            ..
        } if turn_diagnostic_reports_auth_failure(diagnostic) => {
            Some(CredentialSyncReason::AuthenticationFailure)
        }
        RelayObservation::Warning { message } if contains_auth_failure_signature(message) => {
            Some(CredentialSyncReason::AuthenticationFailure)
        }
        RelayObservation::Warning { message }
            if message.contains(crate::acp::PROMPT_EMPTY_RESPONSE_MARKER) =>
        {
            Some(CredentialSyncReason::EmptyPromptResponse)
        }
        RelayObservation::SessionUpdate { update } => match update.as_ref() {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) if contains_auth_failure_signature(&text.text) => {
                    Some(CredentialSyncReason::AuthenticationFailure)
                }
                _ => None,
            },
            // A bridge's typed failure record, here one that arrived outside a
            // turn; a failed turn's record reaches the turn's diagnostic.
            SessionUpdate::SessionInfoUpdate(update) => {
                crate::diagnostic::SessionFailure::from_meta(update.meta.as_ref())
                    .filter(|failure| {
                        turn_diagnostic_reports_auth_failure(&TurnDiagnostic::from_session_failure(
                            failure,
                        ))
                    })
                    .map(|_| CredentialSyncReason::AuthenticationFailure)
            }
            _ => None,
        },
        _ => None,
    }
}

pub fn events_report_auth_failure(_kind: HarnessKind, events: &[RelayEvent]) -> bool {
    events.iter().any(|event| {
        relay_event_credential_sync_reason(event)
            == Some(CredentialSyncReason::AuthenticationFailure)
    })
}

/// Build the harness's own interactive login command for a profile.
///
/// Verified against the locally installed CLIs with `--help`: `codex login`,
/// `claude auth login` (there is no bare `claude login`), `kimi login`,
/// `grok login`, and `muse login`.
///
pub fn login_command(profile: &HarnessProfile) -> Result<(String, Vec<String>)> {
    match profile.auth_scheme() {
        AuthScheme::ApiKey { env_key } => bail!(
            "this profile authenticates with the {env_key} API key, from its `environment` entry or the environment Mjolnir started with, so it has no interactive login"
        ),
        AuthScheme::InlineApiKey => bail!(
            "this profile inlines its provider API key in its Codex `config.toml`, so it has no interactive login"
        ),
        AuthScheme::AwsCredentialChain => bail!(
            "this profile authenticates with the AWS credential chain, so it has no interactive login"
        ),
        AuthScheme::NoAuthentication => {
            bail!("this profile's built-in provider does not require an interactive login")
        }
        AuthScheme::NativeLogin | AuthScheme::NativeLoginStore => {}
    }
    Ok(native_login_command(profile))
}

/// The harness CLI's own login command, ignoring how the profile actually
/// authenticates.
///
/// Setup discovery uses this to find an installed CLI before any profile
/// exists, where "this profile needs no login" is not a useful answer: it only
/// wants the program name to run `--version` against.
pub fn native_login_command(profile: &HarnessProfile) -> (String, Vec<String>) {
    let (command, mut args) = profile.kind.native_login_command();
    args.extend(profile.kind.home_cli_args(&profile.home));
    (command, args)
}

/// One live session the coordinator may reconcile with its profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncTarget {
    pub session_id: String,
    pub profile_id: String,
    pub harness: HarnessKind,
    /// Controller-side canonical home for the profile.
    pub profile_home: PathBuf,
    /// True when the profile has no harness login file to synchronize. Such a
    /// profile has no credential file to exchange with the session, so only
    /// skills and the GitHub token are reconciled.
    pub skips_login_file_sync: bool,
    /// Host AWS chain provisioned into this isolated session, when selected.
    pub aws_credentials: Option<crate::aws_credentials::AwsCredentialSource>,
    /// Local sessions use their host login unless a session credential is configured.
    pub sync_github_token: bool,
    /// Controller-owned source for this session's GitHub credential. Included
    /// in target identity so a reload invalidates obsolete reconciliation.
    pub github: crate::config::GithubConfig,
    /// Target-derived availability of the host CLI skill.
    pub skills_scope: crate::skills::SkillsScope,
    /// Reconnect command for the session's worker proxy.
    pub spec: CommandSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSyncAction {
    /// The canonical copy replaced the session's copy.
    Pushed,
    /// The session's fresher copy became canonical.
    Pulled,
    /// The canonical skills trees replaced the session's trees. Skills sync
    /// is push-only: the controller-side profile home stays authoritative.
    SkillsPushed,
    /// The controller's current GitHub token replaced the session copy.
    GithubTokenPushed,
    /// A stale session token was removed because the controller has none.
    GithubTokenRemoved,
    /// The session's AWS credentials were withdrawn, because the profile
    /// requires role-scoped credentials and none could be issued.
    AwsCredentialsRemoved,
    /// The host's slopcop login replaced the session copy.
    SlopcopLoginPushed,
    /// The session copy was removed because the host is signed out.
    SlopcopLoginRemoved,
}

/// The independent steps of one session's sync. Each fails on its own, and a
/// result can say that a step is fine again only if it checked that step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncStep {
    /// Not a step: the session's whole sync failed (its connection, say)
    /// before any step ran. Reported without a step.
    Session,
    /// The role-scoped AWS credentials, renewed by the session's own task.
    Aws,
    /// The profile's one AWS export, reconciled with its login pass.
    AwsExport,
    HarnessLogin,
    Skills,
    GithubToken,
    Slopcop,
}

/// One step that failed while the others ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    pub step: SyncStep,
    /// What failed, beginning with the step's name.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncOutcome {
    pub session_id: String,
    /// Hash of the access token in the profile's login as this sync read it
    /// (Claude only). With the failed turn's hash it decides what an
    /// authentication failure means; see [`classify_authentication_failure`].
    pub current_access_token: Option<String>,
    /// The snapshot that initially matched the failing session without any
    /// credential mutation. Refusal evidence must refer to this observation,
    /// never to a later reading of the profile's login.
    pub unchanged_login: Option<CredentialSnapshot>,
    /// Every action taken for the session, or why the reconcile failed. An
    /// empty action list means the copies already agreed.
    pub outcome: std::result::Result<Vec<CredentialSyncAction>, String>,
    /// Steps of the sync that failed while the others ran, such as the AWS
    /// credentials. `outcome` then still lists what the other steps did, so
    /// their work is counted and reported.
    pub step_failures: Vec<StepFailure>,
}

impl CredentialSyncOutcome {
    /// The step failures as one line, for a log or a test.
    pub fn failure_text(&self) -> Option<String> {
        (!self.step_failures.is_empty()).then(|| {
            self.step_failures
                .iter()
                .map(|failure| failure.detail.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
}

/// Reported to the UI loops only when something happened: an action was taken,
/// a session failed, or an on-demand sync finished with nothing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncResult {
    pub profile_id: String,
    /// Session event that asked for an immediate sync, when any.
    pub trigger: Option<CredentialSyncCause>,
    /// The whole reconcile stopped before it could report per-session
    /// outcomes. Kept separate so the failure is reported, never dropped.
    pub failure: Option<String>,
    /// The result covers only the sessions in `outcomes` (one session's AWS
    /// renewal), not the whole profile: a failure it does not mention is not
    /// thereby over.
    pub partial: bool,
    pub outcomes: Vec<CredentialSyncOutcome>,
}

impl CredentialSyncResult {
    /// What the triggering authentication failure meant, from the profile's
    /// login as this sync read it. `None` when the sync was not triggered by
    /// an authentication failure or did not reach the triggering session.
    pub fn authentication_verdict(&self) -> Option<AuthenticationVerdict> {
        let cause = self.trigger.as_ref()?;
        if cause.reason != CredentialSyncReason::AuthenticationFailure {
            return None;
        }
        let reached = self
            .outcomes
            .iter()
            .find(|outcome| outcome.session_id == cause.session_id)?;
        Some(classify_authentication_failure(
            cause.login_fingerprint.as_deref(),
            reached.current_access_token.as_deref(),
        ))
    }

    pub fn pushed_to(&self, session_id: &str) -> bool {
        self.outcomes.iter().any(|outcome| {
            outcome.session_id == session_id
                && outcome
                    .outcome
                    .as_ref()
                    .is_ok_and(|actions| actions.contains(&CredentialSyncAction::Pushed))
        })
    }

    /// Every failure, with the step it belongs to (`None` for a session whose
    /// whole sync failed).
    pub fn failures(&self) -> impl Iterator<Item = (&str, Option<SyncStep>, &str)> {
        self.outcomes.iter().flat_map(|outcome| {
            let whole = outcome
                .outcome
                .as_ref()
                .err()
                .map(|detail| (outcome.session_id.as_str(), None, detail.as_str()));
            let steps = outcome.step_failures.iter().map(|failure| {
                (
                    outcome.session_id.as_str(),
                    (failure.step != SyncStep::Session).then_some(failure.step),
                    failure.detail.as_str(),
                )
            });
            whole.into_iter().chain(steps)
        })
    }

    /// Sessions that took at least one action of any kind.
    pub fn actions(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| {
                outcome
                    .outcome
                    .as_ref()
                    .is_ok_and(|actions| !actions.is_empty())
            })
            .count()
    }

    /// Sessions whose harness credentials were pushed or pulled.
    pub fn credential_sessions(&self) -> usize {
        self.count_actions(|action| {
            matches!(
                action,
                CredentialSyncAction::Pushed | CredentialSyncAction::Pulled
            )
        })
    }

    /// Sessions whose skills trees were replaced.
    pub fn skills_sessions(&self) -> usize {
        self.count_actions(|action| action == CredentialSyncAction::SkillsPushed)
    }

    pub fn github_token_pushed_sessions(&self) -> usize {
        self.count_actions(|action| action == CredentialSyncAction::GithubTokenPushed)
    }

    pub fn github_token_removed_sessions(&self) -> usize {
        self.count_actions(|action| action == CredentialSyncAction::GithubTokenRemoved)
    }

    pub fn slopcop_login_pushed_sessions(&self) -> usize {
        self.count_actions(|action| action == CredentialSyncAction::SlopcopLoginPushed)
    }

    pub fn slopcop_login_removed_sessions(&self) -> usize {
        self.count_actions(|action| action == CredentialSyncAction::SlopcopLoginRemoved)
    }

    fn count_actions(&self, wanted: impl Fn(CredentialSyncAction) -> bool) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| {
                outcome
                    .outcome
                    .as_ref()
                    .is_ok_and(|actions| actions.iter().copied().any(&wanted))
            })
            .count()
    }
}

/// Profiles whose own stored login the provider has refused.
///
/// The credential sync is what learns it: a session reported an auth failure,
/// the sync reached that session, and the profile's copy was no fresher than
/// the session's, so there was nothing to push that could help.
///
/// A Claude refusal is recorded for exactly one access token: the failed
/// turn's token, and only when it is still the profile's current token
/// ([`classify_authentication_failure`]). It ends when the profile's token
/// differs, so a renewal, `mj login`, or a fresher copy pulled from another
/// session clears it, and a failure of an older token never records one.
/// Other harnesses report no token identity, so their refusal still holds
/// while the whole login file is unchanged.
#[derive(Debug, Default)]
pub struct RejectedLogins {
    profiles: std::collections::BTreeMap<String, (HarnessKind, PathBuf, LoginFingerprint)>,
    failures: BTreeMap<String, BTreeMap<u64, LoginFailure>>,
    deliveries: BTreeMap<String, LoginDelivery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginFingerprint {
    AccessToken(String),
    File(String),
}

impl LoginFingerprint {
    pub fn read_file(kind: HarnessKind, marker: &Path) -> Result<Option<Self>> {
        let (snapshot, bytes) = read_credential_file(kind, marker)?;
        if !snapshot.present {
            return Ok(None);
        }
        Ok(if kind == HarnessKind::Claude {
            claude_access_token_fingerprint(&bytes).map(Self::AccessToken)
        } else {
            Some(Self::File(snapshot.fingerprint))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginFailureVerdict {
    Deciding,
    Refused,
    Renewed,
    Unrefused { reason: String },
}

#[derive(Debug)]
struct LoginFailure {
    profile_id: String,
    fingerprint: Option<String>,
    conclusion: Option<LoginConclusion>,
}

#[derive(Debug)]
enum LoginConclusion {
    Refused(LoginFingerprint),
    Delivered { renewed: bool },
    Renewed,
    Unrefused(String),
}

impl LoginConclusion {
    fn verdict(&self) -> LoginFailureVerdict {
        match self {
            Self::Refused(_) => LoginFailureVerdict::Refused,
            Self::Delivered { renewed: true } | Self::Renewed => LoginFailureVerdict::Renewed,
            Self::Delivered { renewed: false } => LoginFailureVerdict::Unrefused {
                reason: "credential reconciliation refreshed this session's login".into(),
            },
            Self::Unrefused(reason) => LoginFailureVerdict::Unrefused {
                reason: reason.clone(),
            },
        }
    }
}

impl LoginFailure {
    fn matches(&self, profile_id: &str, cause: &CredentialSyncCause) -> bool {
        self.profile_id == profile_id && self.fingerprint == cause.login_fingerprint
    }
}

/// The current profile login's delivery to a live session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginDeliveryState {
    Pending,
    Concluded { reason: String },
}

#[derive(Debug)]
struct LoginDelivery {
    target: CredentialSyncTarget,
    fingerprint: String,
    state: LoginDeliveryState,
    /// A worker that read back a different login already tried this delivery.
    /// The same requested login is not reinstalled unless the held copy changes.
    mismatch: Option<CredentialSnapshot>,
}

impl RejectedLogins {
    pub fn delivery_state(&self, profile_id: &str, session_id: &str) -> Option<LoginDeliveryState> {
        let delivery = self.deliveries.get(session_id)?;
        (delivery.target.profile_id == profile_id).then(|| delivery.state.clone())
    }

    pub fn start_delivery(
        &mut self,
        target: &CredentialSyncTarget,
        fingerprint: &str,
        state: LoginDeliveryState,
    ) {
        if self
            .deliveries
            .get(&target.session_id)
            .is_some_and(|delivery| {
                delivery.target == *target
                    && delivery.fingerprint == fingerprint
                    && delivery.mismatch.is_some()
            })
            && state == LoginDeliveryState::Pending
        {
            return;
        }
        self.record_delivery(target, fingerprint, state, None);
    }

    pub fn record_delivery(
        &mut self,
        target: &CredentialSyncTarget,
        fingerprint: &str,
        state: LoginDeliveryState,
        mismatch: Option<CredentialSnapshot>,
    ) {
        self.deliveries.insert(
            target.session_id.clone(),
            LoginDelivery {
                target: target.clone(),
                fingerprint: fingerprint.to_owned(),
                state,
                mismatch,
            },
        );
    }

    pub fn delivery_mismatch(
        &self,
        target: &CredentialSyncTarget,
        fingerprint: &str,
        held: &CredentialSnapshot,
    ) -> bool {
        self.deliveries
            .get(&target.session_id)
            .is_some_and(|delivery| {
                delivery.target == *target
                    && delivery.fingerprint == fingerprint
                    && delivery
                        .mismatch
                        .as_ref()
                        .is_some_and(|observed| observed == held)
            })
    }

    pub fn observe_delivered_login(
        &mut self,
        profile_id: &str,
        session_id: &str,
        fingerprint: &str,
    ) {
        if let Some(delivery) = self.deliveries.get_mut(session_id)
            && delivery.target.profile_id == profile_id
            && delivery.fingerprint == fingerprint
            && delivery.state == LoginDeliveryState::Pending
        {
            delivery.state = LoginDeliveryState::Concluded {
                reason: "worker reports holding the current login".into(),
            };
        }
    }

    pub fn retain_deliveries(&mut self, exists: impl Fn(&str) -> bool) {
        self.deliveries.retain(|id, _| exists(id));
    }

    pub fn end_delivery(
        &mut self,
        target: &CredentialSyncTarget,
        fingerprint: Option<&str>,
        reason: String,
    ) {
        if let Some(delivery) = self.deliveries.get_mut(&target.session_id)
            && delivery.target == *target
            && fingerprint.is_none_or(|fingerprint| delivery.fingerprint == fingerprint)
        {
            delivery.state = LoginDeliveryState::Concluded { reason };
        }
    }

    /// The poller accepted a reconciliation for this failure. A later turn
    /// supersedes it even when the worker used the same token for both.
    pub fn accept(&mut self, profile_id: &str, cause: &CredentialSyncCause) {
        if cause.reason == CredentialSyncReason::AuthenticationFailure {
            self.failures
                .entry(cause.session_id.clone())
                .or_default()
                .entry(cause.ordinal)
                .or_insert_with(|| LoginFailure {
                    profile_id: profile_id.to_owned(),
                    fingerprint: cause.login_fingerprint.clone(),
                    conclusion: None,
                });
        }
    }

    /// An authentication event can arrive as both a relay signal and a
    /// completed turn. Both have the same ordinal.
    pub fn was_accepted(&self, session_id: &str, ordinal: u64) -> bool {
        self.failures
            .get(session_id)
            .is_some_and(|session| session.contains_key(&ordinal))
    }

    /// A preceding failure cannot be used to report a later turn. Warnings
    /// after a terminal failure have their own ordinals and leave it intact.
    pub fn supersede(&mut self, session_id: &str, ordinal: u64) {
        if let std::collections::btree_map::Entry::Occupied(mut session) =
            self.failures.entry(session_id.to_owned())
        {
            session.get_mut().retain(|previous, _| *previous >= ordinal);
            if session.get().is_empty() {
                session.remove();
            }
        }
    }

    /// A sync finishing after removal cannot revive a retired decision.
    pub fn retain_failures(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.failures.retain(|id, _| keep(id));
        self.deliveries.retain(|id, _| keep(id));
    }

    /// Take in one finished sync of a configured profile.
    pub fn observe(&mut self, result: &CredentialSyncResult, profile: &HarnessProfile) {
        self.conclude(result, Some(profile));
    }

    /// Record the reconciliation's own observation. A notice describes that
    /// observation, while a later projection revalidates a refusal against
    /// the login the profile holds then.
    pub fn conclude(
        &mut self,
        result: &CredentialSyncResult,
        profile: Option<&HarnessProfile>,
    ) -> Option<LoginFailureVerdict> {
        let cause = result
            .trigger
            .as_ref()
            .filter(|cause| cause.reason == CredentialSyncReason::AuthenticationFailure)?;
        let conclusion = Self::sync_conclusion(result, cause, profile);
        if let (Some(profile), LoginConclusion::Refused(identity)) = (profile, &conclusion) {
            let entry = (
                profile.kind,
                harness_authentication_marker(profile.kind, &profile.home),
                identity.clone(),
            );
            if self.profiles.get(&result.profile_id) != Some(&entry) {
                tracing::info!(
                    profile_id = %result.profile_id,
                    session_id = %cause.session_id,
                    "credential reconciliation found the profile's login refused"
                );
                self.profiles.insert(result.profile_id.clone(), entry);
            }
        }
        let failure = self
            .failures
            .get_mut(&cause.session_id)
            .and_then(|session| session.get_mut(&cause.ordinal))
            .filter(|failure| failure.matches(&result.profile_id, cause))?;
        let verdict = conclusion.verdict();
        failure.conclusion = Some(conclusion);
        Some(verdict)
    }

    fn sync_conclusion(
        result: &CredentialSyncResult,
        cause: &CredentialSyncCause,
        profile: Option<&HarnessProfile>,
    ) -> LoginConclusion {
        let unrefused = |reason: &str| LoginConclusion::Unrefused(reason.to_owned());
        if let Some(detail) = &result.failure {
            return unrefused(detail);
        }
        let Some(profile) = profile else {
            return unrefused("the profile is no longer configured");
        };
        if profile.kind.keeps_logins_in_database() {
            return unrefused("this profile stores its login in a database");
        }
        // Only targets using a login file can return login evidence. Reading
        // the profile again here would check a different login or config.
        let Some(reached) = result
            .outcomes
            .iter()
            .find(|outcome| outcome.session_id == cause.session_id)
        else {
            return unrefused("credential reconciliation did not reach this session");
        };
        let actions = match &reached.outcome {
            Ok(actions) => actions,
            Err(detail) => return unrefused(detail),
        };
        if let Some(failure) = reached
            .step_failures
            .iter()
            .find(|failure| matches!(failure.step, SyncStep::HarnessLogin | SyncStep::Session))
        {
            return unrefused(&failure.detail);
        }
        let verdict = classify_authentication_failure(
            cause.login_fingerprint.as_deref(),
            reached.current_access_token.as_deref(),
        );
        if actions.iter().any(|action| {
            matches!(
                action,
                CredentialSyncAction::Pushed | CredentialSyncAction::Pulled
            )
        }) {
            return LoginConclusion::Delivered {
                renewed: profile.kind == HarnessKind::Claude
                    && verdict == AuthenticationVerdict::LoginRenewed,
            };
        }
        if profile.kind == HarnessKind::Claude && verdict == AuthenticationVerdict::LoginRenewed {
            return LoginConclusion::Renewed;
        }
        let Some(snapshot) = reached
            .unchanged_login
            .as_ref()
            .filter(|snapshot| snapshot.present)
        else {
            return unrefused("credential reconciliation found no unchanged login");
        };
        if profile.kind != HarnessKind::Claude {
            return LoginConclusion::Refused(LoginFingerprint::File(snapshot.fingerprint.clone()));
        }
        match &cause.login_fingerprint {
            Some(token) if verdict == AuthenticationVerdict::CurrentLoginRefused => {
                LoginConclusion::Refused(LoginFingerprint::AccessToken(token.clone()))
            }
            _ => unrefused("credential reconciliation could not identify the failed login"),
        }
    }

    pub fn failure_verdict(
        &self,
        profile_id: &str,
        cause: &CredentialSyncCause,
        current: Option<&LoginFingerprint>,
    ) -> LoginFailureVerdict {
        use LoginFailureVerdict::*;
        let Some(failure) = self
            .failures
            .get(&cause.session_id)
            .and_then(|session| session.get(&cause.ordinal))
            .filter(|failure| failure.matches(profile_id, cause))
        else {
            return Unrefused {
                reason: "credential reconciliation has no result for this turn".into(),
            };
        };
        if self.delivery_state(profile_id, &cause.session_id) == Some(LoginDeliveryState::Pending) {
            return Deciding;
        }
        let Some(conclusion) = &failure.conclusion else {
            return Deciding;
        };
        if let (Some(failed), Some(LoginFingerprint::AccessToken(current))) =
            (&failure.fingerprint, current)
            && failed != current
        {
            return Renewed;
        }
        if let LoginConclusion::Refused(refused) = conclusion {
            self.wake_verdict(refused, current)
        } else {
            conclusion.verdict()
        }
    }

    pub fn wake_verdict(
        &self,
        staged: &LoginFingerprint,
        current: Option<&LoginFingerprint>,
    ) -> LoginFailureVerdict {
        match current {
            Some(current) if current == staged => LoginFailureVerdict::Refused,
            Some(_) => LoginFailureVerdict::Renewed,
            None => LoginFailureVerdict::Unrefused {
                reason: "the current profile login could not be identified".into(),
            },
        }
    }

    /// Why a new session on this profile would fail to sign in, while its
    /// login is still the one the provider refused.
    pub fn refusal(&self, profile_id: &str) -> Option<String> {
        let (kind, marker, refused) = self.profiles.get(profile_id)?;
        let current = LoginFingerprint::read_file(*kind, marker).ok().flatten();
        (self.wake_verdict(refused, current.as_ref()) == LoginFailureVerdict::Refused)
            .then(|| crate::subagent::login_invalid_reason(profile_id))
    }
}

#[derive(Debug, Clone)]
pub struct SyncTrigger {
    pub profile_id: String,
    pub cause: Option<CredentialSyncCause>,
}

/// Handle the UI loops keep. Publishing targets and asking for an immediate
/// sync are both non-blocking.
#[derive(Clone)]
pub struct CredentialSyncHandle {
    pub targets: Arc<watch::Sender<Vec<CredentialSyncTarget>>>,
    pub triggers: mpsc::UnboundedSender<SyncTrigger>,
}

impl CredentialSyncHandle {
    pub fn set_targets(&self, targets: Vec<CredentialSyncTarget>) {
        if *self.targets.borrow() != targets {
            self.targets.send_replace(targets);
        }
    }

    /// Reconcile one profile now instead of waiting for the next cycle.
    pub fn sync_profile_now(&self, profile_id: &str, cause: Option<CredentialSyncCause>) -> bool {
        if let Err(error) = self.triggers.send(SyncTrigger {
            profile_id: profile_id.to_owned(),
            cause,
        }) {
            tracing::debug!(
                %profile_id,
                %error,
                "credential sync request dropped because its coordinator stopped"
            );
            return false;
        }
        true
    }
}

pub fn profiles_with_targets(targets: &[CredentialSyncTarget]) -> Vec<String> {
    let mut profiles = BTreeSet::new();
    for target in targets {
        profiles.insert(target.profile_id.clone());
    }
    profiles.into_iter().collect()
}

pub fn enqueue(queue: &mut VecDeque<SyncTrigger>, trigger: SyncTrigger) {
    if trigger.cause.is_none()
        && queue
            .iter()
            .any(|queued| queued.profile_id == trigger.profile_id)
    {
        return;
    }
    queue.push_back(trigger);
}

/// Authentication variables understood by GitHub CLI on github.com and enterprise hosts.
pub const GITHUB_AUTHENTICATION_ENVIRONMENT: [&str; 4] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// A credential-free launch removes these after environment composition.
pub fn remove_github_authentication_environment(
    environment: &mut std::collections::BTreeMap<String, String>,
) {
    for name in GITHUB_AUTHENTICATION_ENVIRONMENT {
        environment.remove(name);
    }
}

#[cfg(test)]
mod tests;
