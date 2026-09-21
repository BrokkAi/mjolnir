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
    pub(crate) fn cache_identity(&self) -> String {
        use sha2::{Digest, Sha256};
        // Hash configuration, never write credentials into the cache key.
        let mut hash = Sha256::new();
        hash.update(
            serde_json::to_vec(&(&self.profile_id, self.harness, &self.environment))
                .expect("serializable profile"),
        );
        hash.update(self.source_home.as_os_str().as_encoded_bytes());
        if let Some(provider) = &self.provider {
            hash.update(provider.host.as_bytes());
            hash.update(provider.api_key.as_bytes());
        }
        mj_core::hex::lower_hex(hash.finalize())
    }

    /// Build the request for one configured profile. The harness home
    /// environment is composed here so every caller asks for quota the same
    /// way, and so a provider key is read from exactly one place.
    pub fn for_profile(
        profile_id: &str,
        profile: &HarnessProfile,
        cwd: std::path::PathBuf,
    ) -> Self {
        let mut environment = profile.environment.clone();
        profile.kind.configure_home_environment(
            &profile.home,
            mj_core::config::HarnessHost::current(),
            &mut environment,
        );
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
    let cache_identity = request.cache_identity();
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
    if report.error.is_none() {
        let cached = report.clone();
        match tokio::task::spawn_blocking(move || {
            crate::database::save_quota_cache(&cache_identity, &cached)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not preserve quota reset times"),
            Err(error) => tracing::warn!(%error, "quota cache task failed"),
        }
    }
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

pub(crate) fn normalize_reset_at(
    value: &str,
    now: DateTime<FixedOffset>,
) -> Option<DateTime<FixedOffset>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((clock, zone)) = value.rsplit_once('(') {
        let zone: chrono_tz::Tz = zone.strip_suffix(')')?.trim().parse().ok()?;
        let local_now = now.with_timezone(&zone);
        let parsed = normalize_reset_at(clock.trim(), local_now.fixed_offset())?;
        return zone
            .from_local_datetime(&parsed.naive_local())
            .single()
            .map(|t| t.fixed_offset());
    }
    if let Some((clock, offset)) = value.rsplit_once(' ')
        && (offset.starts_with('+') || offset.starts_with('-'))
    {
        let offset: FixedOffset = offset.parse().ok()?;
        return normalize_reset_at(clock, now.with_timezone(&offset));
    }
    if let Some(clock) = value
        .strip_suffix(" UTC")
        .or_else(|| value.strip_suffix(" GMT"))
    {
        return normalize_reset_at(clock, now.with_timezone(&chrono::Utc).fixed_offset());
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
mod tests;

mod recovery;
pub(crate) use recovery::{merge_reset_windows, message_reset, recovery_reset};
