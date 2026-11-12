//! Claude Code quota polling, OAuth renewal, and `/usage` parsing.
//!
//! The Claude ACP agent exposes token usage over ACP, but the subscription
//! quota shown by Claude Code lives behind its local `/usage` command.  Keep
//! this module independent from the UI state machine so the parser can be
//! tested against captured command output without spawning `claude`.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::targets::{CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec};

const USAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long macOS lets Claude Code's own `/usage` run. It is the bound the
/// whole check ran under before the login exchange stopped sharing it.
const CLI_USAGE_TIMEOUT: Duration = Duration::from_secs(60);
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
// The reset-grant endpoint uses the CLI surface to determine eligibility.
const USAGE_USER_AGENT: &str = "claude-cli/2.1.283 (external, cli)";

/// Quota-row copy when the stored Claude OAuth access token is past `expiresAt`.
pub(crate) const LOGIN_EXPIRED: &str = "login expired";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageReport {
    pub monthly_price_usd: Option<u32>,
    pub paid_credits: Option<mj_client::quota::PaidCredits>,
    pub banked_resets: Option<u64>,
    pub five_hour: Option<ClaudeUsageWindow>,
    pub week: Option<ClaudeUsageWindow>,
    pub scoped_weeks: Vec<(String, ClaudeUsageWindow)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageWindow {
    pub remaining_percent: u8,
    pub duration_seconds: Option<i64>,
    /// Text following `reset` in Claude Code output, without the word itself.
    pub reset_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeUsageError {
    TimedOut,
    NotSignedIn,
    LoginExpired,
    Refresh(String),
    Query(String),
    Parse,
    /// The endpoint answered 429. The delay is what `Retry-After` asked for,
    /// when it said.
    RateLimited(Option<Duration>),
}

impl fmt::Display for ClaudeUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => write!(f, "claude /usage timed out"),
            Self::NotSignedIn => write!(f, "Claude Code is not signed in"),
            Self::LoginExpired => write!(f, "{LOGIN_EXPIRED}"),
            Self::Refresh(error) => write!(f, "refresh Claude login: {error}"),
            Self::Query(error) => write!(f, "query Claude usage: {error}"),
            Self::Parse => write!(f, "could not parse claude /usage output"),
            Self::RateLimited(_) => write!(f, "rate limited"),
        }
    }
}

/// Query the same OAuth usage endpoint as Claude Code's interactive `/usage`,
/// except on macOS, where Claude Code is asked directly.
///
/// The endpoint is authenticated with the access token from the profile home's
/// `.credentials.json`. On macOS that file does not exist: Claude Code keeps
/// the credentials in the Keychain, where only Claude Code itself reads them.
/// The API has no token to send there, so its print-mode `/usage` output is
/// the only quota macOS can report.
///
/// With `container`, a login refresh runs the agent-dev image's `claude`
/// instead of one on this host. macOS never uses it: the image cannot read
/// the Keychain.
pub async fn query(
    home: PathBuf,
    environment: HashMap<String, String>,
    container: Option<crate::quota_runtime::ContainerCheck>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    if cfg!(target_os = "macos") {
        let executor = Arc::new(CancellableProcessExecutor::with_timeout(CLI_USAGE_TIMEOUT));
        let _cancel = executor.cancel_on_drop();
        let mut report = query_cli_with(environment, executor.clone()).await?;
        let source = LoginSource::Keychain {
            lock_marker: home.join(".credentials.json"),
            security: executor,
        };
        match source.read().await {
            Ok(credentials) => report.monthly_price_usd = monthly_price_usd(&credentials),
            Err(error) => tracing::warn!(%error, "Claude subscription price unavailable"),
        }
        return Ok(report);
    }
    // The login exchange runs under no deadline of mj's and survives this
    // future being dropped: see `run_claude_refresh`.
    let executor = Arc::new(crate::targets::ProcessExecutor);
    query_with_runtime(
        home,
        environment,
        USAGE_URL,
        executor,
        container,
        ClaudeLauncher::Managed,
    )
    .await
}

/// The program a host check starts.
fn claude_program() -> &'static str {
    if cfg!(windows) {
        "claude.cmd"
    } else {
        "claude"
    }
}

/// Where a host renewal gets its `claude`.
#[derive(Debug, Clone)]
pub(crate) enum ClaudeLauncher {
    /// The pinned Claude Code Mjolnir installs and sessions run, from the same
    /// resolver `mj login` uses. Mjolnir installs no Claude Code on Windows,
    /// where the person's own `claude.cmd` on the profile's `PATH` is the only
    /// one there is.
    Managed,
    /// A program chosen by the caller (tests).
    #[cfg(test)]
    Program(PathBuf),
}

/// The person's own `claude` on the profile's `PATH`, where Mjolnir has no
/// installation of Claude Code to offer.
#[cfg(not(unix))]
fn host_claude_on_path(environment: &HashMap<String, String>) -> Option<PathBuf> {
    let path = environment
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"));
    mj_core::program_path::find_executable_on_path(claude_program(), path.as_deref())
}

/// What keeps a managed Claude Code installed while a renewal runs it.
/// Mjolnir installs none on Windows, so there is nothing to hold there.
#[cfg(unix)]
type Installation = mj_core::harness_install::Located;
#[cfg(not(unix))]
type Installation = std::convert::Infallible;

/// The `claude` a host renewal runs, and what keeps it installed meanwhile.
struct ClaudeProgram {
    command: PathBuf,
    environment: HashMap<String, String>,
    _installation: Option<Installation>,
}

impl ClaudeLauncher {
    /// Whether this host can run Claude Code through this launcher: always where
    /// Mjolnir installs it (Unix), and where it does not (Windows) when the
    /// person's own `claude.cmd` is on the profile's `PATH`. Quota runtime
    /// `auto` falls back to a container exactly when this is false.
    pub(crate) fn host_can_run(&self, environment: &HashMap<String, String>) -> bool {
        match self {
            #[cfg(test)]
            Self::Program(_) => true,
            #[cfg(unix)]
            Self::Managed => {
                let _ = environment;
                true
            }
            #[cfg(not(unix))]
            Self::Managed => host_claude_on_path(environment).is_some(),
        }
    }

    /// Resolve the program, installing the pinned Claude Code when this host
    /// has none yet. A failure says why no `claude` can run, so it is
    /// reported on every renewal that needs it.
    async fn locate(
        &self,
        environment: &HashMap<String, String>,
    ) -> Result<ClaudeProgram, ClaudeUsageError> {
        match self {
            #[cfg(test)]
            Self::Program(command) => Ok(ClaudeProgram {
                command: command.clone(),
                environment: HashMap::new(),
                _installation: None,
            }),
            #[cfg(unix)]
            Self::Managed => {
                let execution_policy = mj_core::config::ExecutionPolicy::ConfiguredApprovals;
                let (command, args) = mj_core::harness_runtime::fallback_launch(
                    mj_core::config::HarnessKind::Claude,
                    execution_policy,
                );
                let search = std::env::vars_os()
                    .filter_map(|(name, value)| {
                        Some((name.into_string().ok()?, value.into_string().ok()?))
                    })
                    .chain(environment.clone())
                    .collect();
                let located = mj_core::harness_install::locate(
                    mj_core::placement::local_harness_runtime(),
                    mj_core::config::HarnessKind::Claude,
                    execution_policy,
                    std::path::Path::new(&command),
                    &args,
                    &search,
                )
                .await
                .map_err(|error| {
                    ClaudeUsageError::Refresh(format!("no Claude Code to run: {error:#}"))
                })?;
                let command = located.cli_entrypoint.clone().ok_or_else(|| {
                    ClaudeUsageError::Refresh(
                        "no Claude Code to run: the installation names no claude program".into(),
                    )
                })?;
                Ok(ClaudeProgram {
                    command,
                    environment: located.environment.clone().into_iter().collect(),
                    _installation: Some(located),
                })
            }
            #[cfg(not(unix))]
            Self::Managed => host_claude_on_path(environment)
                .map(|command| ClaudeProgram {
                    command,
                    environment: HashMap::new(),
                    _installation: None,
                })
                .ok_or_else(|| {
                    ClaudeUsageError::Refresh(format!(
                        "no Claude Code to run: Mjolnir installs none on this platform and {} is not on PATH",
                        claude_program()
                    ))
                }),
        }
    }
}

/// Admit one login renewal, which the daemon's handoff waits for once it has
/// begun: Anthropic spends the refresh token before `claude auth login`
/// persists the new one, so the exchange must not be cut short. Refused while
/// a handoff is waiting; the next daemon redoes the renewal from the file.
fn admit_renewal(
    gate: &Arc<crate::upgrade::Gate>,
) -> Result<crate::upgrade::Work, ClaudeUsageError> {
    gate.enter_unless_draining("Claude login renewal")
        .map_err(|error| ClaudeUsageError::Refresh(format!("{error:#}")))
}

