//! Grok Build subscription usage querying through its ACP billing extension.
//!
//! Grok Build publishes no HTTP quota endpoint. Its own `/usage` view polls an
//! ACP ext request, `_x.ai/billing`, served by `grok agent stdio`. Keep the
//! JSON-RPC exchange isolated from the UI so the response parser stays
//! testable against captured payloads without spawning `grok`.

use std::collections::HashMap;
use std::fmt;
#[cfg(all(test, unix))]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};

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
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<GrokUsageReport, GrokUsageError> {
    let mut session = GrokUsageSession::spawn(cwd, env).await?;
    // The exchange is bounded as a whole: a stalled agent must not hold the
    // quota refresh open, and its child must still be reaped.
    let result = tokio::time::timeout(REQUEST_TIMEOUT, session.exchange()).await;
    session.shutdown().await;
    result.map_err(|_| GrokUsageError::TimedOut)?
}

struct GrokUsageSession {
    child: crate::streamed_child::ProxyChild,
    _program: crate::quota_harness::QuotaProgram,
    /// Held for the whole exchange on purpose: Grok Build stops when its stdin
    /// closes, so dropping this early kills the agent before it answers.
    stdin: crate::streamed_child::ProxyWriter,
    stdout: BufReader<crate::streamed_child::ProxyReader>,
    /// Drains stderr so a chatty agent cannot block on a full pipe.
    stderr: tokio::task::JoinHandle<()>,
}

