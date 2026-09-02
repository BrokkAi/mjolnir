//! One-pane quota collection for Mjolnir harness profiles.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude_usage;
use crate::codex_usage::{self, CodexUsageClient, CodexUsageStatus};
use crate::grok_usage;
use crate::hel_config::HarnessKind;
use crate::hel_config::harness_authentication_marker;
use crate::hel_credentials::{
    MAX_CREDENTIAL_BYTES, credential_expiry, credential_fingerprint, credential_freshness,
};

pub const API_LABEL: &str = "API";

#[derive(Debug, Clone)]
pub struct QuotaRefreshRequest {
    pub profile_id: String,
    pub harness: HarnessKind,
    pub source_home: std::path::PathBuf,
    /// Harness CLI override from the profile, for the backends that shell out
    /// to the CLI itself rather than to an adapter.
    pub executable: Option<std::path::PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub cwd: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaWindow {
    pub label: String,
    pub remaining_percent: Option<u8>,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    pub resets: Option<String>,
    #[serde(default)]
    pub resets_at_epoch_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileQuota {
    pub profile_id: String,
    pub harness: HarnessKind,
    pub windows: Vec<QuotaWindow>,
    pub extra: Option<String>,
    pub error: Option<String>,
    pub refreshed_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRefreshOutcome {
    pub report: ProfileQuota,
    pub credentials_changed: bool,
}

impl ProfileQuota {
    pub fn weekly_window(&self) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| is_weekly_quota_window(&window.label))
    }

    pub fn five_hour_window(&self) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| is_short_quota_window(&window.label))
    }

    /// Whether the report says the profile is usage-priced: an API-billed
    /// harness has no subscription window to fill, so it reports the API label
    /// in place of one rather than inventing a percentage.
    pub fn is_usage_priced(&self) -> bool {
        self.error.is_none() && self.windows.is_empty() && self.extra.as_deref() == Some(API_LABEL)
    }

    pub fn five_hour_projects_exhaustion(&self) -> bool {
        self.five_hour_window().is_some_and(|window| {
            projects_exhaustion_before_reset(window, self.refreshed_at_epoch_seconds)
        })
    }

    pub fn compact(&self) -> String {
        if let Some(error) = &self.error {
            return quota_error_label(error);
        }
        let mut seen_resets = BTreeSet::new();
        let mut parts = self
            .windows
            .iter()
            .filter(|window| {
                !is_short_quota_window(&window.label)
                    || projects_exhaustion_before_reset(window, self.refreshed_at_epoch_seconds)
            })
            .map(|window| {
                let usage = match (window.remaining_percent, window.used, window.limit) {
                    (Some(remaining), _, _) => format!("{remaining}% left"),
                    (_, Some(used), Some(limit)) => format!("{used}/{limit}"),
                    _ => "available".to_string(),
                };
                match window
                    .resets
                    .as_ref()
                    .filter(|reset| seen_resets.insert((*reset).clone()))
                {
                    Some(reset) => format!("{} {usage}, resets {reset}", window.label),
                    None => format!("{} {usage}", window.label),
                }
            })
            .collect::<Vec<_>>();
        if let Some(extra) = &self.extra {
            parts.push(extra.clone());
        }
        if parts.is_empty() {
            "no quota windows reported".to_string()
        } else {
            parts.join(" · ")
        }
    }

    pub fn error_label(&self) -> Option<String> {
        self.error.as_deref().map(quota_error_label)
    }
}

fn quota_error_label(error: &str) -> String {
    if error == claude_usage::LOGIN_EXPIRED {
        claude_usage::LOGIN_EXPIRED.to_string()
    } else {
        format!("unavailable: {error}")
    }
}

/// The dashboard's long-window column. A harness billed monthly rather than
/// weekly belongs in the same column; the label itself names the real period.
fn is_weekly_quota_window(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "week" | "weekly" | "7d" | "month" | "monthly"
    )
}

fn is_short_quota_window(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "5h" | "5-hour" | "5 hour"
    )
}

/// Whether this window is on course to run out before it resets.
///
/// Published so the phone can show the warning without recomputing a rule that
/// belongs here, beside the data it reads.
pub fn projects_exhaustion(window: &QuotaWindow, now: u64) -> bool {
    projects_exhaustion_before_reset(window, now)
}

fn projects_exhaustion_before_reset(window: &QuotaWindow, now: u64) -> bool {
    const FIVE_HOURS_SECONDS: i64 = 5 * 60 * 60;
    let Some(reset) = window.resets_at_epoch_seconds else {
        return false;
    };
    let Ok(now) = i64::try_from(now) else {
        return false;
    };
    let remaining_time = reset - now;
    let elapsed = FIVE_HOURS_SECONDS - remaining_time;
    if remaining_time <= 0 || elapsed <= 0 || elapsed >= FIVE_HOURS_SECONDS {
        return false;
    }
    if let (Some(used), Some(limit)) = (window.used, window.limit)
        && limit > 0
    {
        return i128::from(used.clamp(0, limit)) * i128::from(FIVE_HOURS_SECONDS)
            > i128::from(limit) * i128::from(elapsed);
    }
    window
        .remaining_percent
        .is_some_and(|remaining| i64::from(100 - remaining) * FIVE_HOURS_SECONDS > 100 * elapsed)
}

#[derive(Default)]
pub struct QuotaManager {
    codex_clients: HashMap<String, CodexUsageClient>,
    reports: BTreeMap<String, ProfileQuota>,
}

impl QuotaManager {
    pub fn reports(&self) -> &BTreeMap<String, ProfileQuota> {
        &self.reports
    }

    /// Refresh each profile independently so one slow harness cannot delay the
    /// others. `on_report` runs per profile in completion order, so fast
    /// harnesses report without waiting for the slowest one in the batch.
    pub async fn refresh_profiles<F, Fut>(
        &mut self,
        requests: Vec<QuotaRefreshRequest>,
        mut on_report: F,
    ) where
        F: FnMut(QuotaRefreshOutcome) -> Fut,
        Fut: Future<Output = ()> + Send,
    {
        let batch = requests
            .iter()
            .map(|request| request.profile_id.clone())
            .collect::<BTreeSet<_>>();
        let mut tasks = tokio::task::JoinSet::new();
        for request in requests {
            let client = self.codex_clients.remove(&request.profile_id);
            tasks.spawn(refresh_profile(request, client));
        }

        while let Some(result) = tasks.join_next().await {
            let (outcome, client) = match result {
                Ok(output) => output,
                Err(error) => {
                    tracing::warn!(%error, "quota refresh task failed");
                    continue;
                }
            };
            if let Some(client) = client {
                self.codex_clients
                    .insert(outcome.report.profile_id.clone(), client);
            }
            self.reports
                .insert(outcome.report.profile_id.clone(), outcome.report.clone());
            on_report(outcome).await;
        }
        self.stop_clients_outside_batch(&batch).await;
    }

    /// Stop the cached clients whose profiles are not in `keep`. Every batch
    /// carries the whole configured set, so a client left over from an earlier
    /// batch belongs to a profile the configuration no longer has. Each one
    /// owns a live `codex app-server` child that nothing would ever hand back
    /// to a refresh again, so it would run until the controller exits.
    async fn stop_clients_outside_batch(&mut self, keep: &BTreeSet<String>) {
        let stranded = self
            .codex_clients
            .keys()
            .filter(|profile_id| !keep.contains(*profile_id))
            .cloned()
            .collect::<Vec<_>>();
        for profile_id in stranded {
            if let Some(client) = self.codex_clients.remove(&profile_id) {
                tracing::info!(profile_id, "stopping the quota client of a removed profile");
                client.shutdown().await;
            }
        }
    }

    pub async fn shutdown(mut self) {
        for (_, client) in self.codex_clients.drain() {
            client.shutdown().await;
        }
    }
}

