//! One-pane quota collection for Mjolnir harness profiles.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Days, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude_usage;
use crate::codex_usage::{self, CodexUsageClient, CodexUsageStatus};
use crate::grok_usage;
use mj_core::config::{HarnessKind, HarnessProfile, harness_authentication_marker};
use mj_core::credentials::{
    MAX_CREDENTIAL_BYTES, credential_expiry, credential_fingerprint, credential_freshness,
};

pub use mj_client::quota::{API_LABEL, ProfileQuota, QuotaWindow, projects_exhaustion};

#[derive(Debug, Clone)]
pub struct QuotaRefreshRequest {
    pub profile_id: String,
    pub harness: HarnessKind,
    pub source_home: std::path::PathBuf,
    pub environment: BTreeMap<String, String>,
    pub cwd: std::path::PathBuf,
    /// The custom model provider this profile authenticates to with an API
    /// key, when it has one. A profile using its harness's own login has
    /// `None` here and keeps the harness's native quota path.
    pub provider: Option<ProviderCredential>,
}

/// Where a profile's quota lives when the profile authenticates with an API
/// key against a provider named in its harness configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCredential {
    /// Provider id from the harness configuration, for error messages.
    pub id: String,
    /// Host of the provider's base URL, for example `api.z.ai`.
    pub host: String,
    pub api_key: String,
}

impl QuotaRefreshRequest {
    /// Build the request for one configured profile. The harness home
    /// environment is composed here so every caller asks for quota the same
    /// way, and so a provider key is read from exactly one place.
    pub fn for_profile(
        profile_id: &str,
        profile: &HarnessProfile,
        cwd: std::path::PathBuf,
    ) -> Self {
        let mut environment = profile.environment.clone();
        profile
            .kind
            .configure_home_environment(&profile.home, &mut environment);
        Self {
            profile_id: profile_id.to_owned(),
            harness: profile.kind,
            source_home: profile.home.clone(),
            environment,
            cwd,
            provider: provider_credential(profile),
        }
    }
}

