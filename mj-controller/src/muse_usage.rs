//! Muse subscription usage queried from the native mint endpoint.
//!
//! Muse keeps the access token in each profile's `auth.json`. The endpoint is
//! intentionally queried directly here so each profile stays isolated and no
//! minted API key is persisted or passed through the rest of the controller.
//!
//! The daemon keeps that login fresh itself, as it does for Kimi: a token near
//! its expiry, or one the endpoint rejects, is refreshed under the lock Muse
//! takes for `auth.json`, so the quota does not depend on Muse having run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use mj_core::credentials::MAX_CREDENTIAL_BYTES;

const MINT_BASE_URL_ENV: &str = "TBH_MINT_BASE_URL";
const DEFAULT_MINT_BASE_URL: &str = "https://api.meta.ai";
const MINT_PATH: &str = "/muse-code/key";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Muse's own override for its OAuth host, honoured the same way as the mint
/// override so a profile's environment points both at one deployment.
const AUTH_BASE_URL_ENV: &str = "TBH_AUTH_BASE_URL";
const DEFAULT_AUTH_BASE_URL: &str = "https://auth.meta.com";
/// The token endpoint and public client id `muse login` uses (observed on the
/// wire from Muse 1.4.12). A refresh is the standard `refresh_token` grant
/// against the same endpoint.
const TOKEN_PATH: &str = "/oidc/device/token/";
const OAUTH_CLIENT_ID: &str = "1031625952748946";
/// Muse serializes every `auth.json` write with an exclusive lock on this
/// file, and gives up after waiting 5 seconds for it.
const AUTH_LOCK_FILE: &str = ".auth.json.lock";
const AUTH_LOCK_WAIT: Duration = Duration::from_secs(10);
const AUTH_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Refresh this long before the recorded expiry. It spans two quota polls, so
/// the credential sync hands the new login to running sessions before their
/// copies run out and have to spend the same refresh token themselves.
const REFRESH_AHEAD: Duration = Duration::from_secs(20 * 60);
/// The dashboard's marker for a rejected login; it renders this text as is.
const LOGIN_EXPIRED: &str = "login expired";

/// The mint endpoint is the only source of Muse usage and it is rate limited
/// per account, so every caller in this process shares one reading rather than
/// minting a key of its own.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// First wait after a 429 that carries no `Retry-After`.
const INITIAL_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(30 * 60);
const MAX_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(6 * 60 * 60);
/// Prefix that lets the dashboard render a rate-limited profile honestly
/// instead of calling it unavailable.
pub const RATE_LIMITED_PREFIX: &str = "rate limited";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuseUsageWindow {
    pub label: String,
    pub remaining_percent: u8,
    pub duration_seconds: Option<i64>,
    pub resets_at: Option<i64>,
}

/// One usage reading, plus a short note when the windows are the last good
/// reading served while Meta is rate limiting this account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuseUsageReport {
    pub windows: Vec<MuseUsageWindow>,
    pub note: Option<String>,
}

/// A 429 from the mint endpoint, with whatever wait it asked for.
#[derive(Debug)]
struct RateLimited {
    retry_after: Option<Duration>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Muse usage request returned HTTP 429 Too Many Requests")
    }
}

impl std::error::Error for RateLimited {}

#[derive(Debug, Default)]
struct GateEntry {
    windows: Option<Vec<MuseUsageWindow>>,
    fetched_at: Option<SystemTime>,
    next_allowed_at: Option<SystemTime>,
    consecutive_rate_limits: u32,
    rate_limit_reason: Option<String>,
}

type Gate = Mutex<HashMap<PathBuf, GateEntry>>;

fn shared_gate() -> &'static Gate {
    static GATE: OnceLock<Gate> = OnceLock::new();
    GATE.get_or_init(Gate::default)
}

/// Query the Muse subscription usage for one profile.
pub async fn query(home: &Path, environment: &HashMap<String, String>) -> Result<MuseUsageReport> {
    query_gated(
        home,
        &Endpoints::of(environment),
        REQUEST_TIMEOUT,
        shared_gate(),
        SystemTime::now(),
    )
    .await
}

/// Where one profile's Muse reaches Meta: the mint endpoint that reports
/// usage, and the OAuth host that refreshes the login.
struct Endpoints {
    mint: String,
    auth: String,
}

