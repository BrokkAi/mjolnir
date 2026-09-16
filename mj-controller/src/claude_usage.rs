//! Claude Code `/usage` polling and parsing.
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
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Quota-row copy when the stored Claude OAuth access token is past `expiresAt`.
pub(crate) const LOGIN_EXPIRED: &str = "login expired";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageReport {
    pub five_hour: Option<ClaudeUsageWindow>,
    pub week: Option<ClaudeUsageWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageWindow {
    pub remaining_percent: u8,
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
        }
    }
}

/// Query the same OAuth usage endpoint as Claude Code's interactive `/usage`.
/// The CLI is invoked only to refresh rejected credentials; its print-mode
/// usage output is approximate, so the API response remains authoritative.
pub async fn query(
    home: PathBuf,
    environment: HashMap<String, String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    query_with(
        home,
        environment,
        USAGE_URL,
        Arc::new(CancellableProcessExecutor::with_timeout(REFRESH_TIMEOUT)),
    )
    .await
}

async fn query_with(
    home: PathBuf,
    environment: HashMap<String, String>,
    usage_url: &str,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let client = reqwest::Client::builder()
        .timeout(USAGE_TIMEOUT)
        .build()
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))?;
    let credentials = read_credentials(&home).await?;
    match oauth_access_token(&credentials, mj_core::clock::epoch_millis()) {
        Ok(token) => match query_api(&client, usage_url, token).await {
            Err(ClaudeUsageError::LoginExpired) if oauth_has_refresh_token(&credentials) => {
                refresh_and_retry(
                    &client,
                    usage_url,
                    home,
                    environment,
                    executor,
                    Some(token.to_owned()),
                )
                .await
            }
            result => result,
        },
        Err(ClaudeUsageError::LoginExpired) if oauth_has_refresh_token(&credentials) => {
            refresh_and_retry(&client, usage_url, home, environment, executor, None).await
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
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let response = client
        .get(usage_url)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                ClaudeUsageError::TimedOut
            } else {
                ClaudeUsageError::Query(error.to_string())
            }
        })?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(ClaudeUsageError::LoginExpired);
    }
    if !response.status().is_success() {
        return Err(ClaudeUsageError::Query(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))?;
    parse_api_usage(&payload).ok_or(ClaudeUsageError::Parse)
}

async fn refresh_and_retry(
    client: &reqwest::Client,
    usage_url: &str,
    home: PathBuf,
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    rejected_token: Option<String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let refresh_error = run_claude_refresh(environment, executor).await.err();
    let credentials = match read_credentials(&home).await {
        Ok(credentials) => credentials,
        Err(error) => return Err(refresh_error.unwrap_or(error)),
    };
    let token = match oauth_access_token(&credentials, mj_core::clock::epoch_millis()) {
        Ok(token) => token,
        Err(error) => return Err(refresh_error.unwrap_or(error)),
    };
    if rejected_token.as_deref() == Some(token)
        && let Some(error) = refresh_error
    {
        return Err(error);
    }
    query_api(client, usage_url, token).await
}

async fn run_claude_refresh(
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<(), ClaudeUsageError> {
    let mut command = CommandSpec::new(
        if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        },
        ["-p", "/usage", "--no-session-persistence"],
    )
    .purpose("refresh Claude login");
    command.env.extend(environment);
    let output = tokio::task::spawn_blocking(move || executor.execute(&command))
        .await
        .map_err(|error| ClaudeUsageError::Refresh(format!("worker failed: {error}")))?
        .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))?;
    successful_refresh_output(output)
}

fn successful_refresh_output(output: CommandOutput) -> Result<(), ClaudeUsageError> {
    if output.status == 0 {
        Ok(())
    } else {
        Err(ClaudeUsageError::Refresh(format!(
            "Claude /usage exited with status {}",
            output.status
        )))
    }
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

fn parse_api_usage(payload: &Value) -> Option<ClaudeUsageReport> {
    let mut five_hour = None;
    let mut weekly = Vec::new();

    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(kind) = limit.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let Some(window) = api_window(limit, "percent") else {
                continue;
            };
            match kind {
                "session" => five_hour = Some(window),
                "weekly_all" => weekly.push(window),
                "weekly_scoped" if api_scope_name(limit).is_some_and(|name| name == "fable") => {
                    weekly.push(window);
                }
                _ => {}
            }
        }
    } else {
        five_hour = payload
            .get("five_hour")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"));
        for key in ["seven_day", "seven_day_fable"] {
            if let Some(window) = payload
                .get(key)
                .filter(|value| !value.is_null())
                .and_then(|value| api_window(value, "utilization"))
            {
                weekly.push(window);
            }
        }
    }

    let week = weekly
        .into_iter()
        .min_by_key(|window| window.remaining_percent);
    (five_hour.is_some() || week.is_some()).then_some(ClaudeUsageReport { five_hour, week })
}