fn provider_credential(profile: &HarnessProfile) -> Option<ProviderCredential> {
    let provider = profile.codex_provider().ok().flatten()?;
    let env_key = provider.env_key.as_deref()?;
    let api_key = profile.environment.get(env_key)?;
    Some(ProviderCredential {
        id: provider.id.clone(),
        host: provider.host()?,
        api_key: api_key.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRefreshOutcome {
    pub report: ProfileQuota,
    pub credentials_changed: bool,
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
        self.reports
            .retain(|profile_id, _| batch.contains(profile_id));
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
        environment,
        cwd,
        provider,
    } = request;
    let environment = environment.into_iter().collect::<HashMap<_, _>>();
    let refreshed_at_epoch_seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let result = match harness {
        // A Codex profile that authenticates with an API key against a custom
        // provider has no ChatGPT login to refresh and no ChatGPT rate-limit
        // windows to read. Its quota, when the provider publishes one, comes
        // from the provider's own endpoint.
        HarnessKind::Codex if provider.is_some() => {
            let provider = provider.expect("guarded by the match arm");
            if crate::zai_usage::serves_quota(&provider.host) {
                crate::zai_usage::query(&provider.host, &provider.api_key)
                    .await
                    .map(|windows| ProfileQuota {
                        profile_id: profile_id.clone(),
                        harness,
                        windows: windows
                            .into_iter()
                            .map(|window| QuotaWindow {
                                label: window.label,
                                remaining_percent: Some(window.remaining_percent),
                                used: window.used,
                                limit: window.limit,
                                resets: window.resets_at.and_then(format_reset_local_seconds),
                                resets_at_epoch_seconds: window.resets_at,
                            })
                            .collect(),
                        extra: None,
                        error: None,
                        refreshed_at_epoch_seconds,
                    })
            } else {
                // A provider that publishes no quota endpoint bills by usage,
                // so there is no allowance to report. Saying "API" rather than
                // raising an error keeps the dashboard from showing the profile
                // as unavailable and lets the utility ranker treat it as
                // healthy, which matches how it actually behaves.
                Ok(ProfileQuota {
                    profile_id: profile_id.clone(),
                    harness,
                    windows: Vec::new(),
                    extra: Some(API_LABEL.to_owned()),
                    error: None,
                    refreshed_at_epoch_seconds,
                })
            }
        }
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
                            resets: window.resets_at.and_then(format_reset_local_seconds),
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
                        .and_then(normalize_reset_text),
                    resets_at_epoch_seconds: window
                        .reset_context
                        .as_deref()
                        .and_then(normalize_reset_epoch_seconds),
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
            grok_usage::query(source_home.clone(), cwd, environment)
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
                        resets: report.resets_at.and_then(format_reset_local_seconds),
                        resets_at_epoch_seconds: report.resets_at,
                    }],
                    extra: None,
                    error: None,
                    refreshed_at_epoch_seconds,
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        }
        HarnessKind::Muse => crate::muse_usage::query(&source_home, &environment)
            .await
            .map(|report| ProfileQuota {
                profile_id: profile_id.clone(),
                harness,
                windows: report
                    .windows
                    .into_iter()
                    .map(|window| QuotaWindow {
                        label: window.label,
                        remaining_percent: Some(window.remaining_percent),
                        used: None,
                        limit: None,
                        resets: window.resets_at.and_then(format_reset_local_seconds),
                        resets_at_epoch_seconds: window.resets_at,
                    })
                    .collect(),
                extra: report.note,
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
/// it is stale or when the server rejected it. The refresh runs under the
/// lock the Kimi Code CLI also takes, and the pair is re-read once the lock is
/// held, so a refresh another process just finished is used instead of
/// spending its new refresh token again.
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

    // Held until this function returns, released on drop.
    let _refresh_lock = KimiRefreshLock::acquire(home, KIMI_LOCK_WAIT).await?;
    let active = read_kimi_credentials(credentials_path).await?;
    let changed_while_waiting = active.differs_from(&initial);
    if (!force && !active.needs_refresh())
        || (force
            && (changed_while_waiting
                || rejected_token.is_some_and(|token| token != active.access_token)))
    {
        return Ok(active.access_token);
    }
    if active.refresh_token.is_empty() {
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
                return Ok(recovery.access_token);
            }
        }
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
    save_kimi_credentials(credentials_path, &refreshed)?;
    Ok(refreshed.access_token)
}

fn save_kimi_credentials(path: &Path, credentials: &KimiCredentials) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(credentials)?;
    body.push(b'\n');
    mj_core::config::atomic_write(path, &body).context("save refreshed Kimi Code credentials")
}

fn required_string<'a>(payload: &'a Value, key: &str, context: &str) -> Result<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{context} is missing {key}"))
}

/// The Kimi Code CLI serializes token refreshes with `proper-lockfile` on the
/// directory `oauth/kimi-code.lock` (`stale: 5_000`): a holder keeps the
/// directory's modification time moving, and a lock whose time stopped for
/// longer than the stale window is abandoned and may be removed. Mjolnir takes
/// the same directory the same way, so its refresh and the CLI's never spend
/// the same single-use refresh token.
struct KimiRefreshLock {
    path: std::path::PathBuf,
    heartbeat: tokio::task::JoinHandle<()>,
}

/// How often a holder republishes the lock's modification time. It fits inside
/// the CLI's 5 second stale window several times over.
const KIMI_LOCK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// How long a lock's modification time must be still before Mjolnir removes
/// it as abandoned. Longer than the CLI's own 5 seconds, so Mjolnir never
/// breaks a lock the CLI would still consider live.
const KIMI_LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
const KIMI_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const KIMI_LOCK_WAIT: Duration = Duration::from_secs(60);