async fn refresh_profile(
    request: QuotaRefreshRequest,
    mut codex_client: Option<CodexUsageClient>,
) -> (QuotaRefreshOutcome, Option<CodexUsageClient>) {
    let credential_path = harness_authentication_marker(request.harness, &request.source_home);
    let credential_before = credential_marker_fingerprint(&credential_path).await;
    let QuotaRefreshRequest {
        profile_id,
        harness,
        source_home,
        executable,
        environment,
        cwd,
    } = request;
    let environment = environment.into_iter().collect::<HashMap<_, _>>();
    let refreshed_at_epoch_seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let result = match harness {
        HarnessKind::Codex => {
            if codex_login_is_near_expiry(&credential_path).await {
                match codex_usage::refresh_login(
                    &mut codex_client,
                    cwd.clone(),
                    environment.clone(),
                )
                .await
                {
                    Ok(()) => tracing::info!(
                        profile_id = %profile_id,
                        "refreshed Codex login ahead of expiry"
                    ),
                    Err(error) => tracing::warn!(
                        profile_id = %profile_id,
                        %error,
                        "could not refresh the Codex login ahead of expiry"
                    ),
                }
            }
            let status = codex_usage::refresh(&mut codex_client, cwd, environment).await;
            match status {
                CodexUsageStatus::Available(report) => Ok(ProfileQuota {
                    profile_id: profile_id.clone(),
                    harness,
                    windows: [report.primary, report.secondary]
                        .into_iter()
                        .flatten()
                        .map(|window| QuotaWindow {
                            label: window.label,
                            remaining_percent: Some(window.remaining_percent),
                            used: None,
                            limit: None,
                            resets: window
                                .resets_at
                                .and_then(crate::usage_format::format_reset_local_seconds),
                            resets_at_epoch_seconds: window.resets_at,
                        })
                        .collect(),
                    extra: None,
                    error: None,
                    refreshed_at_epoch_seconds,
                }),
                CodexUsageStatus::Unavailable(error) => Err(anyhow::anyhow!(error)),
            }
        }
        HarnessKind::Claude => claude_usage::query(source_home, environment)
            .await
            .map(|report| ProfileQuota {
                profile_id: profile_id.clone(),
                harness,
                windows: [
                    report.five_hour.map(|window| ("5H", window)),
                    report.week.map(|window| ("Week", window)),
                ]
                .into_iter()
                .flatten()
                .map(|(label, window)| QuotaWindow {
                    label: label.to_string(),
                    remaining_percent: Some(window.remaining_percent),
                    used: None,
                    limit: None,
                    resets: window
                        .reset_context
                        .as_deref()
                        .and_then(crate::usage_format::normalize_reset_text),
                    resets_at_epoch_seconds: window
                        .reset_context
                        .as_deref()
                        .and_then(crate::usage_format::normalize_reset_epoch_seconds),
                })
                .collect(),
                extra: None,
                error: None,
                refreshed_at_epoch_seconds,
            })
            .map_err(|error| anyhow::anyhow!(error.to_string())),
        HarnessKind::Kimi => {
            query_kimi(&source_home, &environment)
                .await
                .map(|(windows, extra)| ProfileQuota {
                    profile_id: profile_id.clone(),
                    harness,
                    windows,
                    extra,
                    error: None,
                    refreshed_at_epoch_seconds,
                })
        }
        // Grok Build publishes no HTTP quota endpoint. Its own usage view polls
        // an ACP billing extension, and so does Mjolnir.
        HarnessKind::Grok => {
            grok_usage::query(executable, source_home.clone(), cwd, environment)
                .await
                .map(|report| ProfileQuota {
                    profile_id: profile_id.clone(),
                    harness,
                    windows: vec![QuotaWindow {
                        label: report.period_label.clone(),
                        remaining_percent: Some(report.remaining_percent()),
                        // Grok Build reports a share of the allowance, not the
                        // credit amounts behind it.
                        used: None,
                        limit: None,
                        resets: report
                            .resets_at
                            .and_then(crate::usage_format::format_reset_local_seconds),
                        resets_at_epoch_seconds: report.resets_at,
                    }],
                    extra: None,
                    error: None,
                    refreshed_at_epoch_seconds,
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        }
        HarnessKind::Deepseek => Ok(ProfileQuota {
            profile_id: profile_id.clone(),
            harness,
            windows: Vec::new(),
            extra: Some(API_LABEL.to_owned()),
            error: None,
            refreshed_at_epoch_seconds,
        }),
    };
    let report = result.unwrap_or_else(|error| ProfileQuota {
        profile_id,
        harness,
        windows: Vec::new(),
        extra: None,
        error: Some(error.to_string()),
        refreshed_at_epoch_seconds,
    });
    let credential_after = credential_marker_fingerprint(&credential_path).await;
    let credentials_changed = match (credential_before, credential_after) {
        (Ok(before), Ok(after)) => before != after,
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(path = %credential_path.display(), %error, "could not fingerprint quota credentials");
            false
        }
    };
    (
        QuotaRefreshOutcome {
            report,
            credentials_changed,
        },
        codex_client,
    )
}

/// Shortest gap to expiry Hel will leave a Codex login sitting at. A token with
/// a long life gets a proportionally wider margin, because the poll interval
/// buys nothing once the whole life is short.
const CODEX_MINIMUM_REFRESH_MARGIN_MS: i64 = 60 * 60 * 1000;

/// Whether the profile's Codex login is close enough to expiry that a container
/// copy of it could reach the single-use refresh race before the next poll.
async fn codex_login_is_near_expiry(marker: &Path) -> bool {
    let Ok(bytes) = tokio::fs::read(marker).await else {
        return false;
    };
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return false;
    }
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    codex_login_needs_refresh(
        credential_expiry(HarnessKind::Codex, &bytes),
        credential_freshness(HarnessKind::Codex, &bytes),
        now,
    )
}

/// The margin is the larger of one hour and a tenth of the token's life, where
/// the life is what the last refresh bought. A credential that says nothing
/// about its own age falls back to the flat hour.
fn codex_login_needs_refresh(
    expiry_millis: Option<i64>,
    last_refresh_millis: Option<i64>,
    now_millis: i64,
) -> bool {
    let Some(expiry) = expiry_millis else {
        return false;
    };
    let lifetime = last_refresh_millis
        .map(|refreshed| expiry.saturating_sub(refreshed))
        .unwrap_or_default();
    let margin = CODEX_MINIMUM_REFRESH_MARGIN_MS.max(lifetime / 10);
    expiry.saturating_sub(now_millis) < margin
}

async fn credential_marker_fingerprint(path: &Path) -> Result<Option<String>> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect credential marker"),
    };
    if metadata.len() > MAX_CREDENTIAL_BYTES as u64 {
        bail!("credential marker exceeds {MAX_CREDENTIAL_BYTES} bytes");
    }
    let bytes = tokio::fs::read(path)
        .await
        .context("read credential marker")?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        bail!("credential marker exceeds {MAX_CREDENTIAL_BYTES} bytes");
    }
    Ok(Some(credential_fingerprint(&bytes)))
}

async fn query_kimi(
    home: &Path,
    environment: &HashMap<String, String>,
) -> Result<(Vec<QuotaWindow>, Option<String>)> {
    let base = environment
        .get("KIMI_CODE_BASE_URL")
        .map(String::as_str)
        .unwrap_or("https://api.kimi.com/coding/v1")
        .trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build Kimi quota client")?;
    let credentials_path = home.join("credentials/kimi-code.json");
    let usage_url = format!("{base}/usages");
    let response = fetch_bearer_with_auth_retry(&client, &usage_url, |force, rejected_token| {
        ensure_fresh_kimi_token(
            &client,
            home,
            &credentials_path,
            environment,
            force,
            rejected_token,
        )
    })
    .await?;
    if !response.status().is_success() {
        bail!("Kimi Code quota returned HTTP {}", response.status());
    }
    let payload: Value = response.json().await.context("decode Kimi Code quota")?;
    Ok(parse_kimi_usage(&payload))
}

const KIMI_OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct KimiCredentials {
    #[serde(alias = "accessToken")]
    access_token: String,
    #[serde(default, alias = "refreshToken")]
    refresh_token: String,
    #[serde(default, alias = "expiresAt")]
    expires_at: i64,
    #[serde(default)]
    scope: String,
    #[serde(default, alias = "tokenType")]
    token_type: String,
    #[serde(default, alias = "expiresIn")]
    expires_in: i64,
}

impl KimiCredentials {
    /// Whether this is a different pair from `other`. A refresh rotates the
    /// access token, the refresh token and the expiry together, so those three
    /// fields are what tells two pairs apart; the rest only describes them.
    fn differs_from(&self, other: &Self) -> bool {
        self.access_token != other.access_token
            || self.refresh_token != other.refresh_token
            || self.expires_at != other.expires_at
    }

    fn needs_refresh(&self) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let threshold = 300.max(self.expires_in / 2);
        self.expires_at - now < threshold
    }
}

async fn read_kimi_credentials(path: &Path) -> Result<KimiCredentials> {
    let bytes = tokio::fs::read(path)
        .await
        .context("Kimi Code credentials are unavailable")?;
    let credentials: KimiCredentials =
        serde_json::from_slice(&bytes).context("Kimi Code credentials are invalid")?;
    if credentials.access_token.is_empty() {
        bail!("Kimi Code access token is missing");
    }
    Ok(credentials)
}

async fn fetch_bearer_with_auth_retry<F, Fut>(
    client: &reqwest::Client,
    url: &str,
    mut authenticate: F,
) -> Result<reqwest::Response>
where
    F: FnMut(bool, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let token = authenticate(false, None).await?;
    let response = client
        .get(url)
        .bearer_auth(&token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("query quota")?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(response);
    }

    let refreshed = authenticate(true, Some(token)).await?;
    client
        .get(url)
        .bearer_auth(refreshed)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("retry quota after authentication refresh")
}