impl Endpoints {
    fn of(environment: &HashMap<String, String>) -> Self {
        let configured = |name: &str, default: &str| {
            environment
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_else(|| default.to_owned())
        };
        Self {
            mint: configured(MINT_BASE_URL_ENV, DEFAULT_MINT_BASE_URL),
            auth: configured(AUTH_BASE_URL_ENV, DEFAULT_AUTH_BASE_URL),
        }
    }
}

/// The rate-limit and minimum-interval gate above the network step. `now` is
/// supplied by the caller so tests can drive the clock without sleeping.
async fn query_gated(
    home: &Path,
    endpoints: &Endpoints,
    timeout: Duration,
    gate: &Gate,
    now: SystemTime,
) -> Result<MuseUsageReport> {
    let key = gate_key(home);
    if let Some(report) = cached_report(gate, &key, now)? {
        return Ok(report);
    }
    match query_with_timeout(home, endpoints, timeout).await {
        Ok(windows) => {
            let mut entries = gate.lock().unwrap_or_else(|error| error.into_inner());
            let entry = entries.entry(key).or_default();
            entry.windows = Some(windows.clone());
            entry.fetched_at = Some(now);
            entry.next_allowed_at = None;
            entry.consecutive_rate_limits = 0;
            entry.rate_limit_reason = None;
            Ok(MuseUsageReport {
                windows,
                note: None,
            })
        }
        Err(error) => {
            let Some(rate_limited) = error.downcast_ref::<RateLimited>() else {
                // A transient failure neither clears the last reading nor
                // blocks the next attempt.
                return Err(error);
            };
            let mut entries = gate.lock().unwrap_or_else(|error| error.into_inner());
            let entry = entries.entry(key).or_default();
            let backoff = rate_limited
                .retry_after
                .unwrap_or_else(|| exponential_backoff(entry.consecutive_rate_limits));
            let next_allowed_at = now + backoff;
            entry.consecutive_rate_limits = entry.consecutive_rate_limits.saturating_add(1);
            entry.next_allowed_at = Some(next_allowed_at);
            entry.rate_limit_reason = Some(rate_limit_reason(next_allowed_at));
            match entry.windows.clone() {
                Some(windows) => Ok(MuseUsageReport {
                    windows,
                    note: Some(RATE_LIMITED_PREFIX.to_owned()),
                }),
                None => Err(anyhow::anyhow!(
                    entry.rate_limit_reason.clone().unwrap_or_default()
                )),
            }
        }
    }
}

/// The cached answer, when the gate says no network call is due. `Ok(None)`
/// means the caller should query the endpoint.
fn cached_report(gate: &Gate, key: &Path, now: SystemTime) -> Result<Option<MuseUsageReport>> {
    let entries = gate.lock().unwrap_or_else(|error| error.into_inner());
    let Some(entry) = entries.get(key) else {
        return Ok(None);
    };
    if entry.next_allowed_at.is_some_and(|allowed| now < allowed) {
        return match entry.windows.clone() {
            Some(windows) => Ok(Some(MuseUsageReport {
                windows,
                note: Some(RATE_LIMITED_PREFIX.to_owned()),
            })),
            None => Err(anyhow::anyhow!(
                entry.rate_limit_reason.clone().unwrap_or_default()
            )),
        };
    }
    let fresh = entry
        .fetched_at
        .and_then(|fetched_at| now.duration_since(fetched_at).ok())
        .is_some_and(|age| age < MIN_REFRESH_INTERVAL);
    match entry.windows.clone().filter(|_| fresh) {
        Some(windows) => Ok(Some(MuseUsageReport {
            windows,
            note: None,
        })),
        None => Ok(None),
    }
}

fn gate_key(home: &Path) -> PathBuf {
    home.canonicalize().unwrap_or_else(|_| home.to_path_buf())
}

fn exponential_backoff(consecutive_rate_limits: u32) -> Duration {
    INITIAL_RATE_LIMIT_BACKOFF
        .saturating_mul(1u32 << consecutive_rate_limits.min(8))
        .min(MAX_RATE_LIMIT_BACKOFF)
}

fn rate_limit_reason(next_allowed_at: SystemTime) -> String {
    let next_check = chrono::DateTime::<chrono::Local>::from(next_allowed_at).format("%H:%M");
    format!("{RATE_LIMITED_PREFIX} by Meta; next check after {next_check}")
}