impl GrokUsageSession {
    async fn spawn(cwd: PathBuf, env: HashMap<String, String>) -> Result<Self, GrokUsageError> {
        let program = crate::quota_harness::locate(mj_core::config::HarnessKind::Grok, &env)
            .await
            .map_err(|error| GrokUsageError::Launch(format!("{error:#}")))?;
        let mut spec =
            mj_core::targets::CommandSpec::new(program.command.clone(), ["agent", "stdio"]);
        spec.cwd = Some(cwd);
        spec.env = env.into_iter().chain(program.environment.clone()).collect();
        let stdio = crate::streamed_child::spawn_proxy(&spec)
            .await
            .map_err(|error| GrokUsageError::Launch(format!("{error:#}")))?;
        let crate::streamed_child::ProxyStdio {
            child,
            stdin,
            stdout,
            stderr,
        } = stdio;
        let stderr = tokio::spawn(async move {
            let _ = crate::streamed_child::collect_bounded(stderr, 64 * 1024).await;
        });
        Ok(Self {
            child,
            _program: program,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
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
        self.stderr.abort();
    }
}

async fn read_bounded_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, GrokUsageError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use mj_core::bounded_frame::{BoundedFrame, BoundedFrameError};
    match mj_core::bounded_frame::read_bounded_frame(reader, MAX_RESPONSE_BYTES).await {
        Ok(BoundedFrame::Line(frame) | BoundedFrame::Truncated(frame)) => Ok(Some(frame)),
        Ok(BoundedFrame::End) => Ok(None),
        Err(BoundedFrameError::TooLarge) => Err(GrokUsageError::Protocol(
            "response frame is too large".into(),
        )),
        Err(BoundedFrameError::Io(error)) => Err(GrokUsageError::Protocol(error.to_string())),
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_grok(directory: &Path, script: &str) -> PathBuf {
        mj_core::test_hooks::install_fake_command(directory, "grok", script);
        directory.join("grok")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_that_exits_without_answering_is_reported_not_awaited() {
        let directory = tempfile::tempdir().unwrap();
        let _executable = fake_grok(directory.path(), "#!/bin/sh\nexit 0\n");

        let error = query(
            directory.path().to_path_buf(),
            HashMap::from([(
                "PATH".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            )]),
        )
        .await
        .unwrap_err();

        assert_eq!(error, GrokUsageError::Closed);
        assert!(
            format!("{error}").contains("grok agent stdio exited"),
            "{error}"
        );
    }

    // The harness installer and quota process must agree on the install receipt
    // and retain its lease, so cache repair cannot replace a running CLI.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_managed_quota_harness_stays_leased_until_its_child_stops() {
        use mj_core::config::HarnessKind;
        use mj_core::harness_runtime::{self, ManagedHarnessManifest};
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let root =
            harness_runtime::managed_harness_cache_root(Some(cache.as_os_str()), None).unwrap();
        let install = harness_runtime::managed_harness_install_dir(&root, HarnessKind::Grok);
        let bin = install.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_grok(&bin, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n");
        std::fs::write(install.join(harness_runtime::LEASE_FILE), []).unwrap();
        std::fs::write(
            install.join(harness_runtime::MANIFEST_FILE),
            serde_json::to_vec(&ManagedHarnessManifest::for_harness(HarnessKind::Grok)).unwrap(),
        )
        .unwrap();
        let mut session = GrokUsageSession::spawn(
            directory.path().to_path_buf(),
            HashMap::from([
                (
                    "PATH".into(),
                    directory.path().to_string_lossy().into_owned(),
                ),
                (
                    "GROK_HOME".into(),
                    directory.path().to_string_lossy().into_owned(),
                ),
                (
                    "XDG_CACHE_HOME".into(),
                    cache.to_string_lossy().into_owned(),
                ),
            ]),
        )
        .await
        .unwrap();
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(install.join(harness_runtime::LEASE_FILE))
            .unwrap();
        assert!(matches!(
            lease.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        // Drive the real stdio child; shutdown must retain the lease until it
        // closes stdin and reaps the process, then release it.
        session
            .write_message(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .await
            .unwrap();
        session.shutdown().await;
        lease
            .try_lock()
            .expect("lease released after quota child stops");
    }

    // Hard-won: 4de4844fb: a second PATH search changed the CLI under a different child cwd, and inherited GROK_HOME was ignored.
    #[tokio::test]
    async fn quota_cli_selection_survives_child_cwd_and_inherited_home() {
        const CHILD_ROOT: &str = "MJ_GROK_LOOKUP_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            // This is an isolated child: no other test shares its cwd.
            std::env::set_current_dir(root.join("daemon")).unwrap();
            let marker = root.join("selected");
            let mut env = HashMap::from([
                (
                    "QUOTA_PROGRAM_MARKER".into(),
                    marker.to_string_lossy().into_owned(),
                ),
                ("XDG_CACHE_HOME".into(), "relative-cache-is-unusable".into()),
                (
                    "PATH".into(),
                    std::env::join_paths([PathBuf::from("relative"), root.join("selected-bin")])
                        .unwrap()
                        .into_string()
                        .unwrap(),
                ),
            ]);
            assert_eq!(
                query(root.join("worker"), env.clone()).await.unwrap_err(),
                GrokUsageError::Closed
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "selected");
            env.remove("PATH");
            assert_eq!(
                query(root.join("worker"), env).await.unwrap_err(),
                GrokUsageError::Closed
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "inherited");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for relative in [
            "daemon",
            "worker/relative",
            "selected-bin",
            "home/bin",
            "empty",
        ] {
            std::fs::create_dir_all(root.join(relative)).unwrap();
        }
        for (bin, selected) in [
            ("selected-bin", "selected"),
            ("worker/relative", "wrong"),
            ("home/bin", "inherited"),
        ] {
            fake_grok(
                &root.join(bin),
                &format!("#!/bin/sh\nprintf '{selected}' > \"$QUOTA_PROGRAM_MARKER\"\nexit 0\n"),
            );
        }
        let child = crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
            module_path!(),
            "quota_cli_selection_survives_child_cwd_and_inherited_home",
        ))
        .env(CHILD_ROOT, root)
        .env("GROK_HOME", root.join("home"))
        .env("PATH", root.join("empty"))
        .isolated_store(&root.join("child-instance"));
        tokio::task::spawn_blocking(move || child.run())
            .await
            .unwrap();
    }
}