/// Hand back a usable Kimi Code access token, refreshing the stored pair when
/// it is stale or when the server rejected it.
///
/// The refresh lock is taken before the round trip, but a peer may break a lock
/// it judges stale while that round trip is in flight, so ownership is checked
/// again immediately before the credentials are written rather than only when
/// the lock is released: see `decide_kimi_refresh_persist` for what a refresh
/// that lost its lock does with the pair it fetched.
async fn ensure_fresh_kimi_token(
    client: &reqwest::Client,
    home: &Path,
    credentials_path: &Path,
    environment: &HashMap<String, String>,
    force: bool,
    rejected_token: Option<String>,
) -> Result<String> {
    let initial = read_kimi_credentials(credentials_path).await?;
    if !force && !initial.needs_refresh() {
        return Ok(initial.access_token);
    }

    let refresh_lock = KimiRefreshLock::acquire(home).await?;
    let active = read_kimi_credentials(credentials_path).await?;
    let changed_while_waiting = active.differs_from(&initial);
    if (!force && !active.needs_refresh())
        || (force
            && (changed_while_waiting
                || rejected_token.is_some_and(|token| token != active.access_token)))
    {
        refresh_lock.release().await?;
        return Ok(active.access_token);
    }
    if active.refresh_token.is_empty() {
        refresh_lock.release().await?;
        bail!("Kimi Code refresh token is missing; run `kimi login`");
    }

    let oauth_host = environment
        .get("KIMI_CODE_OAUTH_HOST")
        .or_else(|| environment.get("KIMI_OAUTH_HOST"))
        .map(String::as_str)
        .unwrap_or("https://auth.kimi.com")
        .trim_end_matches('/');
    let response = client
        .post(format!("{oauth_host}/api/oauth/token"))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("client_id", KIMI_OAUTH_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", active.refresh_token.as_str()),
        ])
        .send()
        .await
        .context("refresh Kimi Code access token")?;
    if !response.status().is_success() {
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let recovery = read_kimi_credentials(credentials_path).await?;
            if recovery.refresh_token != active.refresh_token && !recovery.access_token.is_empty() {
                refresh_lock.release().await?;
                return Ok(recovery.access_token);
            }
        }
        refresh_lock.release().await?;
        bail!("Kimi Code token refresh returned HTTP {status}");
    }

    let payload: Value = response
        .json()
        .await
        .context("decode Kimi Code token refresh")?;
    let access_token = required_string(&payload, "access_token", "Kimi Code token refresh")?;
    let refresh_token = required_string(&payload, "refresh_token", "Kimi Code token refresh")?;
    let expires_in = payload
        .get("expires_in")
        .and_then(value_i64)
        .filter(|value| *value > 0)
        .context("Kimi Code token refresh is missing expires_in")?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let refreshed = KimiCredentials {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now + expires_in,
        scope: payload
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        token_type: payload
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_string(),
        expires_in,
    };
    // Prove the lock is still Mjolnir's before the write, not after it: a peer that
    // broke the lock during the round trip may already have stored a newer pair,
    // and overwriting that would strand both refreshes.
    let ownership = confirm_kimi_lock_ownership(&refresh_lock.path, &refresh_lock.ownership);
    let on_disk = match &ownership {
        // A proven loss makes the file the authority on which pair is live.
        Err(KimiLockLoss::Stolen { .. } | KimiLockLoss::Gone) => {
            read_kimi_credentials(credentials_path).await.ok()
        }
        Ok(_) | Err(KimiLockLoss::Unproven(_)) => None,
    };
    match decide_kimi_refresh_persist(&ownership, on_disk.as_ref(), &active) {
        KimiRefreshPersist::Save => {
            save_kimi_credentials(credentials_path, &refreshed)?;
            if let Err(error) = refresh_lock.release().await {
                // The lock was Mjolnir's when the pair was written and the write
                // landed; losing it in the microseconds since costs the lock,
                // not a valid credential.
                tracing::warn!(
                    %error,
                    "saved refreshed Kimi Code credentials, then lost the OAuth refresh lock before releasing it"
                );
            }
            Ok(refreshed.access_token)
        }
        KimiRefreshPersist::SaveContested(loss) => {
            save_kimi_credentials(credentials_path, &refreshed)?;
            tracing::warn!(
                path = %refresh_lock.path.display(),
                %loss,
                "another Kimi Code token refresh took the OAuth refresh lock, but left behind the pair this refresh already spent; saved Mjolnir's refreshed pair, the only live one"
            );
            Ok(refreshed.access_token)
        }
        KimiRefreshPersist::Adopt { access_token, loss } => {
            tracing::warn!(
                path = %refresh_lock.path.display(),
                %loss,
                "another Kimi Code token refresh took the OAuth refresh lock and stored its own credentials; using those instead of the pair Mjolnir just fetched"
            );
            Ok(access_token)
        }
    }
}

fn save_kimi_credentials(path: &Path, credentials: &KimiCredentials) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(credentials)?;
    body.push(b'\n');
    crate::hel_config::atomic_write(path, &body).context("save refreshed Kimi Code credentials")
}

/// What a completed refresh does with the pair it just fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KimiRefreshPersist {
    /// The lock is Mjolnir's: save the refreshed pair and give the lock back.
    Save,
    /// The lock is another refresher's, and the file still holds the pair this
    /// refresh spent: save the refreshed pair anyway and leave the lock alone.
    SaveContested(KimiLockLoss),
    /// The lock's new holder finished first and stored its own pair: return
    /// that token and write nothing.
    Adopt {
        access_token: String,
        loss: KimiLockLoss,
    },
}

/// Decide how a completed refresh persists its result.
///
/// `ownership` is the lock check taken immediately before the write, `on_disk`
/// the pair the credentials file carried when that check reported a proven
/// loss (`None` when the file was not consulted, or could not be read), and
/// `active` the pair whose refresh token this refresh spent at the server.
///
/// A lost lock never fails the refresh: exactly one of the two pairs is live,
/// and the file says which. A pair on disk that moved on from `active` is the
/// other refresher's, and it is the live one, because the server rotated Mjolnir's
/// pair away from it. A file that still holds `active` is dead whichever
/// refresh wrote it — its refresh token is the one Mjolnir just spent — so the
/// refreshed pair is the only live credential anywhere and has to be stored,
/// even over a contested lock; leaving the spent pair in place would force a
/// `kimi login`. The peer recovers the same way Mjolnir does, by re-reading the
/// file when the server rejects its consumed token.
fn decide_kimi_refresh_persist(
    ownership: &Result<SystemTime, KimiLockLoss>,
    on_disk: Option<&KimiCredentials>,
    active: &KimiCredentials,
) -> KimiRefreshPersist {
    let loss = match ownership {
        Ok(_) => return KimiRefreshPersist::Save,
        // A check that could not read the directory proves nothing about who
        // holds it, so it is no reason to treat the lock as lost.
        Err(KimiLockLoss::Unproven(_)) => return KimiRefreshPersist::Save,
        Err(loss) => loss.clone(),
    };
    match on_disk {
        Some(pair) if pair.differs_from(active) => KimiRefreshPersist::Adopt {
            access_token: pair.access_token.clone(),
            loss,
        },
        _ => KimiRefreshPersist::SaveContested(loss),
    }
}

fn required_string<'a>(payload: &'a Value, key: &str, context: &str) -> Result<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{context} is missing {key}"))
}

struct KimiRefreshLock {
    path: std::path::PathBuf,
    ownership: Arc<Mutex<KimiLockOwnership>>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

/// What Mjolnir knows about the lock directory it created. The Kimi Code CLI
/// breaks a lock whose modification time stopped moving and takes it over, so
/// holding the directory is not the same as owning it: Mjolnir checks the mtime it
/// published is still there before touching or removing the directory.
/// Touching a lock the CLI now owns trips the CLI's own ownership check
/// (`ECOMPROMISED`, proper-lockfile 4.1.2 `lib/lockfile.js:114-140`), and
/// removing it would hand a third holder a lock the CLI is still using.
#[derive(Debug)]
enum KimiLockOwnership {
    /// Mjolnir published this modification time and the directory still carried it
    /// when Mjolnir last looked.
    Held(SystemTime),
    /// Mjolnir must not touch or remove the directory again.
    Lost(KimiLockLoss),
}

/// Why the lock directory is not Mjolnir's any more.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KimiLockLoss {
    /// It carries a modification time Mjolnir never published: another holder broke
    /// the lock and took it.
    Stolen {
        published: SystemTime,
        observed: SystemTime,
    },
    /// It is gone: another holder broke the lock, or Mjolnir already released it.
    Gone,
    /// It could not be inspected, so Mjolnir cannot prove the lock is still its
    /// own. Mjolnir leaves it alone; whoever wants it next breaks it once Mjolnir's
    /// modification time goes stale.
    Unproven(String),
}

impl std::fmt::Display for KimiLockLoss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stolen {
                published,
                observed,
            } => write!(
                formatter,
                "another process took it: Mjolnir published modification time {}, but the directory carries {}",
                epoch_label(*published),
                epoch_label(*observed)
            ),
            Self::Gone => formatter.write_str("another process removed it"),
            Self::Unproven(error) => {
                write!(
                    formatter,
                    "Mjolnir could not confirm it still owns it: {error}"
                )
            }
        }
    }
}

fn epoch_label(time: SystemTime) -> String {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => format!("{:.3}", since.as_secs_f64()),
        Err(_) => "before the epoch".to_string(),
    }
}

/// The Kimi Code CLI is the other holder of this lock, and it agrees that a
/// live holder keeps the directory's mtime moving. It takes the lock through
/// `proper-lockfile` with `stale: 5_000` (kimi-code
/// `packages/oauth/src/oauth-manager.ts:216-220`; the shipped binary carries
/// the same `stale: 5e3`), which rewrites the mtime every `stale / 2` for as
/// long as the lock is held (proper-lockfile 4.1.2 `lib/lockfile.js:99-183`,
/// interval resolved at `lib/lockfile.js:220-221`) and removes any lock whose
/// mtime is older than `stale` (`lib/lockfile.js:67-79, 84-86`). A CLI refresh
/// can hold the lock far longer than that — three tries against a 30s HTTP
/// timeout plus backoff (`packages/oauth/src/oauth.ts:56-73, 226-263`) — but
/// never silently, so a stopped mtime still means the holder is gone. Its
/// mtimes can also land up to a second in the future
/// (`lib/mtime-precision.js:44-52`), which `break_stale_kimi_lock` reads as
/// "not stale" because `duration_since` fails: the safe answer.
const KIMI_CLI_LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
/// A holder republishes the lock directory's modification time on this
/// interval, so a lock whose mtime stopped moving has no live holder. The CLI
/// judges Mjolnir's lock by that same mtime, so the interval has to fit inside
/// `KIMI_CLI_LOCK_STALE_AFTER` several times over: one beat pays for the wait
/// between touches, and the rest is stall the heartbeat task may absorb.
const KIMI_LOCK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// How long the heartbeat task may stall — descheduled, starved, or blocked on
/// a slow filesystem — before the CLI is entitled to break a lock Mjolnir still
/// holds and rotate the credentials alongside it. It is the CLI's window less
/// the interval Mjolnir already spends waiting between touches.
const KIMI_LOCK_HEARTBEAT_STALL_TOLERANCE: Duration = Duration::from_secs(
    KIMI_CLI_LOCK_STALE_AFTER.as_secs() - KIMI_LOCK_HEARTBEAT_INTERVAL.as_secs(),
);
/// Beats of silence Mjolnir waits out before it calls another holder's lock
/// abandoned. Several beats of slack, so a live holder delayed by the scheduler
/// keeps its lock, and deliberately more patient than the CLI's 5s: breaking
/// later than the peer can never steal a live lock, and it costs no recovery
/// time, because the CLI reclaims a lock a crashed Mjolnir left behind after its
/// own 5s.
const KIMI_LOCK_STALE_HEARTBEATS: u64 = 10;
/// Derived from the heartbeat so the two cannot drift apart.
const KIMI_LOCK_STALE_AFTER: Duration =
    Duration::from_secs(KIMI_LOCK_STALE_HEARTBEATS * KIMI_LOCK_HEARTBEAT_INTERVAL.as_secs());
