//! One-pane quota collection for Mjolnir harness profiles.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Days, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::Instrument;

use crate::claude_usage;
use crate::codex_usage::{self, CodexUsageClient, CodexUsageStatus};
use crate::grok_usage;
use mj_core::codex_provider::BuiltInCodexProvider;
use mj_core::config::{AuthScheme, HarnessKind, HarnessProfile, harness_authentication_marker};
use mj_core::credentials::{
    MAX_CREDENTIAL_BYTES, credential_expiry, credential_fingerprint, credential_freshness,
};

pub use mj_client::quota::{
    API_LABEL, NOT_REPORTED_LABEL, ProfileQuota, QuotaWindow, projects_exhaustion,
};

/// Clone a daemon report for a user-facing quota surface, retaining only
/// unscoped windows, Claude's Fable allowance and Antigravity's two pools.
/// Internal selection and health keep the complete report.
pub fn surface_quota_report(report: &ProfileQuota) -> ProfileQuota {
    if report.harness == HarnessKind::Antigravity {
        return report.clone();
    }
    let mut visible = report.clone();
    let fable_weekly = report.fable_weekly_window().cloned();
    visible
        .windows
        .retain(|window| window.model_scope.is_none());
    if let Some(window) = fable_weekly {
        visible.windows.push(window);
    }
    visible
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaRefreshRequest {
    pub profile_id: String,
    pub harness: HarnessKind,
    pub auth_scheme: AuthScheme,
    pub source_home: std::path::PathBuf,
    pub environment: BTreeMap<String, String>,
    pub cwd: std::path::PathBuf,
    /// Where a Codex profile's quota comes from. Other harnesses ignore it.
    pub codex_source: CodexQuotaSource,
    /// Whether an OpenCode profile signs in through an environment-supplied
    /// provider key rather than a login OpenCode itself stores, so it is
    /// billed by usage the way Dsh and a Codex custom provider are. Other
    /// harnesses ignore it.
    pub opencode_usage_priced: bool,
}

/// Whether a quota check for this harness can call the managed installer at
/// all (a host harness or the pinned Antigravity), decided from the kind alone, before reading any profile file. Whether
/// one profile does ([`QuotaRefreshRequest::runs_host_harness`]) also depends
/// on its login and provider.
#[cfg(unix)]
pub fn kind_may_need_managed_quota_harness(kind: HarnessKind) -> bool {
    matches!(
        kind,
        HarnessKind::Codex | HarnessKind::Grok | HarnessKind::Claude | HarnessKind::Antigravity
    )
}

/// Where a Codex profile's quota comes from, decided once from its provider
/// configuration. Only Codex's own OpenAI login has ChatGPT rate-limit
/// windows; every other provider must never be probed through that login,
/// or it reports the ChatGPT account's quota as its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexQuotaSource {
    /// No `model_provider`, or the built-in `openai` provider.
    ChatGptLogin,
    /// A custom provider whose API key and host Mjolnir can read; its quota
    /// comes from the provider's own endpoint when it publishes one.
    Provider(ProviderCredential),
    /// Any other provider: a local or Bedrock built-in, or a custom provider
    /// whose key Mjolnir cannot read. It has no allowance to report.
    UsagePriced { provider: String },
    /// The profile's Codex configuration could not be read.
    Unreadable(String),
}

impl CodexQuotaSource {
    /// The source for one configured profile. `mj doctor` reads the same
    /// answer to say where each profile's quota comes from.
    pub fn of_profile(profile: &HarnessProfile) -> Self {
        let provider = match profile.codex_provider() {
            Ok(None) => return Self::ChatGptLogin,
            Ok(Some(provider)) => provider,
            Err(error) => return Self::Unreadable(format!("{error:#}")),
        };
        if provider.built_in() == Some(BuiltInCodexProvider::OpenAi) {
            return Self::ChatGptLogin;
        }
        match (
            provider.custom(),
            profile.codex_provider_api_key(),
            provider.host(),
        ) {
            (Some(_), Some(api_key), Some(host)) => Self::Provider(ProviderCredential {
                id: provider.id().to_owned(),
                host,
                api_key,
            }),
            _ => Self::UsagePriced {
                provider: provider.id().to_owned(),
            },
        }
    }
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
    pub fn cache_identity(&self) -> String {
        use sha2::{Digest, Sha256};
        // Hash configuration, never write credentials into the cache key.
        let mut hash = Sha256::new();
        hash.update(
            serde_json::to_vec(&(&self.profile_id, self.harness, &self.environment))
                .expect("serializable profile"),
        );
        hash.update(self.source_home.as_os_str().as_encoded_bytes());
        // The ChatGPT login hashes nothing more, so its stored reports stay
        // valid. Every other source gets its own key, so a report from a
        // different source (including one read through the ChatGPT login
        // before the source was decided correctly) is never reused.
        match &self.codex_source {
            CodexQuotaSource::ChatGptLogin => {}
            CodexQuotaSource::Provider(provider) => {
                hash.update(provider.host.as_bytes());
                hash.update(provider.api_key.as_bytes());
            }
            CodexQuotaSource::UsagePriced { provider } => {
                hash.update(b"usage-priced\0");
                hash.update(provider.as_bytes());
            }
            CodexQuotaSource::Unreadable(_) => hash.update(b"unreadable\0"),
        }
        hash.update([u8::from(self.opencode_usage_priced)]);
        mj_core::hex::lower_hex(hash.finalize())
    }

