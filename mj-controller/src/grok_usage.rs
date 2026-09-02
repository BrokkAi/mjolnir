//! Grok Build subscription usage querying through its ACP billing extension.
//!
//! Grok Build publishes no HTTP quota endpoint. Its own `/usage` view polls an
//! ACP ext request, `_x.ai/billing`, served by `grok agent stdio`. Keep the
//! JSON-RPC exchange isolated from the UI so the response parser stays
//! testable against captured payloads without spawning `grok`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A cold agent start plus one round trip to the billing backend. Generous
/// because the alternative — reporting a timeout on a slow machine — reads to
/// the user as a broken account.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Ext method Grok Build serves for billing. The `_` prefix is the ACP ext
/// framing, which is how the method appears on the wire.
const BILLING_METHOD: &str = "_x.ai/billing";

#[derive(Debug, Clone, PartialEq)]
pub struct GrokUsageReport {
    /// Window name for the quota row: `Week` or `Month`, matching the period
    /// the account is billed on.
    pub period_label: String,
    /// Included-credit usage as a percentage of the allowance, 0.0 to 100.0.
    pub used_percent: f64,
    /// Epoch seconds when the current period ends.
    pub resets_at: Option<i64>,
}

impl GrokUsageReport {
    /// Share of the allowance still available, rounded for the quota bar.
    pub fn remaining_percent(&self) -> u8 {
        let remaining = (100.0 - self.used_percent).round();
        remaining.clamp(0.0, 100.0) as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrokUsageError {
    NotInstalled,
    Launch(String),
    TimedOut,
    /// The agent exited before answering. Grok Build tears the process down
    /// when its stdin closes, so this also catches an early hangup.
    Closed,
    Protocol(String),
    /// The agent answered the billing request with a JSON-RPC error.
    Rejected(String),
    /// The agent answered, but with no billing configuration in it.
    NoData,
}

impl fmt::Display for GrokUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => write!(f, "Grok Build executable not found"),
            Self::Launch(error) => write!(f, "launch grok agent stdio: {error}"),
            Self::TimedOut => write!(f, "grok usage request timed out"),
            Self::Closed => write!(
                f,
                "grok agent stdio exited before answering the usage request"
            ),
            Self::Protocol(detail) => write!(f, "grok usage response was unreadable: {detail}"),
            Self::Rejected(detail) => write!(f, "grok refused the usage request: {detail}"),
            Self::NoData => write!(f, "grok reported no billing configuration"),
        }
    }
}

/// Ask a profile's Grok Build for its current billing period.
pub async fn query(
    executable: Option<PathBuf>,
    home: PathBuf,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<GrokUsageReport, GrokUsageError> {
    let mut session = GrokUsageSession::spawn(executable.as_deref(), &home, cwd, env)?;
    // The exchange is bounded as a whole: a stalled agent must not hold the
    // quota refresh open, and its child must still be reaped.
    let result = tokio::time::timeout(REQUEST_TIMEOUT, session.exchange()).await;
    session.shutdown().await;
    result.map_err(|_| GrokUsageError::TimedOut)?
}

/// Executable candidates, in the order Hel tries them.
///
/// A configured `executable` names the harness CLI itself for Grok Build (the
/// same override `login_command` honors), so it wins outright. Otherwise the
/// installed CLI on `PATH` comes first, then the copy the official installer
/// leaves inside the profile home.
fn grok_programs(executable: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    if let Some(executable) = executable {
        return vec![executable.to_path_buf()];
    }
    let name = if cfg!(windows) { "grok.exe" } else { "grok" };
    vec![PathBuf::from(name), home.join("bin").join(name)]
}

struct GrokUsageSession {
    child: Child,
    /// Held for the whole exchange on purpose: Grok Build stops when its stdin
    /// closes, so dropping this early kills the agent before it answers.
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl GrokUsageSession {
    fn spawn(
        executable: Option<&Path>,
        home: &Path,
        cwd: PathBuf,
        env: HashMap<String, String>,
    ) -> Result<Self, GrokUsageError> {
        let programs = grok_programs(executable, home);
        let mut child = None;
        for (index, program) in programs.iter().enumerate() {
            let mut command = Command::new(program);
            command
                .args(["agent", "stdio"])
                .current_dir(&cwd)
                .envs(&env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            match command.spawn() {
                Ok(spawned) => {
                    child = Some(spawned);
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && index + 1 < programs.len() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(GrokUsageError::NotInstalled);
                }
                Err(error) => return Err(GrokUsageError::Launch(error.to_string())),
            }
        }
        let mut child = child.ok_or(GrokUsageError::NotInstalled)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| GrokUsageError::Protocol("agent stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GrokUsageError::Protocol("agent stdout unavailable".into()))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn exchange(&mut self) -> Result<GrokUsageReport, GrokUsageError> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}},
        }))
        .await?;
        self.read_result(1).await?;

        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": BILLING_METHOD,
            "params": {},
        }))
        .await?;
        let billing = self.read_result(2).await?;
        parse(&billing)
    }

    async fn write_message(&mut self, message: &Value) -> Result<(), GrokUsageError> {
        let mut encoded = serde_json::to_vec(message)
            .map_err(|error| GrokUsageError::Protocol(error.to_string()))?;
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|_| GrokUsageError::Closed)?;
        self.stdin.flush().await.map_err(|_| GrokUsageError::Closed)
    }

    /// Read frames until the answer to `expected_id` arrives. Everything else
    /// on the stream is the agent's own startup notifications.
    async fn read_result(&mut self, expected_id: u64) -> Result<Value, GrokUsageError> {
        loop {
            let Some(frame) = read_bounded_frame(&mut self.stdout).await? else {
                return Err(GrokUsageError::Closed);
            };
            let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
                // A login-shell banner or other non-JSON noise is not fatal on
                // its own; the answer may still be coming.
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
                    .to_owned();
                return Err(GrokUsageError::Rejected(detail));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| GrokUsageError::Protocol("response carries no result".into()));
        }
    }

    async fn shutdown(mut self) {
        drop(self.stdin);
        if let Err(error) = self.child.start_kill()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "could not stop the Grok usage process");
        }
        if let Err(error) = self.child.wait().await {
            tracing::warn!(%error, "could not reap the Grok usage process");
        }
    }
}

