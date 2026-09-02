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
//! (see `hel_skills`) into live sessions. Skills are not secrets and do not
//! rotate, so they converge in one direction only: the canonical home wins.
//!
//! Credential bytes travel only in worker request and response frames. They
//! never enter the durable event stream or a checkpoint archive. Fingerprints
//! and freshness timestamps are not secret and may appear in logs.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::hel_config::{HarnessKind, HarnessProfile};
use crate::hel_targets::CommandSpec;
use crate::hel_worker::{RelayEvent, RelayObservation};

/// Credential files are small JSON or YAML documents. The cap keeps a hostile or
/// corrupt worker from making the controller buffer an arbitrary payload.
pub const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;

/// GitHub tokens are opaque but small. Keeping a separate, tight bound avoids
/// treating the live CLI credential as a general-purpose secret transport.
pub const MAX_GITHUB_TOKEN_BYTES: usize = 4 * 1024;

/// How often the coordinator reconciles every profile with its live sessions.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(60);

pub fn credential_fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
    crate::hel_config::atomic_write_existing(path, &body)?;
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
    crate::hel_config::config_dir()
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
    crate::hel_config::atomic_write_existing(path, &body)
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
/// * DeepSeek Harness `~/.dsh/.credentials.yaml`: a versioned credential store
///   without refresh timestamps. Divergent live copies are therefore never
///   ordered by a guessed freshness value.
///
/// Anything unparseable is `None` rather than a guess.
pub fn credential_freshness(kind: HarnessKind, bytes: &[u8]) -> Option<i64> {
    if kind == HarnessKind::Deepseek {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match kind {
        HarnessKind::Claude => value.get("claudeAiOauth")?.get("expiresAt")?.as_i64(),
        HarnessKind::Codex => {
            let last_refresh = value.get("last_refresh")?.as_str()?;
            chrono::DateTime::parse_from_rfc3339(last_refresh)
                .ok()
                .map(|refreshed| refreshed.timestamp_millis())
        }
        HarnessKind::Kimi => value
            .get("expires_at")?
            .as_i64()
            .and_then(|seconds| seconds.checked_mul(1000)),
        HarnessKind::Grok => value
            .as_object()?
            .values()
            .filter_map(|grant| {
                let expires_at = grant.get("expires_at")?.as_str()?;
                chrono::DateTime::parse_from_rfc3339(expires_at)
                    .ok()
                    .map(|expiry| expiry.timestamp_millis())
            })
            .max(),
        HarnessKind::Deepseek => unreachable!("handled before JSON parsing"),
    }
}

/// Epoch milliseconds at which the stored access token stops working, for the
/// harnesses Hel can refresh ahead of that deadline.
///
/// This is not [`credential_freshness`]. Freshness orders two copies of the
/// same grant; expiry says when the grant runs out. Claude states the same
/// number for both, but Codex orders copies by `last_refresh` and expires by
/// the `exp` claim of the access token in `tokens.access_token`. Grok and
/// DeepSeek have no proactive-refresh path, so they report nothing here.
///
/// Anything unparseable is `None` rather than a guess.
pub fn credential_expiry(kind: HarnessKind, bytes: &[u8]) -> Option<i64> {
    if matches!(kind, HarnessKind::Grok | HarnessKind::Deepseek) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match kind {
        HarnessKind::Claude => value.get("claudeAiOauth")?.get("expiresAt")?.as_i64(),
        HarnessKind::Codex => {
            jwt_expiry_millis(value.get("tokens")?.get("access_token")?.as_str()?)
        }
        HarnessKind::Kimi => value
            .get("expires_at")?
            .as_i64()
            .and_then(|seconds| seconds.checked_mul(1000)),
        HarnessKind::Grok | HarnessKind::Deepseek => unreachable!("handled before JSON parsing"),
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
pub fn validate_credential_payload(kind: HarnessKind, bytes: &[u8]) -> Result<()> {
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
    if kind == HarnessKind::Deepseek {
        validate_deepseek_credentials(text)?;
    } else {
        serde_json::from_str::<serde_json::Value>(text)
            .context("credential payload is not JSON")?;
    }
    Ok(())
}

fn validate_deepseek_credentials(text: &str) -> Result<()> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).context("DeepSeek credential payload is not YAML")?;
    let document = value
        .as_mapping()
        .context("DeepSeek credential payload must be a mapping")?;
    let key = |name: &str| serde_yaml::Value::String(name.to_owned());
    if document
        .get(key("version"))
        .and_then(serde_yaml::Value::as_u64)
        != Some(1)
    {
        bail!("DeepSeek credential payload must have version 1");
    }
    for name in document.keys().filter_map(serde_yaml::Value::as_str) {
        if !matches!(name, "version" | "refs" | "records") {
            bail!("DeepSeek credential payload has unknown top-level key {name:?}");
        }
    }
    if document.keys().any(|name| name.as_str().is_none()) {
        bail!("DeepSeek credential payload has a non-string top-level key");
    }
    if let Some(refs) = document.get(key("refs")) {
        let refs = refs
            .as_mapping()
            .context("DeepSeek credential refs must be a mapping")?;
        for (name, value) in refs {
            let name = name
                .as_str()
                .context("DeepSeek credential ref name must be a string")?;
            ensure_posix_credential_name(name)?;
            if value.as_str().is_none_or(str::is_empty) {
                bail!("DeepSeek credential ref {name:?} must be a non-empty string");
            }
        }
    }
    if let Some(records) = document.get(key("records")) {
        let records = records
            .as_mapping()
            .context("DeepSeek credential records must be a mapping")?;
        for (name, record) in records {
            let name = name
                .as_str()
                .context("DeepSeek credential record name must be a string")?;
            let (scope, id) = name
                .split_once('/')
                .context("DeepSeek credential record name must be <scope>/<id>")?;
            if scope.is_empty() || id.is_empty() || id.contains('/') {
                bail!("DeepSeek credential record name must be <scope>/<id>");
            }
            validate_deepseek_credential_record(name, record)?;
        }
    }
    Ok(())
}

fn ensure_posix_credential_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        bail!("DeepSeek credential ref {name:?} is not a POSIX identifier");
    }
    Ok(())
}