/// `Retry-After` as either a delay in seconds or an HTTP date.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let now = chrono::DateTime::<chrono::Utc>::from(now);
    (at.with_timezone(&chrono::Utc) - now).to_std().ok()
}

async fn query_with_timeout(
    home: &Path,
    endpoints: &Endpoints,
    timeout: Duration,
) -> Result<Vec<MuseUsageWindow>> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build Muse usage client")?;
    let token = fresh_access_token(&client, home, &endpoints.auth, None).await?;
    match mint(&client, &endpoints.mint, &token).await {
        // The endpoint, not the recorded expiry, is the judge of a token.
        Err(error) if error.is::<LoginRejected>() => {
            let refreshed =
                fresh_access_token(&client, home, &endpoints.auth, Some(&token)).await?;
            mint(&client, &endpoints.mint, &refreshed).await
        }
        result => result,
    }
    .map_err(|error| {
        if error.is::<LoginRejected>() {
            anyhow::anyhow!(LOGIN_EXPIRED)
        } else {
            error
        }
    })
}

/// Meta refused the login: the mint endpoint rejected the access token, or
/// the OAuth host rejected the refresh token.
#[derive(Debug)]
struct LoginRejected;

impl std::fmt::Display for LoginRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(LOGIN_EXPIRED)
    }
}

impl std::error::Error for LoginRejected {}

async fn mint(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<MuseUsageWindow>> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), MINT_PATH);
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&json!({ "onboard": false }))
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Muse usage request timed out")
            } else {
                // Do not include reqwest's error: it can expose request
                // details when a malformed configured endpoint is involved.
                anyhow::anyhow!("Muse usage request failed")
            }
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(anyhow::Error::new(LoginRejected));
    }
    if status.as_u16() == 429 {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, SystemTime::now()));
        return Err(anyhow::Error::new(RateLimited { retry_after }));
    }
    if !status.is_success() {
        bail!("Muse usage request returned HTTP {status}")
    }
    let body = read_bounded_body(response).await?;
    let payload: Value = serde_json::from_slice(&body).context("decode Muse usage response")?;
    parse(&payload)
}

/// The Meta login in a profile's `auth.json`, with the document it came from
/// so a refresh rewrites only the token fields.
struct MuseLogin {
    document: Value,
    access_token: String,
    refresh_token: Option<String>,
    /// Epoch seconds; absent for a stored API key.
    expires_at: Option<i64>,
}

impl MuseLogin {
    fn refresh_due(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|expires_at| {
            expires_at.saturating_sub(now) < REFRESH_AHEAD.as_secs() as i64
        })
    }
}

/// Hand back an access token to query with, refreshing the stored login when
/// it is near expiry or when `rejected` names the token the endpoint just
/// refused. The refresh runs under Muse's own `auth.json` lock and re-reads
/// the login once the lock is held, so a refresh Muse finished meanwhile is
/// used instead of spending its refresh token a second time.
async fn fresh_access_token(
    client: &reqwest::Client,
    home: &Path,
    auth_base_url: &str,
    rejected: Option<&str>,
) -> Result<String> {
    let initial = read_login(home).await?;
    let now = mj_core::clock::epoch_seconds() as i64;
    if rejected.is_none() && !initial.refresh_due(now) {
        return Ok(initial.access_token);
    }
    if initial.refresh_token.is_none() {
        // A stored API key, or a login without a refresh grant: nothing here
        // can renew it.
        return match rejected {
            Some(_) => Err(anyhow::Error::new(LoginRejected)),
            None => Ok(initial.access_token),
        };
    }

    // Held until this function returns, released on drop.
    let _lock = acquire_auth_lock(home).await?;
    let active = read_login(home).await?;
    let renewed_meanwhile = match rejected {
        Some(rejected) => active.access_token != rejected,
        None => !active.refresh_due(now),
    };
    if renewed_meanwhile {
        return Ok(active.access_token);
    }
    let Some(refresh_token) = active.refresh_token.as_deref() else {
        return Err(anyhow::Error::new(LoginRejected));
    };
    let grant = match refresh_grant(client, auth_base_url, refresh_token).await {
        Ok(grant) => grant,
        // Ahead of expiry the current token still works; the failure is
        // logged and the refresh is tried again on the next poll.
        Err(error) if rejected.is_none() && !error.is::<LoginRejected>() => {
            tracing::warn!(home = %home.display(), %error, "could not refresh the Muse login ahead of expiry");
            return Ok(active.access_token);
        }
        Err(error) => return Err(error),
    };
    save_login(home, active.document, &grant, now)?;
    Ok(grant.access_token)
}

