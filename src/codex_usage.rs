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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Available(CodexUsageReport),
    Unavailable(String),
}

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

impl CodexUsageWindow {
    fn compact_label(&self) -> String {
        let mut label = format!("{} {}% left", self.label, self.remaining_percent);
        if let Some(reset) = self
            .resets_at
            .and_then(crate::usage_format::format_reset_local_seconds)
        {
            label.push_str(" · resets ");
            label.push_str(&reset);
        }
        label
    }
}

pub struct CodexUsageClient {
    child: Child,
    pid: Option<u32>,
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
            pid: child.id(),
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
                        "name": "mjolnir",
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
        // Closing stdin asks app-server to stop; always follow with process-tree
        // cleanup so a wrapper cannot exit successfully while leaving a helper
        // behind. `kill_agent_tree` sends SIGTERM before escalating on Unix.
        crate::acp::kill_agent_tree(&mut self.child, self.pid).await;
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

/// Refresh a persistent app-server client, recreating it after transport or
/// protocol failures. Calls are awaited serially by the session worker.
pub async fn refresh(
    client: &mut Option<CodexUsageClient>,
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> CodexUsageStatus {
    let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        if client.is_none() {
            *client = Some(CodexUsageClient::spawn(cwd, env)?);
        }
        let client = client.as_mut().expect("client initialized above");
        client.initialize().await?;
        client.query().await
    })
    .await;

    match result {
        Ok(Ok(report)) => CodexUsageStatus::Available(report),
        Ok(Err(error)) => {
            if matches!(error, QueryError::Protocol(_) | QueryError::Unsupported)
                && let Some(stale_client) = client.take()
            {
                stale_client.shutdown().await;
            }
            tracing::warn!("codex quota query failed: {error}");
            CodexUsageStatus::Unavailable(error.user_reason().to_string())
        }
        Err(_) => {
            if let Some(stale_client) = client.take() {
                stale_client.shutdown().await;
            }
            tracing::warn!("codex quota query timed out");
            CodexUsageStatus::Unavailable("request timed out".to_string())
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
            .stderr(Stdio::null());
        crate::acp::configure_isolated_child(
            &mut command,
            crate::acp::SpawnIsolation::ProcessGroup,
        );
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
        Some(10_080) => "week".to_string(),
        Some(value) if value > 0 && value < 60 => format!("{value}m"),
        Some(value) if value > 0 && value % 1_440 == 0 => format!("{}d", value / 1_440),
        Some(value) if value > 0 && value % 60 == 0 => format!("{}H", value / 60),
        _ => "limit".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(report.secondary.as_ref().unwrap().label, "week");
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
}