const _: () = assert!(
    KIMI_LOCK_HEARTBEAT_STALL_TOLERANCE.as_secs() >= 4 * KIMI_LOCK_HEARTBEAT_INTERVAL.as_secs(),
    "Mjolnir must be able to miss several beats in a row and still hold a lock the Kimi Code CLI could otherwise break"
);
const _: () = assert!(
    KIMI_LOCK_STALE_AFTER.as_secs() >= KIMI_CLI_LOCK_STALE_AFTER.as_secs(),
    "Mjolnir must not call a lock stale sooner than the Kimi Code CLI does, or it can break a lock the CLI still holds"
);
const KIMI_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const KIMI_LOCK_WAIT: Duration = Duration::from_secs(60);
/// Filesystems record modification times at their own precision — whole
/// seconds on ext3 and HFS+ — and the Kimi Code CLI leans on that, rounding
/// its own writes up to the next whole second so a coarse filesystem stores
/// them unchanged (`lib/mtime-precision.js:44-52`); its mtimes therefore land
/// up to a second in the future. So a time Mjolnir published and the time it reads
/// back can differ by anything under a second and still be the same write.
/// Nothing smaller than a second distinguishes holders: taking the lock from
/// Mjolnir costs another holder at least `KIMI_CLI_LOCK_STALE_AFTER` of silence
/// first, so a thief's modification time is seconds away, never milliseconds.
const KIMI_LOCK_MTIME_TOLERANCE: Duration = Duration::from_secs(1);

impl KimiRefreshLock {
    async fn acquire(home: &Path) -> Result<Self> {
        Self::acquire_within(home, KIMI_LOCK_WAIT).await
    }

    async fn acquire_within(home: &Path, wait: Duration) -> Result<Self> {
        let oauth_dir = home.join("oauth");
        tokio::fs::create_dir_all(&oauth_dir)
            .await
            .context("prepare Kimi Code OAuth lock")?;
        let sentinel = oauth_dir.join("kimi-code");
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sentinel)
            .await
            .context("prepare Kimi Code OAuth lock sentinel")?;
        let path = oauth_dir.join("kimi-code.lock");
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::fs::create_dir(&path).await {
                Ok(()) => return Self::claim(path).await,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    // A holder killed mid-refresh leaves its directory behind
                    // forever; break the lock once its heartbeat has stopped
                    // and retry the create immediately.
                    if !break_stale_kimi_lock(&path).await {
                        tokio::time::sleep(KIMI_LOCK_RETRY_INTERVAL).await;
                    }
                }
                Err(error) => return Err(error).context("acquire Kimi Code OAuth refresh lock"),
            }
        }
        bail!(
            "timed out waiting for Kimi Code OAuth refresh lock {}; another Kimi Code refresh is holding it, or a crashed one left it behind and the directory has to be removed",
            path.display()
        )
    }

    /// Take ownership of a directory Mjolnir just created. The modification time
    /// the filesystem recorded for the create is the first proof of ownership;
    /// every heartbeat republishes it.
    async fn claim(path: std::path::PathBuf) -> Result<Self> {
        let published = kimi_lock_mtime(&path)
            .map_err(anyhow::Error::new)
            .and_then(|mtime| mtime.context("it vanished as Mjolnir created it"));
        match published {
            Ok(published) => Ok(Self::held(path, published)),
            Err(error) => {
                // Without a first modification time Mjolnir could never prove the
                // lock is its own, so it could never release it either. Give it
                // back now instead of leaving it for a stale-breaker.
                let _ = tokio::fs::remove_dir(&path).await;
                Err(error).with_context(|| {
                    format!(
                        "claim the new Kimi Code OAuth refresh lock {}",
                        path.display()
                    )
                })
            }
        }
    }

    fn held(path: std::path::PathBuf, published: SystemTime) -> Self {
        let ownership = Arc::new(Mutex::new(KimiLockOwnership::Held(published)));
        let heartbeat_path = path.clone();
        let heartbeat_ownership = Arc::clone(&ownership);
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(KIMI_LOCK_HEARTBEAT_INTERVAL).await;
                match beat_kimi_lock(&heartbeat_path, &heartbeat_ownership) {
                    Ok(()) => {}
                    Err(KimiLockLoss::Unproven(error)) => {
                        tracing::debug!(path = %heartbeat_path.display(), %error, "heartbeat Kimi Code OAuth refresh lock");
                    }
                    Err(loss) => {
                        tracing::warn!(path = %heartbeat_path.display(), %loss, "stopped heartbeating a Kimi Code OAuth refresh lock Mjolnir no longer holds");
                        return;
                    }
                }
            }
        });
        Self {
            path,
            ownership,
            heartbeat: Some(heartbeat),
        }
    }

    /// Give the lock back. Fails when the lock stopped being Mjolnir's, because the
    /// refresh it was protecting then ran beside another one. Callers that have
    /// already confirmed ownership and stored valid credentials treat that
    /// failure as a lost lock rather than a failed refresh; see
    /// `ensure_fresh_kimi_token`.
    async fn release(mut self) -> Result<()> {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if let Err(loss) = confirm_kimi_lock_ownership(&self.path, &self.ownership) {
            bail!(
                "the Kimi Code OAuth refresh lock {} stopped being Mjolnir's mid-refresh: {loss}; another Kimi Code token refresh may have rotated the credentials beside this one",
                self.path.display()
            );
        }
        match tokio::fs::remove_dir(&self.path).await {
            Ok(()) => {
                *lock_ownership(&self.ownership) = KimiLockOwnership::Lost(KimiLockLoss::Gone)
            }
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "release Kimi Code OAuth refresh lock");
            }
        }
        Ok(())
    }
}

impl Drop for KimiRefreshLock {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if matches!(*lock_ownership(&self.ownership), KimiLockOwnership::Lost(_)) {
            // Already released, or reported where the loss was discovered.
            return;
        }
        match confirm_kimi_lock_ownership(&self.path, &self.ownership) {
            Ok(_) => {
                if let Err(error) = std::fs::remove_dir(&self.path) {
                    tracing::warn!(path = %self.path.display(), %error, "release Kimi Code OAuth refresh lock");
                }
            }
            Err(loss) => {
                tracing::warn!(path = %self.path.display(), %loss, "left a Kimi Code OAuth refresh lock Mjolnir no longer holds in place");
            }
        }
    }
}

fn lock_ownership(
    ownership: &Mutex<KimiLockOwnership>,
) -> std::sync::MutexGuard<'_, KimiLockOwnership> {
    ownership
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Check that the lock directory still carries the modification time Mjolnir
/// published, and remember a loss so every later check agrees. `Ok` hands back
/// the published time; `Err` means Mjolnir must neither touch nor remove the
/// directory.
fn confirm_kimi_lock_ownership(
    path: &Path,
    ownership: &Mutex<KimiLockOwnership>,
) -> Result<SystemTime, KimiLockLoss> {
    let published = match &*lock_ownership(ownership) {
        KimiLockOwnership::Held(published) => *published,
        KimiLockOwnership::Lost(loss) => return Err(loss.clone()),
    };
    let loss = match kimi_lock_mtime(path) {
        Ok(Some(observed)) if kimi_lock_mtime_matches(published, observed) => {
            return Ok(published);
        }
        Ok(Some(observed)) => KimiLockLoss::Stolen {
            published,
            observed,
        },
        Ok(None) => KimiLockLoss::Gone,
        // A stat that fails says nothing about who holds the lock, so the
        // ownership Mjolnir recorded stands and a later beat can confirm it again.
        Err(error) => return Err(KimiLockLoss::Unproven(error.to_string())),
    };
    *lock_ownership(ownership) = KimiLockOwnership::Lost(loss.clone());
    Err(loss)
}

/// One heartbeat: prove the directory is still Mjolnir's, then publish a fresh
/// modification time on it.
fn beat_kimi_lock(path: &Path, ownership: &Mutex<KimiLockOwnership>) -> Result<(), KimiLockLoss> {
    confirm_kimi_lock_ownership(path, ownership)?;
    let published = SystemTime::now();
    if let Err(error) = touch_kimi_lock(path, published) {
        // Only a time actually written may be remembered, or the next check
        // would report a theft that never happened.
        return Err(KimiLockLoss::Unproven(error.to_string()));
    }
    let mut ownership = lock_ownership(ownership);
    if matches!(*ownership, KimiLockOwnership::Held(_)) {
        *ownership = KimiLockOwnership::Held(published);
    }
    Ok(())
}

/// Whether a modification time read back from the lock directory is the one Mjolnir
/// published. See `KIMI_LOCK_MTIME_TOLERANCE` for why a sub-second difference
/// is the same write rather than another holder's.
fn kimi_lock_mtime_matches(published: SystemTime, observed: SystemTime) -> bool {
    observed
        .duration_since(published)
        .or_else(|_| published.duration_since(observed))
        .is_ok_and(|drift| drift < KIMI_LOCK_MTIME_TOLERANCE)
}

