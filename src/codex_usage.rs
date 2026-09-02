//! Codex subscription quota querying through `codex app-server`.
//!
//! Codex exposes ChatGPT subscription rate limits through its local app-server
//! protocol rather than a one-shot CLI command. Keep the JSONL client isolated
//! from the UI so protocol parsing and unavailable states remain testable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

// A cold Codex app-server start can be slow on busy machines. The client is
// reused after initialization, so this primarily bounds the initial probe.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Available(CodexUsageReport),
    Unavailable(String),
}

#[cfg(test)]
impl CodexUsageStatus {
    pub fn compact_label(&self) -> String {
        match self {
            Self::Available(report) => report.compact_label(),
            Self::Unavailable(reason) => format!("Codex usage unavailable: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageReport {
    pub primary: Option<CodexUsageWindow>,
    pub secondary: Option<CodexUsageWindow>,
}

#[cfg(test)]
impl CodexUsageReport {
    fn compact_label(&self) -> String {
        let parts = [&self.primary, &self.secondary]
            .into_iter()
            .flatten()
            .map(CodexUsageWindow::compact_label)
            .collect::<Vec<_>>();
        format!("Codex usage: {}", parts.join(" · "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageWindow {
    pub label: String,
    pub remaining_percent: u8,
    pub resets_at: Option<i64>,
}

#[cfg(test)]
impl CodexUsageWindow {
    fn compact_label(&self) -> String {
        let mut label = format!("{} {}% left", self.label, self.remaining_percent);
        if let Some(reset) = self
            .resets_at
            .and_then(crate::hel_quota::format_reset_local_seconds)
        {
            label.push_str(" · resets ");
            label.push_str(&reset);
        }
        label
    }
}

pub struct CodexUsageClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    initialized: bool,
}

impl CodexUsageClient {
    fn spawn(cwd: PathBuf, env: HashMap<String, String>) -> Result<Self, QueryError> {
        let mut child = spawn_codex(cwd, env)?;

        let stdin = child
            .stdin
            .take()
            .ok_or(QueryError::Protocol(ProtocolError::Io))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(QueryError::Protocol(ProtocolError::Io))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
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
    async fn refresh_token(&mut self) -> Result<(), QueryError> {
        let id = self
            .send_request("account/read", json!({ "refreshToken": true }))
            .await?;
        match self.read_result(id).await {
            Ok(_) | Err(QueryError::Unsupported) => Ok(()),
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
        parse_report(&limits)
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

    pub async fn shutdown(mut self) {
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
    }
}

async fn read_bounded_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, QueryError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let (consumed, found_newline) = {
            let available = reader
                .fill_buf()
                .await
                .map_err(|_| QueryError::Protocol(ProtocolError::Io))?;
            if available.is_empty() {
                if frame.is_empty() {
                    return Ok(None);
                }
                return Err(QueryError::Protocol(ProtocolError::Closed));
            }
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if frame.len().saturating_add(take) > MAX_RESPONSE_BYTES {
                return Err(QueryError::Protocol(ProtocolError::TooLarge));
            }
            frame.extend_from_slice(&available[..take]);
            (take, available.get(take.saturating_sub(1)) == Some(&b'\n'))
        };
        reader.consume(consumed);
        if found_newline {
            return Ok(Some(frame));
        }
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

/// Spawn the cached client if it is missing, then make sure it is initialized.
async fn prepare(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<&mut CodexUsageClient, QueryError> {
    if client.is_none() {
        *client = Some(CodexUsageClient::spawn(cwd, env)?);
    }
    let ready = client.as_mut().expect("client initialized above");
    ready.initialize().await?;
    Ok(ready)
}

/// Drop a client the failure says is no longer usable. A transport or protocol
/// break leaves the stream out of step, so the next call needs a fresh child.
async fn discard_failed_client(client: &mut Option<CodexUsageClient>, replaceable: bool) {
    if replaceable && let Some(stale_client) = client.take() {
        stale_client.shutdown().await;
    }
}

/// Refresh a persistent app-server client, recreating it after transport or
/// protocol failures. Calls are awaited serially by the session worker.
pub async fn refresh(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> CodexUsageStatus {
    let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        prepare(client, cwd, env).await?.query().await
    })
    .await;

    match result {
        Ok(Ok(report)) => CodexUsageStatus::Available(report),
        Ok(Err(error)) => {
            discard_failed_client(
                client,
                matches!(error, QueryError::Protocol(_) | QueryError::Unsupported),
            )
            .await;
            tracing::warn!("codex quota query failed: {error}");
            CodexUsageStatus::Unavailable(error.user_reason().to_string())
        }
        Err(_) => {
            discard_failed_client(client, true).await;
            tracing::warn!("codex quota query timed out");
            CodexUsageStatus::Unavailable("request timed out".to_string())
        }
    }
}

/// Rotate the profile's Codex login ahead of its expiry, reusing the cached
/// client the way [`refresh`] does.
///
/// Codex refresh tokens are single use. A host and a container that reach
/// expiry at the same instant both try to spend the same token, one wins, and
/// the loser's turn dies. Rotating early on the host, so the sync can push the
/// new file, keeps container copies away from that instant.
pub async fn refresh_login(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<(), String> {
    let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        prepare(client, cwd, env).await?.refresh_token().await
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            discard_failed_client(client, matches!(error, QueryError::Protocol(_))).await;
            Err(error.to_string())
        }
        Err(_) => {
            discard_failed_client(client, true).await;
            Err("request timed out".to_string())
        }
    }
}

fn spawn_codex(cwd: PathBuf, env: HashMap<String, String>) -> Result<Child, QueryError> {
    let programs: &[&str] = if cfg!(windows) {
        &["codex.exe", "codex.cmd"]
    } else {
        &["codex"]
    };
    for (index, program) in programs.iter().enumerate() {
        let mut command = Command::new(program);
        command
            .args(["app-server", "--stdio"])
            .current_dir(&cwd)
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command.kill_on_drop(true);
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && index + 1 < programs.len() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(QueryError::NotInstalled);
            }
            Err(error) => return Err(QueryError::Launch(error.to_string())),
        }
    }
    Err(QueryError::NotInstalled)
}

#[derive(Debug)]
enum QueryError {
    NotInstalled,
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
    fn user_reason(&self) -> &'static str {
        match self {
            Self::NotInstalled => "Codex CLI is not installed",
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

fn parse_report(result: &Value) -> Result<CodexUsageReport, QueryError> {
    let codex_snapshot = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .and_then(|buckets| buckets.get("codex"));

    codex_snapshot
        .and_then(parse_snapshot)
        .or_else(|| result.get("rateLimits").and_then(parse_snapshot))
        .ok_or(QueryError::NoData)
}

fn parse_snapshot(snapshot: &Value) -> Option<CodexUsageReport> {
    let report = CodexUsageReport {
        primary: snapshot.get("primary").and_then(parse_window),
        secondary: snapshot.get("secondary").and_then(parse_window),
    };
    if report.primary.is_none() && report.secondary.is_none() {
        None
    } else {
        Some(report)
    }
}

fn parse_window(value: &Value) -> Option<CodexUsageWindow> {
    let used = value.get("usedPercent")?.as_i64()?.clamp(0, 100);
    let duration = value.get("windowDurationMins").and_then(Value::as_i64);
    Some(CodexUsageWindow {
        label: window_label(duration),
        remaining_percent: (100 - used) as u8,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_codex_env(
        temp: &tempfile::TempDir,
        script: &str,
    ) -> (HashMap<String, String>, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let executable = temp.path().join("codex");
        std::fs::write(&executable, script).expect("write fake codex");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake codex metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).expect("make fake codex executable");

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

    #[test]
    fn parses_codex_bucket_and_formats_remaining_windows() {
        let report = parse_report(&json!({
            "rateLimits": { "primary": { "usedPercent": 99, "windowDurationMins": 60 } },
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": { "usedPercent": 25, "windowDurationMins": 300 },
                    "secondary": { "usedPercent": 18, "windowDurationMins": 10080 }
                }
            }
        }))
        .expect("report");

        assert_eq!(report.primary.as_ref().unwrap().remaining_percent, 75);
        assert_eq!(report.primary.as_ref().unwrap().label, "5H");
        assert_eq!(report.secondary.as_ref().unwrap().remaining_percent, 82);
        assert_eq!(report.secondary.as_ref().unwrap().label, "Week");
    }

    #[test]
    fn falls_back_when_codex_bucket_has_no_usable_windows() {
        let report = parse_report(&json!({
            "rateLimits": {
                "primary": { "usedPercent": 20, "windowDurationMins": 300, "resetsAt": 1234 }
            },
            "rateLimitsByLimitId": { "codex": {} }
        }))
        .expect("fallback report");
        let primary = report.primary.expect("primary");
        assert_eq!(primary.remaining_percent, 80);
        assert_eq!(primary.resets_at, Some(1234));
    }

    #[test]
    fn clamps_percentages_and_accepts_one_window() {
        let report = parse_report(&json!({
            "rateLimits": {
                "primary": { "usedPercent": 120, "windowDurationMins": 30 }
            }
        }))
        .expect("report");
        assert_eq!(report.primary.unwrap().remaining_percent, 0);
        assert!(report.secondary.is_none());
    }

    #[test]
    fn clamps_negative_percentages_and_ignores_invalid_window_fields() {
        let report = parse_report(&json!({
            "rateLimits": {
                "primary": {
                    "usedPercent": -5,
                    "windowDurationMins": 1440,
                    "resetsAt": "later"
                },
                "secondary": { "usedPercent": "unknown" }
            }
        }))
        .expect("report");
        let primary = report.primary.expect("primary");
        assert_eq!(primary.remaining_percent, 100);
        assert_eq!(primary.label, "1d");
        assert_eq!(primary.resets_at, None);
        assert!(report.secondary.is_none());
    }

    #[test]
    fn empty_limits_are_unavailable() {
        assert!(matches!(
            parse_report(&json!({ "rateLimits": {} })),
            Err(QueryError::NoData)
        ));
    }

    #[test]
    fn classifies_account_types() {
        assert!(classify_account(&json!({ "account": { "type": "chatgpt" } })).is_ok());
        assert!(matches!(
            classify_account(&json!({ "account": null })),
            Err(QueryError::NotSignedIn)
        ));
        assert!(matches!(
            classify_account(&json!({ "account": { "type": "apiKey" } })),
            Err(QueryError::UnsupportedAccount)
        ));
        assert!(matches!(
            classify_account(&json!({})),
            Err(QueryError::NotSignedIn)
        ));
        assert!(matches!(
            classify_account(&json!({ "account": {} })),
            Err(QueryError::UnsupportedAccount)
        ));
    }

    #[test]
    fn labels_arbitrary_window_durations() {
        assert_eq!(window_label(Some(15)), "15m");
        assert_eq!(window_label(Some(120)), "2H");
        assert_eq!(window_label(Some(2_880)), "2d");
        assert_eq!(window_label(Some(61)), "limit");
        assert_eq!(window_label(Some(0)), "limit");
        assert_eq!(window_label(None), "limit");
    }

    #[test]
    fn status_labels_available_and_unavailable_values() {
        let available = CodexUsageStatus::Available(CodexUsageReport {
            primary: Some(CodexUsageWindow {
                label: "5H".to_string(),
                remaining_percent: 75,
                resets_at: None,
            }),
            secondary: None,
        });
        assert_eq!(available.compact_label(), "Codex usage: 5H 75% left");
        let with_reset = CodexUsageStatus::Available(CodexUsageReport {
            primary: Some(CodexUsageWindow {
                label: "5H".to_string(),
                remaining_percent: 75,
                resets_at: Some(2_000_000_000),
            }),
            secondary: None,
        });
        assert!(with_reset.compact_label().contains(" · resets "));
        assert_eq!(
            CodexUsageStatus::Unavailable("not signed in".to_string()).compact_label(),
            "Codex usage unavailable: not signed in"
        );
    }

    #[test]
    fn response_parser_ignores_notifications_and_matches_request_id() {
        assert_eq!(
            parse_response(
                &json!({ "method": "account/rateLimits/updated", "params": {} }),
                4,
            )
            .expect("notification"),
            None
        );
        assert_eq!(
            parse_response(&json!({ "id": 3, "result": { "old": true } }), 4)
                .expect("different response"),
            None
        );
        assert_eq!(
            parse_response(&json!({ "id": 4, "result": { "ok": true } }), 4)
                .expect("matching response"),
            Some(json!({ "ok": true }))
        );
    }

    #[test]
    fn response_parser_classifies_unsupported_and_protocol_errors() {
        assert!(matches!(
            parse_response(
                &json!({ "id": 4, "error": { "code": -32601, "message": "missing" } }),
                4,
            ),
            Err(QueryError::Unsupported)
        ));
        assert!(matches!(
            parse_response(
                &json!({ "id": 4, "error": { "code": -32000, "message": "denied" } }),
                4,
            ),
            Err(QueryError::Protocol(ProtocolError::RemoteError))
        ));
        assert!(matches!(
            parse_response(&json!({ "id": 4, "error": {} }), 4),
            Err(QueryError::Protocol(ProtocolError::RemoteError))
        ));
        assert!(matches!(
            parse_response(&json!({ "id": 4 }), 4),
            Err(QueryError::Protocol(ProtocolError::InvalidResponse))
        ));
        assert_eq!(
            parse_response(&json!({ "id": "4", "result": {} }), 4).expect("string id"),
            None
        );
    }

    #[test]
    fn query_errors_have_stable_user_reasons_and_diagnostics() {
        let cases = [
            (QueryError::NotInstalled, "Codex CLI is not installed"),
            (
                QueryError::Launch("permission denied".to_string()),
                "could not start Codex CLI",
            ),
            (QueryError::NotSignedIn, "not signed in with ChatGPT"),
            (
                QueryError::UnsupportedAccount,
                "ChatGPT subscription quota is not available for this account",
            ),
            (
                QueryError::Unsupported,
                "installed Codex does not support quota queries",
            ),
            (QueryError::NoData, "no rate-limit data returned"),
            (
                QueryError::Protocol(ProtocolError::Io),
                "Codex quota request failed",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.user_reason(), expected);
        }

        assert_eq!(
            QueryError::Launch("permission denied".to_string()).to_string(),
            "could not start Codex CLI: permission denied"
        );
        assert_eq!(
            QueryError::Protocol(ProtocolError::Closed).to_string(),
            "Codex app-server protocol error (Closed)"
        );
        assert_eq!(
            QueryError::Unsupported.to_string(),
            "installed Codex does not support quota queries"
        );
    }

    #[tokio::test]
    async fn bounded_frame_reads_complete_frames_and_clean_eof() {
        let mut reader = BufReader::new(&b"first\nsecond\n"[..]);
        assert_eq!(
            read_bounded_frame(&mut reader).await.expect("first frame"),
            Some(b"first\n".to_vec())
        );
        assert_eq!(
            read_bounded_frame(&mut reader).await.expect("second frame"),
            Some(b"second\n".to_vec())
        );
        assert_eq!(
            read_bounded_frame(&mut reader).await.expect("clean eof"),
            None
        );
    }

    #[tokio::test]
    async fn bounded_frame_rejects_oversized_or_incomplete_responses() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_RESPONSE_BYTES + 1])
                .await
                .expect("write oversized frame");
        });
        let mut reader = BufReader::new(reader);
        assert!(matches!(
            read_bounded_frame(&mut reader).await,
            Err(QueryError::Protocol(ProtocolError::TooLarge))
        ));
        writer_task.abort();

        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"{\"id\":1").await.expect("write partial");
        drop(writer);
        let mut reader = BufReader::new(reader);
        assert!(matches!(
            read_bounded_frame(&mut reader).await,
            Err(QueryError::Protocol(ProtocolError::Closed))
        ));
    }

    #[tokio::test]
    async fn refresh_reports_missing_codex_without_retaining_a_client() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = HashMap::from([(
            "PATH".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]);
        let mut client = None;

        let status = refresh(&mut client, temp.path().to_path_buf(), env).await;

        assert_eq!(
            status,
            CodexUsageStatus::Unavailable("Codex CLI is not installed".to_string())
        );
        assert!(client.is_none());
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
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}}'
read_and_log
printf '%s\n' '{"id":4,"result":{"account":{"type":"chatgpt"}}}'
read_and_log
printf '%s\n' '{"id":5,"result":{"rateLimits":{"primary":{"usedPercent":50,"windowDurationMins":300}}}}'
"#,
        );
        let mut client = None;

        let first = refresh(&mut client, temp.path().to_path_buf(), env.clone()).await;
        let second = refresh(&mut client, temp.path().to_path_buf(), env).await;

        assert!(matches!(
            first,
            CodexUsageStatus::Available(CodexUsageReport {
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
        assert_eq!(messages.len(), 6);
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

        refresh_login(&mut client, temp.path().to_path_buf(), env)
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

        refresh_login(&mut client, temp.path().to_path_buf(), env.clone())
            .await
            .expect("an app-server without the flag has nothing to refresh");
        assert!(client.is_some(), "the client stays usable for the poll");

        let status = refresh(&mut client, temp.path().to_path_buf(), env).await;

        assert!(matches!(
            status,
            CodexUsageStatus::Available(CodexUsageReport {
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

        let status = refresh(&mut client, temp.path().to_path_buf(), env).await;

        assert_eq!(
            status,
            CodexUsageStatus::Unavailable(
                "installed Codex does not support quota queries".to_string()
            )
        );
        assert!(client.is_none());
    }
}