fn api_window(value: &Value, percent_key: &str) -> Option<ClaudeUsageWindow> {
    let used = value.get(percent_key)?.as_f64()?;
    let used = used.round().clamp(0.0, 100.0) as u8;
    Some(ClaudeUsageWindow {
        remaining_percent: 100 - used,
        reset_context: value
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn api_scope_name(value: &Value) -> Option<String> {
    value
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

/// Scrape Claude Code `/usage` output for the two quota windows we display.
///
/// The command output has changed shape across Claude Code releases (plain
/// lines, markdown-ish tables, and the ACP metadata wording all show up in the
/// wild), so the parser intentionally keys off semantic labels plus nearby
/// percentage words rather than a single exact template.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct RefreshExecutor {
        home: PathBuf,
        replacement: Option<Value>,
        status: i32,
        commands: Arc<Mutex<Vec<CommandSpec>>>,
    }

    impl CommandExecutor for RefreshExecutor {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
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
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
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
                "seven_day": {"utilization": 40.0, "resets_at": "2026-08-29T00:00:00Z"}
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
        });
        if let Some(refresh) = refresh {
            oauth["refreshToken"] = Value::String(refresh.to_owned());
        }
        serde_json::json!({"claudeAiOauth": oauth})
    }

    #[tokio::test]
    async fn expired_login_asks_claude_to_refresh_without_persisting_a_session() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".credentials.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let fresh = credentials(
            "fresh",
            Some("rotated"),
            mj_core::clock::epoch_millis() + 60_000,
        );
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(fresh),
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        let environment = HashMap::from([(
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

        assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].args,
            ["-p", "/usage", "--no-session-persistence"]
        );
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

        assert_eq!(commands.lock().unwrap().len(), 1);
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer old", "Bearer fresh"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn valid_credentials_query_authoritative_usage_without_launching_claude() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "current",
                Some("refresh"),
                mj_core::clock::epoch_millis() + 60_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(false).await;

        query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer current"]
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
    fn api_usage_uses_exhausted_fable_limit_over_overall_limit() {
        let report = parse_api_usage(&serde_json::json!({
            "limits": [
                {
                    "kind": "session",
                    "percent": 13.0,
                    "resets_at": "2026-08-18T23:30:00Z"
                },
                {
                    "kind": "weekly_all",
                    "percent": 96.0,
                    "resets_at": "2026-08-19T22:59:00Z"
                },
                {
                    "kind": "weekly_scoped",
                    "percent": 100.0,
                    "resets_at": "2026-08-19T22:59:00Z",
                    "scope": { "model": { "display_name": "Fable" } }
                }
            ]
        }))
        .expect("report");

        assert_eq!(report.five_hour.unwrap().remaining_percent, 87);
        let week = report.week.unwrap();
        assert_eq!(week.remaining_percent, 0);
        assert_eq!(week.reset_context.as_deref(), Some("2026-08-19T22:59:00Z"));
    }

    #[test]
    fn api_usage_ignores_other_model_scoped_weekly_limits() {
        let report = parse_api_usage(&serde_json::json!({
            "limits": [
                { "kind": "weekly_all", "percent": 40.0 },
                {
                    "kind": "weekly_scoped",
                    "percent": 90.0,
                    "scope": { "model": { "display_name": "Opus" } }
                },
                {
                    "kind": "weekly_scoped",
                    "percent": 50.0,
                    "scope": { "model": { "display_name": "Fable" } }
                }
            ]
        }))
        .expect("report");

        assert_eq!(report.week.unwrap().remaining_percent, 50);
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

    #[test]
    fn oauth_access_token_without_expiry_is_usable() {
        let credentials = serde_json::json!({
            "claudeAiOauth": { "accessToken": "sk-ant-oat01-test" }
        });
        assert_eq!(
            oauth_access_token(&credentials, 9_000).expect("token"),
            "sk-ant-oat01-test"
        );
    }

    #[test]
    fn missing_oauth_access_token_is_not_signed_in() {
        assert_eq!(
            oauth_access_token(&serde_json::json!({}), 1),
            Err(ClaudeUsageError::NotSignedIn)
        );
        assert_eq!(
            oauth_access_token(
                &serde_json::json!({ "claudeAiOauth": { "accessToken": "" } }),
                1
            ),
            Err(ClaudeUsageError::NotSignedIn)
        );
    }
}