impl KimiRefreshLock {
    async fn acquire(home: &Path, wait: Duration) -> Result<Self> {
        let oauth_dir = home.join("oauth");
        tokio::fs::create_dir_all(&oauth_dir)
            .await
            .context("prepare Kimi Code OAuth lock")?;
        // proper-lockfile locks `<file>.lock` for a file that must exist.
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(oauth_dir.join("kimi-code"))
            .await
            .context("prepare Kimi Code OAuth lock sentinel")?;
        let path = oauth_dir.join("kimi-code.lock");
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::fs::create_dir(&path).await {
                Ok(()) => return Ok(Self::held(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if tokio::time::Instant::now() >= deadline {
                        bail!(
                            "timed out waiting for Kimi Code OAuth refresh lock {}",
                            path.display()
                        );
                    }
                    if !break_stale_kimi_lock(&path).await {
                        tokio::time::sleep(KIMI_LOCK_RETRY_INTERVAL).await;
                    }
                }
                Err(error) => return Err(error).context("acquire Kimi Code OAuth refresh lock"),
            }
        }
    }

    fn held(path: std::path::PathBuf) -> Self {
        let heartbeat_path = path.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(KIMI_LOCK_HEARTBEAT_INTERVAL).await;
                if let Err(error) = touch_kimi_lock(&heartbeat_path, SystemTime::now()) {
                    tracing::debug!(path = %heartbeat_path.display(), %error, "heartbeat Kimi Code OAuth refresh lock");
                }
            }
        });
        Self { path, heartbeat }
    }
}

impl Drop for KimiRefreshLock {
    fn drop(&mut self) {
        self.heartbeat.abort();
        match std::fs::remove_dir(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "release Kimi Code OAuth refresh lock");
            }
        }
    }
}

/// The lock directory's modification time, or `None` when it is gone.
fn kimi_lock_mtime(path: &Path) -> std::io::Result<Option<SystemTime>> {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.modified().map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Publish a lock directory's modification time. Windows opens a directory
/// handle only under backup semantics.
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

/// Remove a lock whose holder stopped heartbeating, so a holder killed
/// mid-refresh cannot block every later refresh. Returns whether the caller
/// should retry the create at once.
async fn break_stale_kimi_lock(path: &Path) -> bool {
    let modified = match kimi_lock_mtime(path) {
        Ok(Some(modified)) => modified,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "inspect Kimi Code OAuth refresh lock");
            return false;
        }
    };
    // A modification time in the future (the CLI rounds its writes up) is not
    // stale.
    let Some(age) = SystemTime::now()
        .duration_since(modified)
        .ok()
        .filter(|age| *age >= KIMI_LOCK_STALE_AFTER)
    else {
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
        .and_then(format_reset_local)
        .or_else(|| value.as_str().and_then(normalize_reset_text))
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
        .or_else(|| value.as_str().and_then(normalize_reset_epoch_seconds))
}

/// Format a Unix reset timestamp as wall-clock time in the machine's local
/// time zone. Accepts seconds or milliseconds and rejects non-finite or
/// out-of-range values.
pub(crate) fn format_reset_local(epoch: f64) -> Option<String> {
    if !epoch.is_finite() {
        return None;
    }
    let seconds = if epoch.abs() >= 1_000_000_000_000.0 {
        (epoch / 1000.0).trunc() as i64
    } else {
        epoch.trunc() as i64
    };
    let local = Local.timestamp_opt(seconds, 0).single()?;
    Some(format_reset_label(local.fixed_offset()))
}

pub(crate) fn format_reset_local_seconds(epoch: i64) -> Option<String> {
    format_reset_local(epoch as f64)
}

/// Normalize a provider's textual reset value to the compact 24-hour form
/// used by the dashboard. A time-only value is the next occurrence of that
/// wall-clock time; Claude Code uses this shape for its five-hour window.
pub(crate) fn normalize_reset_text(value: &str) -> Option<String> {
    normalize_reset_at(value, Local::now().fixed_offset()).map(format_reset_label)
}

pub(crate) fn normalize_reset_epoch_seconds(value: &str) -> Option<i64> {
    normalize_reset_at(value, Local::now().fixed_offset()).map(|reset| reset.timestamp())
}

