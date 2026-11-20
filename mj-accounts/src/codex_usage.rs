//! Codex subscription quota querying through `codex app-server`.
//!
//! Codex exposes ChatGPT subscription rate limits through its local app-server
//! protocol rather than a one-shot CLI command. Keep the JSONL client isolated
//! from the UI so protocol parsing and unavailable states remain testable.

use std::collections::{HashMap, VecDeque};
#[cfg(all(any(test, feature = "test-support"), unix))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mj_core::config::HarnessKind;
#[cfg(all(any(test, feature = "test-support"), unix))]
use mj_core::harness_runtime::{CODEX_CLI_ENTRYPOINT, LEASE_FILE, managed_harness_install_dir};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// A cold Codex app-server start can be slow on busy machines. The client is
// reused after initialization, so this primarily bounds the initial probe.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
// Enough of app-server's stderr to explain a failure, such as a rejected
// token refresh, without holding its whole log.
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_BYTES: usize = 2 * 1024;
// How long a stopped app-server's stderr gets to reach end of stream.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Available(CodexUsageReport),
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageReport {
    pub monthly_price_usd: Option<u32>,
    pub paid_credits: Option<mj_client::quota::PaidCredits>,
    pub banked_resets: Option<u64>,
    pub primary: Option<CodexUsageWindow>,
    pub secondary: Option<CodexUsageWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageWindow {
    pub label: String,
    pub remaining_percent: u8,
    pub duration_seconds: Option<i64>,
    pub resets_at: Option<i64>,
}

/// Where a Codex quota check starts its app-server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLauncher {
    /// This host's installed CLI, otherwise the pinned managed installation.
    Host,
    #[cfg(any(test, feature = "test-support"))]
    Program(PathBuf),
}

impl CodexLauncher {
    pub(crate) async fn prepare(
        &self,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Arc<crate::quota_harness::QuotaProgram>> {
        match self {
            Self::Host => crate::quota_harness::locate(HarnessKind::Codex, env)
                .await
                .map(Arc::new),
            #[cfg(any(test, feature = "test-support"))]
            Self::Program(command) => Ok(Arc::new(crate::quota_harness::QuotaProgram {
                command: crate::quota_harness::command_text(command)?,
                environment: HashMap::new(),
                _installation: None,
            })),
        }
    }
}

pub struct CodexUsageClient {
    child: crate::streamed_child::ProxyChild,
    /// Which launcher started this client; a poll that asks for another one
    /// replaces it.
    launcher: CodexLauncher,
    program: Arc<crate::quota_harness::QuotaProgram>,
    stdin: crate::streamed_child::ProxyWriter,
    stdout: BufReader<crate::streamed_child::ProxyReader>,
    stderr: StderrTail,
    stderr_reader: tokio::task::JoinHandle<()>,
    next_id: u64,
    initialized: bool,
}

impl CodexUsageClient {
    fn matches_launcher(&self, launcher: &CodexLauncher) -> bool {
        &self.launcher == launcher
    }

    /// Reuse the selected program and leases while its app-server is alive;
    /// a changed or damaged installation cannot invalidate a running client.
    pub(crate) fn prepared_program(
        &self,
        launcher: &CodexLauncher,
    ) -> Option<Arc<crate::quota_harness::QuotaProgram>> {
        self.matches_launcher(launcher)
            .then(|| Arc::clone(&self.program))
    }

    async fn spawn(
        cwd: PathBuf,
        env: HashMap<String, String>,
        launcher: &CodexLauncher,
        program: Arc<crate::quota_harness::QuotaProgram>,
    ) -> Result<Self, QueryError> {
        let mut env = env;
        env.extend(program.environment.clone());
        let stdio = spawn_app_server(&program.command, cwd, env).await?;
        let crate::streamed_child::ProxyStdio {
            child,
            stdin,
            stdout,
            stderr,
        } = stdio;
        let (stderr, stderr_reader) = StderrTail::capture(stderr);
        Ok(Self {
            child,
            launcher: launcher.clone(),
            program,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
            stderr_reader,
            next_id: 1,
            initialized: false,
        })
    }

    async fn initialize(&mut self) -> Result<(), QueryError> {
        if self.initialized {
            return Ok(());
        }
        let id = self
            .send_request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "mj",
                        "title": "Mjolnir",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        self.read_result(id).await?;
        self.write_message(&json!({ "method": "initialized" }))
            .await?;
        self.initialized = true;
        Ok(())
    }