struct TokenGrant {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

async fn refresh_grant(
    client: &reqwest::Client,
    auth_base_url: &str,
    refresh_token: &str,
) -> Result<TokenGrant> {
    let url = format!("{}{}", auth_base_url.trim_end_matches('/'), TOKEN_PATH);
    let response = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OAUTH_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Muse login refresh timed out")
            } else {
                anyhow::anyhow!("Muse login refresh failed")
            }
        })?;
    let status = response.status();
    let body = read_bounded_body(response).await?;
    let payload: Option<Value> = serde_json::from_slice(&body).ok();
    if !status.is_success() {
        let code = payload
            .as_ref()
            .and_then(|payload| payload.get("error"))
            .and_then(Value::as_str);
        if code == Some("invalid_grant") {
            return Err(anyhow::Error::new(LoginRejected));
        }
        // Anything else is reported as it is: it may mean the request itself
        // is wrong, which "login expired" would hide.
        match code {
            Some(code) => bail!("Muse login refresh returned HTTP {status} ({code})"),
            None => bail!("Muse login refresh returned HTTP {status}"),
        }
    }
    let payload = payload.context("Muse login refresh returned invalid JSON")?;
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty() && !token.chars().any(char::is_whitespace))
        .context("Muse login refresh returned no access token")?;
    Ok(TokenGrant {
        access_token: access_token.to_owned(),
        refresh_token: payload
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned),
        expires_in: payload
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0),
    })
}

/// Write the refreshed grant into `auth.json`, keeping every other field Muse
/// stored. The caller holds Muse's lock.
fn save_login(home: &Path, mut document: Value, grant: &TokenGrant, now: i64) -> Result<()> {
    let meta = document
        .pointer_mut("/providers/meta")
        .and_then(Value::as_object_mut)
        .context("Muse credentials have no Meta login")?;
    meta.insert("access_token".into(), grant.access_token.clone().into());
    if let Some(refresh_token) = &grant.refresh_token {
        meta.insert("refresh_token".into(), refresh_token.clone().into());
    }
    match grant.expires_in {
        Some(expires_in) => {
            meta.insert("expires_at".into(), now.saturating_add(expires_in).into());
        }
        // A stale expiry would ask for a refresh on every poll.
        None => {
            meta.remove("expires_at");
        }
    }
    let mut body = serde_json::to_vec_pretty(&document)?;
    body.push(b'\n');
    mj_core::config::atomic_write(&home.join("auth.json"), &body)
        .context("save the refreshed Muse login")
}

/// Take the exclusive lock Muse holds while it writes `auth.json`.
async fn acquire_auth_lock(home: &Path) -> Result<mj_core::file_lock::FileLock> {
    let path = home.join(AUTH_LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open the Muse login lock {}", path.display()))?;
    let deadline = tokio::time::Instant::now() + AUTH_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(mj_core::file_lock::FileLock::new(file)),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(AUTH_LOCK_RETRY_INTERVAL).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "another Muse process held the login lock {} for {} seconds",
                path.display(),
                AUTH_LOCK_WAIT.as_secs()
            ),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("lock the Muse login {}", path.display()));
            }
        }
    }
}