fn validate_deepseek_credential_record(name: &str, value: &serde_yaml::Value) -> Result<()> {
    let record = value
        .as_mapping()
        .with_context(|| format!("DeepSeek credential record {name:?} must be a mapping"))?;
    let key = |field: &str| serde_yaml::Value::String(field.to_owned());
    let kind = record
        .get(key("kind"))
        .and_then(serde_yaml::Value::as_str)
        .with_context(|| format!("DeepSeek credential record {name:?} has no string kind"))?;
    let allowed: &[&str] = match kind {
        "api-key" => &["kind", "key", "env"],
        "grant" => &["kind", "payload"],
        _ => bail!("DeepSeek credential record {name:?} has an unknown kind"),
    };
    for field in record.keys() {
        let field = field.as_str().with_context(|| {
            format!("DeepSeek credential record {name:?} has a non-string field")
        })?;
        if !allowed.contains(&field) {
            bail!("DeepSeek credential record {name:?} has unknown field {field:?}");
        }
    }
    if kind == "api-key" {
        if let Some(value) = record.get(key("key"))
            && value.as_str().is_none_or(str::is_empty)
        {
            bail!("DeepSeek credential record {name:?} key must be a non-empty string");
        }
        if let Some(env) = record.get(key("env")) {
            let env = env.as_mapping().with_context(|| {
                format!("DeepSeek credential record {name:?} env must be a mapping")
            })?;
            for (env_name, value) in env {
                let env_name = env_name.as_str().with_context(|| {
                    format!("DeepSeek credential record {name:?} env name must be a string")
                })?;
                ensure_posix_credential_name(env_name)?;
                if value.as_str().is_none_or(str::is_empty) {
                    bail!(
                        "DeepSeek credential record {name:?} env {env_name:?} must be a non-empty string"
                    );
                }
            }
        }
    } else {
        let payload = record
            .get(key("payload"))
            .with_context(|| format!("DeepSeek credential record {name:?} has no payload"))?;
        serde_json::to_value(payload).with_context(|| {
            format!("DeepSeek credential record {name:?} payload is not JSON-compatible")
        })?;
    }
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
    crate::hel_config::atomic_write(path, bytes)
}