    /// Whether this profile's check runs a host harness program, through
    /// `quota_harness::locate`, rather than answering from configuration or
    /// an HTTP endpoint.
    /// Whether this profile's check or login renewal would call
    /// `quota_harness::locate`: the question the daemon's prewarm asks, so it
    /// installs nothing a check would not. It reads the Claude login file, so
    /// call it on a blocking thread. Claude's decision is
    /// [`claude_usage::runs_cli`]; Codex and Grok resolve their program
    /// before reading any login, so the kind and Codex's ChatGPT source decide.
    #[cfg(unix)]
    pub fn runs_host_harness(&self) -> bool {
        kind_may_need_managed_quota_harness(self.harness)
            && match self.harness {
                HarnessKind::Claude => {
                    self.auth_scheme != AuthScheme::AwsCredentialChain
                        && claude_usage::runs_cli(&self.source_home)
                }
                HarnessKind::Codex => matches!(self.codex_source, CodexQuotaSource::ChatGptLogin),
                // Grok and Antigravity resolve their program before reading
                // any login.
                _ => true,
            }
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
        // A profile with no stored OpenCode login is usage-priced exactly
        // when the environment supplies a provider key ([`mj doctor`] and the
        // sign-in status recognize the same signal,
        // [`HarnessProfile::opencode_environment_logins`]). A profile with a
        // stored login is usage-priced too when every stored credential is
        // itself a plain API key (`value.type == "key"`, whether exported
        // from the environment or typed into `opencode auth login`); any
        // stored OAuth login, for this provider or another, leaves billing
        // unknown, because OpenCode's own login database does not say which
        // credential the session actually uses.
        let opencode_usage_priced = profile.kind == HarnessKind::OpenCode
            && match crate::opencode_logins::profile_logins(profile) {
                Ok(None) => !profile.opencode_environment_logins().is_empty(),
                Ok(Some(logins)) => {
                    serde_json::from_slice::<Vec<Value>>(&logins).is_ok_and(|logins| {
                        logins.iter().all(|login| {
                            login
                                .get("value")
                                .and_then(|value| value.get("type"))
                                .and_then(Value::as_str)
                                == Some("key")
                        })
                    })
                }
                Err(_) => false,
            };
        Self {
            profile_id: profile_id.to_owned(),
            harness: profile.kind,
            auth_scheme: profile.auth_scheme(),
            source_home: profile.home.clone(),
            environment,
            cwd,
            codex_source: CodexQuotaSource::of_profile(profile),
            opencode_usage_priced,
        }
    }
}

/// How long a profile's probes are held when the provider answers 429 without
/// saying how long.
pub const DEFAULT_RATE_LIMIT_HOLD: Duration = Duration::from_secs(15 * 60);

/// A provider answered 429 (too many requests). Carried as the error of a
/// failed probe so the refresh can turn it into a hold.
#[derive(Debug)]
pub struct RateLimited {
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("rate limited")
    }
}

impl std::error::Error for RateLimited {}

#[cfg(unix)]
pub use crate::quota_harness::prepare_managed;
pub use mj_core::retry_after::retry_after_header;

/// The hold for a 429: what the provider asked for, kept between one minute
/// and six hours so a bad header can neither spin the poller nor park a
/// profile for days.
pub fn rate_limit_hold(retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or(DEFAULT_RATE_LIMIT_HOLD)
        .clamp(Duration::from_secs(60), Duration::from_secs(6 * 3600))
}

/// How often each profile's quota is read again.
pub const QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// For a check refused at `attempt`: when the provider's hold ends, and when
/// the next check runs. A refused check does not shorten the regular cadence:
/// the next one is an interval after this attempt, or the hold's end if later.
pub fn refused_check_schedule(attempt: u64, retry_after: Option<Duration>) -> (u64, u64) {
    let until = attempt + rate_limit_hold(retry_after).as_secs();
    let next_check = until.max(attempt + QUOTA_REFRESH_INTERVAL.as_secs());
    (until, next_check)
}