/// The lock directory's modification time, or `None` when the directory is
/// gone. Mjolnir and the Kimi Code CLI share this one value and nothing else: the
/// CLI releases its lock with a plain `rmdir`, so the directory has to stay
/// empty and the mtime is the whole protocol.
fn kimi_lock_mtime(path: &Path) -> std::io::Result<Option<SystemTime>> {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.modified().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Publish a lock directory's modification time, the signal that its holder is
/// still alive. Windows opens a directory handle only under backup semantics,
/// so the heartbeat would otherwise be a silent no-op there and every live lock
/// would look abandoned.
fn touch_kimi_lock(path: &Path, modified: SystemTime) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options
        .open(path)?
        .set_times(std::fs::FileTimes::new().set_modified(modified))
}

/// Remove a lock directory whose heartbeat has stopped, so a holder killed
/// mid-refresh cannot poison the profile home until someone removes it by hand.
/// Returns whether the lock is gone and the caller should retry the create at
/// once; a lock another process removes or recreates underneath simply loses or
/// wins the next create.
async fn break_stale_kimi_lock(path: &Path) -> bool {
    let modified = match kimi_lock_mtime(path) {
        Ok(Some(modified)) => modified,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "inspect Kimi Code OAuth refresh lock");
            return false;
        }
    };
    let age = SystemTime::now().duration_since(modified).ok();
    let Some(age) = age.filter(|age| *age >= KIMI_LOCK_STALE_AFTER) else {
        return false;
    };
    match tokio::fs::remove_dir(path).await {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                age_seconds = age.as_secs(),
                "removed a Kimi Code OAuth refresh lock whose holder stopped heartbeating"
            );
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "remove stale Kimi Code OAuth refresh lock");
            false
        }
    }
}

fn parse_kimi_usage(payload: &Value) -> (Vec<QuotaWindow>, Option<String>) {
    let mut windows = Vec::new();
    if let Some(summary) = payload.get("usage")
        && let Some(window) = parse_kimi_window(summary, "Weekly limit")
    {
        windows.push(window);
    }
    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for (index, item) in limits.iter().enumerate() {
            let detail = item.get("detail").unwrap_or(item);
            if let Some(window) = parse_kimi_window(detail, &format!("Limit #{}", index + 1)) {
                windows.push(window);
            }
        }
    }
    let extra = payload
        .pointer("/boosterWallet/balance/amountLeft")
        .and_then(value_i64)
        .map(|value| format!("booster {} remaining", value / 1_000_000));
    (windows, extra)
}

fn parse_kimi_window(value: &Value, fallback: &str) -> Option<QuotaWindow> {
    let limit = value.get("limit").and_then(value_i64);
    let used = value.get("used").and_then(value_i64).or_else(|| {
        let remaining = value.get("remaining").and_then(value_i64)?;
        Some(limit? - remaining)
    });
    if used.is_none() && limit.is_none() {
        return None;
    }
    let provider_label = value
        .get("name")
        .or_else(|| value.get("title"))
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    let label = if provider_label.to_ascii_lowercase().contains("week") {
        "Week".to_string()
    } else if provider_label.to_ascii_lowercase().contains("5h") || fallback.starts_with("Limit #")
    {
        "5H".to_string()
    } else {
        provider_label.to_string()
    };
    let reset_value = ["resetAt", "reset_at", "resetTime", "reset_time"]
        .iter()
        .find_map(|key| value.get(*key));
    let resets = reset_value.and_then(normalize_kimi_reset);
    let resets_at_epoch_seconds = reset_value.and_then(kimi_reset_epoch_seconds);
    let remaining_percent = match (used, limit) {
        (Some(used), Some(limit)) if limit > 0 => {
            Some((100 - used.clamp(0, limit) * 100 / limit) as u8)
        }
        _ => None,
    };
    Some(QuotaWindow {
        label,
        remaining_percent,
        used,
        limit,
        resets,
        resets_at_epoch_seconds,
    })
}

fn value_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse::<i64>().ok())
}

fn normalize_kimi_reset(value: &Value) -> Option<String> {
    value
        .as_f64()
        .and_then(crate::usage_format::format_reset_local)
        .or_else(|| {
            value
                .as_str()
                .and_then(crate::usage_format::normalize_reset_text)
        })
}