async fn read_bounded_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, GrokUsageError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let (consumed, complete) = {
            let available = reader
                .fill_buf()
                .await
                .map_err(|error| GrokUsageError::Protocol(error.to_string()))?;
            if available.is_empty() {
                return Ok((!frame.is_empty()).then_some(frame));
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(newline) => {
                    frame.extend_from_slice(&available[..newline]);
                    (newline + 1, true)
                }
                None => {
                    frame.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        reader.consume(consumed);
        if frame.len() > MAX_RESPONSE_BYTES {
            return Err(GrokUsageError::Protocol(
                "response frame is too large".into(),
            ));
        }
        if complete {
            return Ok(Some(frame));
        }
    }
}

/// Project Grok Build's billing response into one quota window.
///
/// The top level is snake_case, but `config` is camelCase, matching the two
/// serde shapes in grok-build. proto3 JSON omits zero scalars, so an absent
/// `creditUsagePercent` means zero usage rather than missing data. The
/// deprecated `monthlyLimit`/`used`/`billingPeriod*` fields are read only when
/// their replacements are absent, exactly as grok's own struct documents.
pub fn parse(result: &Value) -> Result<GrokUsageReport, GrokUsageError> {
    let config = result.get("config").ok_or(GrokUsageError::NoData)?;
    if config.is_null() {
        return Err(GrokUsageError::NoData);
    }
    let period_type = config
        .pointer("/currentPeriod/type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let deprecated_only = config.get("currentPeriod").is_none();
    let period_label = if period_type.ends_with("WEEKLY") {
        "Week"
    } else if period_type.ends_with("MONTHLY") || deprecated_only {
        // The legacy shape only ever described a monthly credit budget.
        "Month"
    } else {
        "Period"
    };
    let used_percent = config
        .get("creditUsagePercent")
        .and_then(Value::as_f64)
        .or_else(|| {
            // Deprecated fallback: derive the share from the cent amounts.
            let limit = cents(config.get("monthlyLimit"))?;
            (limit > 0)
                .then(|| cents(config.get("used")).unwrap_or(0) as f64 * 100.0 / limit as f64)
        })
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let resets_at = config
        .pointer("/currentPeriod/end")
        .or_else(|| config.get("billingPeriodEnd"))
        .and_then(Value::as_str)
        .and_then(epoch_seconds);
    Ok(GrokUsageReport {
        period_label: period_label.to_owned(),
        used_percent,
        resets_at,
    })
}

/// A proto3 `Cent` message. `{}` is a valid zero, and so is an absent field.
fn cents(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    Some(value.get("val").and_then(Value::as_i64).unwrap_or(0))
}

fn epoch_seconds(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|moment| moment.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(result: serde_json::Value) -> GrokUsageReport {
        parse(&result).unwrap()
    }

    #[test]
    fn a_weekly_response_reports_its_period_and_reset() {
        let usage = report(json!({
            "config": {
                "creditUsagePercent": 42.5,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-11T05:22:07.661951+00:00",
                    "end": "2026-08-18T05:22:07.661951+00:00",
                },
                "onDemandCap": {"val": 0},
                "onDemandUsed": {"val": 0},
                "prepaidBalance": {"val": 2_500},
                "isUnifiedBillingUser": true,
                "history": [],
            },
            "on_demand_enabled": false,
            "subscription_tier": "X Premium+",
        }));

        assert_eq!(usage.period_label, "Week");
        assert_eq!(usage.used_percent, 42.5);
        assert_eq!(usage.remaining_percent(), 58);
        assert_eq!(usage.resets_at, Some(1_787_030_527));
    }

    #[test]
    fn a_monthly_response_names_its_own_period() {
        let usage = report(json!({
            "config": {
                "creditUsagePercent": 10.0,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "end": "2026-09-01T00:00:00Z",
                },
            },
            "subscription_tier": "SuperGrok Heavy",
        }));

        assert_eq!(usage.period_label, "Month");
        assert_eq!(usage.remaining_percent(), 90);
        assert_eq!(usage.resets_at, Some(1_788_220_800));
    }

    /// proto3 JSON omits zero scalars, which is exactly what an account at
    /// zero usage looks like on the wire. Captured from a real account.
    #[test]
    fn omitted_zero_fields_read_as_zero_rather_than_missing_data() {
        let usage = report(json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-11T05:22:07.661951+00:00",
                    "end": "2026-08-18T05:22:07.661951+00:00",
                },
                "onDemandCap": {},
                "onDemandUsed": {"val": 0},
                "prepaidBalance": {},
                "isUnifiedBillingUser": true,
                "billingPeriodStart": "2026-08-11T05:22:07.661951+00:00",
                "billingPeriodEnd": "2026-08-18T05:22:07.661951+00:00",
            },
            "subscription_tier": "X Premium+",
        }));

        assert_eq!(usage.used_percent, 0.0);
        assert_eq!(usage.remaining_percent(), 100);
        assert_eq!(usage.period_label, "Week");
        assert_eq!(cents(Some(&json!({}))), Some(0));
    }

    #[test]
    fn the_deprecated_shape_derives_its_share_from_the_cent_amounts() {
        let usage = report(json!({
            "config": {
                "monthlyLimit": {"val": 20_000},
                "used": {"val": 5_000},
                "billingPeriodStart": "2026-08-01T00:00:00Z",
                "billingPeriodEnd": "2026-09-01T00:00:00Z",
            },
        }));

        assert_eq!(usage.used_percent, 25.0);
        assert_eq!(usage.remaining_percent(), 75);
        // The legacy shape only ever described a monthly budget.
        assert_eq!(usage.period_label, "Month");
        assert_eq!(usage.resets_at, Some(1_788_220_800));
    }

    #[test]
    fn the_new_percentage_wins_over_the_deprecated_amounts() {
        let usage = report(json!({
            "config": {
                "creditUsagePercent": 12.0,
                "monthlyLimit": {"val": 20_000},
                "used": {"val": 19_000},
                "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY"},
            },
        }));

        assert_eq!(usage.used_percent, 12.0);
        assert_eq!(usage.resets_at, None);
    }

    #[test]
    fn a_response_without_a_configuration_is_not_a_zero_reading() {
        assert_eq!(parse(&json!({})), Err(GrokUsageError::NoData));
        assert_eq!(parse(&json!({"config": null})), Err(GrokUsageError::NoData));
    }

    #[test]
    fn a_zero_allowance_does_not_divide_by_zero() {
        let usage = report(json!({
            "config": {"monthlyLimit": {"val": 0}, "used": {"val": 100}},
        }));

        assert_eq!(usage.used_percent, 0.0);
    }

    #[test]
    fn the_configured_executable_wins_over_every_discovered_one() {
        let override_path = PathBuf::from("/opt/bin/grok");
        assert_eq!(
            grok_programs(Some(&override_path), Path::new("/profiles/grok")),
            [override_path]
        );

        let discovered = grok_programs(None, Path::new("/profiles/grok"));
        assert_eq!(discovered.len(), 2);
        assert!(
            discovered[0]
                .parent()
                .is_none_or(|parent| parent.as_os_str().is_empty())
        );
        assert!(discovered[1].starts_with("/profiles/grok/bin"));
    }

    #[cfg(unix)]
    fn fake_grok(directory: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("grok");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_usage_query_answers_from_a_live_agent_that_holds_its_stdin_open() {
        let directory = tempfile::tempdir().unwrap();
        // Answers both requests, then blocks on stdin. A caller that closes
        // stdin early would end this process before reading the reply, which
        // is exactly how the real agent behaves.
        let executable = fake_grok(
            directory.path(),
            r#"#!/bin/sh
[ "$1" = "agent" ] && [ "$2" = "stdio" ] || exit 64
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n' ;;
    *'_x.ai/billing'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"config":{"creditUsagePercent":30.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-08-18T05:22:07.661951+00:00"}},"subscription_tier":"X Premium+"}}\n' ;;
  esac
done
"#,
        );

        let usage = query(
            Some(executable),
            directory.path().to_path_buf(),
            directory.path().to_path_buf(),
            HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(usage.period_label, "Week");
        assert_eq!(usage.remaining_percent(), 70);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_that_exits_without_answering_is_reported_not_awaited() {
        let directory = tempfile::tempdir().unwrap();
        let executable = fake_grok(directory.path(), "#!/bin/sh\nexit 0\n");

        let error = query(
            Some(executable),
            directory.path().to_path_buf(),
            directory.path().to_path_buf(),
            HashMap::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, GrokUsageError::Closed);
        assert!(
            format!("{error}").contains("grok agent stdio exited"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_missing_executable_is_reported_as_not_installed() {
        let directory = tempfile::tempdir().unwrap();

        let error = query(
            Some(directory.path().join("no-such-grok")),
            directory.path().to_path_buf(),
            directory.path().to_path_buf(),
            HashMap::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, GrokUsageError::NotInstalled);
    }
}
