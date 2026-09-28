//! One-pane quota collection for Mjolnir harness profiles.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Days, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone};
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
    /// Provider configuration selected native OpenAI, not merely a Codex harness.
    pub native_openai: bool,
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
        let mut environment = profile.environment.resolved().clone();
        profile.kind.configure_profile_home_environment(
            &profile.home,
            mj_core::config::HarnessHost::current(),
            &mut environment,
        );
        let configured_provider = profile.codex_provider();
        Self {
            native_openai: profile.kind == HarnessKind::Codex
                && matches!(configured_provider, Ok(None)),
            profile_id: profile_id.to_owned(),
            harness: profile.kind,
            source_home: profile.home.clone(),
            environment,
            cwd,
            provider: provider_credential_from(
                profile,
                configured_provider.ok().flatten().as_ref(),
            ),
        }
    }
}

/// The provider credential a Codex profile's quota is read with, or `None`
/// when the profile uses ChatGPT's own login. `mj doctor` reads the same
/// answer to say where each profile's quota comes from.
pub(crate) fn provider_credential(profile: &HarnessProfile) -> Option<ProviderCredential> {
    provider_credential_from(profile, profile.codex_provider().ok().flatten().as_ref())
}

fn provider_credential_from(
    profile: &HarnessProfile,
    provider: Option<&mj_core::codex_provider::CodexProvider>,
) -> Option<ProviderCredential> {
    let provider = provider?;
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
            log_quota_change(
                self.reports.get(&outcome.report.profile_id),
                &outcome.report,
            );
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

/// Log why a profile's quota could not be read, and when it can be read
/// again.
///
/// A failed probe becomes the report's `error`, and the panes show only a
/// short label for it ("unavailable", "login expired", "rate limited"), so
/// without this line the reason was recorded nowhere (R12-2). A profile that
/// keeps failing the same way is logged once, not on every refresh.
fn log_quota_change(previous: Option<&ProfileQuota>, report: &ProfileQuota) {
    let previous_error = previous.and_then(|previous| previous.error.as_deref());
    match report.error.as_deref() {
        Some(error) if previous_error != Some(error) => tracing::info!(
            profile_id = %report.profile_id,
            harness = report.harness.display_name(),
            shown_as = report.error_label().unwrap_or_default(),
            error,
            "could not read the profile's quota"
        ),
        None if previous_error.is_some() => tracing::info!(
            profile_id = %report.profile_id,
            harness = report.harness.display_name(),
            "the profile's quota can be read again"
        ),
        _ => {}
    }
}

async fn refresh_profile(
    request: QuotaRefreshRequest,
    mut codex_client: Option<CodexUsageClient>,
) -> (QuotaRefreshOutcome, Option<CodexUsageClient>) {
    let cache_identity = request.cache_identity();
    let credential_path = harness_authentication_marker(request.harness, &request.source_home);
    let fingerprint_path = if request.harness == HarnessKind::Kimi {
        let mut config =
            anvil_client::kimi_auth::KimiServiceConfig::from_home(&request.source_home);
        config.environment = request.environment.clone();
        config.credentials_path()
    } else {
        Ok(credential_path.clone())
    };
    let credential_before = credential_marker_fingerprint(&fingerprint_path).await;
    let QuotaRefreshRequest {
        native_openai,
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
                        banked_resets: None,
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
                    banked_resets: None,
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
                    banked_resets: report.banked_resets.filter(|_| native_openai),
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
                banked_resets: report.banked_resets,
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
                    banked_resets: None,
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
                    banked_resets: None,
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
                banked_resets: None,
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
        banked_resets: None,
        profile_id,
        harness,
        windows: Vec::new(),
        extra: None,
        error: Some(error.to_string()),
        refreshed_at_epoch_seconds,
    });
    // The daemon reads the reset-time cache when a session runs out of quota,
    // and the daemon's own refreshes keep it: the web server's quota poller,
    // and the refresh quota recovery makes just before it reads the cache.
    // The dashboard polls the same quota for its display, but it has no
    // database writer and must not write the store, so it leaves the cache to
    // the daemon (R9-1).
    if report.error.is_none() {
        if crate::database::database_writer_installed() {
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
        } else {
            tracing::debug!(
                profile_id = %report.profile_id,
                "this process has no database writer; leaving quota reset times to the daemon"
            );
        }
    }
    let credential_after = credential_marker_fingerprint(&fingerprint_path).await;
    let credentials_changed = match (credential_before, credential_after) {
        (Ok(before), Ok(after)) => before != after,
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(profile_id = %report.profile_id, %error, "could not fingerprint quota credentials");
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

async fn credential_marker_fingerprint(
    path: &Result<std::path::PathBuf>,
) -> Result<Option<String>> {
    let path = path
        .as_ref()
        .map_err(|error| anyhow::anyhow!("resolve credential marker: {error}"))?;
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
    let auth = crate::kimi_auth::KimiAuth::new(
        home,
        environment
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let payload = auth.usage().await?;
    parse_kimi_usage(&payload)
}

fn parse_kimi_usage(payload: &Value) -> Result<(Vec<QuotaWindow>, Option<String>)> {
    if payload.get("kind").and_then(Value::as_str) == Some("api_key") {
        return Ok((Vec::new(), Some(API_LABEL.to_owned())));
    }
    let usages = payload
        .pointer("/quota/usages")
        .and_then(Value::as_object)
        .context("Kimi vendor usage response is missing quota.usages")?;
    let mut windows = Vec::new();
    for (key, label) in [
        ("limit7d", "Week"),
        ("limit5h", "5H"),
        ("monthTotal", "Month"),
        ("monthCode", "Monthly code"),
    ] {
        let Some(value) = usages.get(key) else {
            continue;
        };
        let ratio = value
            .get("usedRatio")
            .and_then(Value::as_f64)
            .with_context(|| format!("Kimi vendor usage {key} is missing usedRatio"))?;
        let reset = value.get("resetAt");
        windows.push(QuotaWindow {
            label: label.to_owned(),
            remaining_percent: Some(((1.0 - ratio.clamp(0.0, 1.0)) * 100.0).round() as u8),
            used: None,
            limit: None,
            resets: reset.and_then(normalize_kimi_reset),
            resets_at_epoch_seconds: reset.and_then(kimi_reset_epoch_seconds),
        });
    }
    let extra = payload
        .pointer("/quota/extraUsage/balanceCents")
        .and_then(Value::as_i64)
        .map(|value| {
            format!(
                "extra {:.2} {} remaining",
                value as f64 / 100.0,
                payload
                    .pointer("/quota/extraUsage/currency")
                    .and_then(Value::as_str)
                    .unwrap_or("USD")
            )
        });
    Ok((windows, extra))
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