fn normalize_reset_at(value: &str, now: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(epoch) = value.parse::<f64>() {
        let seconds = if epoch.abs() >= 1_000_000_000_000.0 {
            (epoch / 1000.0).trunc() as i64
        } else {
            epoch.trunc() as i64
        };
        return Local
            .timestamp_opt(seconds, 0)
            .single()
            .map(|reset| reset.fixed_offset());
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.with_timezone(&Local).fixed_offset());
    }

    let value = value
        .strip_prefix("at ")
        .unwrap_or(value)
        .split('(')
        .next()
        .unwrap_or(value)
        .trim()
        .trim_end_matches(',');
    let parse_time = |value: &str| {
        let value = value
            .to_ascii_lowercase()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        let value = ["am", "pm"]
            .into_iter()
            .find_map(|suffix| {
                let hour = value.strip_suffix(suffix)?;
                (!hour.contains(':')).then(|| format!("{hour}:00{suffix}"))
            })
            .unwrap_or(value);
        ["%I:%M%P", "%I%P", "%H:%M"]
            .iter()
            .find_map(|format| NaiveTime::parse_from_str(&value, format).ok())
    };

    // Claude has used both `Aug 14 at 4am` and `Aug 14, 4am` across
    // releases. Keep the provider punctuation out of the date/time parsers.
    let dated_time = value.split_once(" at ").or_else(|| {
        value
            .split_once(',')
            .map(|(date, time)| (date, time.trim()))
    });
    if let Some((date, time)) = dated_time {
        let time = parse_time(time.trim())?;
        let date = date.trim().trim_end_matches(',');
        let date = match date.to_ascii_lowercase().as_str() {
            "today" => now.date_naive(),
            "tomorrow" => now.date_naive().checked_add_days(Days::new(1))?,
            _ => NaiveDate::parse_from_str(
                &format!("{} {}", date.replace(',', ""), now.year()),
                "%b %e %Y",
            )
            .ok()?,
        };
        return now
            .timezone()
            .from_local_datetime(&date.and_time(time))
            .single();
    }

    let time = parse_time(value)?;
    let mut date = now.date_naive();
    let mut reset = now
        .timezone()
        .from_local_datetime(&date.and_time(time))
        .single()?;
    if reset <= now {
        date = date.checked_add_days(Days::new(1))?;
        reset = now
            .timezone()
            .from_local_datetime(&date.and_time(time))
            .single()?;
    }
    Some(reset)
}