async fn read_login(home: &Path) -> Result<MuseLogin> {
    let path = home.join("auth.json");
    let file = tokio::fs::File::open(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("Muse login required: auth.json is missing; sign in to Muse")
        } else {
            anyhow::anyhow!("Muse credentials are unavailable")
        }
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CREDENTIAL_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| anyhow::anyhow!("Muse credentials are unavailable"))?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        bail!("Muse credentials exceed the {MAX_CREDENTIAL_BYTES} byte limit")
    }
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Muse credentials are invalid; sign in to Muse"))?;
    let meta = document.pointer("/providers/meta");
    let field = |name: &str| {
        meta.and_then(|meta| meta.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let access_token = field("access_token")
        .ok_or_else(|| anyhow::anyhow!("Muse access token is missing; sign in to Muse"))?;
    if access_token.chars().any(char::is_whitespace) {
        bail!("Muse access token is invalid; sign in to Muse")
    }
    Ok(MuseLogin {
        access_token: access_token.to_owned(),
        refresh_token: field("refresh_token").map(str::to_owned),
        expires_at: meta
            .and_then(|meta| meta.get("expires_at"))
            .and_then(Value::as_i64),
        document: document.clone(),
    })
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Muse usage request timed out")
            } else {
                anyhow::anyhow!("read Muse usage response")
            }
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_CREDENTIAL_BYTES {
            bail!("Muse usage response exceeds the {MAX_CREDENTIAL_BYTES} byte limit")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Parse the native endpoint's subscription usage object.
///
/// A window is shown only when the endpoint supplied the fields needed to
/// describe it. In particular, absent usage never becomes a fabricated 100%
/// allowance. Invalid percentages are rejected so corrupt data cannot look
/// like a healthy quota.
fn parse(payload: &Value) -> Result<Vec<MuseUsageWindow>> {
    let Some(subs_usage_value) = payload.get("subs_usage") else {
        bail!("Muse subscription usage is unavailable")
    };
    let Some(subs_usage) = subs_usage_value.as_object() else {
        if subs_usage_value.is_null() {
            bail!("Muse subscription usage is unavailable")
        }
        bail!("Muse subscription usage is malformed")
    };
    let mut windows = Vec::new();
    if let Some(weekly) = optional_window(subs_usage, "weekly")? {
        let remaining_percent = parse_remaining_percent(weekly.get("used_percent"))?;
        windows.push(MuseUsageWindow {
            label: "Week".to_owned(),
            remaining_percent,
            duration_seconds: Some(7 * 24 * 60 * 60),
            resets_at: weekly.get("resets_at").and_then(Value::as_i64),
        });
    }
    if let Some(window) = optional_window(subs_usage, "window")? {
        let remaining_percent = parse_remaining_percent(window.get("used_percent"))?;
        let duration = parse_duration(window.get("window_duration_mins"))?;
        windows.push(MuseUsageWindow {
            label: window_label(duration),
            remaining_percent,
            duration_seconds: duration.checked_mul(60),
            resets_at: window.get("resets_at").and_then(Value::as_i64),
        });
    }
    if windows.is_empty() {
        bail!("Muse subscription usage is unavailable")
    }
    Ok(windows)
}

fn optional_window<'a>(
    subs_usage: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a serde_json::Map<String, Value>>> {
    match subs_usage.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(window)) => Ok(Some(window)),
        Some(_) => bail!("Muse {key} usage window is malformed"),
    }
}

fn parse_duration(value: Option<&Value>) -> Result<i64> {
    let value = value
        .filter(|value| !value.is_null())
        .context("Muse usage window duration is missing")?;
    let Some(duration) = value.as_i64() else {
        bail!("Muse usage returned an invalid window duration")
    };
    if duration <= 0 {
        bail!("Muse usage returned an invalid window duration")
    }
    Ok(duration)
}

fn parse_remaining_percent(value: Option<&Value>) -> Result<u8> {
    let value = value
        .filter(|value| !value.is_null())
        .context("Muse usage percentage is missing")?;
    let Some(used_percent) = value.as_f64() else {
        bail!("Muse usage returned an invalid percentage")
    };
    if !used_percent.is_finite() || used_percent < 0.0 {
        bail!("Muse usage returned an invalid percentage")
    }
    Ok((100.0 - used_percent.min(100.0)).round() as u8)
}