/// Read the quota out of Claude Code's own `/usage` output.
async fn query_cli_with(
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let output = run_claude_usage(environment, executor, "read Claude usage")
        .await
        .map_err(ClaudeUsageError::Query)?;
    // Claude Code reports a missing login on stdout and exits zero, so both
    // streams are read before the status is judged.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // A report that parses is the answer. Classifying first would let the
    // free-form sections below the windows decide the result: a skill or MCP
    // server whose name happens to contain "unauthorized" is not a login
    // failure.
    if output.status == 0
        && let Some(report) = parse_cli_usage(&text)
    {
        return Ok(report);
    }
    if is_authentication_error(&text) {
        return Err(ClaudeUsageError::NotSignedIn);
    }
    if output.status != 0 {
        return Err(ClaudeUsageError::Query(format!(
            "claude /usage exited with status {}",
            output.status
        )));
    }
    Err(ClaudeUsageError::Parse)
}

#[cfg(test)]
async fn query_with(
    home: PathBuf,
    environment: HashMap<String, String>,
    usage_url: &str,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    query_with_runtime(
        home,
        environment,
        usage_url,
        executor,
        None,
        ClaudeLauncher::Program(PathBuf::from("claude")),
    )
    .await
}

async fn query_with_runtime(
    home: PathBuf,
    environment: HashMap<String, String>,
    usage_url: &str,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    container: Option<crate::quota_runtime::ContainerCheck>,
    launcher: ClaudeLauncher,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let client = reqwest::Client::builder()
        .timeout(USAGE_TIMEOUT)
        .build()
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))?;
    let credentials = read_credentials(&home).await?;
    match oauth_access_token(&credentials, mj_core::clock::epoch_millis()) {
        Ok(token) => match query_api(&client, usage_url, token, &credentials).await {
            Err(ClaudeUsageError::LoginExpired) if oauth_has_refresh_token(&credentials) => {
                refresh_and_retry(
                    &client,
                    usage_url,
                    home,
                    environment,
                    executor,
                    Some(token.to_owned()),
                    RenewalRuntime {
                        container,
                        launcher,
                    },
                )
                .await
            }
            result => result,
        },
        Err(ClaudeUsageError::LoginExpired) if oauth_has_refresh_token(&credentials) => {
            refresh_and_retry(
                &client,
                usage_url,
                home,
                environment,
                executor,
                None,
                RenewalRuntime {
                    container,
                    launcher,
                },
            )
            .await
        }
        Err(error) => Err(error),
    }
}

async fn read_credentials(home: &std::path::Path) -> Result<Value, ClaudeUsageError> {
    let credentials = tokio::fs::read(home.join(".credentials.json"))
        .await
        .map_err(|_| ClaudeUsageError::NotSignedIn)?;
    serde_json::from_slice(&credentials).map_err(|_| ClaudeUsageError::NotSignedIn)
}

async fn query_api(
    client: &reqwest::Client,
    usage_url: &str,
    token: &str,
    credentials: &Value,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let mut url = reqwest::Url::parse(usage_url)
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))?;
    url.query_pairs_mut().append_pair("cedar_ember", "1");
    let payload = query_oauth_json(client, url, token, USAGE_TIMEOUT, None).await?;
    let mut report = parse_api_usage(&payload).ok_or(ClaudeUsageError::Parse)?;
    report.monthly_price_usd = monthly_price_usd(credentials);
    if let Some(extra) = payload
        .get("extra_usage")
        .filter(|extra| extra.get("is_enabled").and_then(Value::as_bool) == Some(true))
    {
        match query_paid_credits(client, usage_url, token, extra).await {
            Ok(credits) => report.paid_credits = Some(credits),
            Err(error) => tracing::warn!(%error, "Claude paid-credit balance unavailable"),
        }
    }
    Ok(report)
}

async fn query_oauth_json(
    client: &reqwest::Client,
    url: reqwest::Url,
    token: &str,
    timeout: Duration,
    organization: Option<&str>,
) -> Result<Value, ClaudeUsageError> {
    let mut request = client
        .get(url)
        .timeout(timeout)
        .header(reqwest::header::USER_AGENT, USAGE_USER_AGENT)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01");
    if let Some(organization) = organization {
        request = request.header("x-organization-uuid", organization);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            ClaudeUsageError::TimedOut
        } else {
            ClaudeUsageError::Query(error.to_string())
        }
    })?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(ClaudeUsageError::LoginExpired);
    }
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ClaudeUsageError::RateLimited(
            crate::quota::retry_after_header(response.headers()),
        ));
    }
    if !response.status().is_success() {
        return Err(ClaudeUsageError::Query(format!(
            "HTTP {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))
}

async fn query_paid_credits(
    client: &reqwest::Client,
    usage_url: &str,
    token: &str,
    extra: &Value,
) -> Result<mj_client::quota::PaidCredits, ClaudeUsageError> {
    let invalid = || ClaudeUsageError::Query("invalid paid-credit balance or spending cap".into());
    let mut url = reqwest::Url::parse(usage_url).map_err(|_| invalid())?;
    url.set_path("/api/oauth/profile");
    url.set_query(None);
    let profile =
        query_oauth_json(client, url.clone(), token, Duration::from_secs(5), None).await?;
    let organization = profile
        .pointer("/organization/uuid")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(invalid)?;
    url.set_path("/api/oauth/organizations/");
    url.path_segments_mut()
        .map_err(|_| invalid())?
        .pop_if_empty()
        .push(organization)
        .push("prepaid")
        .push("credits");
    let wallet = query_oauth_json(
        client,
        url,
        token,
        Duration::from_secs(5),
        Some(organization),
    )
    .await?;
    let mut balance = wallet
        .get("amount")
        .and_then(Value::as_f64)
        .ok_or_else(invalid)?;
    if let Some(limit) = extra.get("monthly_limit").filter(|value| !value.is_null()) {
        let limit = limit.as_f64().ok_or_else(invalid)?;
        let used = extra
            .get("used_credits")
            .and_then(Value::as_f64)
            .ok_or_else(invalid)?;
        balance = balance.min(limit - used);
    } else if extra.get("monthly_limit").is_none() {
        return Err(invalid());
    }
    if let (Some(spend), Some(funds)) = (
        extra.get("currency").and_then(Value::as_str),
        wallet.get("currency").and_then(Value::as_str),
    ) && !spend.eq_ignore_ascii_case(funds)
    {
        return Err(ClaudeUsageError::Query(
            "paid-credit currencies disagree".into(),
        ));
    }
    mj_client::quota::PaidCredits::limited(balance).ok_or_else(invalid)
}

/// Where a renewal runs `claude`: in a quota container, or the host's program.
struct RenewalRuntime {
    container: Option<crate::quota_runtime::ContainerCheck>,
    launcher: ClaudeLauncher,
}

async fn refresh_and_retry(
    client: &reqwest::Client,
    usage_url: &str,
    home: PathBuf,
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    rejected_token: Option<String>,
    runtime: RenewalRuntime,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let RenewalRuntime {
        container,
        launcher,
    } = runtime;
    let program = match &container {
        Some(_) => None,
        None => Some(launcher.locate(&environment).await?),
    };
    // Held until the renewal has persisted the new login, even if this
    // future is dropped (see `run_claude_refresh`).
    let admission = admit_renewal(crate::upgrade::gate())?;
    let source = LoginSource::File { home: home.clone() };
    let locks = source.lock().await?;
    let active = read_credentials(&home).await?;
    if let Ok(token) = oauth_access_token(&active, mj_core::clock::epoch_millis())
        && rejected_token.as_deref() != Some(token)
    {
        drop(locks);
        return query_api(client, usage_url, token, &active).await;
    }
    let credentials = renew_locked(
        active,
        &source,
        environment,
        executor,
        Renewal {
            _locks: locks,
            _admission: admission,
            program,
        },
        container,
    )
    .await?;
    let token = oauth_access_token(&credentials, mj_core::clock::epoch_millis())
        .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))?;
    query_api(client, usage_url, token, &credentials).await
}

/// Claude access tokens last 8 hours (observed: `expiresAt` is the issue time
/// plus 8 h). The daemon refreshes when less than half of that remains, which
/// leaves every session copy, none of which can refresh, 4 hours of valid token
/// while no daemon runs (#1490).
pub(crate) const GRANT_REFRESH_LEAD: Duration = Duration::from_secs(4 * 60 * 60);

/// When a login expiring at `expires_at_ms` is due for renewal.
pub(crate) fn grant_due_at(expires_at_ms: i64) -> i64 {
    let lead_ms = i64::try_from(GRANT_REFRESH_LEAD.as_millis()).unwrap_or(i64::MAX);
    expires_at_ms.saturating_sub(lead_ms)
}

/// What [`refresh_grant_if_due`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantRefresh {
    /// Nothing to refresh: no login here, or it carries no refresh token.
    NotRefreshable,
    /// Enough lifetime remains. The login becomes due at this epoch time.
    Current { due_at_ms: i64 },
    /// Due, but a daemon handoff is waiting. The renewal did not start; the
    /// next daemon decides again from the file.
    Deferred,
    /// The login was renewed, by this call or by another daemon that held the
    /// native lock first. `expires_at_ms` is the new expiry.
    Renewed {
        by_this_call: bool,
        expires_at_ms: Option<i64>,
    },
}