/// Full-phrase markers that a harness rejected the session's credentials.
/// Kept tight on purpose: a false positive costs one redundant sync and one
/// notice, but a noisy list would train operators to ignore both.
const AUTH_FAILURE_PHRASES: [&str; 4] = [
    "oauth session expired and could not be refreshed",
    "please run /login",
    "authorization grant is invalid",
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
/// observations originating from the harness. Durable prompt commands are
/// deliberately excluded, so user text cannot trigger a sync.
pub fn relay_event_credential_sync_reason(event: &RelayEvent) -> Option<CredentialSyncReason> {
    match &event.observation {
        RelayObservation::Warning { message } if contains_auth_failure_signature(message) => {
            Some(CredentialSyncReason::AuthenticationFailure)
        }
        RelayObservation::Warning { message }
            if message.contains(crate::hel_acp::PROMPT_EMPTY_RESPONSE_MARKER) =>
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
/// `grok login`, and DeepSeek's `dsh web` credential settings UI.
///
/// `profile.executable` overrides the *ACP bridge*, not the harness CLI: for
/// Codex and Claude it names an adapter binary (`codex-acp`,
/// `claude-agent-acp`) that has no login command, so only Kimi and Grok —
/// whose bridges are the `kimi` and `grok` CLIs themselves — honor the
/// override here.
pub fn login_command(profile: &HarnessProfile) -> (String, Vec<String>) {
    let overridable = |fallback: &str| {
        profile
            .executable
            .as_ref()
            .map(|executable| executable.to_string_lossy().into_owned())
            .unwrap_or_else(|| fallback.to_owned())
    };
    match profile.kind {
        HarnessKind::Codex => ("codex".to_owned(), vec!["login".to_owned()]),
        HarnessKind::Claude => (
            "claude".to_owned(),
            vec!["auth".to_owned(), "login".to_owned()],
        ),
        HarnessKind::Kimi => (overridable("kimi"), vec!["login".to_owned()]),
        HarnessKind::Grok => (overridable("grok"), vec!["login".to_owned()]),
        HarnessKind::Deepseek => ("dsh".to_owned(), vec!["web".to_owned()]),
    }
}

/// One live session the coordinator may reconcile with its profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSyncTarget {
    pub session_id: String,
    pub profile_id: String,
    pub harness: HarnessKind,
    /// Controller-side canonical home for the profile.
    pub profile_home: PathBuf,
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
pub(crate) struct SyncTrigger {
    pub(crate) profile_id: String,
    pub(crate) cause: Option<CredentialSyncCause>,
}

/// Handle the UI loops keep. Publishing targets and asking for an immediate
/// sync are both non-blocking.
#[derive(Clone)]
pub struct CredentialSyncHandle {
    pub(crate) targets: Arc<watch::Sender<Vec<CredentialSyncTarget>>>,
    pub(crate) triggers: mpsc::UnboundedSender<SyncTrigger>,
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

pub(crate) fn profiles_with_targets(targets: &[CredentialSyncTarget]) -> Vec<String> {
    let mut profiles = BTreeSet::new();
    for target in targets {
        profiles.insert(target.profile_id.clone());
    }
    profiles.into_iter().collect()
}

pub(crate) fn enqueue(queue: &mut VecDeque<SyncTrigger>, trigger: SyncTrigger) {
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
mod tests {
    use super::*;
    use crate::hel_setup::harness_authentication_marker;

    fn claude_credentials(expires_at: i64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access",
                "refreshToken": "refresh",
                "expiresAt": expires_at,
                "refreshTokenExpiresAt": expires_at + 1_000,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
                "rateLimitTier": "default",
            }
        }))
        .unwrap()
    }

    /// `exp` of the access token every Codex fixture carries.
    const CODEX_FIXTURE_EXPIRY_SECONDS: i64 = 1_785_901_860;

    /// A JWT shaped like a Codex access token: header, `exp` claim, and a
    /// signature nothing here verifies.
    fn codex_access_token(expiry_seconds: i64) -> String {
        use base64::Engine as _;

        let segment = |value: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&value).unwrap())
        };
        format!(
            "{}.{}.signature-is-never-checked",
            segment(serde_json::json!({ "alg": "RS256", "typ": "JWT" })),
            segment(serde_json::json!({ "exp": expiry_seconds, "sub": "account" })),
        )
    }

    fn codex_credentials(last_refresh: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": codex_access_token(CODEX_FIXTURE_EXPIRY_SECONDS),
                "refresh_token": "refresh",
                "id_token": "id",
                "account_id": "account",
            },
            "last_refresh": last_refresh,
        }))
        .unwrap()
    }

    fn kimi_credentials(expires_at: i64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_at": expires_at,
            "expires_in": 900,
            "scope": "all",
            "token_type": "Bearer",
        }))
        .unwrap()
    }

    fn grok_credentials(expiries: &[&str]) -> Vec<u8> {
        let grants = expiries
            .iter()
            .enumerate()
            .map(|(index, expires_at)| {
                (
                    format!("https://auth.x.ai::grant-{index}"),
                    serde_json::json!({
                        "key": "access",
                        "auth_mode": "oidc",
                        "refresh_token": "refresh",
                        "expires_at": expires_at,
                        "oidc_issuer": "https://auth.x.ai",
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::to_vec(&serde_json::Value::Object(grants)).unwrap()
    }

    fn snapshot(fingerprint: &str, freshness: Option<i64>) -> CredentialSnapshot {
        CredentialSnapshot {
            present: true,
            fingerprint: fingerprint.to_owned(),
            freshness_epoch_ms: freshness,
        }
    }

    #[test]
    fn claude_freshness_reads_oauth_expiry_milliseconds() {
        assert_eq!(
            credential_freshness(HarnessKind::Claude, &claude_credentials(1_755_000_000_000)),
            Some(1_755_000_000_000)
        );
    }

    #[test]
    fn codex_freshness_converts_last_refresh_to_milliseconds() {
        assert_eq!(
            credential_freshness(
                HarnessKind::Codex,
                &codex_credentials("2026-08-05T02:51:00.864587231Z")
            ),
            Some(1_785_898_260_864)
        );
    }

    #[test]
    fn kimi_freshness_converts_expiry_seconds_to_milliseconds() {
        assert_eq!(
            credential_freshness(HarnessKind::Kimi, &kimi_credentials(1_755_000_000)),
            Some(1_755_000_000_000)
        );
    }

    #[test]
    fn grok_freshness_reads_the_latest_rfc3339_grant_expiry() {
        assert_eq!(
            credential_freshness(
                HarnessKind::Grok,
                &grok_credentials(&["2026-08-17T02:19:01.724226598Z"])
            ),
            Some(1_786_933_141_724)
        );
        // A home may hold several grants; the newest expiry decides freshness.
        assert_eq!(
            credential_freshness(
                HarnessKind::Grok,
                &grok_credentials(&[
                    "2026-08-17T02:19:01.724226598Z",
                    "2026-08-17T04:19:01.724226598Z",
                ])
            ),
            Some(1_786_940_341_724)
        );
        // Non-UTC offsets normalize to the same instant.
        assert_eq!(
            credential_freshness(
                HarnessKind::Grok,
                &grok_credentials(&["2026-08-16T22:19:01.724226598-04:00"])
            ),
            Some(1_786_933_141_724)
        );
    }

    #[test]
    fn every_harness_reports_freshness_from_its_own_credential_shape() {
        let fixtures = [
            (HarnessKind::Claude, claude_credentials(1_755_000_000_000)),
            (
                HarnessKind::Codex,
                codex_credentials("2026-08-05T02:51:00.864587231Z"),
            ),
            (HarnessKind::Kimi, kimi_credentials(1_755_000_000)),
            (
                HarnessKind::Grok,
                grok_credentials(&["2026-08-17T02:19:01.724226598Z"]),
            ),
        ];
        for kind in HarnessKind::ALL
            .into_iter()
            .filter(|kind| *kind != HarnessKind::Deepseek)
        {
            let (_, bytes) = fixtures
                .iter()
                .find(|(fixture, _)| *fixture == kind)
                .unwrap_or_else(|| panic!("{kind:?} needs a credential fixture"));
            assert!(
                credential_freshness(kind, bytes).is_some(),
                "{kind:?} freshness"
            );
        }
    }

    #[test]
    fn every_harness_reports_expiry_only_where_hel_can_refresh_ahead_of_it() {
        let fixtures = [
            (
                HarnessKind::Claude,
                claude_credentials(1_755_000_000_000),
                Some(1_755_000_000_000),
            ),
            (
                HarnessKind::Codex,
                codex_credentials("2026-08-05T02:51:00.864587231Z"),
                Some(CODEX_FIXTURE_EXPIRY_SECONDS * 1_000),
            ),
            (
                HarnessKind::Kimi,
                kimi_credentials(1_755_000_000),
                Some(1_755_000_000_000),
            ),
            (
                HarnessKind::Grok,
                grok_credentials(&["2026-08-17T02:19:01.724226598Z"]),
                None,
            ),
            (HarnessKind::Deepseek, b"version: 1\n".to_vec(), None),
        ];
        for kind in HarnessKind::ALL {
            let (_, bytes, expected) = fixtures
                .iter()
                .find(|(fixture, _, _)| *fixture == kind)
                .unwrap_or_else(|| panic!("{kind:?} needs a credential fixture"));
            assert_eq!(credential_expiry(kind, bytes), *expected, "{kind:?} expiry");
        }
    }

    #[test]
    fn codex_expiry_comes_from_the_access_token_rather_than_the_refresh_time() {
        // `last_refresh` orders two copies; only the token itself says when the
        // grant runs out, and the two are not the same number.
        let bytes = codex_credentials("2026-08-05T02:51:00.864587231Z");
        assert_eq!(
            credential_freshness(HarnessKind::Codex, &bytes),
            Some(1_785_898_260_864)
        );
        assert_eq!(
            credential_expiry(HarnessKind::Codex, &bytes),
            Some(CODEX_FIXTURE_EXPIRY_SECONDS * 1_000)
        );
    }

    #[test]
    fn unreadable_access_tokens_report_no_expiry() {
        for access_token in [
            serde_json::Value::from("not-a-jwt"),
            serde_json::Value::from("header.not-base64!!.signature"),
            serde_json::Value::from(format!("header.{}.signature", {
                use base64::Engine as _;
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"sub\":\"only\"}")
            })),
            serde_json::Value::Null,
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": { "access_token": access_token },
                "last_refresh": "2026-08-05T02:51:00.864587231Z",
            }))
            .unwrap();
            assert_eq!(credential_expiry(HarnessKind::Codex, &bytes), None);
        }
        assert_eq!(credential_expiry(HarnessKind::Codex, b"not json"), None);
        assert_eq!(credential_expiry(HarnessKind::Claude, b"{}"), None);
    }

    #[test]
    fn unparseable_credentials_report_no_freshness() {
        for kind in HarnessKind::ALL {
            assert_eq!(credential_freshness(kind, b"not json"), None);
            assert_eq!(credential_freshness(kind, b"{}"), None);
        }
        assert_eq!(
            credential_freshness(HarnessKind::Codex, &codex_credentials("yesterday")),
            None
        );
    }

    #[test]
    fn payload_validation_rejects_empty_oversized_and_malformed_documents() {
        assert!(validate_credential_payload(HarnessKind::Claude, b"").is_err());
        assert!(validate_credential_payload(HarnessKind::Claude, b"not json").is_err());
        assert!(
            validate_credential_payload(HarnessKind::Claude, &vec![b'a'; MAX_CREDENTIAL_BYTES + 1])
                .is_err()
        );
        assert!(validate_credential_payload(HarnessKind::Claude, &claude_credentials(1)).is_ok());
        assert!(
            validate_credential_payload(
                HarnessKind::Deepseek,
                b"version: 1\nrefs:\n  DEEPSEEK_API_KEY: secret\n"
            )
            .is_ok()
        );
        assert!(validate_credential_payload(HarnessKind::Deepseek, b"version: 2\n").is_err());
        for malformed in [
            "version: 1\nsecret: value\n",
            "version: 1\nrefs:\n  bad-name: secret\n",
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: ''\n",
            "version: 1\nrecords:\n  owner/model:\n    kind: mystery\n",
        ] {
            assert!(
                validate_credential_payload(HarnessKind::Deepseek, malformed.as_bytes()).is_err(),
                "accepted malformed DeepSeek credentials: {malformed}"
            );
        }
    }

    #[test]
    fn identical_or_absent_copies_need_no_sync() {
        assert_eq!(
            reconcile(&CredentialSnapshot::absent(), &CredentialSnapshot::absent()),
            SyncAction::None
        );
        assert_eq!(
            reconcile(&snapshot("a", Some(2)), &snapshot("a", Some(1))),
            SyncAction::None
        );
    }

    #[test]
    fn a_missing_side_takes_the_other_side_copy() {
        assert_eq!(
            reconcile(&snapshot("a", Some(1)), &CredentialSnapshot::absent()),
            SyncAction::Push
        );
        assert_eq!(
            reconcile(&CredentialSnapshot::absent(), &snapshot("b", Some(1))),
            SyncAction::Pull
        );
    }

    #[test]
    fn the_fresher_copy_wins_and_a_known_time_beats_an_unknown_one() {
        assert_eq!(
            reconcile(&snapshot("a", Some(2)), &snapshot("b", Some(1))),
            SyncAction::Push
        );
        assert_eq!(
            reconcile(&snapshot("a", Some(1)), &snapshot("b", Some(2))),
            SyncAction::Pull
        );
        assert_eq!(
            reconcile(&snapshot("a", Some(1)), &snapshot("b", None)),
            SyncAction::Push
        );
        assert_eq!(
            reconcile(&snapshot("a", None), &snapshot("b", Some(1))),
            SyncAction::Pull
        );
    }

    #[test]
    fn differing_copies_without_any_freshness_are_left_alone() {
        assert_eq!(
            reconcile(&snapshot("a", None), &snapshot("b", None)),
            SyncAction::None
        );
        assert_eq!(
            reconcile(&snapshot("a", Some(5)), &snapshot("b", Some(5))),
            SyncAction::None
        );
    }

    #[test]
    fn canonical_write_is_owner_only_and_replaces_the_previous_file() {
        let home = tempfile::tempdir().unwrap();
        let path = harness_authentication_marker(HarnessKind::Kimi, home.path());
        write_credential_file(HarnessKind::Kimi, &path, &kimi_credentials(1)).unwrap();
        write_credential_file(HarnessKind::Kimi, &path, &kimi_credentials(2)).unwrap();
        let (snapshot, bytes) = read_credential_file(HarnessKind::Kimi, &path).unwrap();
        assert!(snapshot.present);
        assert_eq!(bytes, kimi_credentials(2));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_write_refuses_a_symlinked_destination() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = home.path().join("elsewhere.json");
        std::fs::write(&elsewhere, b"{}").unwrap();
        let path = home.path().join("auth.json");
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        let error = write_credential_file(
            HarnessKind::Codex,
            &path,
            &codex_credentials("2026-01-01T00:00:00Z"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"{}");
    }

    #[test]
    fn a_missing_credential_file_reads_as_an_absent_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let path = harness_authentication_marker(HarnessKind::Codex, home.path());
        let (snapshot, bytes) = read_credential_file(HarnessKind::Codex, &path).unwrap();
        assert!(!snapshot.present);
        assert!(bytes.is_empty());
    }

    #[test]
    fn auth_failure_phrases_match_and_near_misses_do_not() {
        assert!(auth_failure_signature(
            HarnessKind::Claude,
            "Error: OAuth session expired and could not be refreshed"
        ));
        assert!(auth_failure_signature(
            HarnessKind::Codex,
            "{\"error\":{\"type\":\"authentication_error\"}}"
        ));
        assert!(auth_failure_signature(
            HarnessKind::Kimi,
            "invalid_grant: refresh token rejected"
        ));
        assert!(auth_failure_signature(
            HarnessKind::Kimi,
            "OAuthUnauthorizedError: The provided authorization grant is invalid"
        ));
        assert!(auth_failure_signature(
            HarnessKind::Codex,
            "error=INVALID_GRANT"
        ));
        assert!(!auth_failure_signature(
            HarnessKind::Claude,
            "the OAuth session expired last week, but we refreshed it"
        ));
        assert!(!auth_failure_signature(
            HarnessKind::Claude,
            "authentication succeeded"
        ));
        assert!(!auth_failure_signature(
            HarnessKind::Codex,
            "docs_src/authentication_error_status_code/tutorial001_an_py310.py"
        ));
        assert!(!auth_failure_signature(
            HarnessKind::Codex,
            "someauthentication_error"
        ));
        assert!(!auth_failure_signature(
            HarnessKind::Codex,
            "invalid_grant_result"
        ));
    }

    #[test]
    fn only_harness_observations_request_credential_sync() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallUpdate,
            ToolCallUpdateFields,
        };

        let event = |observation| RelayEvent {
            format: crate::hel_worker::RELAY_EVENT_FORMAT_V1,
            ordinal: 1,
            previous_digest: crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into(),
            digest: "a".repeat(64),
            recorded_at_ms: 1,
            command_id: None,
            observation,
        };
        assert!(events_report_auth_failure(
            HarnessKind::Claude,
            &[event(RelayObservation::Warning {
                message: "OAuth session expired and could not be refreshed".into(),
            })]
        ));
        assert!(events_report_auth_failure(
            HarnessKind::Claude,
            &[event(RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from("Please run /login to continue"),
                ))),
            })]
        ));
        assert!(events_report_auth_failure(
            HarnessKind::Codex,
            &[event(RelayObservation::Warning {
                message: format!("{}: codex", crate::hel_acp::PROMPT_AUTH_REQUIRED_MARKER),
            })]
        ));

        let observed_false_positive =
            "docs_src/authentication_error_status_code/tutorial001_an_py310.py";
        assert!(!events_report_auth_failure(
            HarnessKind::Codex,
            &[
                event(RelayObservation::SessionUpdate {
                    update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        "call-1",
                        ToolCallUpdateFields::new().title(observed_false_positive),
                    ))),
                }),
                event(RelayObservation::SessionUpdate {
                    update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                        "call-2",
                        observed_false_positive,
                    ))),
                }),
                event(RelayObservation::SessionUpdate {
                    update: Box::new(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                        ContentBlock::from(observed_false_positive),
                    ))),
                }),
                event(RelayObservation::SessionUpdate {
                    update: Box::new(SessionUpdate::UserMessageChunk(ContentChunk::new(
                        ContentBlock::from("explain authentication_error"),
                    ))),
                }),
                event(RelayObservation::TerminalOutput {
                    terminal_id: "terminal-1".into(),
                    output: observed_false_positive.into(),
                    truncated: false,
                    exit_code: Some(0),
                    signal: None,
                }),
            ]
        ));
        assert!(!events_report_auth_failure(
            HarnessKind::Claude,
            &[event(RelayObservation::CommandInterrupted {
                command_id: "command-1".into(),
                command: crate::hel_worker::RelayCommandKind::Prompt,
                message: "invalid_grant".into(),
            })]
        ));
        assert!(!events_report_auth_failure(
            HarnessKind::Claude,
            &[event(RelayObservation::CommandQueued {
                command_id: "command-1".into(),
                command: crate::hel_worker::RelayCommand::Prompt {
                    prompt: vec![ContentBlock::from("explain invalid_grant")],
                },
                created_at_ms: 1,
            })]
        ));
        assert_eq!(
            relay_event_credential_sync_reason(&event(RelayObservation::Warning {
                message: crate::hel_acp::PROMPT_EMPTY_RESPONSE_MARKER.into(),
            })),
            Some(CredentialSyncReason::EmptyPromptResponse)
        );
    }

    #[test]
    fn login_commands_match_each_harness_cli() {
        let profile = |kind: HarnessKind, executable: Option<&str>| HarnessProfile {
            kind,
            home: PathBuf::from("/home/user/.config"),
            executable: executable.map(PathBuf::from),
            environment: Default::default(),
            context_window_bytes: None,
        };
        assert_eq!(
            login_command(&profile(HarnessKind::Codex, None)),
            ("codex".to_owned(), vec!["login".to_owned()])
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Claude, None)),
            (
                "claude".to_owned(),
                vec!["auth".to_owned(), "login".to_owned()]
            )
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Kimi, None)),
            ("kimi".to_owned(), vec!["login".to_owned()])
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Grok, None)),
            ("grok".to_owned(), vec!["login".to_owned()])
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Deepseek, None)),
            ("dsh".to_owned(), vec!["web".to_owned()])
        );
    }

    #[test]
    fn only_a_cli_bridge_executable_override_names_the_harness_cli() {
        let profile = |kind: HarnessKind| HarnessProfile {
            kind,
            home: PathBuf::from("/home/user/.config"),
            executable: Some(PathBuf::from("/opt/bin/custom")),
            environment: Default::default(),
            context_window_bytes: None,
        };
        assert_eq!(
            login_command(&profile(HarnessKind::Kimi)),
            ("/opt/bin/custom".to_owned(), vec!["login".to_owned()])
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Grok)),
            ("/opt/bin/custom".to_owned(), vec!["login".to_owned()])
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Codex)).0,
            "codex".to_owned()
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Claude)).0,
            "claude".to_owned()
        );
        assert_eq!(
            login_command(&profile(HarnessKind::Deepseek)).0,
            "dsh".to_owned()
        );
    }

    #[test]
    fn a_queued_periodic_sync_is_not_queued_twice() {
        let mut queue = VecDeque::new();
        enqueue(
            &mut queue,
            SyncTrigger {
                profile_id: "work".into(),
                cause: None,
            },
        );
        enqueue(
            &mut queue,
            SyncTrigger {
                profile_id: "work".into(),
                cause: None,
            },
        );
        enqueue(
            &mut queue,
            SyncTrigger {
                profile_id: "work".into(),
                cause: Some(CredentialSyncCause {
                    session_id: "session".into(),
                    reason: CredentialSyncReason::EmptyPromptResponse,
                }),
            },
        );
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue[1]
                .cause
                .as_ref()
                .map(|cause| cause.session_id.as_str()),
            Some("session")
        );
    }

    #[cfg(unix)]
    #[test]
    fn github_tokens_are_validated_fingerprinted_and_removed_safely() {
        use std::os::unix::fs::PermissionsExt;

        assert!(validate_github_token(b"").is_err());
        assert!(validate_github_token(b"contains whitespace").is_err());
        assert!(validate_github_token(&vec![b'x'; MAX_GITHUB_TOKEN_BYTES + 1]).is_err());

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("github-token");
        let installed = write_github_token(&path, b"controller-token").unwrap();
        assert_eq!(installed, GithubTokenSnapshot::of("controller-token"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (state, token) = read_github_token(&path).unwrap();
        assert_eq!(state, installed);
        assert_eq!(token.as_deref(), Some("controller-token"));

        remove_github_token(&path).unwrap();
        remove_github_token(&path).unwrap();
        assert_eq!(
            read_github_token(&path).unwrap().0,
            GithubTokenSnapshot::absent()
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_tokens_round_trip_through_an_owner_only_profile_directory() {
        use std::os::unix::fs::PermissionsExt;

        assert!(validate_claude_oauth_token(b"").is_err());
        assert!(validate_claude_oauth_token(b"contains whitespace").is_err());
        assert!(
            validate_claude_oauth_token(&vec![b'x'; MAX_CLAUDE_OAUTH_TOKEN_BYTES + 1]).is_err()
        );

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("profiles/claude");
        let path = directory.join("claude-oauth-token");
        assert_eq!(read_claude_oauth_token(&path).unwrap(), None);

        write_claude_oauth_token(&path, b"sk-ant-oat01-example\n").unwrap();

        assert_eq!(
            read_claude_oauth_token(&path).unwrap().as_deref(),
            Some("sk-ant-oat01-example")
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // Rotating the token replaces the file in place.
        write_claude_oauth_token(&path, b"sk-ant-oat01-second").unwrap();
        assert_eq!(
            read_claude_oauth_token(&path).unwrap().as_deref(),
            Some("sk-ant-oat01-second")
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_token_reads_and_writes_refuse_symlink_destinations() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::write(&elsewhere, b"keep").unwrap();
        let path = root.path().join("claude-oauth-token");
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

        assert!(write_claude_oauth_token(&path, b"sk-ant-oat01-example").is_err());
        assert!(read_claude_oauth_token(&path).is_err());
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
    }

    #[test]
    fn a_profile_setup_token_lives_beside_the_configuration_not_in_the_profile_home() {
        let path = claude_oauth_token_path("claude-max");
        assert!(path.ends_with("profiles/claude-max/claude-oauth-token"));
        assert!(path.starts_with(crate::hel_config::config_dir()));
    }

    #[cfg(unix)]
    #[test]
    fn github_token_install_and_remove_refuse_symlink_destinations() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::write(&elsewhere, b"keep").unwrap();
        let path = root.path().join("github-token");
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

        assert!(write_github_token(&path, b"controller-token").is_err());
        assert!(remove_github_token(&path).is_err());
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
    }
}