    /// Ask app-server to rotate the stored login now.
    ///
    /// `account/read` with `refreshToken: true` "requests a proactive token
    /// refresh before returning; in managed auth mode this triggers the normal
    /// refresh-token flow; in external auth mode this flag is ignored". An
    /// app-server too old to know the flag answers `-32601`, which means there
    /// is nothing to do here rather than a failed poll.
    ///
    /// The request succeeds even when the refresh does not: app-server records
    /// the failure and answers with no account. Only a ChatGPT account in the
    /// answer means the login is still usable.
    async fn refresh_token(&mut self) -> Result<(), QueryError> {
        let id = self
            .send_request("account/read", json!({ "refreshToken": true }))
            .await?;
        match self.read_result(id).await {
            Ok(account) => classify_account(&account),
            Err(QueryError::Unsupported) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn query(&mut self) -> Result<CodexUsageReport, QueryError> {
        let account_id = self
            .send_request("account/read", json!({ "refreshToken": false }))
            .await?;
        let account = self.read_result(account_id).await?;
        classify_account(&account)?;

        let limits_id = self
            .send_request("account/rateLimits/read", Value::Null)
            .await?;
        let limits = self.read_result(limits_id).await?;
        let account_plan = account.pointer("/account/planType").and_then(Value::as_str);
        parse_report(&limits, account_plan)
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<u64, QueryError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_message(&json!({ "method": method, "id": id, "params": params }))
            .await?;
        Ok(id)
    }

    async fn write_message(&mut self, message: &Value) -> Result<(), QueryError> {
        let mut encoded = serde_json::to_vec(message)
            .map_err(|_| QueryError::Protocol(ProtocolError::InvalidResponse))?;
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|_| QueryError::Protocol(ProtocolError::Io))?;
        self.stdin
            .flush()
            .await
            .map_err(|_| QueryError::Protocol(ProtocolError::Io))
    }

    async fn read_result(&mut self, expected_id: u64) -> Result<Value, QueryError> {
        loop {
            let Some(line) = read_bounded_frame(&mut self.stdout).await? else {
                return Err(QueryError::Protocol(ProtocolError::Closed));
            };
            let message: Value = serde_json::from_slice(&line)
                .map_err(|_| QueryError::Protocol(ProtocolError::InvalidResponse))?;
            match parse_response(&message, expected_id)? {
                Some(result) => return Ok(result),
                None => continue,
            }
        }
    }

    /// Stop app-server and return the end of its stderr. Waiting for the
    /// reader to reach end of stream keeps lines that were still in the pipe
    /// when the failed answer arrived.
    async fn shutdown_with_diagnostics(self) -> Vec<String> {
        let tail = self.stderr.clone();
        let reader = self.stop().await;
        // A process app-server started could still hold the pipe open.
        let _ = tokio::time::timeout(STDERR_DRAIN_TIMEOUT, reader).await;
        tail.lines()
    }

    pub async fn shutdown(self) {
        self.stop().await;
    }

    /// Stop the process and hand back its stderr reader, which ends on its
    /// own once the pipe closes.
    async fn stop(mut self) -> tokio::task::JoinHandle<()> {
        drop(self.stdin);
        // Closing stdin asks app-server to stop. The quota process is always
        // launched directly (never through npx), so killing the recorded child
        // is sufficient if it does not notice EOF promptly.
        if let Err(error) = self.child.start_kill()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "could not stop the Codex quota process");
        }
        if let Err(error) = self.child.wait().await {
            tracing::warn!(%error, "could not reap the Codex quota process");
        }

        self.stderr_reader
    }
}

/// The last lines app-server wrote to stderr, cleaned of terminal controls and
/// anything that looks like a token.
#[derive(Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    /// Keep reading `stderr` until app-server closes it. Reading also keeps a
    /// chatty process from blocking on a full pipe.
    fn capture(stderr: crate::streamed_child::ProxyReader) -> (Self, tokio::task::JoinHandle<()>) {
        let tail = Self::default();
        let writer = tail.clone();
        let reader = mj_core::runtime::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                let available = match reader.fill_buf().await {
                    Ok(available) if !available.is_empty() => available,
                    _ => break,
                };
                let newline = available.iter().position(|byte| *byte == b'\n');
                let content = newline.unwrap_or(available.len());
                let room = STDERR_LINE_BYTES.saturating_sub(line.len());
                line.extend_from_slice(&available[..content.min(room)]);
                reader.consume(content + usize::from(newline.is_some()));
                if newline.is_some() {
                    writer.push(&line);
                    line.clear();
                }
            }
            writer.push(&line);
        });
        (tail, reader)
    }

    fn push(&self, line: &[u8]) {
        let line = mj_core::redact::redact_secrets(&mj_core::transcript::sanitize_terminal_text(
            &String::from_utf8_lossy(line),
        ));
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut lines = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line.to_owned());
    }

    fn lines(&self) -> Vec<String> {
        let lines = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lines.iter().cloned().collect()
    }
}

