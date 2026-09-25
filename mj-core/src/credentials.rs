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

use crate::hex::lower_hex;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::config::{AuthScheme, HarnessKind, HarnessProfile};
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
fn validate_opaque_token<'a>(label: &str, limit: usize, bytes: &'a [u8]) -> Result<&'a str> {
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
fn refuse_symlinked_token(label: &str, path: &Path) -> Result<()> {
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
    create_owner_only_directory(directory)?;
    let mut body = token.as_bytes().to_vec();
    body.push(b'\n');
    crate::config::atomic_write_existing(path, &body)
}

/// Create `directory` and any missing parent reachable only by its owner.
fn create_owner_only_directory(directory: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .with_context(|| format!("create {}", directory.display()))
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
/// the `exp` claim of the access token in `tokens.access_token`. Grok and Muse
/// have no proactive-refresh path, so they report nothing here.
///
/// Anything unparseable is `None` rather than a guess.
pub fn credential_expiry(kind: HarnessKind, bytes: &[u8]) -> Option<i64> {
    kind.credential_expiry_ms(&serde_json::from_slice(bytes).ok()?)
}

impl HarnessKind {
    /// Read [`credential_freshness`] out of this harness's own credential JSON.
    pub fn credential_freshness_ms(self, json: &serde_json::Value) -> Option<i64> {
        match self {
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
            // Muse stores no timestamp Hel can order two copies by.
            Self::Muse => None,
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
            // Neither has a proactive-refresh path, so neither reports an
            // expiry Hel would act on.
            Self::Grok | Self::Muse => None,
        }
    }

    /// The harness CLI's own interactive login command.
    ///
    /// Verified against the locally installed CLIs with `--help`: `codex
    /// login`, `claude auth login` (there is no bare `claude login`), `kimi
    /// login`, `grok login`, and `muse login`.
    pub fn native_login_command(self) -> (String, Vec<String>) {
        let arguments = match self {
            Self::Claude => vec!["auth".to_owned(), "login".to_owned()],
            Self::Codex | Self::Kimi | Self::Grok | Self::Muse => vec!["login".to_owned()],
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
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "credential destination {} is a symbolic link",
            path.display()
        );
    }
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
const AUTH_FAILURE_PHRASES: [&str; 6] = [
    "oauth session expired and could not be refreshed",
    "please run /login",
    "authorization grant is invalid",
    // Codex, when its refresh token is rejected (R14-1): "Your access token
    // could not be refreshed. Please log out and sign in again."
    "access token could not be refreshed",
    "log out and sign in again",
    // Hel's own marker for a turn the bridge failed with ACP `auth_required`.
    // The bridge's wording ("Authentication required") is too generic to match.
    "acp auth_required",
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

/// Error kinds a harness names beside a failed turn when the provider
/// rejected its login. Codex sends `codexErrorInfo: "unauthorized"` (R14-1),
/// and a turn's diagnostic keeps that kind as its code. Matched against the
/// code only, never against free text, where "401 Unauthorized" is common.
const AUTH_FAILURE_ERROR_KINDS: [&str; 1] = ["unauthorized"];

/// Whether a failed turn's diagnostic says the provider rejected the
/// session's login: by its error kind, or by an auth failure phrase in its
/// message. A usage limit is never one, as in the worker's warning label.
pub fn turn_diagnostic_reports_auth_failure(diagnostic: &TurnDiagnostic) -> bool {
    if diagnostic.is_usage_limit() {
        return false;
    }
    diagnostic.code.as_deref().is_some_and(|code| {
        AUTH_FAILURE_ERROR_KINDS
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncCause {
    pub session_id: String,
    pub reason: CredentialSyncReason,
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
    if let AuthScheme::ApiKey { env_key } = profile.auth_scheme() {
        bail!(
            "this profile authenticates with the {env_key} API key from its `environment` entry, so it has no interactive login"
        );
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
    profile.kind.native_login_command()
}

/// One live session the coordinator may reconcile with its profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncTarget {
    pub session_id: String,
    pub profile_id: String,
    pub harness: HarnessKind,
    /// Controller-side canonical home for the profile.
    pub profile_home: PathBuf,
    /// True when the profile authenticates with an API key from its own
    /// `environment`. Such a profile has no credential file to exchange with
    /// the session, so only skills and the GitHub token are reconciled.
    pub authenticates_with_api_key: bool,
    /// GitHub CLI credentials are pushed to every target except raw localhost.
    pub sync_github_token: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncOutcome {
    pub session_id: String,
    /// Every action taken for the session, or why the reconcile failed. An
    /// empty action list means the copies already agreed.
    pub outcome: std::result::Result<Vec<CredentialSyncAction>, String>,
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
    pub outcomes: Vec<CredentialSyncOutcome>,
}

impl CredentialSyncResult {
    pub fn pushed_to(&self, session_id: &str) -> bool {
        self.outcomes.iter().any(|outcome| {
            outcome.session_id == session_id
                && outcome
                    .outcome
                    .as_ref()
                    .is_ok_and(|actions| actions.contains(&CredentialSyncAction::Pushed))
        })
    }

    pub fn failures(&self) -> impl Iterator<Item = (&str, &str)> {
        self.outcomes
            .iter()
            .filter_map(|outcome| match &outcome.outcome {
                Err(detail) => Some((outcome.session_id.as_str(), detail.as_str())),
                Ok(_) => None,
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
    pub fn sync_profile_now(&self, profile_id: &str, cause: Option<CredentialSyncCause>) {
        if let Err(error) = self.triggers.send(SyncTrigger {
            profile_id: profile_id.to_owned(),
            cause,
        }) {
            tracing::debug!(
                %profile_id,
                %error,
                "credential sync request dropped because its coordinator stopped"
            );
        }
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

#[cfg(test)]
mod tests;