fn window_label(minutes: i64) -> String {
    match minutes {
        300 => "5H".to_owned(),
        value if value % 1_440 == 0 => format!("{}d", value / 1_440),
        value if value % 60 == 0 => format!("{}H", value / 60),
        value => format!("{value}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode};
    use axum::routing::post;
    use axum::{Router, response::Response};
    use std::sync::{Arc, Mutex};
    use tokio::time::sleep;

    #[derive(Clone, Default)]
    struct ServerState {
        requests: Arc<Mutex<Vec<(Method, String, String)>>>,
        response: Arc<Mutex<(StatusCode, String)>>,
        delay: Option<Duration>,
    }

    async fn usage_handler(
        State(state): State<ServerState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response<String> {
        if let Some(delay) = state.delay {
            sleep(delay).await;
        }
        let authorization = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        state.requests.lock().unwrap().push((
            Method::POST,
            authorization,
            String::from_utf8(body.to_vec()).unwrap(),
        ));
        let (status, body) = state.response.lock().unwrap().clone();
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    async fn spawn_server(state: ServerState) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/muse-code/key", post(usage_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    fn endpoints(base_url: &str) -> Endpoints {
        Endpoints {
            mint: base_url.to_owned(),
            auth: base_url.to_owned(),
        }
    }

    fn write_credentials(home: &Path, token: &str) {
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&json!({
                "providers": { "meta": { "access_token": token } }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn preserves_optional_windows_and_resets_but_rejects_incomplete_usage() {
        let payload = json!({
            "subs_usage": {
                "weekly": {"used_percent": 25},
                "window": null
            }
        });
        let windows = parse(&payload).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Week");
        assert_eq!(windows[0].remaining_percent, 75);
        assert_eq!(windows[0].resets_at, None);
        assert_eq!(
            parse(&json!({})).unwrap_err().to_string(),
            "Muse subscription usage is unavailable"
        );
        for window in [
            json!({}),
            json!({"used_percent": 1}),
            json!({"window_duration_mins": 300}),
        ] {
            assert!(
                parse(&json!({"subs_usage": {
                    "weekly": {"used_percent": 25}, "window": window
                }}))
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn rejects_missing_invalid_and_oversized_credentials() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            read_login(home.path())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("login required")
        );
        for bytes in [
            b"not-json".to_vec(),
            b"{}".to_vec(),
            vec![b' '; MAX_CREDENTIAL_BYTES + 1],
        ] {
            std::fs::write(home.path().join("auth.json"), bytes).unwrap();
            assert!(read_login(home.path()).await.is_err());
        }
    }

    #[tokio::test]
    async fn posts_profile_token_and_native_body_without_persisting_response() {
        let state = ServerState {
            response: Arc::new(Mutex::new((
                StatusCode::OK,
                json!({
                    "subs_usage": {
                        "weekly": {"used_percent": 1, "resets_at": 1_789_344_000_i64}
                    }
                })
                .to_string(),
            ))),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(state.clone()).await;
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_credentials(first.path(), "first-token");
        write_credentials(second.path(), "second-token");
        let first_environment = HashMap::from([(MINT_BASE_URL_ENV.to_owned(), base_url.clone())]);
        let second_environment = HashMap::from([(MINT_BASE_URL_ENV.to_owned(), base_url)]);

        query(first.path(), &first_environment).await.unwrap();
        query(second.path(), &second_environment).await.unwrap();
        assert!(
            std::fs::read_to_string(first.path().join("auth.json"))
                .unwrap()
                .contains("first-token")
        );
        assert!(
            std::fs::read_to_string(second.path().join("auth.json"))
                .unwrap()
                .contains("second-token")
        );
        {
            let requests = state.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].0, Method::POST);
            assert_eq!(requests[0].1, "Bearer first-token");
            assert_eq!(requests[1].1, "Bearer second-token");
            assert_eq!(requests[0].2, r#"{"onboard":false}"#);
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    fn usage_body() -> String {
        json!({
            "subs_usage": {"weekly": {"used_percent": 1, "resets_at": 1_789_344_000_i64}}
        })
        .to_string()
    }

    // Hard-won: fd350b04: a live Muse 429 outage was amplified by duplicate key-mint calls and the dashboard discarded its last good reading.
    #[tokio::test]
    async fn gate_serves_last_reading_while_rate_limited_and_dedupes_refreshes() {
        let home = tempfile::tempdir().unwrap();
        write_credentials(home.path(), "profile-token");
        let state = ServerState {
            response: Arc::new(Mutex::new((StatusCode::OK, usage_body()))),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(state.clone()).await;
        let gate = Gate::default();
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let report = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            start,
        )
        .await
        .unwrap();
        assert_eq!(report.windows[0].remaining_percent, 99);
        assert_eq!(report.note, None);

        // A second caller inside the minimum interval reuses the reading.
        let report = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            start + Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(report.note, None);
        assert_eq!(state.requests.lock().unwrap().len(), 1);

        // After the interval a 429 keeps the last windows and marks the note.
        *state.response.lock().unwrap() = (StatusCode::TOO_MANY_REQUESTS, "{}".into());
        let limited = start + MIN_REFRESH_INTERVAL + Duration::from_secs(1);
        let report = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            limited,
        )
        .await
        .unwrap();
        assert_eq!(report.windows[0].remaining_percent, 99);
        assert_eq!(report.note.as_deref(), Some("rate limited"));
        assert_eq!(state.requests.lock().unwrap().len(), 2);

        // No further network call until the backoff elapses.
        let report = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            limited + INITIAL_RATE_LIMIT_BACKOFF - Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(report.note.as_deref(), Some("rate limited"));
        assert_eq!(state.requests.lock().unwrap().len(), 2);

        // Once it elapses the endpoint is tried again, and a success clears
        // both the note and the consecutive-429 count.
        *state.response.lock().unwrap() = (StatusCode::OK, usage_body());
        let recovered = limited + INITIAL_RATE_LIMIT_BACKOFF;
        let report = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            recovered,
        )
        .await
        .unwrap();
        assert_eq!(report.note, None);
        assert_eq!(state.requests.lock().unwrap().len(), 3);
        assert_eq!(
            gate.lock().unwrap()[&gate_key(home.path())].consecutive_rate_limits,
            0
        );

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: fd350b04: the live Muse 429 case without cached usage was shown as unavailable and retried on every poll.
    #[tokio::test]
    async fn rate_limit_without_a_prior_reading_reports_the_rate_limited_marker() {
        let home = tempfile::tempdir().unwrap();
        write_credentials(home.path(), "profile-token");
        let state = ServerState {
            response: Arc::new(Mutex::new((StatusCode::TOO_MANY_REQUESTS, "{}".into()))),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(state.clone()).await;
        let gate = Gate::default();
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let error = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            start,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("rate limited"), "{error}");
        let error = query_gated(
            home.path(),
            &endpoints(&base_url),
            REQUEST_TIMEOUT,
            &gate,
            start + Duration::from_secs(60),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("rate limited"), "{error}");
        assert_eq!(state.requests.lock().unwrap().len(), 1);

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: fd350b04: Muse’s live rate limit required honoring Retry-After to stop repeated key-mint requests.
    #[tokio::test]
    async fn retry_after_seconds_set_the_next_allowed_check() {
        let home = tempfile::tempdir().unwrap();
        write_credentials(home.path(), "profile-token");
        let app = Router::new().route(
            "/muse-code/key",
            post(|| async {
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("retry-after", "120")
                    .body("{}".to_owned())
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base_url = format!("http://{address}");
        let gate = Gate::default();
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        assert!(
            query_gated(
                home.path(),
                &endpoints(&base_url),
                REQUEST_TIMEOUT,
                &gate,
                start
            )
            .await
            .is_err()
        );
        assert_eq!(
            gate.lock().unwrap()[&gate_key(home.path())].next_allowed_at,
            Some(start + Duration::from_secs(120))
        );

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: fd350b04: the live Muse 429 fix needed bounded clock-based backoff and Retry-After date/seconds handling.
    #[test]
    fn retry_after_accepts_seconds_and_http_dates_and_backoff_is_capped() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(parse_retry_after("30", now), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_retry_after("Tue, 14 Nov 2023 22:23:20 GMT", now),
            Some(Duration::from_secs(600))
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(exponential_backoff(0), INITIAL_RATE_LIMIT_BACKOFF);
        assert_eq!(exponential_backoff(1), INITIAL_RATE_LIMIT_BACKOFF * 2);
        assert_eq!(exponential_backoff(9), MAX_RATE_LIMIT_BACKOFF);
    }

    #[tokio::test]
    async fn returns_auth_status_and_timeout_errors_without_response_details() {
        let home = tempfile::tempdir().unwrap();
        write_credentials(home.path(), "secret-token");
        let state = ServerState {
            response: Arc::new(Mutex::new((
                StatusCode::UNAUTHORIZED,
                "secret-payload".into(),
            ))),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(state).await;
        let error = query_with_timeout(home.path(), &endpoints(&base_url), Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, LOGIN_EXPIRED);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());

        let delayed = ServerState {
            response: Arc::new(Mutex::new((StatusCode::OK, "{}".into()))),
            delay: Some(Duration::from_secs(1)),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(delayed).await;
        let error = query_with_timeout(
            home.path(),
            &endpoints(&base_url),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Muse usage request timed out");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    /// A Meta endpoint pair: the mint accepts only `fresh-access`, and the
    /// token endpoint records each refresh it is asked for.
    async fn spawn_meta(
        refreshes: Arc<Mutex<Vec<String>>>,
        minted_with: Arc<Mutex<Vec<String>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let mint = {
            let minted_with = minted_with.clone();
            move |headers: HeaderMap| async move {
                let authorization = headers
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                minted_with.lock().unwrap().push(authorization.clone());
                let status = if authorization.ends_with("-access")
                    && authorization != "Bearer stale-access"
                {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                };
                Response::builder()
                    .status(status)
                    .body(usage_body())
                    .unwrap()
            }
        };
        let token = move |body: String| async move {
            refreshes.lock().unwrap().push(body);
            Response::builder()
                .status(StatusCode::OK)
                .body(
                    json!({
                        "access_token": "fresh-access",
                        "refresh_token": "fresh-refresh",
                        "expires_in": 3600,
                        "token_type": "Bearer"
                    })
                    .to_string(),
                )
                .unwrap()
        };
        let app = Router::new()
            .route("/muse-code/key", post(mint))
            .route("/oidc/device/token/", post(token));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), server)
    }

    /// The `auth.json` shape `muse login` writes (Muse 1.4.12).
    fn write_login(home: &Path, access_token: &str, expires_at: i64) {
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "providers": {"meta": {
                    "access_token": access_token,
                    "api_base_url": "https://api.meta.ai",
                    "api_key": "minted-key",
                    "expires_at": expires_at,
                    "mechanism": "oauth",
                    "obtained_via": "device_code",
                    "refresh_token": "old-refresh"
                }}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_rejected_login_is_refreshed_once_under_muses_lock_and_the_query_retried() {
        let refreshes = Arc::new(Mutex::new(Vec::new()));
        let minted_with = Arc::new(Mutex::new(Vec::new()));
        let (base_url, server) = spawn_meta(refreshes.clone(), minted_with.clone()).await;
        let far_future = mj_core::clock::epoch_seconds() as i64 + 24 * 60 * 60;

        // The recorded expiry says the token is good; the endpoint disagrees.
        let home = tempfile::tempdir().unwrap();
        write_login(home.path(), "stale-access", far_future);
        let windows = query_with_timeout(home.path(), &endpoints(&base_url), REQUEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(windows[0].remaining_percent, 99);
        assert_eq!(
            *minted_with.lock().unwrap(),
            ["Bearer stale-access", "Bearer fresh-access"]
        );
        assert_eq!(
            *refreshes.lock().unwrap(),
            [format!(
                "grant_type=refresh_token&refresh_token=old-refresh&client_id={OAUTH_CLIENT_ID}"
            )]
        );
        let saved: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("auth.json")).unwrap()).unwrap();
        let meta = &saved["providers"]["meta"];
        assert_eq!(meta["access_token"], "fresh-access");
        assert_eq!(meta["refresh_token"], "fresh-refresh");
        assert_eq!(meta["api_key"], "minted-key");
        assert_eq!(meta["mechanism"], "oauth");
        assert_eq!(saved["schema_version"], 1);
        assert!(meta["expires_at"].as_i64().unwrap() > mj_core::clock::epoch_seconds() as i64);

        // Muse renews the login while the daemon waits for its lock: the
        // daemon uses that login and never spends the refresh token Muse
        // already spent.
        refreshes.lock().unwrap().clear();
        minted_with.lock().unwrap().clear();
        let raced = tempfile::tempdir().unwrap();
        write_login(raced.path(), "stale-access", 1_700_000_000);
        let muse = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(raced.path().join(AUTH_LOCK_FILE))
            .unwrap();
        muse.lock().unwrap();
        let query = tokio::spawn({
            let home = raced.path().to_owned();
            let endpoints = endpoints(&base_url);
            async move { query_with_timeout(&home, &endpoints, REQUEST_TIMEOUT).await }
        });
        sleep(Duration::from_millis(300)).await;
        assert!(!query.is_finished(), "the daemon must wait for Muse's lock");
        write_login(raced.path(), "muse-renewed-access", far_future);
        drop(muse);
        query.await.unwrap().unwrap();
        assert!(refreshes.lock().unwrap().is_empty());
        assert_eq!(*minted_with.lock().unwrap(), ["Bearer muse-renewed-access"]);

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