async fn read_bounded_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, QueryError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use mj_core::bounded_frame::{BoundedFrame, BoundedFrameError};
    match mj_core::bounded_frame::read_bounded_frame(reader, MAX_RESPONSE_BYTES).await {
        Ok(BoundedFrame::Line(frame)) => Ok(Some(frame)),
        Ok(BoundedFrame::End) => Ok(None),
        Ok(BoundedFrame::Truncated(_)) => Err(QueryError::Protocol(ProtocolError::Closed)),
        Err(BoundedFrameError::TooLarge) => Err(QueryError::Protocol(ProtocolError::TooLarge)),
        Err(BoundedFrameError::Io(_)) => Err(QueryError::Protocol(ProtocolError::Io)),
    }
}

fn parse_response(message: &Value, expected_id: u64) -> Result<Option<Value>, QueryError> {
    if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Ok(None);
    }
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        if code == Some(-32601) {
            return Err(QueryError::Unsupported);
        }
        return Err(QueryError::Protocol(ProtocolError::RemoteError));
    }
    message
        .get("result")
        .cloned()
        .map(Some)
        .ok_or(QueryError::Protocol(ProtocolError::InvalidResponse))
}

/// Installation is preparation, outside the app-server request deadline.
async fn start(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
    launcher: &CodexLauncher,
    prepared: &mut Option<Arc<crate::quota_harness::QuotaProgram>>,
) -> Result<(), QueryError> {
    if let Some(stale) = client.take_if(|ready| !ready.matches_launcher(launcher)) {
        stale.shutdown().await;
    }
    if client.is_none() {
        let program = match prepared.take() {
            Some(program) => program,
            None => launcher
                .prepare(&env)
                .await
                .map_err(|error| QueryError::Launch(format!("{error:#}")))?,
        };
        *client = Some(CodexUsageClient::spawn(cwd, env, launcher, program).await?);
    }
    Ok(())
}

async fn prepare(
    client: &mut Option<CodexUsageClient>,
) -> Result<&mut CodexUsageClient, QueryError> {
    let ready = client
        .as_mut()
        .expect("client started before the request deadline");
    ready.initialize().await?;
    Ok(ready)
}

/// Drop a client the failure says is no longer usable, logging what its
/// app-server wrote to stderr so the cause is not lost with the process.
async fn discard_failed_client(client: &mut Option<CodexUsageClient>, replaceable: bool) {
    if replaceable && let Some(stale_client) = client.take() {
        let diagnostics = stale_client.shutdown_with_diagnostics().await;
        if !diagnostics.is_empty() {
            tracing::warn!(
                stderr = %diagnostics.join("\n"),
                "Codex quota process diagnostics before restart"
            );
        }
    }
}

/// Refresh a persistent app-server client, recreating it after transport or
/// protocol failures. Calls are awaited serially by the session worker.
pub(crate) async fn refresh_with_cache_root(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
    launcher: &CodexLauncher,
    prepared: &mut Option<Arc<crate::quota_harness::QuotaProgram>>,
) -> CodexUsageStatus {
    let result = match start(client, cwd, env, launcher, prepared).await {
        Ok(()) => {
            tokio::time::timeout(REQUEST_TIMEOUT, async {
                prepare(client).await?.query().await
            })
            .await
        }
        Err(error) => Ok(Err(error)),
    };

    match result {
        Ok(Ok(report)) => CodexUsageStatus::Available(report),
        Ok(Err(error)) => {
            discard_failed_client(client, error.needs_fresh_client()).await;
            tracing::warn!("codex quota query failed: {error}");
            CodexUsageStatus::Unavailable(error.unavailable_reason())
        }
        Err(_) => {
            discard_failed_client(client, true).await;
            tracing::warn!("codex quota query timed out");
            CodexUsageStatus::Unavailable("request timed out".to_string())
        }
    }
}

/// Why [`refresh_login_with_cache_root`] could not rotate the login.
#[derive(Debug)]
pub struct LoginRefreshFailure {
    /// The failure in full, for the log.
    pub detail: String,
    /// The failure as [`refresh_with_cache_root`] would report it to the user.
    pub reason: String,
}