/// Pure formatter split from local-zone discovery for deterministic tests.
fn format_reset_label(reset: DateTime<FixedOffset>) -> String {
    reset.format("%H:%M %b %-d").to_string()
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

    fn zai_profile(home: &Path, base_url: &str) -> HarnessProfile {
        std::fs::write(
            home.join("config.toml"),
            format!(
                "model_provider = \"zai\"\n\
                 [model_providers.zai]\n\
                 base_url = \"{base_url}\"\n\
                 env_key = \"ZAI_API_KEY\"\n\
                 wire_api = \"responses\"\n"
            ),
        )
        .unwrap();
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: home.to_path_buf(),
            environment: [("ZAI_API_KEY".to_owned(), "coding-plan-key".to_owned())]
                .into_iter()
                .collect(),
            context_window_bytes: None,
            guardian_review_model: None,
        }
    }

    #[test]
    fn a_custom_provider_profile_asks_its_provider_for_quota_not_chatgpt() {
        let home = tempfile::tempdir().unwrap();
        let request = QuotaRefreshRequest::for_profile(
            "glm",
            &zai_profile(home.path(), "https://api.z.ai/api/v1"),
            home.path().to_path_buf(),
        );
        assert_eq!(
            request.provider,
            Some(ProviderCredential {
                id: "zai".to_owned(),
                host: "api.z.ai".to_owned(),
                api_key: "coding-plan-key".to_owned(),
            })
        );
        assert!(crate::zai_usage::serves_quota(
            &request.provider.unwrap().host
        ));

        // A Codex profile that uses its own ChatGPT login keeps that path.
        let native = tempfile::tempdir().unwrap();
        let request = QuotaRefreshRequest::for_profile(
            "work",
            &HarnessProfile {
                enabled: true,
                kind: HarnessKind::Codex,
                home: native.path().to_path_buf(),
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
            native.path().to_path_buf(),
        );
        assert_eq!(request.provider, None);
    }

    #[tokio::test]
    async fn a_provider_without_a_quota_endpoint_reports_usage_pricing() {
        let home = tempfile::tempdir().unwrap();
        let request = QuotaRefreshRequest::for_profile(
            "other",
            &zai_profile(home.path(), "https://example.invalid/v1"),
            home.path().to_path_buf(),
        );
        let (outcome, _) = refresh_profile(request, None).await;
        assert_eq!(outcome.report.error, None);
        assert!(outcome.report.windows.is_empty());
        assert!(outcome.report.is_usage_priced());
        assert_eq!(outcome.report.compact(), API_LABEL);
    }

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
    fn compact_shows_other_errors_as_unavailable() {
        let report = ProfileQuota {
            profile_id: "claude2".into(),
            harness: HarnessKind::Claude,
            windows: vec![],
            extra: None,
            error: Some("query Claude usage: HTTP 429".into()),
            refreshed_at_epoch_seconds: 0,
        };
        assert_eq!(report.compact(), "unavailable");
        assert_eq!(report.error_label().as_deref(), Some("unavailable"));
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
        let environment = BTreeMap::from([
            (
                "GROK_HOME".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            ),
            (
                "PATH".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            ),
        ]);

        let (outcome, _) = refresh_profile(
            QuotaRefreshRequest {
                profile_id: "grok".into(),
                harness: HarnessKind::Grok,
                source_home: directory.path().to_path_buf(),
                environment,
                cwd: directory.path().to_path_buf(),
                provider: None,
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
                environment,
                cwd: directory.to_path_buf(),
                provider: None,
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
                environment: BTreeMap::from([(
                    "PATH".to_owned(),
                    directory.path().to_string_lossy().into_owned(),
                )]),
                cwd: directory.path().to_path_buf(),
                provider: None,
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
    async fn muse_quota_refresh_recovers_and_populates_dashboard_windows() {
        let directory = tempfile::tempdir().unwrap();
        let credentials = br#"{"providers":{"meta":{"access_token":"profile-token"}}}"#;
        std::fs::write(directory.path().join("auth.json"), credentials).unwrap();
        let rejected = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let app = Router::new()
            .route(
                "/muse-code/key",
                post(
                    |State(rejected): State<Arc<std::sync::atomic::AtomicBool>>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        assert_eq!(headers["authorization"], "Bearer profile-token");
                        assert_eq!(body, serde_json::json!({"onboard": false}));
                        if rejected.load(std::sync::atomic::Ordering::SeqCst) {
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "api_key": "must-not-be-persisted",
                                "subs_usage": {
                                    "weekly": {"used_percent": 1, "resets_at": 1789344000},
                                    "window": {
                                        "used_percent": 3,
                                        "window_duration_mins": 300,
                                        "resets_at": 1788890595
                                    }
                                }
                            })),
                        )
                    },
                ),
            )
            .with_state(rejected.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let request = QuotaRefreshRequest {
            profile_id: "muse".into(),
            harness: HarnessKind::Muse,
            source_home: directory.path().to_path_buf(),
            environment: BTreeMap::from([(
                "TBH_MINT_BASE_URL".into(),
                format!("http://{address}"),
            )]),
            cwd: directory.path().to_path_buf(),
            provider: None,
        };
        let mut manager = QuotaManager::default();
        manager
            .refresh_profiles(vec![request.clone()], |_| async {})
            .await;
        assert!(manager.reports()["muse"].error.is_some());
        rejected.store(false, std::sync::atomic::Ordering::SeqCst);
        manager
            .refresh_profiles(vec![request], |outcome| async move {
                assert!(!outcome.credentials_changed);
            })
            .await;
        let report = &manager.reports()["muse"];
        assert_eq!(report.error, None);
        assert_eq!(report.extra, None);
        assert_eq!(report.weekly_window().unwrap().remaining_percent, Some(99));
        assert_eq!(
            report.five_hour_window().unwrap().remaining_percent,
            Some(97)
        );
        assert_eq!(
            report.weekly_window().unwrap().resets_at_epoch_seconds,
            Some(1789344000)
        );
        assert!(report.weekly_window().unwrap().resets.is_some());
        assert!(report.compact().contains("Week 99% left"));
        assert_eq!(
            std::fs::read(directory.path().join("auth.json")).unwrap(),
            credentials
        );
        manager.shutdown().await;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
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
                environment: BTreeMap::new(),
                cwd: directory.path().to_path_buf(),
                provider: None,
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
        assert!(projects_exhaustion(&window, 0));

        let sustainable = QuotaWindow {
            remaining_percent: Some(80),
            ..window
        };
        assert!(!projects_exhaustion(&sustainable, 0));
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
        let held = KimiRefreshLock::acquire(home.path(), Duration::from_secs(10))
            .await
            .expect("an orphaned lock must not block a refresh");
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(5),
            "acquisition waited {waited:?}"
        );
        drop(held);
        assert!(!lock.exists(), "the released lock must be gone");
    }

    #[tokio::test]
    async fn a_heartbeating_kimi_refresh_lock_is_not_broken_by_a_waiter() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");
        std::fs::create_dir_all(&lock).unwrap();

        let error = KimiRefreshLock::acquire(home.path(), Duration::from_millis(600))
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
    /// five seconds old, so Mjolnir's beats have to be frequent enough that a
    /// stalled heartbeat task still cannot cost it a live lock.
    #[tokio::test]
    async fn a_held_kimi_lock_republishes_its_mtime_several_times_per_cli_break_window() {
        let home = tempfile::tempdir().unwrap();
        let held = KimiRefreshLock::acquire(home.path(), KIMI_LOCK_WAIT)
            .await
            .unwrap();
        let lock = home.path().join("oauth/kimi-code.lock");

        // Half the peer's break window: two beats have to land inside it, so
        // Mjolnir publishes at least four times per window and can miss several in
        // a row and still hold the lock.
        const KIMI_CLI_LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
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
        drop(held);
    }

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
            provider: None,
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

    #[test]
    fn reset_time_normalization_uses_24_hour_month_day_format() {
        let paris = FixedOffset::east_opt(2 * 3_600).expect("offset");
        let reset = paris
            .with_ymd_and_hms(2026, 6, 17, 16, 49, 0)
            .single()
            .expect("instant");
        assert_eq!(format_reset_label(reset), "16:49 Jun 17");
        assert_eq!(
            normalize_reset_text("Jun 17 at 4:49pm").as_deref(),
            Some("16:49 Jun 17")
        );
    }

    #[test]
    fn reset_timestamp_accepts_seconds_and_milliseconds() {
        let seconds = 1_781_712_540_f64;
        assert_eq!(
            format_reset_local(seconds),
            format_reset_local(seconds * 1_000.0)
        );
        assert_eq!(
            format_reset_local(seconds),
            format_reset_local_seconds(seconds as i64)
        );
    }

    #[test]
    fn time_only_reset_is_rendered_as_the_next_datetime() {
        let zone = FixedOffset::west_opt(5 * 3_600).expect("offset");
        let now = zone
            .with_ymd_and_hms(2026, 8, 10, 14, 0, 0)
            .single()
            .expect("now");
        assert_eq!(
            normalize_reset_at("3:30 PM (America/Chicago)", now)
                .map(format_reset_label)
                .as_deref(),
            Some("15:30 Aug 10")
        );
        assert_eq!(
            normalize_reset_at("at 1pm (America/Chicago)", now)
                .map(format_reset_label)
                .as_deref(),
            Some("13:00 Aug 11")
        );
    }

    #[test]
    fn claude_comma_separated_reset_is_normalized() {
        let zone = FixedOffset::west_opt(5 * 3_600).expect("offset");
        let now = zone
            .with_ymd_and_hms(2026, 8, 11, 7, 0, 0)
            .single()
            .expect("now");
        assert_eq!(
            normalize_reset_at("Aug 14, 4am (America/Chicago)", now)
                .map(format_reset_label)
                .as_deref(),
            Some("04:00 Aug 14")
        );
    }
}