/// Renew the profile's login when less than [`GRANT_REFRESH_LEAD`] of its
/// access token remains, under the native refresh locks with a reread, so only
/// one of several daemons sharing the home exchanges the refresh token. This
/// is the grant's only refresher: copies handed to harnesses carry no refresh
/// token. The decision does not consult the usage API's rate-limit hold, which
/// covers usage requests only.
pub(crate) async fn refresh_grant_if_due(
    source: LoginSource,
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    container: Option<crate::quota_runtime::ContainerCheck>,
    launcher: &ClaudeLauncher,
    gate: &Arc<crate::upgrade::Gate>,
    now_ms: i64,
) -> Result<GrantRefresh, ClaudeUsageError> {
    let due_at = |credentials: &Value| -> Option<i64> {
        if !oauth_has_refresh_token(credentials) {
            return None;
        }
        Some(grant_due_at(oauth_expires_at(
            credentials.get("claudeAiOauth")?,
        )?))
    };
    let credentials = match source.read().await {
        Ok(credentials) => credentials,
        Err(ClaudeUsageError::NotSignedIn) => return Ok(GrantRefresh::NotRefreshable),
        Err(error) => return Err(error),
    };
    let Some(due_at_ms) = due_at(&credentials) else {
        return Ok(GrantRefresh::NotRefreshable);
    };
    if due_at_ms > now_ms {
        return Ok(GrantRefresh::Current { due_at_ms });
    }
    // Due. Find a runnable `claude` first: with none, say so on every attempt
    // rather than strip copies nothing can renew.
    let program = match &container {
        Some(_) => None,
        None => Some(launcher.locate(&environment).await?),
    };
    // Admission covers the exchange only, from here until the new login is
    // persisted (it moves into the subprocess's thread with the locks). A
    // waiting handoff is not delayed by a renewal that has not begun.
    let admission = match admit_renewal(gate) {
        Ok(admission) => admission,
        Err(error) => {
            tracing::debug!(%error, "deferring the Claude login renewal until after the daemon handoff");
            return Ok(GrantRefresh::Deferred);
        }
    };
    let locks = source.lock().await?;
    let active = source.read().await?;
    match due_at(&active) {
        None => return Ok(GrantRefresh::NotRefreshable),
        Some(due_at_ms) if due_at_ms > now_ms => {
            // Another daemon or Claude Code renewed it while this one waited.
            let expires_at_ms = active.get("claudeAiOauth").and_then(oauth_expires_at);
            return Ok(GrantRefresh::Renewed {
                by_this_call: false,
                expires_at_ms,
            });
        }
        Some(_) => {}
    }
    let renewal = Renewal {
        _locks: locks,
        _admission: admission,
        program,
    };
    let renewed = renew_locked(active, &source, environment, executor, renewal, container).await?;
    Ok(GrantRefresh::Renewed {
        by_this_call: true,
        expires_at_ms: renewed.get("claudeAiOauth").and_then(oauth_expires_at),
    })
}

/// Where the login a renewal works on is kept, and how Claude Code is told to
/// find it. Built from [`mj_core::config::HarnessKind::login_store`].
#[derive(Clone)]
pub(crate) enum LoginSource {
    /// `<home>/.credentials.json`, with `CLAUDE_CONFIG_DIR` pointing at `home`.
    File { home: PathBuf },
    /// The macOS Keychain item every profile shares. `claude auth login` must
    /// run with `CLAUDE_CONFIG_DIR` unset, which is the only way it writes
    /// that item. `lock_marker` is the credential file under `~/.claude`
    /// whose directory carries the locks Claude Code itself takes; `security`
    /// reads the item.
    Keychain {
        lock_marker: PathBuf,
        security: Arc<dyn CommandExecutor + Send + Sync>,
    },
}

impl LoginSource {
    /// Take the native refresh locks every writer of this login takes.
    async fn lock(&self) -> Result<mj_core::credentials::CredentialWriteGuard, ClaudeUsageError> {
        let marker = match self {
            Self::File { home } => home.join(".credentials.json"),
            Self::Keychain { lock_marker, .. } => lock_marker.clone(),
        };
        mj_core::credentials::CredentialWriteGuard::acquire(
            mj_core::config::HarnessKind::Claude,
            &marker,
        )
        .await
        .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))
    }

    /// The login as it is now. A Keychain that cannot be read is an error,
    /// never "not signed in": a renewal spends the refresh token, and one
    /// started on a login that merely could not be read would lose the grant.
    async fn read(&self) -> Result<Value, ClaudeUsageError> {
        match self {
            Self::File { home } => read_credentials(home).await,
            Self::Keychain { security, .. } => {
                let security = Arc::clone(security);
                let login = tokio::task::spawn_blocking(move || {
                    crate::setup::read_claude_keychain_login(&*security)
                })
                .await
                .map_err(|error| ClaudeUsageError::Refresh(format!("worker failed: {error}")))?
                .map_err(|error| ClaudeUsageError::Refresh(format!("{error:#}")))?;
                match login {
                    crate::setup::KeychainLogin::Missing => Err(ClaudeUsageError::NotSignedIn),
                    crate::setup::KeychainLogin::Unreadable { status } => {
                        Err(ClaudeUsageError::Refresh(format!(
                            "the macOS Keychain could not be read (security exited with status {status}); unlock it and the renewal will be retried"
                        )))
                    }
                    crate::setup::KeychainLogin::Present(bytes) => {
                        serde_json::from_slice(&bytes).map_err(|_| ClaudeUsageError::NotSignedIn)
                    }
                }
            }
        }
    }

    /// Set the variables `claude auth login` needs to find this login.
    fn configure_environment(&self, environment: &mut HashMap<String, String>) {
        match self {
            Self::File { home } => {
                environment.insert(
                    "CLAUDE_CONFIG_DIR".into(),
                    home.to_string_lossy().into_owned(),
                );
            }
            Self::Keychain { .. } => {
                environment.remove("CLAUDE_CONFIG_DIR");
            }
        }
    }

    /// Point a Keychain renewal at the item the daemon reads and writes. An
    /// inherited `CLAUDE_CONFIG_DIR` would rename the item, and a `USER` from
    /// the profile's environment would change its account (Claude Code names
    /// the account after `USER`): either way the renewed login would land
    /// where nothing reads it, after the old refresh token was spent.
    fn confine_command(&self, command: &mut CommandSpec) {
        if matches!(self, Self::Keychain { .. }) {
            command.env.remove("CLAUDE_CONFIG_DIR");
            command.env_remove.push("CLAUDE_CONFIG_DIR".into());
            command.env_remove.retain(|name| name != "USER");
            command
                .env
                .insert("USER".into(), crate::setup::claude_keychain_account());
        }
    }
}

/// What one login renewal holds from before its first lock until the child
/// has exited: the native locks, the handoff admission, and the installation
/// of the `claude` it runs.
struct Renewal {
    _locks: mj_core::credentials::CredentialWriteGuard,
    _admission: crate::upgrade::Work,
    program: Option<ClaudeProgram>,
}

/// Run `claude auth login` with the stored refresh token while the caller
/// holds the native locks, and return the credentials it left. Fails when the
/// access token did not change.
async fn renew_locked(
    active: Value,
    source: &LoginSource,
    mut environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    renewal: Renewal,
    container: Option<crate::quota_runtime::ContainerCheck>,
) -> Result<Value, ClaudeUsageError> {
    let before = active
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .map(str::to_owned);
    source.configure_environment(&mut environment);
    let refresh_error =
        run_claude_refresh(active, source, environment, executor, renewal, container)
            .await
            .err();
    let credentials = match source.read().await {
        Ok(credentials) => credentials,
        Err(error) => return Err(refresh_error.unwrap_or(error)),
    };
    let token = match oauth_access_token(&credentials, mj_core::clock::epoch_millis()) {
        Ok(token) => token,
        Err(error) => {
            return Err(refresh_error.unwrap_or_else(|| {
                ClaudeUsageError::Refresh(format!(
                    "Claude auth login left no usable access token: {error}"
                ))
            }));
        }
    };
    if before.as_deref() == Some(token) {
        return Err(refresh_error.unwrap_or_else(|| {
            ClaudeUsageError::Refresh("Claude auth login did not renew the access token".to_owned())
        }));
    }
    Ok(credentials)
}