/// Rotate the profile's Codex login ahead of its expiry, reusing the cached
/// client the way [`refresh_with_cache_root`] does.
///
/// Codex refresh tokens are single use. A host and a container that reach
/// expiry at the same instant both try to spend the same token, one wins, and
/// the loser's turn dies. Rotating early on the host, so the sync can push the
/// new file, keeps container copies away from that instant.
pub(crate) async fn refresh_login_with_cache_root(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
    launcher: &CodexLauncher,
    prepared: &mut Option<Arc<crate::quota_harness::QuotaProgram>>,
) -> Result<(), LoginRefreshFailure> {
    let result = match start(client, cwd, env, launcher, prepared).await {
        Ok(()) => {
            tokio::time::timeout(REQUEST_TIMEOUT, async {
                prepare(client).await?.refresh_token().await
            })
            .await
        }
        Err(error) => Ok(Err(error)),
    };

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            discard_failed_client(client, error.needs_fresh_client()).await;
            Err(LoginRefreshFailure {
                detail: error.to_string(),
                reason: error.unavailable_reason(),
            })
        }
        Err(_) => {
            discard_failed_client(client, true).await;
            Err(LoginRefreshFailure {
                detail: "request timed out".to_string(),
                reason: "request timed out".to_owned(),
            })
        }
    }
}

#[cfg(all(any(test, feature = "test-support"), unix))]
pub fn install_fake_managed_codex(cache_root: &Path, script: &str) -> PathBuf {
    let install = managed_harness_install_dir(cache_root, HarnessKind::Codex);
    let launcher = install.join(CODEX_CLI_ENTRYPOINT);
    let launcher_directory = launcher.parent().expect("launcher parent");
    std::fs::create_dir_all(launcher_directory).expect("create managed install");
    mj_core::test_hooks::install_fake_command(launcher_directory, "codex", script);
    std::fs::write(install.join(LEASE_FILE), []).expect("create managed install lease");
    std::fs::write(
        install.join(mj_core::harness_runtime::MANIFEST_FILE),
        serde_json::to_vec(
            &mj_core::harness_runtime::ManagedHarnessManifest::for_harness(HarnessKind::Codex),
        )
        .expect("serialize managed install manifest"),
    )
    .expect("write managed install manifest");
    install
}