/// What to publish after a probe was rate limited: the last good reading with
/// the hold attached, so its numbers and age stay visible. With no good
/// reading to keep, the rate-limit report itself.
pub fn hold_report(previous: Option<&ProfileQuota>, limited: ProfileQuota) -> ProfileQuota {
    match previous.filter(|previous| previous.error.is_none()) {
        Some(good) => ProfileQuota {
            rate_limited_until_epoch_seconds: limited.rate_limited_until_epoch_seconds,
            next_check_at_epoch_seconds: limited.next_check_at_epoch_seconds,
            ..good.clone()
        },
        None => limited,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRefreshOutcome {
    /// Identity captured from the request that produced this outcome.
    pub cache_identity: String,
    pub report: ProfileQuota,
    pub credentials_changed: bool,
}

/// One profile's authoritative quota publication. `None` means that its
/// current identity has not completed a check yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReportPublication {
    /// Renewal failure, independent of the usage API result.
    pub login_error: Option<String>,
    pub cache_identity: String,
    pub report: Option<ProfileQuota>,
}

impl QuotaReportPublication {
    pub fn observe_claude_grant(
        &mut self,
        profile_id: &str,
        check: ClaudeGrantCheck,
        rejected: bool,
    ) -> bool {
        let error = (rejected
            && matches!(
                check,
                ClaudeGrantCheck::Failed | ClaudeGrantCheck::Waiting { due_at_ms: None }
            ))
        .then(|| mj_client::quota::refused_login_message(profile_id));
        if self.login_error == error {
            return false;
        }
        self.login_error = error;
        true
    }

    pub fn effective_report(&self, profile_id: &str) -> Option<ProfileQuota> {
        let Some(error) = &self.login_error else {
            return self.report.clone();
        };
        let mut report = self.report.clone().unwrap_or_else(|| ProfileQuota {
            profile_id: profile_id.to_owned(),
            harness: HarnessKind::Claude,
            monthly_price_usd: None,
            paid_credits: None,
            banked_resets: None,
            windows: Vec::new(),
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: mj_core::clock::epoch_seconds(),
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        });
        report.error = Some(error.clone());
        Some(report)
    }
}

pub type QuotaReportMap = BTreeMap<String, QuotaReportPublication>;

#[derive(Default)]
pub struct QuotaManager {
    codex_clients: HashMap<String, CodexUsageClient>,
    reports: BTreeMap<String, ProfileQuota>,
    #[cfg(any(test, feature = "test-support"))]
    cache_root: Option<PathBuf>,
}

impl QuotaManager {
    pub fn reports(&self) -> &BTreeMap<String, ProfileQuota> {
        &self.reports
    }

    /// The latest report for one profile, if there is one.
    pub fn report(&self, profile_id: &str) -> Option<&ProfileQuota> {
        self.reports.get(profile_id)
    }

    /// Adopt a report read from the store, so the profile counts as probed at
    /// the report's time.
    pub fn seed(&mut self, report: ProfileQuota) {
        self.reports.insert(report.profile_id.clone(), report);
    }

    /// Forget a profile's report and stop its cached client, for a profile
    /// whose configuration changed. The client's `codex app-server` was
    /// started with the old home and environment.
    pub async fn forget(&mut self, profile_id: &str) {
        self.reports.remove(profile_id);
        if let Some(client) = self.codex_clients.remove(profile_id) {
            client.shutdown().await;
        }
    }

    /// Keep only the profiles in `keep`: drop the reports of the others and
    /// stop their cached clients.
    pub async fn retain_profiles(&mut self, keep: &BTreeSet<String>) {
        self.reports
            .retain(|profile_id, _| keep.contains(profile_id));
        self.stop_clients_outside_batch(keep).await;
    }

    /// Refresh each profile independently so one slow harness cannot delay the
    /// others. `on_report` runs per profile in completion order, so fast
    /// harnesses report without waiting for the slowest one in the batch.
    /// The batch is the whole configured set: profiles outside it are dropped.
    pub async fn refresh_profiles<F, Fut>(
        &mut self,
        requests: Vec<QuotaRefreshRequest>,
        on_report: F,
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
        self.probe(requests, on_report).await;
        self.stop_clients_outside_batch(&batch).await;
    }

    /// Probe exactly these profiles and leave every other report alone.
    pub async fn probe<F, Fut>(&mut self, requests: Vec<QuotaRefreshRequest>, mut on_report: F)
    where
        F: FnMut(QuotaRefreshOutcome) -> Fut,
        Fut: Future<Output = ()> + Send,
    {
        #[cfg(any(test, feature = "test-support"))]
        let cache_root = self.cache_root.clone();
        #[cfg(not(any(test, feature = "test-support")))]
        let cache_root = None;

        let mut tasks = tokio::task::JoinSet::new();
        for request in requests {
            let client = self.codex_clients.remove(&request.profile_id);
            // Probes run concurrently, so the span is what ties a quota
            // client's log lines to its profile.
            let span = tracing::info_span!("quota_probe", profile_id = %request.profile_id);
            tasks.spawn(
                refresh_profile_with_cache_root(request, client, cache_root.clone())
                    .instrument(span),
            );
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
            let previous = self.reports.get(&outcome.report.profile_id);
            let outcome = if outcome.report.rate_limited_until_epoch_seconds.is_some() {
                QuotaRefreshOutcome {
                    cache_identity: outcome.cache_identity,
                    report: hold_report(previous, outcome.report),
                    credentials_changed: outcome.credentials_changed,
                }
            } else {
                outcome
            };
            log_quota_change(previous, &outcome.report);
            self.reports
                .insert(outcome.report.profile_id.clone(), outcome.report.clone());
            on_report(outcome).await;
        }
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
    if let Some(until) = report.rate_limited_until_epoch_seconds {
        // A hold is logged once, when it starts. The provider's hold and the
        // next scheduled check differ when the hold is shorter than the
        // regular interval.
        if previous.and_then(|previous| previous.rate_limited_until_epoch_seconds) != Some(until) {
            let now = mj_core::clock::epoch_seconds();
            tracing::info!(
                profile_id = %report.profile_id,
                harness = report.harness.display_name(),
                provider_hold_seconds = until.saturating_sub(now),
                next_check_in_seconds = report.next_check_at().unwrap_or(until).saturating_sub(now),
                "the provider rate limited the quota probe; holding this profile's probes"
            );
        }
        return;
    }
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

/// What the daemon's early Claude grant refresh decided for one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeGrantCheck {
    /// Not a profile whose grant the daemon renews (another harness, or one
    /// that does not sign in with the harness's own login).
    NotApplicable,
    /// Nothing to do now. `due_at_ms` is when the grant needs renewing, from
    /// the login as it is now (after a renewal, the new one). `None` when the
    /// login is missing, holds no refresh token, or states no expiry.
    Waiting { due_at_ms: Option<i64> },
    /// A handoff is draining admission; the next daemon checks the login.
    Deferred,
    /// A replacement is ready for immediate credential delivery.
    Renewed { due_at_ms: Option<i64> },
    /// The refresh failed. The error is already logged with its safe category.
    Failed,
}

/// Renew a Claude profile's grant ahead of its expiry (#1490). The daemon is
/// the only refresher: session copies carry no refresh token. This is the
/// decision's one owner and runs apart from the usage probe, so a 429 hold on
/// the usage API never postpones it.
pub async fn refresh_claude_grant(
    request: &QuotaRefreshRequest,
    now_ms: i64,
    launcher: &claude_usage::ClaudeLauncher,
    gate: &std::sync::Arc<crate::upgrade::Gate>,
    rejected_logins: &BTreeSet<String>,
) -> ClaudeGrantCheck {
    if request.harness != HarnessKind::Claude || !request.auth_scheme.uses_native_login_file() {
        return ClaudeGrantCheck::NotApplicable;
    }
    let environment = request
        .environment
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    let source = match request.harness.login_store(
        mj_core::config::HarnessHost::current(),
        &request.source_home,
    ) {
        // A Keychain login is renewed by Claude Code on this host, never in
        // a container: the image cannot write the Keychain. A host that
        // cannot run Claude Code fails visibly in `refresh_grant_if_due`.
        mj_core::config::HarnessLoginStore::MacKeychain => {
            let lock_marker = match mj_core::config::HarnessLoginStore::MacKeychain.lock_marker() {
                Ok(marker) => marker,
                Err(error) => {
                    tracing::warn!(
                        profile_id = %request.profile_id,
                        error = %format!("{error:#}"),
                        "could not find where Claude Code locks the Keychain login"
                    );
                    return ClaudeGrantCheck::Failed;
                }
            };
            claude_usage::LoginSource::Keychain {
                lock_marker,
                // `security` can wait on a Keychain prompt; the same bound
                // as the other host lookups. The login exchange itself
                // runs under no deadline.
                security: std::sync::Arc::new(crate::targets::BoundedProcessExecutor::new(
                    crate::harness_auth::KEYCHAIN_READ_TIMEOUT,
                )),
            }
        }
        mj_core::config::HarnessLoginStore::File { .. } => claude_usage::LoginSource::File {
            home: request.source_home.clone(),
        },
    };
    // No deadline and no cancellation: a started login exchange runs to its
    // own end (see `claude_usage::run_claude_refresh`).
    let executor = std::sync::Arc::new(crate::targets::ProcessExecutor);
    match claude_usage::refresh_grant_if_due(
        source,
        environment,
        executor,
        launcher,
        gate,
        claude_usage::GrantRefreshTrigger {
            now_ms,
            rejected_logins,
        },
    )
    .await
    {
        Ok(claude_usage::GrantRefresh::NotRefreshable) => {
            ClaudeGrantCheck::Waiting { due_at_ms: None }
        }
        // Due, but a handoff is waiting: the next daemon decides again.
        Ok(claude_usage::GrantRefresh::Deferred) => ClaudeGrantCheck::Deferred,
        Ok(claude_usage::GrantRefresh::Current { due_at_ms }) => ClaudeGrantCheck::Waiting {
            due_at_ms: Some(due_at_ms),
        },
        Ok(claude_usage::GrantRefresh::Renewed {
            by_this_call,
            expires_at_ms,
        }) => {
            tracing::info!(
                profile_id = %request.profile_id,
                new_expires_at_ms = ?expires_at_ms,
                refreshed_by_this_daemon = by_this_call,
                "refreshed the Claude login"
            );
            // The credential sync pushes the renewed login to sessions.
            ClaudeGrantCheck::Renewed {
                due_at_ms: expires_at_ms.map(claude_usage::grant_due_at),
            }
        }
        Err(error) => {
            tracing::warn!(
                profile_id = %request.profile_id,
                %error,
                "could not refresh the Claude login"
            );
            ClaudeGrantCheck::Failed
        }
    }
}

async fn refresh_profile_with_cache_root(
    request: QuotaRefreshRequest,
    mut codex_client: Option<CodexUsageClient>,
    cache_root: Option<PathBuf>,
) -> (QuotaRefreshOutcome, Option<CodexUsageClient>) {
    let cache_identity = request.cache_identity();
    let credential_path = match request.harness {
        HarnessKind::Kimi => {
            crate::kimi_auth::KimiRuntimeAuth::resolve(&request.source_home, &request.environment)
                .map(|runtime| runtime.credentials_path)
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        profile_id = %request.profile_id,
                        %error,
                        "could not resolve Kimi Code credentials"
                    );
                    harness_authentication_marker(request.harness, &request.source_home)
                })
        }
        _ => harness_authentication_marker(request.harness, &request.source_home),
    };
    // Only the ChatGPT login uses a Codex app-server. A client cached while
    // the profile used it is stopped here rather than handed back.
    if request.codex_source != CodexQuotaSource::ChatGptLogin
        && let Some(client) = codex_client.take()
    {
        client.shutdown().await;
    }
    let credential_before = credential_marker_fingerprint(&credential_path).await;
    let QuotaRefreshRequest {
        profile_id,
        harness,
        auth_scheme,
        source_home,
        environment,
        cwd,
        codex_source,
        opencode_usage_priced,
    } = request;
    let environment = environment.into_iter().collect::<HashMap<_, _>>();
    let refreshed_at_epoch_seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // A service that publishes no quota endpoint bills by usage, so there is
    // no allowance to report. Saying "API" rather than raising an error keeps
    // the dashboard from showing the profile as unavailable and lets the
    // utility ranker treat it as healthy, which matches how it actually
    // behaves.
    let usage_priced = || ProfileQuota {
        paid_credits: None,
        monthly_price_usd: None,
        banked_resets: None,
        profile_id: profile_id.clone(),
        harness,
        windows: Vec::new(),
        extra: Some(API_LABEL.to_owned()),
        error: None,
        refreshed_at_epoch_seconds,
        rate_limited_until_epoch_seconds: None,
        next_check_at_epoch_seconds: None,
    };
    // A harness that never shares its quota has nothing to probe and nothing
    // that failed, so it says `n/a` instead of reading as unavailable.
    let not_reported = || ProfileQuota {
        extra: Some(NOT_REPORTED_LABEL.to_owned()),
        ..usage_priced()
    };
    let result = match harness {
        // A Codex profile that authenticates with an API key against a custom
        // provider has no ChatGPT login to refresh and no ChatGPT rate-limit
        // windows to read. Its quota, when the provider publishes one, comes
        // from the provider's own endpoint.
        HarnessKind::Codex => {
            match codex_source {
                CodexQuotaSource::Provider(provider)
                    if crate::zai_usage::serves_quota(&provider.host) =>
                {
                    crate::zai_usage::query(&provider.host, &provider.api_key)
                        .await
                        .map(|windows| ProfileQuota {
                            paid_credits: None,
                            monthly_price_usd: None,
                            banked_resets: None,
                            profile_id: profile_id.clone(),
                            harness,
                            windows: windows
                                .into_iter()
                                .map(|window| QuotaWindow {
                                    label: window.label,
                                    remaining_percent: Some(window.remaining_percent),
                                    duration_seconds: None,
                                    model_scope: None,
                                    used: window.used,
                                    limit: window.limit,
                                    resets: window.resets_at.and_then(format_reset_local_seconds),
                                    resets_at_epoch_seconds: window.resets_at,
                                })
                                .collect(),
                            extra: None,
                            error: None,
                            refreshed_at_epoch_seconds,
                            rate_limited_until_epoch_seconds: None,
                            next_check_at_epoch_seconds: None,
                        })
                }
                CodexQuotaSource::Provider(_) | CodexQuotaSource::UsagePriced { .. } => {
                    Ok(usage_priced())
                }
                CodexQuotaSource::Unreadable(error) => Err(anyhow::anyhow!(
                    "could not read the Codex configuration: {error}"
                )),
                CodexQuotaSource::ChatGptLogin => 'codex: {
                    use mj_core::credentials::CredentialWriteGuard;
                    let launcher = codex_usage::CodexLauncher::Host;
                    #[cfg(test)]
                    let launcher = cache_root.as_ref().map_or(launcher, |root| {
                        codex_usage::CodexLauncher::Program(
                            mj_core::harness_runtime::managed_harness_install_dir(
                                root,
                                HarnessKind::Codex,
                            )
                            .join(mj_core::harness_runtime::CODEX_CLI_ENTRYPOINT),
                        )
                    });
                    #[cfg(not(test))]
                    let _ = cache_root;
                    // Installation touches no credentials; finish it before
                    // reserving the login slot used by credential synchronization.
                    let mut prepared = codex_client
                        .as_ref()
                        .and_then(|client| client.prepared_program(&launcher));
                    if prepared.is_none() {
                        prepared = match launcher.prepare(&environment).await {
                            Ok(program) => Some(program),
                            Err(error) => break 'codex Err(error),
                        };
                    }
                    // Serialize the expiry decision and every app-server request with
                    // other daemons and credential syncs using this login slot.
                    // Waiting for ownership is outside the RPC request timeout.
                    let mut credential_guard =
                        match CredentialWriteGuard::acquire(HarnessKind::Codex, &credential_path)
                            .await
                        {
                            Ok(guard) => guard,
                            Err(error) => break 'codex Err(error),
                        };
                    // A poll starts at most one app-server. A failed rotation drops its
                    // client. When that client was cached from an earlier poll, a new
                    // one may still read the quota, which is how a login that the old
                    // process gave up on recovers (#1224). When this poll started it,
                    // another start would only repeat the failure, so the next poll
                    // tries again instead.
                    let mut failed_rotation = None;
                    if codex_login_is_near_expiry(&credential_path).await {
                        let started_here = codex_client.is_none();
                        let login_refresh = codex_usage::refresh_login_with_cache_root(
                            &mut codex_client,
                            cwd.clone(),
                            environment.clone(),
                            &launcher,
                            &mut prepared,
                        )
                        .await;
                        match login_refresh {
                            Ok(()) => tracing::info!(
                                profile_id = %profile_id,
                                "refreshed Codex login ahead of expiry"
                            ),
                            Err(failure) => {
                                tracing::warn!(
                                    profile_id = %profile_id,
                                    error = %failure.detail,
                                    "could not refresh the Codex login ahead of expiry"
                                );
                                if codex_client.is_none() {
                                    if started_here {
                                        failed_rotation = Some(failure.reason);
                                    } else {
                                        // The failed cached process has stopped. Drop its
                                        // lease and login admission before preparing a
                                        // replacement, which may need a fresh install.
                                        drop(prepared.take());
                                        drop(credential_guard);
                                        prepared = match launcher.prepare(&environment).await {
                                            Ok(program) => Some(program),
                                            Err(error) => break 'codex Err(error),
                                        };
                                        let replacement_guard = CredentialWriteGuard::acquire(
                                            HarnessKind::Codex,
                                            &credential_path,
                                        )
                                        .await;
                                        credential_guard = match replacement_guard {
                                            Ok(guard) => guard,
                                            Err(error) => break 'codex Err(error),
                                        };
                                    }
                                }
                            }
                        }
                    }
                    let status = match failed_rotation {
                        Some(reason) => CodexUsageStatus::Unavailable(reason.to_owned()),
                        None => {
                            codex_usage::refresh_with_cache_root(
                                &mut codex_client,
                                cwd,
                                environment,
                                &launcher,
                                &mut prepared,
                            )
                            .await
                        }
                    };
                    drop(credential_guard);
                    match status {
                        CodexUsageStatus::Available(report) => Ok(ProfileQuota {
                            paid_credits: report.paid_credits,
                            monthly_price_usd: None,
                            banked_resets: report.banked_resets,
                            profile_id: profile_id.clone(),
                            harness,
                            windows: [report.primary, report.secondary]
                                .into_iter()
                                .flatten()
                                .map(|window| QuotaWindow {
                                    label: window.label,
                                    remaining_percent: Some(window.remaining_percent),
                                    duration_seconds: window.duration_seconds,
                                    model_scope: None,
                                    used: None,
                                    limit: None,
                                    resets: window.resets_at.and_then(format_reset_local_seconds),
                                    resets_at_epoch_seconds: window.resets_at,
                                })
                                .collect(),
                            extra: None,
                            error: None,
                            refreshed_at_epoch_seconds,
                            rate_limited_until_epoch_seconds: None,
                            next_check_at_epoch_seconds: None,
                        }),
                        CodexUsageStatus::Unavailable(error) => Err(anyhow::anyhow!(error)),
                    }
                }
            }
        }
        // Bedrock bills through AWS and has no Claude subscription allowance.
        HarnessKind::Claude if auth_scheme == AuthScheme::AwsCredentialChain => Ok(usage_priced()),
        HarnessKind::Claude => claude_usage::query(source_home, environment)
            .await
            .map(|report| ProfileQuota {
                paid_credits: report.paid_credits,
                monthly_price_usd: report.monthly_price_usd,
                banked_resets: report.banked_resets,
                profile_id: profile_id.clone(),
                harness,
                windows: {
                    let mut windows = Vec::new();
                    if let Some(window) = report.five_hour {
                        windows.push(QuotaWindow {
                            label: "5H".to_owned(),
                            remaining_percent: Some(window.remaining_percent),
                            duration_seconds: window.duration_seconds,
                            model_scope: None,
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
                        });
                    }
                    if let Some(window) = report.week {
                        windows.push(QuotaWindow {
                            label: "Week".to_owned(),
                            remaining_percent: Some(window.remaining_percent),
                            duration_seconds: window.duration_seconds,
                            model_scope: None,
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
                        });
                    }
                    for (scope, window) in report.scoped_weeks {
                        let mut chars = scope.chars();
                        let label_scope = chars.next().map_or_else(String::new, |first| {
                            first.to_ascii_uppercase().to_string() + chars.as_str()
                        });
                        windows.push(QuotaWindow {
                            label: format!("Week ({label_scope})"),
                            remaining_percent: Some(window.remaining_percent),
                            duration_seconds: window.duration_seconds,
                            model_scope: Some(scope),
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
                        });
                    }
                    windows
                },
                extra: None,
                error: None,
                refreshed_at_epoch_seconds,
                rate_limited_until_epoch_seconds: None,
                next_check_at_epoch_seconds: None,
            })
            .map_err(|error| match error {
                claude_usage::ClaudeUsageError::RateLimited(retry_after) => {
                    anyhow::Error::new(RateLimited { retry_after })
                }
                error => anyhow::anyhow!(error.to_string()),
            }),
        HarnessKind::Kimi => {
            query_kimi(&source_home, &environment)
                .await
                .map(|(windows, extra)| ProfileQuota {
                    paid_credits: None,
                    monthly_price_usd: None,
                    banked_resets: None,
                    profile_id: profile_id.clone(),
                    harness,
                    windows,
                    extra,
                    error: None,
                    refreshed_at_epoch_seconds,
                    rate_limited_until_epoch_seconds: None,
                    next_check_at_epoch_seconds: None,
                })
        }
        // Grok Build publishes no HTTP quota endpoint. Its own usage view polls
        // an ACP billing extension, and so does Mjolnir.
        HarnessKind::Grok => {
            grok_usage::query(cwd, environment)
                .await
                .map(|report| ProfileQuota {
                    paid_credits: None,
                    monthly_price_usd: None,
                    banked_resets: None,
                    profile_id: profile_id.clone(),
                    harness,
                    windows: vec![QuotaWindow {
                        label: report.period_label.clone(),
                        remaining_percent: Some(report.remaining_percent()),
                        duration_seconds: None,
                        model_scope: None,
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
                    rate_limited_until_epoch_seconds: None,
                    next_check_at_epoch_seconds: None,
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        }
        HarnessKind::Muse => crate::muse_usage::query(&source_home, &environment)
            .await
            .map(|report| ProfileQuota {
                paid_credits: None,
                monthly_price_usd: None,
                banked_resets: None,
                profile_id: profile_id.clone(),
                harness,
                windows: report
                    .windows
                    .into_iter()
                    .map(|window| QuotaWindow {
                        label: window.label,
                        remaining_percent: Some(window.remaining_percent),
                        duration_seconds: window.duration_seconds,
                        model_scope: None,
                        used: None,
                        limit: None,
                        resets: window.resets_at.and_then(format_reset_local_seconds),
                        resets_at_epoch_seconds: window.resets_at,
                    })
                    .collect(),
                extra: report.note,
                error: None,
                refreshed_at_epoch_seconds,
                rate_limited_until_epoch_seconds: None,
                next_check_at_epoch_seconds: None,
            }),
        // OpenCode reads no allowance from any provider: rate-limit headers
        // reach it only on failed requests. A profile signed in through an
        // environment-supplied provider key (OpenRouter and the like) is
        // still billed by usage even though OpenCode never shares it, so it
        // gets the same `API` label as Dsh rather than reading as unavailable.
        HarnessKind::OpenCode => Ok(if opencode_usage_priced {
            usage_priced()
        } else {
            not_reported()
        }),
        // dsh-acp talks to DeepSeek's API, which bills by usage.
        HarnessKind::Dsh => Ok(usage_priced()),
        HarnessKind::Antigravity => crate::antigravity_usage::query(&source_home, &environment)
            .await
            .map(|windows| ProfileQuota {
                paid_credits: None,
                monthly_price_usd: None,
                banked_resets: None,
                profile_id: profile_id.clone(),
                harness,
                windows,
                extra: None,
                error: None,
                refreshed_at_epoch_seconds,
                rate_limited_until_epoch_seconds: None,
                next_check_at_epoch_seconds: None,
            }),
    };
    let report = result.unwrap_or_else(|error| {
        let schedule = error
            .downcast_ref::<RateLimited>()
            .map(|limited| refused_check_schedule(refreshed_at_epoch_seconds, limited.retry_after));
        ProfileQuota {
            paid_credits: None,
            monthly_price_usd: None,
            banked_resets: None,
            profile_id,
            harness,
            windows: Vec::new(),
            extra: None,
            error: Some(format!("{error:#}")),
            refreshed_at_epoch_seconds,
            rate_limited_until_epoch_seconds: schedule.map(|(until, _)| until),
            next_check_at_epoch_seconds: schedule.map(|(_, next_check)| next_check),
        }
    });
    // Only refresh attempts reach this point, not reports adopted from the
    // daemon's reset cache. A reader can still reuse its recent provider data.
    // Persist failures before the manager substitutes old windows for display.
    // Only the daemon owns the writer; other callers leave persistence to it.
    if crate::database::database_writer_installed() {
        let observed = report.clone();
        let identity = cache_identity.clone();
        match tokio::task::spawn_blocking(move || {
            crate::database::save_quota_report(&identity, &observed)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                profile_id = %report.profile_id,
                %error,
                "could not preserve quota history and reset times"
            ),
            Err(error) => tracing::warn!(
                profile_id = %report.profile_id,
                %error,
                "quota persistence task failed"
            ),
        }
    } else {
        tracing::debug!(
            profile_id = %report.profile_id,
            "this process has no database writer; leaving quota history and reset times to the daemon"
        );
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
            cache_identity,
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
    let environment = environment
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let runtime = crate::kimi_auth::KimiRuntimeAuth::resolve(home, &environment)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build Kimi quota client")?;
    let usage_url = format!("{}/usages", runtime.base_url);
    let response = match environment
        .get("KIMI_API_KEY")
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
    {
        Some(api_key) => fetch_bearer(&client, &usage_url, api_key).await?,
        None => {
            fetch_bearer_with_auth_retry(&client, &usage_url, |force, rejected_token| {
                ensure_fresh_kimi_token(&client, &runtime, force, rejected_token)
            })
            .await?
        }
    };
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(RateLimited {
            retry_after: retry_after_header(response.headers()),
        }
        .into());
    }
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
    let response = fetch_bearer(client, url, &token).await?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(response);
    }

    let refreshed = authenticate(true, Some(token)).await?;
    fetch_bearer(client, url, &refreshed).await
}

async fn fetch_bearer(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<reqwest::Response> {
    client
        .get(url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("query quota")
}

/// Hand back a usable Kimi Code access token, refreshing the stored pair when
/// it is stale or when the server rejected it. The refresh runs under the
/// lock the Kimi Code CLI also takes, and the pair is re-read once the lock is
/// held, so a refresh another process just finished is used instead of
/// spending its new refresh token again.
pub async fn ensure_fresh_kimi_token(
    client: &reqwest::Client,
    runtime: &crate::kimi_auth::KimiRuntimeAuth,
    force: bool,
    rejected_token: Option<String>,
) -> Result<String> {
    let initial = read_kimi_credentials(&runtime.credentials_path).await?;
    if !force && !initial.needs_refresh() {
        return Ok(initial.access_token);
    }

    // Held until this function returns, released on drop.
    let _refresh_lock = mj_core::credentials::CredentialWriteGuard::acquire(
        HarnessKind::Kimi,
        &runtime.credentials_path,
    )
    .await?;
    let active = read_kimi_credentials(&runtime.credentials_path).await?;
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

    let response = client
        .post(format!("{}/api/oauth/token", runtime.oauth_host))
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
            let recovery = read_kimi_credentials(&runtime.credentials_path).await?;
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
    save_kimi_credentials(&runtime.credentials_path, &refreshed)?;
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

fn parse_kimi_usage(payload: &Value) -> (Vec<QuotaWindow>, Option<String>) {
    let mut windows = Vec::new();
    let mut labels = BTreeSet::new();
    if let Some(summary) = payload.get("usage")
        && let Some(window) = parse_kimi_window(summary, "Weekly limit")
    {
        labels.insert(window.label.clone());
        windows.push(window);
    }
    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for (index, item) in limits.iter().enumerate() {
            let detail = item.get("detail").unwrap_or(item);
            let fallback = kimi_limit_label(item, index);
            if let Some(window) = parse_kimi_window(detail, &fallback)
                && labels.insert(window.label.clone())
            {
                windows.push(window);
            }
        }
    }
    if let Some(usages) = payload.get("usages").and_then(Value::as_object) {
        // `limits[]` carries absolute counts while `usages` carries ratios for
        // the same windows. Keep the more precise absolute row when both name
        // the same window.
        for (keys, label) in [
            (&["limit_5h"][..], "5H"),
            (&["limit_7d"][..], "Week"),
            (&["limit_month_total", "month_total"][..], "Month"),
            (&["limit_month_code", "month_code"][..], "Month code"),
        ] {
            let Some(value) = keys.iter().find_map(|key| usages.get(*key)) else {
                continue;
            };
            if let Some(window) = parse_kimi_ratio_window(value, label)
                && labels.insert(window.label.clone())
            {
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

fn kimi_limit_label(item: &Value, index: usize) -> String {
    let duration = item
        .pointer("/window/duration")
        .and_then(value_i64)
        .or_else(|| item.get("duration").and_then(value_i64));
    let unit = item
        .pointer("/window/timeUnit")
        .or_else(|| item.pointer("/window/time_unit"))
        .or_else(|| item.get("timeUnit"))
        .or_else(|| item.get("time_unit"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let seconds = duration.map(|duration| match unit.as_str() {
        "TIME_UNIT_MINUTE" | "MINUTE" | "MINUTES" => duration.saturating_mul(60),
        "TIME_UNIT_HOUR" | "HOUR" | "HOURS" => duration.saturating_mul(3_600),
        "TIME_UNIT_DAY" | "DAY" | "DAYS" => duration.saturating_mul(86_400),
        _ => duration,
    });
    match seconds {
        Some(18_000) => "5H".to_owned(),
        Some(604_800) => "Week".to_owned(),
        None if index == 0 => "5H".to_owned(),
        _ => format!("Limit #{}", index + 1),
    }
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
    let label = canonical_kimi_label(provider_label);
    let reset_value = kimi_reset_value(value);
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
        duration_seconds: None,
        model_scope: None,
        used,
        limit,
        resets,
        resets_at_epoch_seconds,
    })
}

fn parse_kimi_ratio_window(value: &Value, label: &str) -> Option<QuotaWindow> {
    let used_ratio = value
        .get("used_ratio")
        .or_else(|| value.get("usedRatio"))
        .and_then(value_f64)
        .filter(|ratio| ratio.is_finite())?
        .clamp(0.0, 1.0);
    let used_percent = (used_ratio * 100.0).ceil().clamp(0.0, 100.0) as u8;
    let reset_value = kimi_reset_value(value);
    Some(QuotaWindow {
        label: label.to_owned(),
        remaining_percent: Some(100 - used_percent),
        duration_seconds: None,
        model_scope: None,
        used: None,
        limit: None,
        resets: reset_value.and_then(normalize_kimi_reset),
        resets_at_epoch_seconds: reset_value.and_then(kimi_reset_epoch_seconds),
    })
}

fn canonical_kimi_label(label: &str) -> String {
    let lowercase = label.to_ascii_lowercase();
    if lowercase.contains("5h") || lowercase.contains("5-hour") || lowercase.contains("5 hour") {
        "5H".to_owned()
    } else if lowercase.contains("week") || lowercase.contains("7d") {
        "Week".to_owned()
    } else if lowercase.contains("month") && lowercase.contains("code") {
        "Month code".to_owned()
    } else if lowercase.contains("month") {
        "Month".to_owned()
    } else {
        label.to_owned()
    }
}

fn kimi_reset_value(value: &Value) -> Option<&Value> {
    ["resetAt", "reset_at", "resetTime", "reset_time"]
        .iter()
        .find_map(|key| value.get(*key))
}

fn value_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse::<i64>().ok())
}

fn value_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
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
pub fn format_reset_local(epoch: f64) -> Option<String> {
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

pub fn format_reset_local_seconds(epoch: i64) -> Option<String> {
    format_reset_local(epoch as f64)
}

/// Normalize a provider's textual reset value to the compact 24-hour form
/// used by the dashboard. A time-only value is the next occurrence of that
/// wall-clock time; Claude Code uses this shape for its five-hour window.
pub fn normalize_reset_text(value: &str) -> Option<String> {
    normalize_reset_at(value, Local::now().fixed_offset()).map(format_reset_label)
}

pub fn normalize_reset_epoch_seconds(value: &str) -> Option<i64> {
    normalize_reset_at(value, Local::now().fixed_offset()).map(|reset| reset.timestamp())
}

pub fn normalize_reset_at(
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
pub use recovery::{
    StatedReset, exhausted, long_window_stop, merge_reset_windows, message_reset, recovery_reset,
    retry_deadline,
};
pub use recovery::{quota_score, window_applies};