/// On macOS, read Claude's quota through its Keychain-aware CLI.
async fn run_claude_usage(
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    purpose: &'static str,
) -> Result<CommandOutput, String> {
    let mut command = CommandSpec::new(
        claude_program(),
        ["-p", "/usage", "--no-session-persistence"],
    )
    .purpose(purpose);
    command.env.extend(environment);
    tokio::task::spawn_blocking(move || executor.execute(&command))
        .await
        .map_err(|error| format!("worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

async fn run_claude_refresh(
    credentials: Value,
    source: &LoginSource,
    mut environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    renewal: Renewal,
    container: Option<crate::quota_runtime::ContainerCheck>,
) -> Result<(), ClaudeUsageError> {
    let oauth = &credentials["claudeAiOauth"];
    let refresh = oauth["refreshToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ClaudeUsageError::Refresh("stored refresh token is missing".into()))?;
    let scopes = oauth["scopes"]
        .as_array()
        .filter(|scopes| !scopes.is_empty())
        .and_then(|scopes| scopes.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .filter(|scopes| scopes.iter().all(|scope| !scope.is_empty()))
        .ok_or_else(|| {
            ClaudeUsageError::Refresh("stored OAuth scopes are missing or invalid".into())
        })?;
    environment.insert("CLAUDE_CODE_OAUTH_REFRESH_TOKEN".into(), refresh.to_owned());
    environment.insert("CLAUDE_CODE_OAUTH_SCOPES".into(), scopes.join(" "));
    // An empty client ID selects Claude's default and overrides an ambient ID
    // belonging to another account's automation.
    environment.insert(
        "CLAUDE_CODE_OAUTH_CLIENT_ID".into(),
        oauth["clientId"].as_str().unwrap_or_default().to_owned(),
    );
    let (mut command, quota_container) = match &container {
        None => {
            let program = renewal
                .program
                .as_ref()
                .ok_or_else(|| ClaudeUsageError::Refresh("no Claude Code to run".into()))?;
            let mut command = CommandSpec::new(
                mj_core::target_path::text(&program.command),
                ["auth", "login"],
            );
            command.env.extend(program.environment.clone());
            command.env.extend(environment);
            source.confine_command(&mut command);
            (command, None)
        }
        Some(check) => {
            if matches!(source, LoginSource::Keychain { .. }) {
                // The image cannot write the host's Keychain.
                return Err(ClaudeUsageError::Refresh(
                    "a login held in the macOS Keychain can only be renewed by Claude Code on this Mac"
                        .into(),
                ));
            }
            check
                .runtime
                .ensure_image_async()
                .await
                .map_err(|error| ClaudeUsageError::Refresh(format!("{error:#}")))?;
            let launch = check.launch(
                "claude",
                &["auth", "login"],
                &environment.into_iter().collect(),
            );
            let mut command =
                CommandSpec::new(mj_core::target_path::text(&launch.program), launch.args);
            command.env.extend(launch.env);
            (command, Some(launch.container))
        }
    };
    command.purpose = "refresh Claude login".to_owned();
    command.sensitive_output = true;
    // Once the child starts it runs to its own end. Anthropic spends the
    // refresh token on the first request, so a kill (a mj deadline, or the
    // caller being dropped) before the new login is written loses the grant.
    // Claude Code bounds its own network steps: the token request
    // (`fn(30000, ...)` in `Npe`, CC 2.1.321) and the profile lookup after it
    // (`fn(1e4, ...)`) each time out. The executor therefore has no deadline
    // and no cancellation, and the lock wait is counted apart
    // (`CLAUDE_REFRESH_LOCK_WAIT`).
    let output = tokio::task::spawn_blocking(move || {
        // Keep the locks, the installation and the handoff admission until
        // the executor has reaped its child, even if the async caller is
        // cancelled while this thread is still running.
        let _renewal = renewal;
        executor.execute(&command)
    })
    .await;
    if let Some(container) = quota_container {
        container.remove().await;
    }
    let output = output
        .map_err(|error| ClaudeUsageError::Refresh(format!("worker failed: {error}")))?
        .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))?;
    successful_refresh_output(output)
}

fn successful_refresh_output(output: CommandOutput) -> Result<(), ClaudeUsageError> {
    if output.status == 0 {
        Ok(())
    } else {
        let category = [output.stderr.as_slice(), output.stdout.as_slice()]
            .into_iter()
            .find_map(claude_refresh_failure_category);
        let detail = category
            .map(|category| format!(": {category}"))
            .unwrap_or_default();
        Err(ClaudeUsageError::Refresh(format!(
            "Claude auth login exited with status {}{detail}",
            output.status,
        )))
    }
}

fn claude_refresh_failure_category(bytes: &[u8]) -> Option<&'static str> {
    // Native auth login prints `Login failed: ${error.message}`. Axios can
    // also dump request bodies containing tokens: never return captured text.
    // Match complete status messages so token text or e.g. 4010 is not a status.
    for line in String::from_utf8_lossy(bytes).lines().map(str::trim) {
        let message = line
            .strip_prefix("Login failed: ")
            .or_else(|| line.strip_prefix("AxiosError: "));
        match message {
            Some("Request failed with status code 400") => return Some("HTTP 400 rejection"),
            Some("Request failed with status code 401") => return Some("HTTP 401 rejection"),
            Some("Request failed with status code 403") => return Some("HTTP 403 rejection"),
            Some(message)
                if message.starts_with("getaddrinfo ENOTFOUND ")
                    || message.starts_with("getaddrinfo EAI_AGAIN ") =>
            {
                return Some("DNS lookup failure");
            }
            Some(message) if message.starts_with("connect ECONNREFUSED ") => {
                return Some("network connection refused");
            }
            _ => {}
        }
        // Native auth login appends this fixed warning after a post-exchange
        // policy refusal. The surrounding organization details stay private.
        if line
            == "The refresh token in CLAUDE_CODE_OAUTH_REFRESH_TOKEN was used for this attempt and cannot be used again; issue a new one to retry."
        {
            return Some("native sign-in policy refusal after token exchange");
        }
    }
    None
}

fn oauth_access_token(credentials: &Value, now_ms: i64) -> Result<&str, ClaudeUsageError> {
    let oauth = credentials
        .get("claudeAiOauth")
        .ok_or(ClaudeUsageError::NotSignedIn)?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(ClaudeUsageError::NotSignedIn)?;
    if oauth_expires_at(oauth).is_some_and(|expires_at| expires_at <= now_ms) {
        return Err(ClaudeUsageError::LoginExpired);
    }
    Ok(token)
}

fn oauth_expires_at(oauth: &Value) -> Option<i64> {
    let value = oauth.get("expiresAt")?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|ms| i64::try_from(ms).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

fn oauth_has_refresh_token(credentials: &Value) -> bool {
    credentials
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
}

/// Monthly list prices, deliberately independent of the advertised usage
/// multipliers: Max's weekly allowance does not scale with its five-hour one.
fn monthly_price_usd(credentials: &Value) -> Option<u32> {
    match credentials
        .pointer("/claudeAiOauth/rateLimitTier")?
        .as_str()?
    {
        "default_claude_max_5x" => Some(100),
        "default_claude_max_20x" => Some(200),
        "default_claude_pro" => Some(20),
        _ => None,
    }
}

fn parse_api_usage(payload: &Value) -> Option<ClaudeUsageReport> {
    let mut five_hour = None;
    let mut week = None;
    let mut scoped_weeks = Vec::new();

    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(kind) = limit.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let Some(mut window) = api_window(limit, "percent") else {
                continue;
            };
            match kind {
                "session" => {
                    window.duration_seconds = Some(5 * 60 * 60);
                    five_hour = Some(window);
                }
                "weekly_all" => {
                    window.duration_seconds = Some(7 * 24 * 60 * 60);
                    week = Some(window);
                }
                "weekly_scoped" => {
                    if let Some(scope) = api_scope_name(limit) {
                        window.duration_seconds = Some(7 * 24 * 60 * 60);
                        scoped_weeks.push((scope, window));
                    }
                }
                _ => {}
            }
        }
    } else {
        five_hour = payload
            .get("five_hour")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"));
        if let Some(mut window) = payload
            .get("seven_day")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"))
        {
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            week = Some(window);
        }
        if let Some(mut window) = payload
            .get("seven_day_fable")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"))
        {
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            scoped_weeks.push(("fable".to_owned(), window));
        }
    }

    (five_hour.is_some()
        || week.is_some()
        || !scoped_weeks.is_empty()
        || payload
            .pointer("/extra_usage/is_enabled")
            .and_then(Value::as_bool)
            == Some(true))
    .then(|| ClaudeUsageReport {
        monthly_price_usd: None,
        paid_credits: None,
        banked_resets: parse_banked_resets(payload, mj_core::clock::epoch_millis() / 1000),
        five_hour,
        week,
        scoped_weeks,
    })
}

fn parse_banked_resets(payload: &Value, now: i64) -> Option<u64> {
    let block = payload
        .get("cedar_ember")
        .filter(|value| !value.is_null())?;
    let parsed = (|| {
        if !block.get("eligible")?.as_bool()? {
            return Some(0);
        }
        let mut total = 0_u64;
        for grant in block.get("grants")?.as_array()? {
            let remaining = grant.get("resets_left")?.as_u64()?;
            let timestamp = |key| -> Option<Option<i64>> {
                match grant.get(key).filter(|value| !value.is_null()) {
                    None => Some(None),
                    Some(value) => chrono::DateTime::parse_from_rfc3339(value.as_str()?)
                        .ok()
                        .map(|date| Some(date.timestamp())),
                }
            };
            if timestamp("starts_at")?.is_some_and(|start| start > now)
                || timestamp("ends_at")?.is_some_and(|end| end <= now)
            {
                continue;
            }
            // A banked reset can require reaching a limit before use.
            // `usable_now` is not the balance of unused resets.
            total = total.checked_add(remaining)?;
        }
        Some(total)
    })();
    if parsed.is_none() {
        tracing::warn!("Claude usage returned malformed reset-grant metadata");
    }
    parsed
}

fn api_window(value: &Value, percent_key: &str) -> Option<ClaudeUsageWindow> {
    let used = value.get(percent_key)?.as_f64()?;
    let used = used.round().clamp(0.0, 100.0) as u8;
    Some(ClaudeUsageWindow {
        remaining_percent: 100 - used,
        duration_seconds: None,
        reset_context: value
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Scrape `claude -p /usage` for the two windows the quota row shows.
///
/// The command prints one line per window, each naming its own percentage:
///
/// ```text
/// Current session: 6% used · resets Sep 18 at 6:20pm (Europe/Paris)
/// Current week (all models): 30% used · resets Sep 20 at 1:59pm (Europe/Paris)
/// Current week (Fable): 40% used · resets Sep 20 at 2pm (Europe/Paris)
/// ```
///
/// The sections below those quote unrelated percentages ("57% of your usage
/// was at >150k context", "Top skills: /code-review 3%"), so only a line whose
/// label is one of the windows is read, and only when the number is followed
/// by `used`. A percentage that means something else is worth reporting as
/// unparsed rather than reporting backwards.
fn parse_cli_usage(output: &str) -> Option<ClaudeUsageReport> {
    let mut five_hour = None;
    let mut week = None;
    let mut scoped_weeks = Vec::new();

    for line in strip_ansi(output).lines() {
        let Some((label, rest)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim().to_ascii_lowercase();
        if label != "current session" && !label.starts_with("current week") {
            continue;
        }
        let Some(mut window) = cli_window(rest) else {
            continue;
        };
        if label == "current session" {
            window.duration_seconds = Some(5 * 60 * 60);
            five_hour = Some(window);
        } else if label == "current week (all models)" || label == "current week" {
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            week = Some(window);
        } else {
            let Some(scope) = label
                .strip_prefix("current week (")
                .and_then(|label| label.strip_suffix(')'))
            else {
                continue;
            };
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            scoped_weeks.push((scope.to_owned(), window));
        }
    }

    (five_hour.is_some() || week.is_some() || !scoped_weeks.is_empty()).then_some(
        ClaudeUsageReport {
            paid_credits: None,
            monthly_price_usd: None,
            banked_resets: None,
            five_hour,
            week,
            scoped_weeks,
        },
    )
}

/// One window from the text after a `/usage` line's label.
fn cli_window(rest: &str) -> Option<ClaudeUsageWindow> {
    let (used, tail) = rest.trim_start().split_once('%')?;
    let used: f64 = used.trim().parse().ok()?;
    if !tail.trim_start().starts_with("used") {
        return None;
    }
    Some(ClaudeUsageWindow {
        remaining_percent: 100 - (used.round().clamp(0.0, 100.0) as u8),
        duration_seconds: None,
        reset_context: tail
            .split_once("resets ")
            .map(|(_, context)| context.trim().to_owned())
            .filter(|context| !context.is_empty()),
    })
}

/// Claude Code paints `/usage` for a terminal, so its output can carry SGR
/// escapes even in print mode.
fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        // CSI sequences end at their final byte; anything else ends at the
        // next character.
        if characters.next() == Some('[') {
            for character in characters.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    output
}

fn is_authentication_error(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    [
        "not logged in",
        "not signed in",
        "unauthenticated",
        "unauthorized",
        "please log in",
        "please login",
        "invalid api key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn api_scope_name(value: &Value) -> Option<String> {
    value
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Mutex;

    /// Captured verbatim from `claude -p /usage --no-session-persistence`.
    const CLI_USAGE: &str = "\
You are currently using your subscription to power your Claude Code usage

Current session: 6% used · resets Sep 18 at 6:20pm (Europe/Paris)
Current week (all models): 30% used · resets Sep 20 at 1:59pm (Europe/Paris)
Current week (Fable): 40% used · resets Sep 20 at 2pm (Europe/Paris)

What's contributing to your limits usage?
Approximate, based on local sessions on this machine — does not include other devices or claude.ai. Behaviors are independent characteristics, not a breakdown.

Last 24h · 2108 requests · 13 sessions
  57% of your usage was at >150k context
  41% of your usage was while 4+ sessions ran in parallel
  31% of your usage came from subagent-heavy sessions
  Top skills: /code-review 3%
  Top MCP servers: claude-in-chrome 1%

Last 7d · 5966 requests · 78 sessions
  52% of your usage was at >150k context
  28% of your usage came from subagent-heavy sessions
  28% of your usage was while 4+ sessions ran in parallel
  Top skills: /code-review 2%
  Top subagents: Explore 2%, Plan 1%
  Top MCP servers: claude-in-chrome 2%
";

    #[test]
    fn banked_resets_count_unspent_grants_without_requiring_immediate_use() {
        let payload = serde_json::json!({"cedar_ember": {"eligible": true, "grants": [
            {"resets_left": 1, "usable_now": false, "use_requires_limit": true},
            {"resets_left": 2, "starts_at": "1970-01-01T00:01:40Z", "ends_at": "1970-01-01T00:03:20Z"},
            {"resets_left": 5, "ends_at": "1970-01-01T00:01:40Z"},
            {"resets_left": 7, "starts_at": "1970-01-01T00:03:20Z"}
        ]}});
        assert_eq!(super::parse_banked_resets(&payload, 100), Some(3));
        assert_eq!(
            super::parse_banked_resets(
                &serde_json::json!({"cedar_ember": {"eligible": false}}),
                100
            ),
            Some(0)
        );
        assert_eq!(
            super::parse_banked_resets(
                &serde_json::json!({"cedar_ember": {"eligible": true, "grants": []}}),
                100
            ),
            Some(0)
        );
    }

    #[test]
    fn unknown_reset_metadata_does_not_discard_claude_usage() {
        for block in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"eligible": true}),
            serde_json::json!({"eligible": true, "grants": [{"resets_left": -1}]}),
            serde_json::json!({"eligible": true, "grants": [{"resets_left": "1"}]}),
            serde_json::json!({"eligible": true, "grants": [{"resets_left": 1, "ends_at": "invalid"}]}),
        ] {
            let report = super::parse_api_usage(&serde_json::json!({
                "five_hour": {"utilization": 25}, "cedar_ember": block
            }))
            .unwrap();
            assert_eq!(report.banked_resets, None);
            assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        }
    }
    #[test]
    fn cli_usage_reports_both_windows_and_the_binding_weekly_limit() {
        let report = parse_cli_usage(CLI_USAGE).expect("the captured output parses");

        let five_hour = report.five_hour.expect("session window");
        assert_eq!(five_hour.remaining_percent, 94);
        assert_eq!(
            five_hour.reset_context.as_deref(),
            Some("Sep 18 at 6:20pm (Europe/Paris)")
        );
        // The all-models limit remains distinct from the Fable limit.
        let week = report.week.expect("weekly window");
        assert_eq!(week.remaining_percent, 70);
        assert_eq!(
            week.reset_context.as_deref(),
            Some("Sep 20 at 1:59pm (Europe/Paris)")
        );
        assert_eq!(report.scoped_weeks.len(), 1);
        assert_eq!(report.scoped_weeks[0].0, "fable");
        assert_eq!(report.scoped_weeks[0].1.remaining_percent, 60);
        assert_eq!(report.scoped_weeks[0].1.duration_seconds, Some(604_800));
    }

    #[test]
    fn api_usage_keeps_all_and_scoped_weekly_windows_separate() {
        let report = parse_api_usage(&serde_json::json!({
            "limits": [
                {"kind": "weekly_all", "percent": 30},
                {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}}, "percent": 40},
                {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Mythos"}}, "percent": 50}
            ]
        })).unwrap();
        assert_eq!(report.week.unwrap().remaining_percent, 70);
        assert_eq!(report.scoped_weeks.len(), 2);
        assert_eq!(report.scoped_weeks[0].0, "fable");
        assert_eq!(report.scoped_weeks[0].1.remaining_percent, 60);
        assert_eq!(report.scoped_weeks[1].0, "mythos");

        let legacy = parse_api_usage(&serde_json::json!({
            "seven_day": {"utilization": 25},
            "seven_day_fable": {"utilization": 100}
        }))
        .unwrap();
        assert_eq!(legacy.week.unwrap().remaining_percent, 75);
        assert_eq!(legacy.scoped_weeks[0].0, "fable");
        assert_eq!(legacy.scoped_weeks[0].1.remaining_percent, 0);
    }

    /// The later sections are full of percentages that are not quota.
    #[test]
    fn cli_usage_ignores_percentages_that_are_not_a_window() {
        let report = parse_cli_usage(
            "Last 24h · 2108 requests · 13 sessions\n  \
             57% of your usage was at >150k context\n  \
             Top skills: /code-review 3%\n",
        );
        assert_eq!(report, None);
    }

    #[test]
    fn cli_usage_survives_terminal_colouring() {
        let coloured = "\u{1b}[1mCurrent session:\u{1b}[0m \u{1b}[32m6% used\u{1b}[0m · resets Sep 18 at 6:20pm\n";
        let report = parse_cli_usage(coloured).expect("coloured output parses");
        assert_eq!(report.five_hour.unwrap().remaining_percent, 94);
    }

    /// A window whose number stops meaning "used" must read as unparsed
    /// rather than as its own complement.
    #[test]
    fn cli_usage_refuses_a_percentage_it_cannot_interpret() {
        assert_eq!(
            parse_cli_usage("Current session: 6% left · resets Sep 18 at 6:20pm\n"),
            None
        );
    }

    // Hard-won: #1485: discarded native failures hid the reason while raw Axios output can expose tokens.
    #[test]
    fn refresh_failure_categories_never_disclose_native_output() {
        let cases = [
            (
                "Login failed: Request failed with status code 400",
                ": HTTP 400 rejection",
            ),
            (
                "AxiosError: Request failed with status code 401",
                ": HTTP 401 rejection",
            ),
            (
                "Login failed: Request failed with status code 403",
                ": HTTP 403 rejection",
            ),
            (
                "Login failed: getaddrinfo ENOTFOUND private.example",
                ": DNS lookup failure",
            ),
            (
                "Login failed: getaddrinfo EAI_AGAIN private.example",
                ": DNS lookup failure",
            ),
            (
                "Login failed: connect ECONNREFUSED 127.0.0.1:8443",
                ": network connection refused",
            ),
            (
                "Private organization refused sign-in\nThe refresh token in CLAUDE_CODE_OAUTH_REFRESH_TOKEN was used for this attempt and cannot be used again; issue a new one to retry.",
                ": native sign-in policy refusal after token exchange",
            ),
            ("Login failed: Request failed with status code 4010", ""),
            ("Login failed: provider sent private response 401", ""),
            ("Login failed: unknown private failure", ""),
            ("Login failed: Request failed with status code 500", ""),
        ];
        for (message, category) in cases {
            // Simulate Axios's token-bearing request dump and independent
            // stdout, including arbitrary bytes that must never be echoed.
            let mut stderr = format!(
                "{message}\nconfig: {{data: '{{\"refresh_token\":\"SECRET_REFRESH\"}}', headers: {{Authorization: 'Bearer SECRET_ACCESS'}}}}\n"
            )
            .into_bytes();
            stderr.extend_from_slice(b"\xffFAILED_STDERR_SENTINEL");
            for stderr_first in [true, false] {
                let sentinel = b"FAILED_STDOUT_SENTINEL".to_vec();
                let (stdout, stderr) = if stderr_first {
                    (sentinel, stderr.clone())
                } else {
                    (stderr.clone(), sentinel)
                };
                let error = successful_refresh_output(CommandOutput {
                    status: 7,
                    stdout,
                    stderr,
                })
                .unwrap_err();
                assert_eq!(
                    error,
                    ClaudeUsageError::Refresh(format!(
                        "Claude auth login exited with status 7{category}"
                    )),
                    "classification must return only fixed text for {message:?}"
                );
            }
        }
        assert!(
            successful_refresh_output(CommandOutput {
                status: 0,
                stdout: b"Login failed: Request failed with status code 401".to_vec(),
                stderr: b"SECRET_SUCCESS_SENTINEL".to_vec(),
            })
            .is_ok()
        );
    }

    #[derive(Clone)]
    struct RefreshExecutor {
        home: PathBuf,
        replacement: Option<Value>,
        status: i32,
        commands: Arc<Mutex<Vec<CommandSpec>>>,
    }

    impl CommandExecutor for RefreshExecutor {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            assert_eq!(command.args, ["auth", "login"]);
            assert_eq!(
                command.env["CLAUDE_CONFIG_DIR"],
                self.home.to_string_lossy()
            );
            assert!(self.home.join(".oauth_refresh.lock").is_dir());
            let mut legacy_path = self.home.canonicalize().unwrap().into_os_string();
            legacy_path.push(".lock");
            assert!(PathBuf::from(legacy_path).is_dir());
            self.commands.lock().unwrap().push(command.clone());
            if let Some(replacement) = &self.replacement {
                std::fs::write(
                    self.home.join(".credentials.json"),
                    serde_json::to_vec(replacement)?,
                )?;
            }
            Ok(CommandOutput {
                status: self.status,
                stdout: b"Approximate local usage".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[derive(Clone)]
    struct UsageServerState {
        reject_first: bool,
        authorizations: Arc<Mutex<Vec<String>>>,
    }

    async fn test_usage(
        State(state): State<UsageServerState>,
        axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(query.get("cedar_ember").map(String::as_str), Some("1"));
        assert!(!query.contains_key("skip_spend"));
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            USAGE_USER_AGENT
        );
        let authorization = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut authorizations = state.authorizations.lock().unwrap();
        let reject = state.reject_first && authorizations.is_empty();
        authorizations.push(authorization);
        drop(authorizations);
        if reject {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
        }
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "five_hour": {"utilization": 25.0, "resets_at": "2026-08-23T00:00:00Z"},
                "seven_day": {"utilization": 40.0, "resets_at": "2026-08-29T00:00:00Z"},
                "cedar_ember": {"eligible": true, "grants": [{"resets_left": 1}]}
            })),
        )
    }

    async fn spawn_usage_server(
        reject_first: bool,
    ) -> (String, UsageServerState, tokio::task::JoinHandle<()>) {
        let state = UsageServerState {
            reject_first,
            authorizations: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/usage", get(test_usage))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/usage"), state, server)
    }

    fn credentials(token: &str, refresh: Option<&str>, expires_at: i64) -> Value {
        let mut oauth = serde_json::json!({
            "accessToken": token,
            "expiresAt": expires_at,
            "scopes": ["user:profile", "user:inference"],
        });
        if let Some(refresh) = refresh {
            oauth["refreshToken"] = Value::String(refresh.to_owned());
        }
        serde_json::json!({"claudeAiOauth": oauth})
    }

    #[tokio::test]
    async fn paid_usage_reads_wallet_and_respects_enabled_status_and_spending_cap() {
        use mj_client::quota::PaidCredits;
        for (funds, cap, spent, enabled, wallet_status, expected) in [
            (1200, Some(2000), 500, true, StatusCode::OK, Some(1200.0)),
            (5000, Some(2000), 500, true, StatusCode::OK, Some(1500.0)),
            (5000, Some(2000), 2100, true, StatusCode::OK, Some(0.0)),
            (5000, None, 500, true, StatusCode::OK, Some(5000.0)),
            (0, Some(2000), 500, true, StatusCode::OK, Some(0.0)),
            (5000, Some(2000), 500, false, StatusCode::OK, None),
            (5000, Some(2000), 500, true, StatusCode::FORBIDDEN, None),
        ] {
            let wallet_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reads = wallet_reads.clone();
            let app = Router::new()
                .route("/usage", get(move || async move { Json(serde_json::json!({
                    "five_hour": {"utilization": 100},
                    "extra_usage": {"is_enabled": enabled, "monthly_limit": cap, "used_credits": spent, "currency": "USD"}
                })) }))
                .route("/api/oauth/profile", get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer test-token");
                    Json(serde_json::json!({"organization": {"uuid": "test-org"}}))
                }))
                .route("/api/oauth/organizations/test-org/prepaid/credits", get(move |headers: HeaderMap| {
                    let reads = reads.clone();
                    async move {
                        reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        assert_eq!(headers["authorization"], "Bearer test-token");
                        assert_eq!(headers["x-organization-uuid"], "test-org");
                        (wallet_status, Json(serde_json::json!({"amount": funds, "currency": "USD"})))
                    }
                }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let report = query_api(
                &reqwest::Client::new(),
                &format!("http://{address}/usage"),
                "test-token",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
            task.abort();
            assert_eq!(report.five_hour.unwrap().remaining_percent, 0);
            assert_eq!(report.paid_credits, expected.and_then(PaidCredits::limited));
            assert_eq!(
                wallet_reads.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(enabled)
            );
        }
    }

    async fn usage_status_only(
        status: StatusCode,
        retry_after: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/usage",
            get(move || async move {
                let mut headers = HeaderMap::new();
                if let Some(value) = retry_after {
                    headers.insert("retry-after", value.parse().unwrap());
                }
                (status, headers)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/usage"), server)
    }

    #[tokio::test]
    async fn a_429_is_a_rate_limit_that_carries_the_servers_retry_after() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "token",
                None,
                mj_core::clock::epoch_millis() + 3_600_000,
            ))
            .unwrap(),
        )
        .unwrap();
        for (header, expected) in [
            (Some("120"), Some(Duration::from_secs(120))),
            (None, None),
            (Some("not a delay"), None),
        ] {
            let (url, server) = usage_status_only(StatusCode::TOO_MANY_REQUESTS, header).await;
            let executor = RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: None,
                status: 0,
                commands: Arc::new(Mutex::new(Vec::new())),
            };
            let error = query_with(
                home.path().to_path_buf(),
                HashMap::new(),
                &url,
                Arc::new(executor),
            )
            .await
            .unwrap_err();
            assert_eq!(error, ClaudeUsageError::RateLimited(expected), "{header:?}");
            assert_eq!(error.to_string(), "rate limited");
            server.abort();
        }
    }

    // Hard-won: 958aba5ce: successful /usage stopped renewing expired OAuth tokens.
    #[tokio::test]
    async fn expired_login_exchanges_its_refresh_token_without_starting_a_session() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".credentials.json");
        let mut old = credentials("old", Some("refresh"), 1);
        old["claudeAiOauth"]["rateLimitTier"] = "default_claude_max_5x".into();
        old["claudeAiOauth"]["clientId"] = "custom-client".into();
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut fresh = credentials(
            "fresh",
            Some("rotated"),
            mj_core::clock::epoch_millis() + 60_000,
        );
        fresh["claudeAiOauth"]["rateLimitTier"] = "default_claude_max_20x".into();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(fresh),
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        let mut environment = HashMap::from([(
            "CLAUDE_CONFIG_DIR".to_owned(),
            home.path().to_string_lossy().into_owned(),
        )]);

        let report = query_with(
            home.path().to_path_buf(),
            environment.clone(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        assert_eq!(report.monthly_price_usd, Some(200));
        assert_eq!(report.banked_resets, Some(1));
        assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args, ["auth", "login"]);
        environment.insert("CLAUDE_CODE_OAUTH_REFRESH_TOKEN".into(), "refresh".into());
        environment.insert(
            "CLAUDE_CODE_OAUTH_SCOPES".into(),
            "user:profile user:inference".into(),
        );
        environment.insert("CLAUDE_CODE_OAUTH_CLIENT_ID".into(), "custom-client".into());
        assert_eq!(commands[0].env, environment.into_iter().collect());
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer fresh"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn authoritative_rejection_refreshes_once_and_retries_with_new_credentials() {
        let home = tempfile::tempdir().unwrap();
        let fresh_expiry = mj_core::clock::epoch_millis() + 60_000;
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), fresh_expiry)).unwrap(),
        )
        .unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(credentials("fresh", Some("rotated"), fresh_expiry)),
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(true).await;

        query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].env["CLAUDE_CODE_OAUTH_CLIENT_ID"], "");
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer old", "Bearer fresh"]
        );
        server.abort();
    }

    struct BlockedRefreshExecutor {
        inner: RefreshExecutor,
        started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        finish: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl CommandExecutor for BlockedRefreshExecutor {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            self.finish
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))?;
            self.inner.execute(command)
        }
    }

    #[tokio::test]
    async fn concurrent_quota_readers_reuse_the_token_renewed_under_claudes_lock() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let executor = Arc::new(BlockedRefreshExecutor {
            inner: RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: Some(credentials(
                    "fresh",
                    Some("rotated"),
                    mj_core::clock::epoch_millis() + 60_000,
                )),
                status: 0,
                commands: commands.clone(),
            },
            started: Mutex::new(Some(started_tx)),
            finish: Mutex::new(finish_rx),
        });
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        let first_home = home.path().to_path_buf();
        let first_url = usage_url.clone();
        let first_executor = executor.clone();
        let first = tokio::spawn(async move {
            query_with(first_home, HashMap::new(), &first_url, first_executor).await
        });
        started_rx.await.unwrap();
        let second = query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            executor,
        );
        // Poll the second reader while the first remains inside its exchange.
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut second)
                .await
                .is_err()
        );
        finish_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap();
        assert_eq!(commands.lock().unwrap().len(), 1);
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer fresh", "Bearer fresh"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_a_quota_reader_keeps_its_refresh_lock_until_the_child_finishes() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let executor = Arc::new(BlockedRefreshExecutor {
            inner: RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: None,
                status: 1,
                commands: Arc::new(Mutex::new(Vec::new())),
            },
            started: Mutex::new(Some(started_tx)),
            finish: Mutex::new(finish_rx),
        });
        let path = home.path().to_path_buf();
        let task = tokio::spawn(async move {
            query_with(path, HashMap::new(), "http://127.0.0.1:1/unused", executor).await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let lock = home.path().join(".oauth_refresh.lock");
        assert!(
            lock.is_dir(),
            "the cancelled wait still has a running child"
        );
        finish_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    // Hard-won: 85b96a6d5: full-suite load could delay lock acquisition past the test's 150ms wait.
    #[tokio::test]
    async fn a_legacy_claude_lock_delays_refresh_and_the_waiter_rereads_the_renewed_token() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".credentials.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let mut legacy = home.path().canonicalize().unwrap().into_os_string();
        legacy.push(".lock");
        let legacy = PathBuf::from(legacy);
        std::fs::create_dir(&legacy).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 0,
            commands: commands.clone(),
        });
        let (url, state, server) = spawn_usage_server(false).await;
        let query = query_with(home.path().to_path_buf(), HashMap::new(), &url, executor);
        tokio::pin!(query);
        let lock = home.path().join(".oauth_refresh.lock");
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = &mut query => panic!("quota reader finished while the legacy lock was held"),
                () = async {
                    while !lock.is_dir() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => {}
            }
        })
        .await
        .expect("quota reader did not acquire its first refresh lock");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut query)
                .await
                .is_err()
        );
        assert!(commands.lock().unwrap().is_empty());
        assert!(lock.is_dir());
        std::fs::write(
            &path,
            serde_json::to_vec(&credentials(
                "fresh",
                Some("rotated"),
                mj_core::clock::epoch_millis() + 60_000,
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::remove_dir(legacy).unwrap();
        query.await.unwrap();
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(
            state.authorizations.lock().unwrap().as_slice(),
            ["Bearer fresh"]
        );
        server.abort();
    }

    // Hard-won: 958aba5ce: a successful CLI exit did not prove that credentials were renewed.
    #[tokio::test]
    async fn successful_cli_without_token_renewal_is_reported_as_a_refresh_failure() {
        for expires_at in [1, mj_core::clock::epoch_millis() + 60_000] {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(
                home.path().join(".credentials.json"),
                serde_json::to_vec(&credentials("old", Some("refresh"), expires_at)).unwrap(),
            )
            .unwrap();
            let executor = Arc::new(RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: None,
                status: 0,
                commands: Arc::new(Mutex::new(Vec::new())),
            });
            let (url, _, server) = spawn_usage_server(true).await;
            let error = query_with(home.path().to_path_buf(), HashMap::new(), &url, executor)
                .await
                .unwrap_err();
            assert!(matches!(error, ClaudeUsageError::Refresh(_)), "{error}");
            assert!(!home.path().join(".oauth_refresh.lock").exists());
            server.abort();
        }
    }

    #[tokio::test]
    async fn valid_credentials_query_authoritative_usage_without_launching_claude() {
        let home = tempfile::tempdir().unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 0,
            commands: commands.clone(),
        });
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        // Real OAuth tier metadata must survive the HTTP query, including a
        // plan change between polls. Unknown plans must not invent a price.
        for (tier, expected) in [
            ("default_claude_max_5x", Some(100)),
            ("default_claude_max_20x", Some(200)),
            ("default_claude_pro", Some(20)),
            ("future-plan", None),
        ] {
            let mut login = credentials(
                "current",
                Some("refresh"),
                mj_core::clock::epoch_millis() + 60_000,
            );
            login["claudeAiOauth"]["rateLimitTier"] = tier.into();
            std::fs::write(
                home.path().join(".credentials.json"),
                serde_json::to_vec(&login).unwrap(),
            )
            .unwrap();
            let report = query_with(
                home.path().to_path_buf(),
                HashMap::new(),
                &usage_url,
                executor.clone(),
            )
            .await
            .unwrap();
            assert_eq!(report.monthly_price_usd, expected);
        }
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer current"; 4]
        );
        server.abort();
    }

    #[tokio::test]
    async fn failed_refresh_of_a_rejected_token_is_not_mislabeled_login_expired() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "rejected",
                Some("refresh"),
                mj_core::clock::epoch_millis() + 60_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 1,
            commands: Arc::new(Mutex::new(Vec::new())),
        };
        let (usage_url, server_state, server) = spawn_usage_server(true).await;

        let error = query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ClaudeUsageError::Refresh(_)));
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer rejected"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn failed_cli_is_accepted_when_credentials_were_refreshed_concurrently() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(credentials(
                "fresh",
                Some("rotated"),
                mj_core::clock::epoch_millis() + 60_000,
            )),
            status: 1,
            commands: Arc::new(Mutex::new(Vec::new())),
        };
        let (usage_url, _, server) = spawn_usage_server(false).await;

        let report = query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        assert_eq!(report.week.unwrap().remaining_percent, 60);
        server.abort();
    }

    #[test]
    fn expired_oauth_access_token_is_login_expired() {
        let credentials = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "expiresAt": 1_000
            }
        });
        assert_eq!(
            oauth_access_token(&credentials, 1_001),
            Err(ClaudeUsageError::LoginExpired)
        );
        assert_eq!(
            oauth_access_token(&credentials, 1_000),
            Err(ClaudeUsageError::LoginExpired)
        );
    }

    #[test]
    fn current_oauth_access_token_is_usable() {
        let credentials = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "expiresAt": 2_000
            }
        });
        assert_eq!(
            oauth_access_token(&credentials, 1_999).expect("token"),
            "sk-ant-oat01-test"
        );
    }
    /// A macOS Keychain and the `claude` that writes it, in one fake: `security`
    /// answers with the stored login, `claude auth login` swaps in the renewed
    /// one. It checks what a Keychain renewal must look like to Claude Code:
    /// no `CLAUDE_CONFIG_DIR`, and the `~/.claude` locks held.
    struct FakeMac {
        login: Mutex<Option<Value>>,
        security_status: i32,
        renewed: Value,
        /// The directory Claude Code takes its refresh locks under.
        lock_home: PathBuf,
        claude_runs: Mutex<Vec<CommandSpec>>,
    }

    impl CommandExecutor for FakeMac {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            if command.program == "security" {
                assert_eq!(command.args[0], "find-generic-password");
                assert_eq!(command.args[1], "-a");
                assert_eq!(command.args[3..], ["-s", "Claude Code-credentials", "-w"]);
                return Ok(CommandOutput {
                    status: self.security_status,
                    stdout: self
                        .login
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|login| serde_json::to_vec(login).unwrap())
                        .unwrap_or_default(),
                    stderr: Vec::new(),
                });
            }
            assert_eq!(command.args, ["auth", "login"]);
            assert!(!command.env.contains_key("CLAUDE_CONFIG_DIR"));
            assert!(command.env_remove.contains(&"CLAUDE_CONFIG_DIR".to_owned()));
            assert_eq!(
                command.env.get("USER"),
                Some(&crate::setup::claude_keychain_account()),
                "the renewal writes the account the daemon reads"
            );
            assert!(self.lock_home.join(".oauth_refresh.lock").is_dir());
            self.claude_runs.lock().unwrap().push(command.clone());
            *self.login.lock().unwrap() = Some(self.renewed.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn fake_mac(
        directory: &std::path::Path,
        security_status: i32,
        expires_in_ms: i64,
    ) -> Arc<FakeMac> {
        let now = mj_core::clock::epoch_millis();
        Arc::new(FakeMac {
            login: Mutex::new(Some(credentials(
                "access-1",
                Some("refresh-1"),
                now + expires_in_ms,
            ))),
            security_status,
            renewed: credentials("access-2", Some("refresh-2"), now + 8 * 3_600_000),
            lock_home: directory.canonicalize().unwrap(),
            claude_runs: Mutex::new(Vec::new()),
        })
    }

    fn keychain_source(directory: &std::path::Path, mac: &Arc<FakeMac>) -> LoginSource {
        LoginSource::Keychain {
            lock_marker: directory.join(".credentials.json"),
            security: mac.clone(),
        }
    }

    const ONE_HOUR_MS: i64 = 3_600_000;

    async fn renew_keychain_login(
        source: LoginSource,
        mac: &Arc<FakeMac>,
    ) -> Result<GrantRefresh, ClaudeUsageError> {
        // An ambient `CLAUDE_CONFIG_DIR`, as a profile's own environment may
        // set, must not reach the renewal.
        // Nor may a `USER` from that environment, which would move the
        // renewed login to another Keychain account.
        let environment = HashMap::from([
            ("CLAUDE_CONFIG_DIR".to_owned(), "/elsewhere".to_owned()),
            ("USER".to_owned(), "someone-else".to_owned()),
        ]);
        refresh_grant_if_due(
            source,
            environment,
            mac.clone(),
            None,
            &ClaudeLauncher::Program(PathBuf::from("claude")),
            &Arc::new(crate::upgrade::Gate::default()),
            mj_core::clock::epoch_millis(),
        )
        .await
    }

    /// #1491: a Keychain login is renewed through the Keychain as the source,
    /// under the locks Claude Code takes for `~/.claude`, with
    /// `CLAUDE_CONFIG_DIR` unset so `claude auth login` writes the item the
    /// sessions' copies come from; the next look finds it current.
    #[tokio::test]
    async fn a_due_keychain_login_is_renewed_with_no_config_dir_under_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let mac = fake_mac(directory.path(), 0, ONE_HOUR_MS);

        let first = renew_keychain_login(keychain_source(directory.path(), &mac), &mac)
            .await
            .unwrap();
        assert!(
            matches!(
                first,
                GrantRefresh::Renewed {
                    by_this_call: true,
                    ..
                }
            ),
            "{first:?}"
        );
        {
            let runs = mac.claude_runs.lock().unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].env["CLAUDE_CODE_OAUTH_REFRESH_TOKEN"], "refresh-1");
        }
        assert!(!directory.path().join(".oauth_refresh.lock").exists());

        let second = renew_keychain_login(keychain_source(directory.path(), &mac), &mac)
            .await
            .unwrap();
        assert!(matches!(second, GrantRefresh::Current { .. }), "{second:?}");
        assert_eq!(mac.claude_runs.lock().unwrap().len(), 1);
    }

    /// #1491: profiles on one Mac share the Keychain item. A renewal that waits
    /// for another's lock rereads the item under it, finds it renewed, and
    /// spends no second refresh token.
    #[tokio::test]
    async fn a_waiting_keychain_renewal_follows_the_one_that_held_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let mac = fake_mac(directory.path(), 0, ONE_HOUR_MS);
        let held = mj_core::credentials::CredentialWriteGuard::acquire(
            mj_core::config::HarnessKind::Claude,
            &directory.path().join(".credentials.json"),
        )
        .await
        .unwrap();
        let follower = tokio::spawn({
            let (source, mac) = (keychain_source(directory.path(), &mac), mac.clone());
            async move { renew_keychain_login(source, &mac).await }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!follower.is_finished(), "the lock is held");
        // The holder finishes its renewal, then lets go.
        *mac.login.lock().unwrap() = Some(mac.renewed.clone());
        drop(held);

        let outcome = follower.await.unwrap().unwrap();
        assert!(
            matches!(
                outcome,
                GrantRefresh::Renewed {
                    by_this_call: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(mac.claude_runs.lock().unwrap().is_empty());
    }

    /// #1491: a Keychain that cannot be read (locked) is not a missing login.
    /// The exchange spends the single-use refresh token, so none may start
    /// while the result could not be read back; the failure is reported.
    #[tokio::test]
    async fn a_locked_keychain_starts_no_renewal() {
        let directory = tempfile::tempdir().unwrap();
        let mac = fake_mac(directory.path(), 36, ONE_HOUR_MS);

        let error = renew_keychain_login(keychain_source(directory.path(), &mac), &mac)
            .await
            .unwrap_err();

        assert!(
            matches!(&error, ClaudeUsageError::Refresh(message) if message.contains("Keychain")),
            "{error}"
        );
        assert!(mac.claude_runs.lock().unwrap().is_empty());
        // And no login in the Keychain is simply nothing to renew.
        let absent = fake_mac(directory.path(), 44, ONE_HOUR_MS);
        let outcome = renew_keychain_login(keychain_source(directory.path(), &absent), &absent)
            .await
            .unwrap();
        assert_eq!(outcome, GrantRefresh::NotRefreshable);
    }
}