/// Start `codex app-server` from `launcher`, with the environment a quota
/// check gives it.
async fn spawn_app_server(
    launcher: &str,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<crate::streamed_child::ProxyStdio, QueryError> {
    let mut spec = mj_core::targets::CommandSpec::new(launcher, ["app-server", "--stdio"]);
    spec.cwd = Some(cwd);
    spec.env = env.into_iter().collect();
    // This probe reads a ChatGPT login's rate limits and refreshes that
    // login. An API key from the profile's environment or the daemon's own
    // would let Codex use the key instead (#1160).
    spec.env_remove = mj_core::config::CODEX_CREDENTIAL_ENVIRONMENT
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    // Its stderr is kept for diagnostics. At error level app-server reports
    // a rejected token refresh; a broader level inherited from the daemon
    // would add HTTP request bodies that carry the refresh token.
    spec.env.insert("RUST_LOG".to_owned(), "error".to_owned());
    match crate::streamed_child::spawn_proxy(&spec).await {
        Ok(stdio) => Ok(stdio),
        Err(error) if crate::streamed_child::is_missing_program(&error) => {
            Err(QueryError::NotInstalled {
                expected: None,
                detail: Some(format!("launcher {launcher} could not be found")),
            })
        }
        Err(error) => Err(QueryError::Launch(format!(
            "could not start Codex launcher {launcher}: {error:#}"
        ))),
    }
}

#[derive(Debug)]
enum QueryError {
    NotInstalled {
        expected: Option<PathBuf>,
        detail: Option<String>,
    },
    Launch(String),
    NotSignedIn,
    UnsupportedAccount,
    Unsupported,
    NoData,
    Protocol(ProtocolError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolError {
    Io,
    Closed,
    InvalidResponse,
    TooLarge,
    RemoteError,
}

impl QueryError {
    /// Whether the cached app-server must be replaced before the next call.
    ///
    /// A transport or protocol break leaves the stream out of step. An
    /// authentication failure can be state inside app-server rather than in
    /// the credential file: once a token refresh fails, app-server reports no
    /// account until it restarts, even after the file is fixed (#1224). A new
    /// process reads the file again. While the login stays invalid each poll
    /// starts one process, so the poll interval bounds the cost.
    fn needs_fresh_client(&self) -> bool {
        matches!(
            self,
            Self::Protocol(_) | Self::Unsupported | Self::NotSignedIn | Self::UnsupportedAccount
        )
    }

    fn unavailable_reason(&self) -> String {
        match self {
            Self::Launch(_) | Self::NotInstalled { .. } => self.to_string(),
            _ => self.user_reason().to_owned(),
        }
    }

    fn user_reason(&self) -> &'static str {
        match self {
            Self::NotInstalled { .. } => "Codex CLI is unavailable",
            Self::Launch(_) => "could not start Codex CLI",
            Self::NotSignedIn => "not signed in with ChatGPT",
            Self::UnsupportedAccount => {
                "ChatGPT subscription quota is not available for this account"
            }
            Self::Unsupported => "installed Codex does not support quota queries",
            Self::NoData => "no rate-limit data returned",
            Self::Protocol(_) => "Codex quota request failed",
        }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch(detail) => write!(f, "could not start Codex CLI: {detail}"),
            Self::Protocol(kind) => write!(f, "Codex app-server protocol error ({kind:?})"),
            Self::NotInstalled { expected, detail } => {
                f.write_str(self.user_reason())?;
                if let Some(expected) = expected {
                    write!(f, "; expected installation at {}", expected.display())?;
                }
                if let Some(detail) = detail {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
            _ => f.write_str(self.user_reason()),
        }
    }
}

fn classify_account(result: &Value) -> Result<(), QueryError> {
    let Some(account) = result.get("account") else {
        return Err(QueryError::NotSignedIn);
    };
    if account.is_null() {
        return Err(QueryError::NotSignedIn);
    }
    match account.get("type").and_then(Value::as_str) {
        Some("chatgpt") => Ok(()),
        _ => Err(QueryError::UnsupportedAccount),
    }
}

fn parse_report(
    result: &Value,
    account_plan: Option<&str>,
) -> Result<CodexUsageReport, QueryError> {
    let codex_snapshot = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .and_then(|buckets| buckets.get("codex"));

    let mut report = codex_snapshot
        .and_then(|snapshot| parse_snapshot(snapshot, account_plan))
        .or_else(|| {
            result
                .get("rateLimits")
                .and_then(|snapshot| parse_snapshot(snapshot, account_plan))
        })
        .ok_or(QueryError::NoData)?;
    report.banked_resets = result
        .get("rateLimitResetCredits")
        .filter(|value| !value.is_null())
        .and_then(|credits| {
            let count = credits.get("availableCount").and_then(Value::as_u64);
            if count.is_none() {
                tracing::warn!("Codex usage returned malformed reset-credit metadata");
            }
            count
        });
    Ok(report)
}

fn parse_snapshot(snapshot: &Value, account_plan: Option<&str>) -> Option<CodexUsageReport> {
    let plan = snapshot
        .get("planType")
        .and_then(Value::as_str)
        .or(account_plan);
    let report = CodexUsageReport {
        monthly_price_usd: plan.and_then(plan_monthly_price),
        paid_credits: snapshot.get("credits").and_then(parse_paid_credits),
        banked_resets: None,
        primary: snapshot.get("primary").and_then(parse_window),
        secondary: snapshot.get("secondary").and_then(parse_window),
    };
    if report.primary.is_none() && report.secondary.is_none() && report.paid_credits.is_none() {
        None
    } else {
        Some(report)
    }
}

// Codex 0.170.0's status tests identify the personal plan labels:
// https://github.com/openai/codex/blob/rust-v0.170.0/codex-rs/tui/src/status/helpers.rs#L187-L215
fn plan_monthly_price(plan: &str) -> Option<u32> {
    match plan {
        // ChatGPT Go includes a Codex allowance at a US list price of $8:
        // https://openai.com/index/introducing-chatgpt-go/
        "go" => Some(8),
        "plus" => Some(20),
        "prolite" => Some(100),
        "pro" => Some(200),
        "promax" => Some(500),
        _ => None,
    }
}

fn parse_paid_credits(value: &Value) -> Option<mj_client::quota::PaidCredits> {
    use mj_client::quota::PaidCredits;
    if value.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return Some(PaidCredits::Unlimited);
    }
    if !value.get("hasCredits")?.as_bool()? {
        return PaidCredits::limited(0.0);
    }
    match value.get("balance").filter(|value| !value.is_null()) {
        None => Some(PaidCredits::Available),
        Some(value) => value
            .as_str()?
            .parse::<f64>()
            .ok()
            .and_then(PaidCredits::limited),
    }
}

fn parse_window(value: &Value) -> Option<CodexUsageWindow> {
    let used = value.get("usedPercent")?.as_i64()?.clamp(0, 100);
    let duration = value.get("windowDurationMins").and_then(Value::as_i64);
    Some(CodexUsageWindow {
        label: window_label(duration),
        remaining_percent: (100 - used) as u8,
        duration_seconds: duration.and_then(|minutes| minutes.checked_mul(60)),
        resets_at: value.get("resetsAt").and_then(Value::as_i64),
    })
}

fn window_label(minutes: Option<i64>) -> String {
    match minutes {
        Some(300) => "5H".to_string(),
        Some(10_080) => "Week".to_string(),
        Some(value) if value > 0 && value < 60 => format!("{value}m"),
        Some(value) if value > 0 && value % 1_440 == 0 => format!("{}d", value / 1_440),
        Some(value) if value > 0 && value % 60 == 0 => format!("{}H", value / 60),
        _ => "limit".to_string(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_cache_root(temp: &tempfile::TempDir) -> PathBuf {
        temp.path().join("cache")
    }

    #[cfg(unix)]
    fn fake_codex_env(
        temp: &tempfile::TempDir,
        script: &str,
    ) -> (HashMap<String, String>, PathBuf) {
        let cache_root = test_cache_root(temp);
        install_fake_managed_codex(&cache_root, script);
        mj_core::test_hooks::install_fake_command(temp.path(), "codex", script);

        let log = temp.path().join("requests.jsonl");
        let env = HashMap::from([
            (
                "PATH".to_string(),
                temp.path().to_string_lossy().into_owned(),
            ),
            (
                "CODEX_USAGE_TEST_LOG".to_string(),
                log.to_string_lossy().into_owned(),
            ),
        ]);
        (env, log)
    }

    #[cfg(unix)]
    async fn refresh_in_temp(
        client: &mut Option<CodexUsageClient>,
        cwd: PathBuf,
        env: HashMap<String, String>,
        _temp: &tempfile::TempDir,
    ) -> CodexUsageStatus {
        refresh_with_cache_root(client, cwd, env, &CodexLauncher::Host, &mut None).await
    }

    #[cfg(unix)]
    async fn refresh_login_in_temp(
        client: &mut Option<CodexUsageClient>,
        cwd: PathBuf,
        env: HashMap<String, String>,
        _temp: &tempfile::TempDir,
    ) -> Result<(), LoginRefreshFailure> {
        refresh_login_with_cache_root(client, cwd, env, &CodexLauncher::Host, &mut None).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_quota_probe_starts_codex_without_an_api_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let seen = temp.path().join("seen");
        let (mut env, _log) = fake_codex_env(
            &temp,
            &format!(
                "#!/bin/sh\nprintf '%s|%s|%s|%s|%s\\n' \"${{OPENAI_API_KEY-unset}}\" \"${{CODEX_API_KEY-unset}}\" \"${{CODEX_ACCESS_TOKEN-unset}}\" \"${{OPENAI_BASE_URL-unset}}\" \"${{RUST_LOG-unset}}\" > {}\nexit 1\n",
                mj_core::targets::posix_quote(&seen.to_string_lossy())
            ),
        );
        for name in mj_core::config::CODEX_CREDENTIAL_ENVIRONMENT {
            env.insert(name.to_owned(), "sk-svcacct-test".to_owned());
        }
        // Trace level would log request bodies that carry the refresh token.
        env.insert("RUST_LOG".to_owned(), "trace".to_owned());
        let mut client = None;

        let _ = refresh_in_temp(&mut client, temp.path().to_path_buf(), env, &temp).await;

        assert_eq!(
            std::fs::read_to_string(seen).expect("the fake codex ran"),
            "unset|unset|unset|unset|error\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_uses_one_initialized_client_for_repeated_queries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, log) = fake_codex_env(
            &temp,
            r#"#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"method":"account/rateLimits/updated"}' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300},"credits":{"hasCredits":true,"unlimited":false,"balance":"50.25"}}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
read_and_log
printf '%s\n' '{"id":5,"result":{"rateLimitsByLimitId":{"codex":{"planType":"promax","primary":{"usedPercent":50,"windowDurationMins":300},"credits":{"hasCredits":false,"unlimited":false,"balance":"50.25"}}}}}'
read_and_log
printf '%s\n' '{"id":6,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
read_and_log
printf '%s\n' '{"id":7,"result":{"rateLimits":{"planType":"business","primary":{"usedPercent":50,"windowDurationMins":300}}}}'
read_and_log
printf '%s\n' '{"id":8,"result":{"account":{"type":"chatgpt","planType":"prolite"}}}'
read_and_log
printf '%s\n' '{"id":9,"result":{"rateLimits":{"primary":{"usedPercent":50,"windowDurationMins":300}}}}'
"#,
        );
        let mut client = None;

        let first =
            refresh_in_temp(&mut client, temp.path().to_path_buf(), env.clone(), &temp).await;
        let second =
            refresh_in_temp(&mut client, temp.path().to_path_buf(), env.clone(), &temp).await;
        let third =
            refresh_in_temp(&mut client, temp.path().to_path_buf(), env.clone(), &temp).await;
        let fourth = refresh_in_temp(&mut client, temp.path().to_path_buf(), env, &temp).await;

        for (status, balance, price) in [
            (&first, Some(50.25), Some(20)),
            (&second, Some(0.0), Some(500)),
            (&third, None, None),
            (&fourth, None, Some(100)),
        ] {
            let CodexUsageStatus::Available(report) = status else {
                panic!("missing quota report")
            };
            assert_eq!(report.monthly_price_usd, price);
            assert_eq!(
                report.paid_credits,
                balance.and_then(mj_client::quota::PaidCredits::limited)
            );
        }

        assert!(matches!(
            first,
            CodexUsageStatus::Available(CodexUsageReport {
                paid_credits: Some(_),
                banked_resets: None,
                primary: Some(CodexUsageWindow {
                    remaining_percent: 75,
                    ..
                }),
                ..
            })
        ));
        assert!(matches!(
            second,
            CodexUsageStatus::Available(CodexUsageReport {
                paid_credits: Some(_),
                banked_resets: None,
                primary: Some(CodexUsageWindow {
                    remaining_percent: 50,
                    ..
                }),
                ..
            })
        ));

        let requests = std::fs::read_to_string(log).expect("request log");
        let messages = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("request json"))
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 10);
        assert_eq!(messages[0]["method"], "initialize");
        assert_eq!(messages[0]["id"], 1);
        assert_eq!(messages[1]["method"], "initialized");
        assert_eq!(messages[2]["method"], "account/read");
        assert_eq!(messages[3]["method"], "account/rateLimits/read");
        assert_eq!(messages[4]["id"], 4);
        assert_eq!(messages[5]["id"], 5);

        client.take().expect("client").shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_login_asks_app_server_for_a_proactive_token_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, log) = fake_codex_env(
            &temp,
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
"#,
        );
        let mut client = None;

        refresh_login_in_temp(&mut client, temp.path().to_path_buf(), env, &temp)
            .await
            .expect("proactive refresh");

        let requests = std::fs::read_to_string(log).expect("request log");
        let messages = requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("request json"))
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["method"], "initialize");
        assert_eq!(messages[1]["method"], "initialized");
        assert_eq!(messages[2]["method"], "account/read");
        assert_eq!(messages[2]["params"]["refreshToken"], true);

        client.take().expect("client").shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_login_keeps_the_client_when_the_refresh_flag_is_unsupported() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, _log) = fake_codex_env(
            &temp,
            r#"#!/bin/sh
read_and_log() {
    IFS= read -r line || exit 1
    printf '%s\n' "$line" >> "$CODEX_USAGE_TEST_LOG"
}
read_and_log
printf '%s\n' '{"id":1,"result":{}}'
read_and_log
read_and_log
printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"unknown parameter"}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":300}}}}'
"#,
        );
        let mut client = None;

        refresh_login_in_temp(&mut client, temp.path().to_path_buf(), env.clone(), &temp)
            .await
            .expect("an app-server without the flag has nothing to refresh");
        assert!(client.is_some(), "the client stays usable for the poll");

        let status = refresh_in_temp(&mut client, temp.path().to_path_buf(), env, &temp).await;

        assert!(matches!(
            status,
            CodexUsageStatus::Available(CodexUsageReport {
                paid_credits: None,
                banked_resets: None,
                primary: Some(CodexUsageWindow {
                    remaining_percent: 90,
                    ..
                }),
                ..
            })
        ));

        client.take().expect("client").shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_discards_client_when_app_server_is_unsupported() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, _log) = fake_codex_env(
            &temp,
            r#"#!/bin/sh
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"error":{"code":-32601,"message":"unknown method"}}'
"#,
        );
        let mut client = None;

        let status = refresh_in_temp(&mut client, temp.path().to_path_buf(), env, &temp).await;

        assert_eq!(
            status,
            CodexUsageStatus::Unavailable(
                "installed Codex does not support quota queries".to_string()
            )
        );
        assert!(client.is_none());
    }

    /// A fake app-server that records each start and answers `account/read`
    /// with no account on its first start, as one whose token refresh failed
    /// does, and with a usable login on every later start.
    #[cfg(unix)]
    const SIGNED_OUT_UNTIL_RESTART: &str = r#"#!/bin/sh