fn kimi_reset_epoch_seconds(value: &Value) -> Option<i64> {
    value
        .as_f64()
        .map(|epoch| {
            if epoch.abs() >= 1_000_000_000_000.0 {
                (epoch / 1000.0).trunc() as i64
            } else {
                epoch.trunc() as i64
            }
        })
        .or_else(|| {
            value
                .as_str()
                .and_then(crate::usage_format::normalize_reset_epoch_seconds)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    #[test]
    fn parses_kimi_summary_limits_and_booster_without_credentials() {
        let payload = serde_json::json!({
            "usage": {"name":"Weekly", "used":40, "limit":1000, "resetAt":"tomorrow"},
            "limits": [{"detail":{"remaining":"90", "limit":"100", "name":"5h"}}],
            "boosterWallet": {"balance":{"amountLeft":42000000}}
        });
        let (windows, extra) = parse_kimi_usage(&payload);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used, Some(40));
        assert_eq!(windows[1].used, Some(10));
        assert_eq!(windows[0].label, "Week");
        assert_eq!(windows[0].remaining_percent, Some(96));
        assert_eq!(windows[1].label, "5H");
        assert_eq!(windows[1].remaining_percent, Some(90));
        assert_eq!(extra.as_deref(), Some("booster 42 remaining"));
    }

    #[test]
    fn compact_includes_reset_and_error_states() {
        let report = ProfileQuota {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            windows: vec![QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(70),
                used: None,
                limit: None,
                resets: Some("10:00 Jun 17".into()),
                resets_at_epoch_seconds: Some(14_400),
            }],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };
        assert!(report.compact().contains("70% left"));
        assert!(report.compact().contains("resets 10:00 Jun 17"));
    }

    #[test]
    fn compact_shows_login_expired_without_unavailable_prefix() {
        let report = ProfileQuota {
            profile_id: "claude2".into(),
            harness: HarnessKind::Claude,
            windows: vec![],
            extra: None,
            error: Some(claude_usage::LOGIN_EXPIRED.into()),
            refreshed_at_epoch_seconds: 0,
        };
        assert_eq!(report.compact(), claude_usage::LOGIN_EXPIRED);
        assert_eq!(
            report.error_label().as_deref(),
            Some(claude_usage::LOGIN_EXPIRED)
        );
    }

    #[test]
    fn compact_still_prefixes_other_errors_with_unavailable() {
        let report = ProfileQuota {
            profile_id: "claude2".into(),
            harness: HarnessKind::Claude,
            windows: vec![],
            extra: None,
            error: Some("query Claude usage: HTTP 429".into()),
            refreshed_at_epoch_seconds: 0,
        };
        assert_eq!(
            report.compact(),
            "unavailable: query Claude usage: HTTP 429"
        );
    }

    #[test]
    fn compact_displays_a_shared_reset_once() {
        let report = ProfileQuota {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            windows: vec![
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(70),
                    used: None,
                    limit: None,
                    resets: Some("10:00 Jun 17".into()),
                    resets_at_epoch_seconds: Some(14_400),
                },
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(55),
                    used: None,
                    limit: None,
                    resets: Some("10:00 Jun 17".into()),
                    resets_at_epoch_seconds: Some(14_400),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };
        assert_eq!(
            report.compact(),
            "5H 70% left, resets 10:00 Jun 17 · Week 55% left"
        );
    }

    #[test]
    fn compact_hides_claude_short_window_when_week_is_exhausted() {
        let report = ProfileQuota {
            profile_id: "claude".into(),
            harness: HarnessKind::Claude,
            windows: vec![
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(100),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: None,
                },
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(0),
                    used: None,
                    limit: None,
                    resets: Some("03:59 Aug 14".into()),
                    resets_at_epoch_seconds: None,
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        };

        assert_eq!(report.compact(), "Week 0% left, resets 03:59 Aug 14");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_grok_profile_reports_its_billing_period_as_one_quota_window() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("grok");
        std::fs::write(directory.path().join("auth.json"), b"old credentials").unwrap();
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'refreshed credentials' > \"$GROK_HOME/auth.json\"\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *initialize*) printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n' ;;\n    *billing*) printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"config\":{\"creditUsagePercent\":25.0,\"currentPeriod\":{\"type\":\"USAGE_PERIOD_TYPE_WEEKLY\",\"end\":\"2026-08-18T05:22:07+00:00\"}},\"subscription_tier\":\"X Premium+\"}}\\n' ;;\n  esac\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let environment = BTreeMap::from([(
            "GROK_HOME".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        )]);

        let (outcome, _) = refresh_profile(
            QuotaRefreshRequest {
                profile_id: "grok".into(),
                harness: HarnessKind::Grok,
                source_home: directory.path().to_path_buf(),
                executable: Some(executable),
                environment,
                cwd: directory.path().to_path_buf(),
            },
            None,
        )
        .await;
        assert!(outcome.credentials_changed);
        let report = outcome.report;

        assert_eq!(report.error, None, "{:?}", report.error);
        // One long window and no short one: Grok Build has no 5-hour budget.
        assert_eq!(report.windows.len(), 1);
        assert_eq!(report.weekly_window().unwrap().remaining_percent, Some(75));
        assert_eq!(report.five_hour_window(), None);
        // The subscription tier stays off the row; the fixture carries it to
        // prove it is ignored.
        assert_eq!(report.extra, None);
        assert!(report.compact().starts_with("Week 75% left, resets "));
    }

    /// A `codex app-server` stand-in on `PATH` that logs every request line it
    /// reads, so a test can assert the exact protocol exchange.
    #[cfg(unix)]
    fn fake_codex_app_server(
        directory: &Path,
        script: &str,
    ) -> (BTreeMap<String, String>, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("codex");
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = directory.join("requests.jsonl");
        let environment = BTreeMap::from([
            ("PATH".to_owned(), directory.to_string_lossy().into_owned()),
            (
                "CODEX_USAGE_TEST_LOG".to_owned(),
                log.to_string_lossy().into_owned(),
            ),
            (
                "CODEX_AUTH_FILE".to_owned(),
                directory.join("auth.json").to_string_lossy().into_owned(),
            ),
        ]);
        (environment, log)
    }

    /// A Codex `auth.json` whose access token is a JWT expiring `expires_in`
    /// from now, last refreshed `refreshed_ago` before now.
    #[cfg(unix)]
    fn write_codex_auth(home: &Path, expires_in: Duration, refreshed_ago: Duration) {
        use base64::Engine as _;

        let now = chrono::Utc::now();
        let expiry = (now + chrono::TimeDelta::from_std(expires_in).unwrap()).timestamp();
        let segment = |value: Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&value).unwrap())
        };
        let access_token = format!(
            "{}.{}.signature-is-never-checked",
            segment(serde_json::json!({ "alg": "RS256", "typ": "JWT" })),
            segment(serde_json::json!({ "exp": expiry })),
        );
        let body = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access_token,
                "refresh_token": "refresh",
                "id_token": "id",
                "account_id": "account",
            },
            "last_refresh": (now - chrono::TimeDelta::from_std(refreshed_ago).unwrap())
                .to_rfc3339(),
        });
        std::fs::write(home.join("auth.json"), serde_json::to_vec(&body).unwrap()).unwrap();
    }

    #[cfg(unix)]
    fn codex_request_log(log: &Path) -> Vec<Value> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    #[cfg(unix)]
    async fn poll_codex_profile(
        directory: &Path,
        environment: BTreeMap<String, String>,
    ) -> QuotaRefreshOutcome {
        let (outcome, client) = refresh_profile(
            QuotaRefreshRequest {
                profile_id: "codex".into(),
                harness: HarnessKind::Codex,
                source_home: directory.to_path_buf(),
                executable: None,
                environment,
                cwd: directory.to_path_buf(),
            },
            None,
        )
        .await;
        if let Some(client) = client {
            client.shutdown().await;
        }
        outcome
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_codex_login_near_expiry_is_rotated_before_the_usage_query() {
        let directory = tempfile::tempdir().unwrap();
        // Ten minutes left on a one-hour token: inside the one-hour margin.
        write_codex_auth(
            directory.path(),
            Duration::from_secs(600),
            Duration::from_secs(3_000),
        );
        let (environment, log) = fake_codex_app_server(
            directory.path(),
            r#"#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"auth_mode":"chatgpt","tokens":{"access_token":"rotated"}}' > "$CODEX_AUTH_FILE"
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
"#,
        );

        let outcome = poll_codex_profile(directory.path(), environment).await;

        assert_eq!(outcome.report.error, None);
        assert_eq!(
            outcome.report.five_hour_window().unwrap().remaining_percent,
            Some(75)
        );
        // The rotated file has to reach live sessions, which is what the
        // changed-credentials flag asks the daemon to do.
        assert!(outcome.credentials_changed);

        let messages = codex_request_log(&log);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["method"], "initialize");
        assert_eq!(messages[1]["method"], "initialized");
        assert_eq!(messages[2]["method"], "account/read");
        assert_eq!(messages[2]["params"]["refreshToken"], true);
        assert_eq!(messages[3]["method"], "account/read");
        assert_eq!(messages[3]["params"]["refreshToken"], false);
        assert_eq!(messages[4]["method"], "account/rateLimits/read");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_codex_login_far_from_expiry_is_polled_without_a_rotation() {
        let directory = tempfile::tempdir().unwrap();
        // Ten hours left on an eleven-hour token: outside both margins.
        write_codex_auth(
            directory.path(),
            Duration::from_secs(10 * 3_600),
            Duration::from_secs(3_600),
        );
        let (environment, log) = fake_codex_app_server(
            directory.path(),
            r#"#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
"#,
        );

        let outcome = poll_codex_profile(directory.path(), environment).await;

        assert_eq!(outcome.report.error, None);
        assert!(!outcome.credentials_changed);

        let messages = codex_request_log(&log);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["method"], "initialize");
        assert_eq!(messages[1]["method"], "initialized");
        assert_eq!(messages[2]["params"]["refreshToken"], false);
        assert_eq!(messages[3]["method"], "account/rateLimits/read");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_codex_app_server_without_the_refresh_flag_still_reports_quota() {
        let directory = tempfile::tempdir().unwrap();
        write_codex_auth(
            directory.path(),
            Duration::from_secs(600),
            Duration::from_secs(3_000),
        );
        let (environment, _log) = fake_codex_app_server(
            directory.path(),
            r#"#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"unknown parameter"}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 1
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":40,"windowDurationMins":300}}}}'
"#,
        );

        let outcome = poll_codex_profile(directory.path(), environment).await;

        assert_eq!(outcome.report.error, None);
        assert_eq!(
            outcome.report.five_hour_window().unwrap().remaining_percent,
            Some(60)
        );
    }

    #[test]
    fn a_codex_refresh_margin_is_an_hour_or_a_tenth_of_the_token_life() {
        let hour = 3_600_000;
        let now = 1_800_000_000_000;
        // A short-lived token: the flat hour decides.
        assert!(codex_login_needs_refresh(
            Some(now + hour / 2),
            Some(now - hour / 2),
            now
        ));
        assert!(!codex_login_needs_refresh(
            Some(now + 2 * hour),
            Some(now - hour),
            now
        ));
        // A long-lived token: a tenth of its life is wider than the hour.
        assert!(codex_login_needs_refresh(
            Some(now + 3 * hour),
            Some(now - 40 * hour),
            now
        ));
        // Without a last refresh, only the flat hour is known.
        assert!(codex_login_needs_refresh(Some(now + hour / 2), None, now));
        assert!(!codex_login_needs_refresh(Some(now + 3 * hour), None, now));
        // An unreadable expiry is not a reason to spend the refresh token.
        assert!(!codex_login_needs_refresh(None, Some(now - hour), now));
    }

    #[tokio::test]
    async fn a_missing_codex_credential_file_asks_for_no_rotation() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!codex_login_is_near_expiry(&directory.path().join("auth.json")).await);
    }

    #[tokio::test]
    async fn an_unreachable_grok_reports_the_failure_instead_of_a_zero_reading() {
        let directory = tempfile::tempdir().unwrap();

        let (outcome, _) = refresh_profile(
            QuotaRefreshRequest {
                profile_id: "grok".into(),
                harness: HarnessKind::Grok,
                source_home: directory.path().to_path_buf(),
                executable: Some(directory.path().join("no-such-grok")),
                environment: BTreeMap::new(),
                cwd: directory.path().to_path_buf(),
            },
            None,
        )
        .await;
        let report = outcome.report;

        assert!(report.windows.is_empty());
        assert_eq!(
            report.error.as_deref(),
            Some("Grok Build executable not found")
        );
    }

    #[tokio::test]
    async fn deepseek_reports_api_instead_of_inventing_quota() {
        let directory = tempfile::tempdir().unwrap();
        let (outcome, _) = refresh_profile(
            QuotaRefreshRequest {
                profile_id: "deepseek".into(),
                harness: HarnessKind::Deepseek,
                source_home: directory.path().to_path_buf(),
                executable: None,
                environment: BTreeMap::new(),
                cwd: directory.path().to_path_buf(),
            },
            None,
        )
        .await;

        assert!(outcome.report.windows.is_empty());
        assert_eq!(outcome.report.error, None);
        assert_eq!(outcome.report.extra.as_deref(), Some(API_LABEL));
        assert_eq!(outcome.report.compact(), API_LABEL);
    }

    #[tokio::test]
    async fn expired_claude_credentials_report_login_expired() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(".credentials.json"),
            serde_json::to_vec(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-expired",
                    "expiresAt": 1,
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let (outcome, _) = refresh_profile(
            QuotaRefreshRequest {
                profile_id: "claude2".into(),
                harness: HarnessKind::Claude,
                source_home: directory.path().to_path_buf(),
                executable: None,
                environment: BTreeMap::new(),
                cwd: directory.path().to_path_buf(),
            },
            None,
        )
        .await;
        let report = outcome.report;

        assert!(report.windows.is_empty());
        assert_eq!(report.error.as_deref(), Some(claude_usage::LOGIN_EXPIRED));
        assert_eq!(report.compact(), claude_usage::LOGIN_EXPIRED);
    }

    #[test]
    fn a_monthly_window_shares_the_long_window_column_with_a_weekly_one() {
        for label in ["Week", "Month"] {
            let report = ProfileQuota {
                profile_id: "grok".into(),
                harness: HarnessKind::Grok,
                windows: vec![QuotaWindow {
                    label: label.into(),
                    remaining_percent: Some(60),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: None,
                }],
                extra: None,
                error: None,
                refreshed_at_epoch_seconds: 0,
            };

            assert!(report.weekly_window().is_some(), "{label}");
            assert_eq!(report.compact(), format!("{label} 60% left"));
        }
    }

    #[test]
    fn kimi_uses_percent_left_and_hides_a_short_window_on_sustainable_pace() {
        let report = ProfileQuota {
            profile_id: "kimi".into(),
            harness: HarnessKind::Kimi,
            windows: vec![
                QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(94),
                    used: Some(6),
                    limit: Some(100),
                    resets: Some("12:22 Aug 18".into()),
                    resets_at_epoch_seconds: Some(604_800),
                },
                QuotaWindow {
                    label: "5H".into(),
                    remaining_percent: Some(97),
                    used: Some(3),
                    limit: Some(100),
                    resets: Some("10:22 Aug 13".into()),
                    resets_at_epoch_seconds: Some(18_000),
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 3_600,
        };

        assert_eq!(report.compact(), "Week 94% left, resets 12:22 Aug 18");
    }

    #[test]
    fn short_window_is_shown_only_when_burn_rate_projects_early_exhaustion() {
        let window = QuotaWindow {
            label: "5H".into(),
            remaining_percent: Some(70),
            used: None,
            limit: None,
            resets: Some("later".into()),
            resets_at_epoch_seconds: Some(14_400),
        };
        assert!(projects_exhaustion_before_reset(&window, 0));

        let sustainable = QuotaWindow {
            remaining_percent: Some(80),
            ..window
        };
        assert!(!projects_exhaustion_before_reset(&sustainable, 0));
    }

    #[derive(Clone, Default)]
    struct KimiServerState {
        refresh_forms: Arc<Mutex<Vec<String>>>,
    }

    async fn test_kimi_usage(headers: HeaderMap) -> (StatusCode, Json<Value>) {
        let accepted = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some("Bearer fresh-access");
        if accepted {
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "usage": {"name": "Weekly", "used": 1, "limit": 100}
                })),
            )
        } else {
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})))
        }
    }

    async fn test_kimi_refresh(State(state): State<KimiServerState>, body: Bytes) -> Json<Value> {
        state
            .refresh_forms
            .lock()
            .unwrap()
            .push(String::from_utf8(body.to_vec()).unwrap());
        Json(serde_json::json!({
            "access_token": "fresh-access",
            "refresh_token": "fresh-refresh",
            "expires_in": 900,
            "scope": "kimi-code",
            "token_type": "Bearer"
        }))
    }

    #[tokio::test]
    async fn kimi_quota_refreshes_after_unauthorized_and_retries() {
        let state = KimiServerState::default();
        let app = Router::new()
            .route("/coding/v1/usages", get(test_kimi_usage))
            .route("/api/oauth/token", post(test_kimi_refresh))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let home = tempfile::tempdir().unwrap();
        let credentials_path = home.path().join("credentials/kimi-code.json");
        tokio::fs::create_dir_all(credentials_path.parent().unwrap())
            .await
            .unwrap();
        let future_expiry = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        tokio::fs::write(
            &credentials_path,
            serde_json::to_vec(&serde_json::json!({
                "access_token": "rejected-access",
                "refresh_token": "old-refresh",
                "expires_at": future_expiry,
                "scope": "kimi-code",
                "token_type": "Bearer",
                "expires_in": 900
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let endpoint = format!("http://{address}");
        let environment = HashMap::from([
            ("KIMI_CODE_BASE_URL".into(), format!("{endpoint}/coding/v1")),
            ("KIMI_CODE_OAUTH_HOST".into(), endpoint),
        ]);

        let (windows, _) = query_kimi(home.path(), &environment).await.unwrap();

        assert_eq!(windows[0].used, Some(1));
        let form = {
            let forms = state.refresh_forms.lock().unwrap();
            assert_eq!(forms.len(), 1);
            url::form_urlencoded::parse(forms[0].as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>()
        };
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("old-refresh")
        );
        let saved = read_kimi_credentials(&credentials_path).await.unwrap();
        assert_eq!(saved.access_token, "fresh-access");
        assert_eq!(saved.refresh_token, "fresh-refresh");
        assert!(!home.path().join("oauth/kimi-code.lock").exists());
        server.abort();
    }

    /// Backdate the lock directory the way a holder that stopped heartbeating
    /// leaves it behind.
    fn age_kimi_lock(path: &Path, age: Duration) {
        touch_kimi_lock(path, SystemTime::now() - age).expect("backdate lock directory");
    }

    #[tokio::test]
    async fn a_kimi_refresh_lock_left_by_a_crashed_holder_is_broken_and_reacquired() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");
        std::fs::create_dir_all(&lock).unwrap();
        age_kimi_lock(&lock, KIMI_LOCK_STALE_AFTER + Duration::from_secs(60));

        let started = std::time::Instant::now();
        let held = KimiRefreshLock::acquire_within(home.path(), Duration::from_secs(10))
            .await
            .expect("an orphaned lock must not block a refresh");
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(5),
            "acquisition waited {waited:?}"
        );
        held.release().await.expect("release an uncontested lock");
        assert!(!lock.exists(), "the released lock must be gone");
    }

    #[tokio::test]
    async fn a_heartbeating_kimi_refresh_lock_is_not_broken_by_a_waiter() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");
        std::fs::create_dir_all(&lock).unwrap();

        let error = KimiRefreshLock::acquire_within(home.path(), Duration::from_millis(600))
            .await
            .err()
            .expect("a lock with a live holder must be waited out, not stolen");

        assert!(
            error.to_string().contains("kimi-code.lock"),
            "the timeout must name the lock: {error}"
        );
        assert!(lock.exists(), "a live holder's lock must survive a waiter");
    }

    /// The Kimi Code CLI breaks a lock whose modification time is more than
    /// `KIMI_CLI_LOCK_STALE_AFTER` old, so Mjolnir's beats have to be frequent
    /// enough that a stalled heartbeat task still cannot cost it a live lock.
    #[tokio::test]
    async fn a_held_kimi_lock_republishes_its_mtime_several_times_per_cli_break_window() {
        let home = tempfile::tempdir().unwrap();
        let held = KimiRefreshLock::acquire(home.path()).await.unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");

        // Half the peer's break window: two beats have to land inside it, so
        // Mjolnir publishes at least four times per window and can miss several in
        // a row and still hold the lock.
        let watched = KIMI_CLI_LOCK_STALE_AFTER / 2;
        let deadline = tokio::time::Instant::now() + watched;
        let mut published = vec![kimi_lock_mtime(&lock).unwrap().expect("the created lock")];
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let observed = kimi_lock_mtime(&lock).unwrap().expect("a held lock");
            if published.last() != Some(&observed) {
                published.push(observed);
            }
            assert_eq!(
                std::fs::read_dir(&lock).unwrap().count(),
                0,
                "the CLI releases this lock with a plain rmdir, so it must stay empty"
            );
        }

        assert!(
            published.len() >= 3,
            "the lock's modification time moved {} times in {watched:?}; the Kimi Code CLI breaks a lock after {KIMI_CLI_LOCK_STALE_AFTER:?} without a beat",
            published.len() - 1
        );
        held.release().await.expect("release an uncontested lock");
    }

    #[tokio::test]
    async fn a_stolen_kimi_refresh_lock_is_left_to_its_new_holder_and_fails_the_refresh() {
        let home = tempfile::tempdir().unwrap();
        let held = KimiRefreshLock::acquire(home.path()).await.unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");

        // The Kimi Code CLI breaks a lock it judges stale and takes it over:
        // rmdir, mkdir, then its own modification time. The CLI dates those
        // ahead of the clock; date this one far enough ahead that only the new
        // holder could have written it.
        std::fs::remove_dir(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        let thief = SystemTime::now() + Duration::from_secs(30);
        touch_kimi_lock(&lock, thief).unwrap();

        // Mjolnir must stop beating: a touch on the CLI's lock trips the CLI's own
        // mtime ownership check and it abandons its refresh with ECOMPROMISED.
        tokio::time::sleep(2 * KIMI_LOCK_HEARTBEAT_INTERVAL + Duration::from_millis(400)).await;
        let observed = kimi_lock_mtime(&lock)
            .unwrap()
            .expect("the new holder's lock");
        assert!(
            observed.duration_since(SystemTime::now()).is_ok(),
            "Mjolnir kept heartbeating a lock it no longer holds: the directory carries {} instead of the new holder's {}",
            epoch_label(observed),
            epoch_label(thief)
        );

        let error = held
            .release()
            .await
            .expect_err("a refresh that lost its lock must fail loudly");
        let message = error.to_string();
        assert!(message.contains("kimi-code.lock"), "{message}");
        assert!(message.contains("another process took it"), "{message}");
        assert!(
            lock.exists(),
            "Mjolnir must not remove a lock another holder owns"
        );
    }

    #[tokio::test]
    async fn a_kimi_refresh_lock_removed_underneath_hel_fails_the_refresh() {
        let home = tempfile::tempdir().unwrap();
        let held = KimiRefreshLock::acquire(home.path()).await.unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");
        std::fs::remove_dir(&lock).unwrap();

        let error = held
            .release()
            .await
            .expect_err("a refresh that lost its lock must fail loudly");

        assert!(
            error.to_string().contains("another process removed it"),
            "{error}"
        );
        assert!(!lock.exists(), "Mjolnir must not recreate a lock it lost");
    }

    fn kimi_pair(access: &str, refresh: &str, expires_at: i64) -> KimiCredentials {
        KimiCredentials {
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at,
            scope: "kimi-code".into(),
            token_type: "Bearer".into(),
            expires_in: 900,
        }
    }

    fn a_stolen_lock() -> KimiLockLoss {
        KimiLockLoss::Stolen {
            published: SystemTime::UNIX_EPOCH,
            observed: SystemTime::UNIX_EPOCH + Duration::from_secs(30),
        }
    }

    #[test]
    fn a_lock_still_held_saves_the_refreshed_pair_and_releases() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);

        assert_eq!(
            decide_kimi_refresh_persist(&Ok(SystemTime::now()), None, &active),
            KimiRefreshPersist::Save
        );
    }

    #[test]
    fn an_unprovable_ownership_check_still_saves_the_refreshed_pair() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);
        let unproven = Err(KimiLockLoss::Unproven("stat failed".into()));

        assert_eq!(
            decide_kimi_refresh_persist(&unproven, None, &active),
            KimiRefreshPersist::Save,
            "a failed stat proves nothing about the lock and must not discard valid tokens"
        );
    }

    #[test]
    fn a_stolen_lock_adopts_the_thiefs_newer_credentials() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);
        let thiefs = kimi_pair("thief-access", "thief-refresh", 900);

        assert_eq!(
            decide_kimi_refresh_persist(&Err(a_stolen_lock()), Some(&thiefs), &active),
            KimiRefreshPersist::Adopt {
                access_token: "thief-access".into(),
                loss: a_stolen_lock(),
            }
        );
    }

    #[test]
    fn a_removed_lock_adopts_the_newer_credentials_left_on_disk() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);
        let thiefs = kimi_pair("thief-access", "thief-refresh", 900);

        assert_eq!(
            decide_kimi_refresh_persist(&Err(KimiLockLoss::Gone), Some(&thiefs), &active),
            KimiRefreshPersist::Adopt {
                access_token: "thief-access".into(),
                loss: KimiLockLoss::Gone,
            }
        );
    }

    /// The pair on disk is dead whoever wrote it: its refresh token is the one
    /// this refresh just spent at the server.
    #[test]
    fn a_stolen_lock_saves_hels_pair_over_the_spent_one_on_disk() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);
        let on_disk = active.clone();

        assert_eq!(
            decide_kimi_refresh_persist(&Err(a_stolen_lock()), Some(&on_disk), &active),
            KimiRefreshPersist::SaveContested(a_stolen_lock())
        );
    }

    #[test]
    fn an_unreadable_credentials_file_after_a_lost_lock_saves_hels_pair() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);

        assert_eq!(
            decide_kimi_refresh_persist(&Err(KimiLockLoss::Gone), None, &active),
            KimiRefreshPersist::SaveContested(KimiLockLoss::Gone),
            "nothing readable is newer, so the refreshed pair is the only live one"
        );
    }

    #[test]
    fn any_rotated_field_marks_the_disk_pair_as_the_other_refreshers() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);
        let rotations = [
            kimi_pair("other-access", "spent-refresh", 100),
            kimi_pair("spent-access", "other-refresh", 100),
            kimi_pair("spent-access", "spent-refresh", 900),
        ];

        for on_disk in &rotations {
            assert!(
                matches!(
                    decide_kimi_refresh_persist(&Err(a_stolen_lock()), Some(on_disk), &active),
                    KimiRefreshPersist::Adopt { .. }
                ),
                "{on_disk:?} is a rotated pair, not the spent one"
            );
        }
    }

    #[test]
    fn a_disk_pair_that_differs_only_in_description_is_still_the_spent_one() {
        let active = kimi_pair("spent-access", "spent-refresh", 100);
        let on_disk = KimiCredentials {
            scope: "kimi-code extra".into(),
            token_type: "bearer".into(),
            expires_in: 1_800,
            ..active.clone()
        };

        assert_eq!(
            decide_kimi_refresh_persist(&Err(a_stolen_lock()), Some(&on_disk), &active),
            KimiRefreshPersist::SaveContested(a_stolen_lock())
        );
    }

    #[derive(Clone)]
    struct KimiThiefState {
        home: std::path::PathBuf,
        /// The pair the lock's new holder stores before Mjolnir's own refresh
        /// returns, when it got that far.
        winner: Option<Value>,
    }

    /// Answer the refresh, but take the lock over first the way the Kimi Code
    /// CLI takes over one it judges stale: rmdir, mkdir, then a modification
    /// time of its own, dated ahead of the clock as the CLI dates its locks.
    async fn test_kimi_refresh_stealing_the_lock(
        State(state): State<KimiThiefState>,
        _body: Bytes,
    ) -> Json<Value> {
        let lock = state.home.join("oauth/kimi-code.lock");
        std::fs::remove_dir(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        touch_kimi_lock(&lock, SystemTime::now() + Duration::from_secs(30)).unwrap();
        if let Some(winner) = &state.winner {
            std::fs::write(
                state.home.join("credentials/kimi-code.json"),
                serde_json::to_vec(winner).unwrap(),
            )
            .unwrap();
        }
        Json(serde_json::json!({
            "access_token": "hel-access",
            "refresh_token": "hel-refresh",
            "expires_in": 900,
            "scope": "kimi-code",
            "token_type": "Bearer"
        }))
    }

    /// Serve one refresh that steals the lock while it is in flight, and hand
    /// back what `ensure_fresh_kimi_token` made of it.
    async fn refresh_against_a_lock_thief(
        home: &Path,
        winner: Option<Value>,
    ) -> (Result<String>, std::path::PathBuf) {
        let credentials_path = home.join("credentials/kimi-code.json");
        tokio::fs::create_dir_all(credentials_path.parent().unwrap())
            .await
            .unwrap();
        let soon = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 10;
        tokio::fs::write(
            &credentials_path,
            serde_json::to_vec(&serde_json::json!({
                "access_token": "spent-access",
                "refresh_token": "spent-refresh",
                "expires_at": soon,
                "scope": "kimi-code",
                "token_type": "Bearer",
                "expires_in": 900
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        let app = Router::new()
            .route(
                "/api/oauth/token",
                post(test_kimi_refresh_stealing_the_lock),
            )
            .with_state(KimiThiefState {
                home: home.to_path_buf(),
                winner,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let environment =
            HashMap::from([("KIMI_CODE_OAUTH_HOST".into(), format!("http://{address}"))]);

        let token = ensure_fresh_kimi_token(
            &reqwest::Client::new(),
            home,
            &credentials_path,
            &environment,
            false,
            None,
        )
        .await;
        server.abort();
        (token, credentials_path)
    }

    #[tokio::test]
    async fn a_refresh_that_loses_its_lock_returns_the_winners_stored_token() {
        let home = tempfile::tempdir().unwrap();
        let winner = serde_json::json!({
            "access_token": "winner-access",
            "refresh_token": "winner-refresh",
            "expires_at": 4_102_444_800i64,
            "scope": "kimi-code",
            "token_type": "Bearer",
            "expires_in": 900
        });

        let (token, credentials_path) =
            refresh_against_a_lock_thief(home.path(), Some(winner)).await;

        assert_eq!(
            token.unwrap(),
            "winner-access",
            "a contested lock must not fail a refresh when a live token exists"
        );
        let saved = read_kimi_credentials(&credentials_path).await.unwrap();
        assert_eq!(
            saved.access_token, "winner-access",
            "Mjolnir must not clobber the credentials the lock's new holder stored"
        );
        assert!(
            home.path().join("oauth/kimi-code.lock").exists(),
            "Mjolnir must not remove a lock another holder owns"
        );
    }

    /// The pair the thief left behind is the one this refresh already spent, so
    /// it is dead: only Mjolnir's pair can still authenticate, and storing it is
    /// what keeps the peer's own recovery re-read working.
    #[tokio::test]
    async fn a_refresh_that_loses_its_lock_saves_its_pair_over_the_spent_one() {
        let home = tempfile::tempdir().unwrap();

        let (token, credentials_path) = refresh_against_a_lock_thief(home.path(), None).await;

        assert_eq!(token.unwrap(), "hel-access");
        let saved = read_kimi_credentials(&credentials_path).await.unwrap();
        assert_eq!(
            saved.access_token, "hel-access",
            "leaving the spent pair on disk would force a `kimi login`"
        );
        assert_eq!(saved.refresh_token, "hel-refresh");
        assert!(
            home.path().join("oauth/kimi-code.lock").exists(),
            "Mjolnir must not remove a lock another holder owns"
        );
    }

    /// The child is reaped by the shutdown, so a live pid means it was never
    /// stopped.
    #[cfg(unix)]
    fn process_is_gone(pid: i32) -> bool {
        // SAFETY: signal 0 only probes whether the process exists.
        unsafe { libc::kill(pid, 0) != 0 }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_profile_from_the_configuration_stops_its_codex_quota_client() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("codex");
        let pid_file = directory.path().join("codex.pid");
        // A `codex app-server` stand-in: answer one quota refresh, then stay
        // alive on stdin the way the real one does between refreshes.
        std::fs::write(
            &executable,
            r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_QUOTA_TEST_PID"
IFS= read -r line || exit 0
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 0
IFS= read -r line || exit 0
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
IFS= read -r line || exit 0
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let request = QuotaRefreshRequest {
            profile_id: "codex-1".into(),
            harness: HarnessKind::Codex,
            source_home: directory.path().to_path_buf(),
            executable: None,
            environment: BTreeMap::from([
                (
                    "PATH".to_owned(),
                    directory.path().to_string_lossy().into_owned(),
                ),
                (
                    "CODEX_QUOTA_TEST_PID".to_owned(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ]),
            cwd: directory.path().to_path_buf(),
        };

        let mut quotas = QuotaManager::default();
        quotas.refresh_profiles(vec![request], |_| async {}).await;

        assert_eq!(
            quotas.reports()["codex-1"].error,
            None,
            "the stand-in app-server must answer the quota query"
        );
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert!(
            !process_is_gone(pid),
            "the app-server child is cached between refreshes"
        );

        // The profile leaves the configuration, so the next batch no longer
        // carries it.
        quotas.refresh_profiles(Vec::new(), |_| async {}).await;

        assert!(
            process_is_gone(pid),
            "a profile removed from the configuration must not leave its `codex app-server` child running"
        );
        quotas.shutdown().await;
    }
}