starts="$CODEX_USAGE_TEST_LOG.starts"
started_before=no
[ -e "$starts" ] && started_before=yes
printf 'start\n' >> "$starts"
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
if [ "$started_before" = no ]; then
    printf '%s\n' '{"id":2,"result":{"account":null}}'
else
    printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
    IFS= read -r line || exit 1
    printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":29,"windowDurationMins":10080}}}}'
fi
IFS= read -r line
"#;

    #[cfg(unix)]
    fn starts(log: &std::path::Path) -> usize {
        let mut path = log.as_os_str().to_owned();
        path.push(".starts");
        std::fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// App-server answers a refresh whose token exchange failed with no
    /// account rather than an error (#1224).
    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_login_reports_a_missing_account_as_a_failed_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, log) = fake_codex_env(&temp, SIGNED_OUT_UNTIL_RESTART);
        let mut client = None;

        let failure = refresh_login_in_temp(&mut client, temp.path().to_path_buf(), env, &temp)
            .await
            .expect_err("no account means the refresh failed");

        assert_eq!(failure.reason, "not signed in with ChatGPT");
        assert!(client.is_none(), "the signed-out app-server is replaced");
        assert_eq!(starts(&log), 1);
    }

    /// An app-server that has recorded an authentication failure keeps
    /// reporting it until it restarts, even once the credential file works
    /// again (#1224). The next poll starts a new one.
    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_recovers_after_a_cached_authentication_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, log) = fake_codex_env(&temp, SIGNED_OUT_UNTIL_RESTART);
        let mut client = None;

        let first =
            refresh_in_temp(&mut client, temp.path().to_path_buf(), env.clone(), &temp).await;
        assert_eq!(
            first,
            CodexUsageStatus::Unavailable("not signed in with ChatGPT".to_string())
        );
        assert!(client.is_none());

        let second = refresh_in_temp(&mut client, temp.path().to_path_buf(), env, &temp).await;
        assert!(matches!(
            second,
            CodexUsageStatus::Available(CodexUsageReport {
                primary: Some(CodexUsageWindow {
                    remaining_percent: 71,
                    ..
                }),
                ..
            })
        ));
        assert_eq!(starts(&log), 2);

        client.take().expect("client").shutdown().await;
    }

    /// A login that is invalid in the file too costs one app-server start per
    /// call, never a retry inside the call.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_invalid_login_starts_one_app_server_per_call() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, log) = fake_codex_env(
            &temp,
            r#"#!/bin/sh
printf 'start\n' >> "$CODEX_USAGE_TEST_LOG.starts"
IFS= read -r line || exit 1
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line || exit 1
IFS= read -r line || exit 1
printf '%s\n' '{"id":2,"result":{"account":null}}'
IFS= read -r line
"#,
        );
        let mut client = None;

        for poll in 1..=3 {
            let status =
                refresh_in_temp(&mut client, temp.path().to_path_buf(), env.clone(), &temp).await;
            assert_eq!(
                status,
                CodexUsageStatus::Unavailable("not signed in with ChatGPT".to_string())
            );
            assert!(client.is_none());
            assert_eq!(starts(&log), poll);
        }
    }

    /// The tail is read only after app-server has stopped and its stderr has
    /// drained, so a line written just before the failed answer is kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_client_keeps_a_redacted_tail_of_app_server_stderr() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (env, _log) = fake_codex_env(
            &temp,
            r#"#!/bin/sh
i=0
while [ "$i" -lt 25 ]; do
    printf 'noise %s\n' "$i" >&2
    i=$((i + 1))
done
IFS= read -r line || exit 1
printf '\033[31mERROR\033[0m failed to refresh token: refresh_token=rt_abcdefghijklmnopqrstuvwxyz0123 reused\n' >&2
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line
"#,
        );
        let mut client = None;
        start(
            &mut client,
            temp.path().to_path_buf(),
            env,
            &CodexLauncher::Program(
                managed_harness_install_dir(&test_cache_root(&temp), HarnessKind::Codex)
                    .join(CODEX_CLI_ENTRYPOINT),
            ),
            &mut None,
        )
        .await
        .expect("started client");
        prepare(&mut client).await.expect("initialized client");

        let lines = client
            .take()
            .expect("client")
            .shutdown_with_diagnostics()
            .await;

        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        assert_eq!(lines[0], "noise 6");
        assert_eq!(
            lines.last().unwrap(),
            "ERROR failed to refresh token: refresh_token=[redacted] reused"
        );
    }
}
