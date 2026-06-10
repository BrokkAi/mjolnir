//! Simple remote-control server and local session registration.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::{
    ContentBlock, PermissionOptionKind, SessionUpdate, ToolCallContent,
};
use anyhow::{Context, Result, anyhow};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, COOKIE, HeaderValue, SET_COOKIE};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{Clear, ClearType},
};
use qrcode::QrCode;
use qrcode::types::Color;
use rcgen::generate_simple_self_signed;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::SelectedAgent;
use crate::event::{PermissionPrompt, UiCommand, UiEvent};

const REMOTE_CONTROL_LOCAL_ADDR: &str = "127.0.0.1:11921";
const REMOTE_CONTROL_PUBLIC_ADDR: &str = "0.0.0.0:11921";
const REMOTE_CONTROL_UPSERT_URL: &str = "https://localhost:11921/api/sessions";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const CONNECTED_SESSION_TTL: Duration = Duration::from_secs(75);
/// How often `mj server` sweeps dead queue entries out of sqlite.
const QUEUE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
/// Queued prompts survive disconnects on purpose: `mj resume <session-id>`
/// re-registers the same session id and claims them. They only become dead
/// weight once it is clear nobody will resume, so the cap is generous.
const QUEUED_PROMPT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Permission decisions, by contrast, can only ever apply to a prompt held
/// in a live session's memory. A live session claims within seconds, so an
/// old unclaimed decision is unambiguously dead.
const PERMISSION_DECISION_TTL: Duration = Duration::from_secs(60 * 60);
const SESSION_COOKIE_NAME: &str = "mj_remote_session";
/// The six-digit viewer code is only ~20 bits of entropy, so the manual-unlock
/// endpoint must be throttled or it can be brute-forced — especially once the
/// server is bound publicly via `--hostname`. After this many consecutive
/// failures the code path is locked for `VIEWER_CODE_LOCKOUT`; the QR/token
/// path is unaffected, so the legitimate operator is never locked out.
const MAX_VIEWER_CODE_ATTEMPTS: u32 = 5;
const VIEWER_CODE_LOCKOUT: Duration = Duration::from_secs(30);
/// A `SessionRecord` can include the full transcript history; allow room for
/// larger snapshots while still capping request bodies to something reasonable.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Tracks consecutive failed viewer-code attempts to rate-limit brute force.
#[derive(Debug, Default)]
struct CodeAuthGuard {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub name: String,
    pub start_time: String,
    pub last_update: String,
    pub total_messages: u64,
    pub project: String,
    pub agent: String,
    #[serde(default)]
    pub transcript: Vec<TranscriptEntry>,
    #[serde(default)]
    pub queued_prompt_count: u64,
    /// Permission prompts currently waiting for an answer in this session.
    #[serde(default)]
    pub pending_permissions: Vec<PendingPermissionRecord>,
}

/// A permission prompt the session is blocked on, published so the remote
/// viewer can render the options and queue a decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPermissionRecord {
    /// The tool-call id of the request; decisions reference it so a stale
    /// answer can never resolve a different prompt.
    pub request_id: String,
    pub title: String,
    pub options: Vec<PermissionOptionRecord>,
    pub requested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionOptionRecord {
    pub option_id: String,
    pub label: String,
    /// Stable machine-readable kind (`allow_once`, `reject_always`, ...)
    /// so the viewer can style allow/reject buttons differently.
    pub kind: String,
}

/// A viewer-made permission decision queued until the session claims it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionDecisionRecord {
    pub id: i64,
    pub session_id: String,
    pub request_id: String,
    pub option_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub id: i64,
    pub session_id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionAuthRequest {
    code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionAuthQuery {
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuePromptRequest {
    session_id: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClaimQueuedPromptRequest {
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionQueueQuery {
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuePermissionDecisionRequest {
    session_id: String,
    request_id: String,
    option_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClaimPermissionDecisionRequest {
    session_id: String,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionTracker {
    client: Option<reqwest::Client>,
    token: Option<Arc<String>>,
    state: Arc<Mutex<TrackerState>>,
    /// Single task that owns every snapshot upload (including heartbeats),
    /// with at most one request in flight. Serializing here means a newer
    /// snapshot can never be overtaken by an older one — the fast
    /// pending-permission add/remove path depends on that ordering.
    publisher: Arc<Mutex<Option<JoinHandle<()>>>>,
    publish_signal: Arc<tokio::sync::Notify>,
    queue_poller: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// False when no UI event channel exists (headless): remote permission
    /// decisions could never be applied, so pending permissions must not
    /// be advertised to viewers at all.
    publish_permissions: bool,
    shutting_down: Arc<AtomicBool>,
}

#[derive(Debug)]
struct TrackerState {
    session_id: Option<String>,
    name: Option<String>,
    start_time: Option<String>,
    last_update: Option<String>,
    total_messages: u64,
    project: String,
    agent: String,
    agent_message_open: bool,
    prompt_in_flight: bool,
    transcript: Vec<TranscriptEntry>,
    pending_permissions: Vec<PendingPermissionRecord>,
}

#[derive(Debug, Clone)]
struct ServerPaths {
    db_path: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    token_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerListenConfig {
    bind_addr: String,
    viewer_host: String,
}

#[derive(Debug, Clone)]
struct ServerState {
    db_path: Arc<PathBuf>,
    token: Arc<String>,
    viewer_code: Arc<String>,
    /// Active viewer session cookie values. Each successful unlock mints a fresh
    /// random id so logout can revoke exactly that browser's session, and a lost
    /// cookie does not stay valid forever like a single shared secret would.
    sessions: Arc<Mutex<HashSet<String>>>,
    code_guard: Arc<Mutex<CodeAuthGuard>>,
}

impl TrackerState {
    fn new(project: String, agent: String) -> Self {
        Self {
            session_id: None,
            name: None,
            start_time: None,
            last_update: None,
            total_messages: 0,
            project,
            agent,
            agent_message_open: false,
            prompt_in_flight: false,
            transcript: Vec::new(),
            pending_permissions: Vec::new(),
        }
    }

    fn observe_command(&mut self, command: &UiCommand) {
        if let UiCommand::SendPrompt { text, .. } = command {
            self.total_messages = self.total_messages.saturating_add(1);
            self.agent_message_open = false;
            self.prompt_in_flight = true;
            self.push_transcript_entry("user", text.clone());
            self.touch();
        }
    }

    fn observe_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::SessionStarted { session_id, .. } => {
                let now = now_rfc3339();
                self.session_id = Some(session_id.clone());
                if self.name.is_none() {
                    self.name = Some(session_id.clone());
                }
                if self.start_time.is_none() {
                    self.start_time = Some(now.clone());
                }
                self.last_update = Some(now);
                self.agent_message_open = false;
                self.prompt_in_flight = false;
                self.pending_permissions.clear();
            }
            UiEvent::SessionUpdate(update) => {
                self.observe_session_update(update);
            }
            UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } | UiEvent::Fatal(_) => {
                self.agent_message_open = false;
                self.prompt_in_flight = false;
                // The turn is over; any prompt still listed here was
                // cancelled by the runtime, so don't advertise it.
                self.pending_permissions.clear();
                self.touch();
            }
            UiEvent::Connected { .. }
            | UiEvent::SessionConfigOptions { .. }
            | UiEvent::PermissionRequest(_)
            | UiEvent::RemotePermissionDecision { .. }
            | UiEvent::Warning(_) => {}
        }
    }

    fn observe_session_update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if !self.agent_message_open {
                    self.total_messages = self.total_messages.saturating_add(1);
                    self.agent_message_open = true;
                }
                self.append_transcript_text("agent", content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.agent_message_open = false;
                self.append_transcript_text("thought", content_block_text(&chunk.content));
                self.touch();
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.agent_message_open = false;
                self.push_transcript_entry(
                    "tool",
                    format_tool_call(tool_call.title.as_str(), &tool_call.content),
                );
                self.touch();
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.agent_message_open = false;
                if let Some(content) = &update.fields.content {
                    self.push_transcript_entry(
                        "tool",
                        format_tool_call(update.fields.title.as_deref().unwrap_or("tool"), content),
                    );
                }
                self.touch();
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value() {
                    self.name = Some(title.clone());
                }
                self.agent_message_open = false;
                self.touch();
            }
            _ => {
                self.agent_message_open = false;
                self.touch();
            }
        }
    }

    fn append_transcript_text(&mut self, kind: &str, text: String) {
        if let Some(last) = self.transcript.last_mut()
            && last.kind == kind
        {
            last.text.push_str(&text);
            return;
        }
        self.push_transcript_entry(kind, text);
    }

    fn push_transcript_entry(&mut self, kind: &str, text: String) {
        self.transcript.push(TranscriptEntry {
            kind: kind.to_string(),
            text,
            timestamp: now_rfc3339(),
        });
    }

    fn snapshot(&self) -> Option<SessionRecord> {
        let session_id = self.session_id.clone()?;
        let start_time = self.start_time.clone()?;
        let last_update = self.last_update.clone()?;
        Some(SessionRecord {
            name: self.name.clone().unwrap_or_else(|| session_id.clone()),
            session_id,
            start_time,
            last_update,
            total_messages: self.total_messages,
            project: self.project.clone(),
            agent: self.agent.clone(),
            transcript: self.transcript.clone(),
            queued_prompt_count: 0,
            pending_permissions: self.pending_permissions.clone(),
        })
    }

    fn touch(&mut self) {
        self.last_update = Some(now_rfc3339());
    }

    fn reserve_remote_prompt_slot(&mut self) -> Option<String> {
        if self.prompt_in_flight {
            return None;
        }
        let session_id = self.session_id.clone()?;
        self.prompt_in_flight = true;
        Some(session_id)
    }

    fn release_remote_prompt_slot(&mut self) {
        self.prompt_in_flight = false;
    }

    fn push_pending_permission(&mut self, record: PendingPermissionRecord) {
        self.pending_permissions.push(record);
        self.touch();
    }

    fn remove_pending_permission(&mut self, request_id: &str) {
        self.pending_permissions
            .retain(|pending| pending.request_id != request_id);
        self.touch();
    }

    /// Session id to claim permission decisions for, when at least one
    /// permission prompt is waiting.
    fn permission_claim_session(&self) -> Option<String> {
        if self.pending_permissions.is_empty() {
            return None;
        }
        self.session_id.clone()
    }
}

impl RemoteSessionTracker {
    pub fn new(
        project: String,
        agent: String,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    ) -> Self {
        let dir = remote_control_dir();
        let token = read_token(&dir.join("token")).map(Arc::new);
        let client = build_client(&dir.join("cert.pem"));
        let tracker = Self {
            client,
            token,
            state: Arc::new(Mutex::new(TrackerState::new(project, agent))),
            publisher: Arc::new(Mutex::new(None)),
            publish_signal: Arc::new(tokio::sync::Notify::new()),
            queue_poller: Arc::new(Mutex::new(None)),
            publish_permissions: ui_event_tx.is_some(),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };
        tracker.ensure_queue_poller(command_tx, ui_event_tx);
        tracker
    }

    /// Tracker with no HTTP client and no pollers, so tests can exercise
    /// state transitions without touching the filesystem or network.
    #[cfg(test)]
    fn new_disconnected(project: String, agent: String) -> Self {
        Self {
            client: None,
            token: None,
            state: Arc::new(Mutex::new(TrackerState::new(project, agent))),
            publisher: Arc::new(Mutex::new(None)),
            publish_signal: Arc::new(tokio::sync::Notify::new()),
            queue_poller: Arc::new(Mutex::new(None)),
            publish_permissions: true,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Pass events through on their way to the UI. Permission prompts get
    /// their responder wrapped so the tracker can publish the pending
    /// request to the remote-control server and retract it the moment it
    /// is answered — locally, remotely, or by cancellation.
    ///
    /// A no-op when remote decisions cannot be applied (headless): viewers
    /// must never see approval buttons that would be accepted with a 202
    /// and then silently dropped.
    pub fn intercept_event(&self, event: UiEvent) -> UiEvent {
        if !self.publish_permissions || self.shutting_down.load(Ordering::Relaxed) {
            return event;
        }
        match event {
            UiEvent::PermissionRequest(prompt) => {
                UiEvent::PermissionRequest(self.track_permission_prompt(prompt))
            }
            other => other,
        }
    }

    fn track_permission_prompt(&self, prompt: PermissionPrompt) -> PermissionPrompt {
        let request_id = prompt.tool_call.tool_call_id.to_string();
        let record = PendingPermissionRecord {
            request_id: request_id.clone(),
            title: prompt
                .tool_call
                .fields
                .title
                .clone()
                .map(|title| title.replace("\\n", "\n"))
                .unwrap_or_else(|| request_id.clone()),
            options: prompt
                .options
                .iter()
                .map(|option| PermissionOptionRecord {
                    option_id: option.option_id.to_string(),
                    label: option.name.clone(),
                    kind: permission_option_kind_id(option.kind).to_string(),
                })
                .collect(),
            requested_at: now_rfc3339(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.push_pending_permission(record);
        }
        self.request_flush();

        let PermissionPrompt {
            tool_call,
            options,
            responder,
        } = prompt;
        let (wrapped_tx, wrapped_rx) = tokio::sync::oneshot::channel();
        let tracker = self.clone();
        tokio::spawn(async move {
            let decision = wrapped_rx.await;
            if let Ok(mut state) = tracker.state.lock() {
                state.remove_pending_permission(&request_id);
            }
            // On Err the UI dropped its sender (cancel); dropping
            // `responder` here forwards exactly that signal.
            if let Ok(decision) = decision {
                let _ = responder.send(decision);
            }
            tracker.request_flush();
        });
        PermissionPrompt {
            tool_call,
            options,
            responder: wrapped_tx,
        }
    }

    pub fn observe_command(&self, command: &UiCommand) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_command(command);
        }
        self.request_flush();
    }

    pub fn observe_event(&self, event: &UiEvent) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.observe_event(event);
        }
        self.request_flush();
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let handle = self.publisher.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
        let queue_poller = self
            .queue_poller
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(handle) = queue_poller {
            handle.abort();
            let _ = handle.await;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let snapshot = self.state.lock().ok().and_then(|state| state.snapshot());
        let session_id = snapshot
            .as_ref()
            .map(|snapshot| snapshot.session_id.clone());
        if let Some(snapshot) = snapshot
            && let Err(error) = send_snapshot(client.clone(), self.token.clone(), snapshot).await
        {
            debug!("final remote-control flush failed: {error:#}");
        }
        if let Some(session_id) = session_id
            && let Err(error) = send_disconnect(client, self.token.clone(), &session_id).await
        {
            debug!("remote-control disconnect failed: {error:#}");
        }
    }

    /// Ask the publisher for a fresh snapshot upload. Signals coalesce: any
    /// number of requests while an upload is in flight result in exactly one
    /// follow-up upload, which re-reads the state and therefore always
    /// carries the newest snapshot.
    fn request_flush(&self) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        self.ensure_publisher();
        self.publish_signal.notify_one();
    }

    fn ensure_publisher(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Ok(mut slot) = self.publisher.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let state = Arc::clone(&self.state);
        let token = self.token.clone();
        let signal = Arc::clone(&self.publish_signal);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = signal.notified() => {}
                    _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                        // Heartbeat: refresh last_update so an idle session
                        // stays inside the server's liveness window.
                        if let Ok(mut state) = state.lock() {
                            state.touch();
                        }
                    }
                }
                let snapshot = state.lock().ok().and_then(|state| state.snapshot());
                let Some(snapshot) = snapshot else {
                    continue;
                };
                if let Err(error) = send_snapshot(client.clone(), token.clone(), snapshot).await {
                    debug!("remote-control publish failed: {error:#}");
                }
            }
        }));
    }

    fn ensure_queue_poller(
        &self,
        command_tx: Option<tokio::sync::mpsc::UnboundedSender<UiCommand>>,
        ui_event_tx: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(command_tx) = command_tx else {
            return;
        };
        let Ok(mut slot) = self.queue_poller.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let token = self.token.clone();
        let state = Arc::clone(&self.state);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;

                // Permission decisions first: while a permission prompt is
                // pending the turn is blocked, so the prompt-claim path
                // below is a no-op anyway. Decisions only make sense when
                // a UI is attached to apply them; headless answers
                // permissions by policy instead.
                if let Some(ui_event_tx) = ui_event_tx.as_ref() {
                    let claim_session = state
                        .lock()
                        .ok()
                        .and_then(|guard| guard.permission_claim_session());
                    if let Some(session_id) = claim_session {
                        match claim_remote_permission_decision(
                            client.clone(),
                            token.clone(),
                            &session_id,
                        )
                        .await
                        {
                            Ok(Some(decision)) => {
                                let _ = ui_event_tx.send(UiEvent::RemotePermissionDecision {
                                    request_id: decision.request_id,
                                    option_id: decision.option_id,
                                });
                            }
                            Ok(None) => {}
                            Err(error) => {
                                debug!("remote permission-decision poll failed: {error:#}");
                            }
                        }
                    }
                }

                let session_id = {
                    let Ok(mut guard) = state.lock() else {
                        continue;
                    };
                    guard.reserve_remote_prompt_slot()
                };
                let Some(session_id) = session_id else {
                    continue;
                };

                let queued = claim_remote_prompt(client.clone(), token.clone(), &session_id).await;
                match queued {
                    Ok(Some(prompt)) => {
                        let command = UiCommand::SendPrompt {
                            text: prompt.text,
                            images: Vec::new(),
                        };
                        if let Ok(mut guard) = state.lock() {
                            guard.observe_command(&command);
                        }
                        if command_tx.send(command).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        if let Ok(mut guard) = state.lock() {
                            guard.release_remote_prompt_slot();
                        }
                    }
                    Err(error) => {
                        debug!("remote queued-prompt poll failed: {error:#}");
                        if let Ok(mut guard) = state.lock() {
                            guard.release_remote_prompt_slot();
                        }
                    }
                }
            }
        }));
    }
}

/// Build the HTTP client used to report sessions to the local server.
///
/// The server uses a self-signed certificate, so rather than disabling
/// certificate validation we pin that exact certificate. When it is missing
/// (the server has never run) we leave the client disabled: there is nothing
/// trustworthy to talk to, and reporting anyway would risk leaking the bearer
/// token to whatever is squatting the port.
fn build_client(cert_path: &Path) -> Option<reqwest::Client> {
    let pem = match std::fs::read(cert_path) {
        Ok(pem) => pem,
        Err(_) => return None,
    };
    let cert = match reqwest::Certificate::from_pem(&pem) {
        Ok(cert) => cert,
        Err(error) => {
            warn!(
                "remote-control: ignoring invalid certificate at {}: {error}",
                cert_path.display()
            );
            return None;
        }
    };
    match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .tls_built_in_root_certs(false)
        .add_root_certificate(cert)
        .build()
    {
        Ok(client) => Some(client),
        Err(error) => {
            warn!("remote-control: failed to build HTTP client: {error}");
            None
        }
    }
}

pub async fn run_server(hostname: Option<String>, history_days: u32) -> Result<()> {
    clear_terminal_screen()?;
    install_crypto_provider();

    let requested_hostname = normalize_requested_hostname(hostname.as_deref());
    let listen = server_listen_config(requested_hostname.as_deref())?;
    let paths = ensure_server_paths(requested_hostname.as_deref())?;
    init_db(&paths.db_path)?;
    let token = ensure_token(&paths.token_path)?;
    let viewer_code = generate_viewer_code()?;
    let viewer_url = remote_qr_login_url(&listen.viewer_host, &token);

    let app = build_router(paths.db_path.clone(), token, viewer_code.clone());

    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert_path, &paths.key_path)
            .await
            .context("load remote-control TLS certificate")?;

    let listener = bind_server_listener(&listen.bind_addr)?;

    let history_ttl =
        (history_days > 0).then(|| Duration::from_secs(u64::from(history_days) * 24 * 60 * 60));
    spawn_queue_pruner(paths.db_path.clone(), history_ttl);

    println!(
        "Remote control listening on https://{}:11921",
        listen.viewer_host
    );
    println!("{}", render_login_qr(&viewer_url)?);
    println!("viewer code: {viewer_code}");

    axum_server::from_tcp_rustls(listener, tls_config)
        .serve(app.into_make_service())
        .await
        .with_context(|| format!("serve remote-control API on {}", listen.bind_addr))
}

/// Periodically sweep dead queue entries and expired session history out
/// of sqlite. Runs once immediately so a restart also cleans up garbage
/// left by the previous run.
fn spawn_queue_pruner(db_path: PathBuf, history_ttl: Option<Duration>) {
    tokio::spawn(async move {
        loop {
            let prune_path = db_path.clone();
            let pruned =
                tokio::task::spawn_blocking(move || prune_stale_records(&prune_path, history_ttl))
                    .await;
            match pruned {
                Ok(Ok(counts)) if counts.any() => {
                    debug!(
                        "remote-control prune removed {} queued prompt(s), \
                         {} permission decision(s), and {} session(s)",
                        counts.prompts, counts.decisions, counts.sessions
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => debug!("remote-control prune failed: {error:#}"),
                Err(error) => debug!("remote-control prune task panicked: {error}"),
            }
            tokio::time::sleep(QUEUE_PRUNE_INTERVAL).await;
        }
    });
}

fn bind_server_listener(bind_addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(bind_addr).with_context(|| {
        format!(
            "bind remote-control listener on {bind_addr} (is another `mj server` already running?)"
        )
    })?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set remote-control listener on {bind_addr} to non-blocking"))?;
    Ok(listener)
}

fn clear_terminal_screen() -> Result<()> {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return Ok(());
    }
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))
        .context("clear terminal before starting remote-control server")?;
    Ok(())
}

fn normalize_requested_hostname(hostname: Option<&str>) -> Option<String> {
    hostname
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn remote_qr_login_url(host: &str, token: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    // Target `/auth/login` (not `/?token=`) so the server validates the token,
    // sets the session cookie, and redirects to a clean `/`. This keeps the
    // long-lived token out of the browser history and out of later requests.
    format!("https://{host}:11921/auth/login?token={encoded}")
}

fn render_login_qr(url: &str) -> Result<String> {
    let qr = QrCode::new(url.as_bytes()).context("encode remote viewer QR code")?;
    let mut output = String::new();
    for y in (0..qr.width()).step_by(2) {
        for x in 0..qr.width() {
            let top = qr[(x, y)] == Color::Dark;
            let bottom = if y + 1 < qr.width() {
                qr[(x, y + 1)] == Color::Dark
            } else {
                false
            };
            let ch = match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            output.push(ch);
        }
        output.push('\n');
    }
    Ok(output)
}

/// Install the ring CryptoProvider so we do not depend on aws-lc-rs (which needs
/// cmake + a C toolchain). reqwest and rcgen already pull ring in. Idempotent:
/// a second call is a no-op.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn build_router(db_path: PathBuf, token: String, viewer_code: String) -> Router {
    let state = ServerState {
        db_path: Arc::new(db_path),
        token: Arc::new(token),
        viewer_code: Arc::new(viewer_code),
        sessions: Arc::new(Mutex::new(HashSet::new())),
        code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
    };

    let protected = Router::new()
        .route("/live/sessions", get(list_live_sessions))
        .route("/sessions", get(list_sessions))
        .route("/api/sessions", post(upsert_session))
        .route(
            "/api/sessions/{session_id}",
            axum::routing::delete(disconnect_session),
        )
        .route(
            "/api/queued-prompts",
            get(list_queued_prompts).post(queue_prompt),
        )
        .route("/api/queued-prompts/claim", post(claim_queued_prompt))
        .route("/api/permission-decisions", post(queue_permission_decision))
        .route(
            "/api/permission-decisions/claim",
            post(claim_permission_decision),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));

    Router::new()
        .route("/", get(remote_viewer))
        .route("/auth/login", get(create_viewer_session_from_query))
        .route(
            "/auth/session",
            post(create_viewer_session).delete(clear_viewer_session),
        )
        .merge(protected)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Reject any request that does not carry the expected credentials. The
/// loopback interface is reachable by every local user, so without this any
/// local process could read or overwrite the session registry.
async fn require_token(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, (StatusCode, String)> {
    if request_is_authorized(&state, &request) {
        Ok(next.run(request).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))
    }
}

fn request_is_authorized(state: &ServerState, request: &Request) -> bool {
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let query_token = request.uri().query().and_then(query_token_value);
    if token_matches(state.token.as_str(), bearer)
        || token_matches(state.token.as_str(), query_token.as_deref())
    {
        return true;
    }
    let cookie_header = request
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok());
    let sessions = state.sessions.lock().expect("viewer sessions poisoned");
    sessions
        .iter()
        .any(|session| cookie_matches(cookie_header, SESSION_COOKIE_NAME, session))
}

fn query_token_value(query: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
}

fn cookie_value<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?
        .split(';')
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(cookie_name, _)| *cookie_name == name)
        .map(|(_, value)| value)
}

fn cookie_matches(header: Option<&str>, name: &str, expected: &str) -> bool {
    cookie_value(header, name)
        .is_some_and(|value| constant_time_eq(expected.as_bytes(), value.as_bytes()))
}

fn token_matches(expected: &str, provided: Option<&str>) -> bool {
    match provided {
        Some(token) => constant_time_eq(expected.as_bytes(), token.as_bytes()),
        None => false,
    }
}

/// Length-independent only for equal-length inputs; the token length is fixed,
/// so this avoids leaking how many leading bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn remote_viewer() -> Html<&'static str> {
    Html(include_str!("remote_viewer.html"))
}

async fn create_viewer_session(
    State(state): State<ServerState>,
    Json(payload): Json<SessionAuthRequest>,
) -> std::result::Result<Response, (StatusCode, String)> {
    create_code_session_response(&state, payload.code.trim(), StatusCode::NO_CONTENT)
}

async fn create_viewer_session_from_query(
    State(state): State<ServerState>,
    Query(query): Query<SessionAuthQuery>,
) -> std::result::Result<Response, (StatusCode, String)> {
    create_session_response(&state, query.token.trim(), StatusCode::SEE_OTHER).map(
        |mut response| {
            response
                .headers_mut()
                .insert(axum::http::header::LOCATION, HeaderValue::from_static("/"));
            response
        },
    )
}

fn create_session_response(
    state: &ServerState,
    token: &str,
    status: StatusCode,
) -> std::result::Result<Response, (StatusCode, String)> {
    if !token_matches(state.token.as_str(), Some(token)) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }

    issue_session_cookie(state, status)
}

fn create_code_session_response(
    state: &ServerState,
    code: &str,
    status: StatusCode,
) -> std::result::Result<Response, (StatusCode, String)> {
    if viewer_code_locked(state) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "too many incorrect codes; wait a moment and try again".to_string(),
        ));
    }

    if !token_matches(state.viewer_code.as_str(), Some(code)) {
        record_viewer_code_failure(state);
        return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()));
    }

    reset_viewer_code_failures(state);
    issue_session_cookie(state, status)
}

/// Returns whether the viewer-code path is currently locked out, clearing an
/// expired lockout so the next failure starts a fresh count.
fn viewer_code_locked(state: &ServerState) -> bool {
    let mut guard = state.code_guard.lock().expect("viewer code guard poisoned");
    match guard.locked_until {
        Some(until) if Instant::now() < until => true,
        Some(_) => {
            guard.locked_until = None;
            guard.failures = 0;
            false
        }
        None => false,
    }
}

fn record_viewer_code_failure(state: &ServerState) {
    let mut guard = state.code_guard.lock().expect("viewer code guard poisoned");
    guard.failures = guard.failures.saturating_add(1);
    if guard.failures >= MAX_VIEWER_CODE_ATTEMPTS {
        guard.failures = 0;
        guard.locked_until = Some(Instant::now() + VIEWER_CODE_LOCKOUT);
    }
}

fn reset_viewer_code_failures(state: &ServerState) {
    let mut guard = state.code_guard.lock().expect("viewer code guard poisoned");
    guard.failures = 0;
    guard.locked_until = None;
}

fn issue_session_cookie(
    state: &ServerState,
    status: StatusCode,
) -> std::result::Result<Response, (StatusCode, String)> {
    let session_id = generate_token().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to mint viewer session".to_string(),
        )
    })?;
    let header = session_cookie_header(&session_id)?;
    state
        .sessions
        .lock()
        .expect("viewer sessions poisoned")
        .insert(session_id);

    let mut response = status.into_response();
    response.headers_mut().insert(SET_COOKIE, header);
    Ok(response)
}

async fn clear_viewer_session(
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let cookie_header = headers.get(COOKIE).and_then(|value| value.to_str().ok());
    if let Some(session_id) = cookie_value(cookie_header, SESSION_COOKIE_NAME) {
        state
            .sessions
            .lock()
            .expect("viewer sessions poisoned")
            .remove(session_id);
    }

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_session_cookie_header());
    response
}

fn session_cookie_header(value: &str) -> std::result::Result<HeaderValue, (StatusCode, String)> {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}={value}; Path=/; HttpOnly; Secure; SameSite=Strict"
    ))
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build session cookie".to_string(),
        )
    })
}

fn clear_session_cookie_header() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0"
    ))
    .expect("valid cleared session cookie header")
}

pub fn agent_display_label(agent: &SelectedAgent) -> String {
    if agent.source_id == "custom" {
        let mut words = Vec::with_capacity(agent.args.len() + 1);
        words.push(agent.program.to_string_lossy().into_owned());
        words.extend(agent.args.iter().cloned());
        shell_words::join(words)
    } else {
        agent.source_id.clone()
    }
}

async fn upsert_session(
    State(state): State<ServerState>,
    Json(session): Json<SessionRecord>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        upsert_session_record(db_path.as_ref().as_path(), &session)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn disconnect_session(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        disconnect_session_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let sessions =
        tokio::task::spawn_blocking(move || load_session_records(db_path.as_ref().as_path()))
            .await
            .map_err(internal_error)?
            .map_err(internal_error)?;
    Ok(Json(sessions))
}

async fn list_live_sessions(
    State(state): State<ServerState>,
) -> std::result::Result<Json<Vec<SessionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let cutoff = connected_session_cutoff_rfc3339();
    let sessions = tokio::task::spawn_blocking(move || {
        load_connected_session_records(db_path.as_ref().as_path(), &cutoff)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(sessions))
}

async fn list_queued_prompts(
    State(state): State<ServerState>,
    Query(query): Query<SessionQueueQuery>,
) -> std::result::Result<Json<Vec<QueuedPrompt>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = query.session_id;
    let prompts = tokio::task::spawn_blocking(move || {
        load_queued_prompts(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(prompts))
}

async fn queue_prompt(
    State(state): State<ServerState>,
    Json(request): Json<QueuePromptRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.text.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt text must not be empty".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        queue_prompt_record(
            db_path.as_ref().as_path(),
            &request.session_id,
            &request.text,
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn claim_queued_prompt(
    State(state): State<ServerState>,
    Json(request): Json<ClaimQueuedPromptRequest>,
) -> std::result::Result<Json<Option<QueuedPrompt>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let prompt = tokio::task::spawn_blocking(move || {
        claim_queued_prompt_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(prompt))
}

async fn queue_permission_decision(
    State(state): State<ServerState>,
    Json(request): Json<QueuePermissionDecisionRequest>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    if request.request_id.trim().is_empty() || request.option_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "request_id and option_id must not be empty".to_string(),
        ));
    }
    let db_path = Arc::clone(&state.db_path);
    tokio::task::spawn_blocking(move || {
        queue_permission_decision_record(
            db_path.as_ref().as_path(),
            &request.session_id,
            &request.request_id,
            &request.option_id,
        )
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn claim_permission_decision(
    State(state): State<ServerState>,
    Json(request): Json<ClaimPermissionDecisionRequest>,
) -> std::result::Result<Json<Option<PermissionDecisionRecord>>, (StatusCode, String)> {
    let db_path = Arc::clone(&state.db_path);
    let session_id = request.session_id;
    let decision = tokio::task::spawn_blocking(move || {
        claim_permission_decision_record(db_path.as_ref().as_path(), &session_id)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    Ok(Json(decision))
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn remote_control_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("remote-control")
}

fn server_listen_config(hostname: Option<&str>) -> Result<ServerListenConfig> {
    match normalize_requested_hostname(hostname).as_deref() {
        Some(hostname) => Ok(ServerListenConfig {
            bind_addr: REMOTE_CONTROL_PUBLIC_ADDR.to_string(),
            viewer_host: hostname.to_string(),
        }),
        None => Ok(ServerListenConfig {
            bind_addr: REMOTE_CONTROL_LOCAL_ADDR.to_string(),
            viewer_host: "localhost".to_string(),
        }),
    }
}

fn ensure_server_paths(hostname: Option<&str>) -> Result<ServerPaths> {
    ensure_server_paths_in(&remote_control_dir(), hostname)
}

fn ensure_server_paths_in(root: &Path, hostname: Option<&str>) -> Result<ServerPaths> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create remote-control dir {}", root.display()))?;

    let normalized_hostname = normalize_requested_hostname(hostname);
    let normalized_hostname = normalized_hostname.as_deref().unwrap_or("localhost");
    let cert_path = root.join("cert.pem");
    let key_path = root.join("key.pem");
    let cert_hostname_path = root.join("cert-hostname");
    let existing_hostname = read_token(&cert_hostname_path).unwrap_or_default();
    let hostname_changed = existing_hostname != normalized_hostname;
    if hostname_changed || !cert_path.exists() || !key_path.exists() {
        let mut names = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        if normalized_hostname != "localhost" {
            names.push(normalized_hostname.to_string());
        }
        let cert = generate_simple_self_signed(names)
            .context("generate remote-control self-signed certificate")?;
        std::fs::write(&cert_path, cert.cert.pem())
            .with_context(|| format!("write {}", cert_path.display()))?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())
            .with_context(|| format!("write {}", key_path.display()))?;
        std::fs::write(&cert_hostname_path, normalized_hostname)
            .with_context(|| format!("write {}", cert_hostname_path.display()))?;
        restrict_permissions(&key_path)?;
        restrict_permissions(&cert_hostname_path)?;
    }

    Ok(ServerPaths {
        db_path: root.join("sessions.sqlite3"),
        cert_path,
        key_path,
        token_path: root.join("token"),
    })
}

/// Load the shared bearer token, generating and persisting one on first run.
fn ensure_token(token_path: &Path) -> Result<String> {
    if let Some(existing) = read_token(token_path) {
        return Ok(existing);
    }
    let token = generate_token()?;
    std::fs::write(token_path, &token)
        .with_context(|| format!("write {}", token_path.display()))?;
    restrict_permissions(token_path)?;
    Ok(token)
}

fn read_token(token_path: &Path) -> Option<String> {
    std::fs::read_to_string(token_path)
        .ok()
        .map(|contents| contents.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow!("generate remote-control token: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn generate_viewer_code() -> Result<String> {
    const RANGE: u64 = 1_000_000;
    // Reject the unaligned tail of the u32 space so every six-digit code is
    // equally likely; a plain `% RANGE` would bias toward lower codes.
    let bound = (1u64 << 32) - ((1u64 << 32) % RANGE);
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow!("generate remote-control viewer code: {error}"))?;
        let raw = u32::from_le_bytes(bytes) as u64;
        if raw < bound {
            return Ok(format!("{:06}", raw % RANGE));
        }
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn init_db(db_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    conn.execute_batch(
        "create table if not exists sessions (
            session_id text primary key,
            name text not null,
            start_time text not null,
            last_update text not null,
            total_messages integer not null,
            project text not null,
            agent text not null,
            transcript_json text not null default '[]',
            connected integer not null default 0
        );
        create table if not exists queued_prompts (
            id integer primary key autoincrement,
            session_id text not null,
            text text not null,
            created_at text not null
        );
        create table if not exists permission_decisions (
            id integer primary key autoincrement,
            session_id text not null,
            request_id text not null,
            option_id text not null,
            created_at text not null
        );",
    )
    .context("create remote-control schema")?;
    ensure_sessions_column(&conn, "transcript_json", "text not null default '[]'")?;
    ensure_sessions_column(&conn, "connected", "integer not null default 0")?;
    ensure_sessions_column(
        &conn,
        "pending_permissions_json",
        "text not null default '[]'",
    )?;
    Ok(())
}

fn ensure_sessions_column(conn: &Connection, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("pragma table_info(sessions)")
        .context("prepare sessions schema query")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("query sessions schema")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions schema")?;
    if columns.iter().any(|existing| existing == column) {
        return Ok(());
    }

    conn.execute_batch(&format!(
        "alter table sessions add column {column} {definition}"
    ))
    .with_context(|| format!("add sessions.{column} column"))?;
    Ok(())
}

fn open_db(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("set sqlite journal mode")?;
    Ok(conn)
}

fn upsert_session_record(db_path: &Path, session: &SessionRecord) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let total_messages =
        i64::try_from(session.total_messages).context("total_messages exceeds sqlite integer")?;
    let transcript_json = serde_json::to_string(&session.transcript)
        .context("serialize remote-control transcript")?;
    let pending_permissions_json = serde_json::to_string(&session.pending_permissions)
        .context("serialize remote-control pending permissions")?;
    // The conflict arm refuses to move `last_update` backwards: every state
    // change touches the timestamp before the snapshot is taken, so a
    // delayed or replayed upload can never overwrite newer session state
    // (in particular a cleared pending permission).
    conn.execute(
        "insert into sessions (
            session_id,
            name,
            start_time,
            last_update,
            total_messages,
            project,
            agent,
            transcript_json,
            pending_permissions_json,
            connected
        ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1)
        on conflict(session_id) do update set
            name = excluded.name,
            start_time = sessions.start_time,
            last_update = excluded.last_update,
            total_messages = excluded.total_messages,
            project = excluded.project,
            agent = excluded.agent,
            transcript_json = excluded.transcript_json,
            pending_permissions_json = excluded.pending_permissions_json,
            connected = 1
        where excluded.last_update >= sessions.last_update",
        params![
            session.session_id,
            session.name,
            session.start_time,
            session.last_update,
            total_messages,
            session.project,
            session.agent,
            transcript_json,
            pending_permissions_json,
        ],
    )
    .context("upsert remote-control session")?;
    Ok(())
}

fn disconnect_session_record(db_path: &Path, session_id: &str) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "update sessions set connected = 0 where session_id = ?1",
        params![session_id],
    )
    .context("disconnect remote-control session")?;
    // A permission decision can only resolve a prompt held in the live
    // session's memory, so the session going away makes its queued
    // decisions unclaimable; drop them immediately. Queued prompts stay:
    // resuming the session re-registers the same id and claims them.
    conn.execute(
        "delete from permission_decisions where session_id = ?1",
        params![session_id],
    )
    .context("clear permission decisions on disconnect")?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PruneCounts {
    prompts: usize,
    decisions: usize,
    sessions: usize,
}

impl PruneCounts {
    fn any(&self) -> bool {
        self.prompts > 0 || self.decisions > 0 || self.sessions > 0
    }
}

/// Remove records that can never be useful again.
///
/// Three different policies on purpose:
/// - Session history: disconnected sessions whose last update is older
///   than `history_ttl` are deleted along with their queued prompts.
///   `None` keeps history forever (`--history-days 0`).
/// - Permission decisions die with their session: anything whose session
///   is not currently live (or that sat unclaimed past a generous age cap)
///   is unclaimable garbage.
/// - Queued prompts survive disconnects so `mj resume` can claim them;
///   beyond expired-session cleanup, only entries past `QUEUED_PROMPT_TTL`
///   are dropped.
fn prune_stale_records(db_path: &Path, history_ttl: Option<Duration>) -> Result<PruneCounts> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut counts = PruneCounts::default();

    if let Some(history_ttl) = history_ttl {
        let history_cutoff = rfc3339_before(history_ttl);
        counts.prompts += conn
            .execute(
                "delete from queued_prompts
                where session_id in (
                    select session_id from sessions
                    where connected = 0 and last_update < ?1
                )",
                params![history_cutoff],
            )
            .context("prune queued prompts of expired sessions")?;
        counts.sessions = conn
            .execute(
                "delete from sessions where connected = 0 and last_update < ?1",
                params![history_cutoff],
            )
            .context("prune expired session history")?;
    }

    let live_cutoff = connected_session_cutoff_rfc3339();
    let decision_cutoff = rfc3339_before(PERMISSION_DECISION_TTL);
    let prompt_cutoff = rfc3339_before(QUEUED_PROMPT_TTL);
    counts.decisions = conn
        .execute(
            "delete from permission_decisions
            where created_at < ?1
                or session_id not in (
                    select session_id from sessions
                    where connected = 1 and last_update >= ?2
                )",
            params![decision_cutoff, live_cutoff],
        )
        .context("prune stale permission decisions")?;
    counts.prompts += conn
        .execute(
            "delete from queued_prompts where created_at < ?1",
            params![prompt_cutoff],
        )
        .context("prune stale queued prompts")?;
    Ok(counts)
}

fn load_session_records(db_path: &Path) -> Result<Vec<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select
                session_id,
                name,
                start_time,
                last_update,
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count
            from sessions
            order by last_update desc, session_id asc",
        )
        .context("prepare session query")?;
    let rows = stmt
        .query_map([], session_record_from_row)
        .context("query sessions")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions")
}

fn load_connected_session_records(db_path: &Path, cutoff: &str) -> Result<Vec<SessionRecord>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select
                session_id,
                name,
                start_time,
                last_update,
                total_messages,
                project,
                agent,
                transcript_json,
                pending_permissions_json,
                (
                    select count(*)
                    from queued_prompts
                    where queued_prompts.session_id = sessions.session_id
                ) as queued_prompt_count
            from sessions
            where connected = 1 and last_update >= ?1
            order by last_update desc, session_id asc",
        )
        .context("prepare connected session query")?;
    let rows = stmt
        .query_map(params![cutoff], session_record_from_row)
        .context("query connected sessions")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect connected sessions")
}

fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let total_messages: i64 = row.get(4)?;
    let transcript_json: String = row.get(7)?;
    let pending_permissions_json: String = row.get(8)?;
    let queued_prompt_count: i64 = row.get(9)?;
    let transcript = serde_json::from_str(&transcript_json).unwrap_or_default();
    let pending_permissions = serde_json::from_str(&pending_permissions_json).unwrap_or_default();
    Ok(SessionRecord {
        session_id: row.get(0)?,
        name: row.get(1)?,
        start_time: row.get(2)?,
        last_update: row.get(3)?,
        total_messages: u64::try_from(total_messages).unwrap_or(0),
        project: row.get(5)?,
        agent: row.get(6)?,
        transcript,
        queued_prompt_count: u64::try_from(queued_prompt_count).unwrap_or(0),
        pending_permissions,
    })
}

fn load_queued_prompts(db_path: &Path, session_id: &str) -> Result<Vec<QueuedPrompt>> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "select id, session_id, text, created_at
            from queued_prompts
            where session_id = ?1
            order by id asc",
        )
        .context("prepare queued-prompt query")?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok(QueuedPrompt {
                id: row.get(0)?,
                session_id: row.get(1)?,
                text: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .context("query queued prompts")?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect queued prompts")
}

fn queue_prompt_record(db_path: &Path, session_id: &str, text: &str) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "insert into queued_prompts (session_id, text, created_at)
        values (?1, ?2, ?3)",
        params![session_id, text, now_rfc3339()],
    )
    .context("insert queued prompt")?;
    Ok(())
}

fn claim_queued_prompt_record(db_path: &Path, session_id: &str) -> Result<Option<QueuedPrompt>> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let tx = conn
        .transaction()
        .context("begin queued-prompt transaction")?;
    let prompt = {
        let mut stmt = tx
            .prepare(
                "select id, session_id, text, created_at
                from queued_prompts
                where session_id = ?1
                order by id asc
                limit 1",
            )
            .context("prepare queued-prompt claim query")?;
        stmt.query_row(params![session_id], |row| {
            Ok(QueuedPrompt {
                id: row.get(0)?,
                session_id: row.get(1)?,
                text: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .optional()
        .context("load queued prompt to claim")?
    };
    if let Some(prompt) = prompt {
        tx.execute(
            "delete from queued_prompts where id = ?1",
            params![prompt.id],
        )
        .context("delete claimed queued prompt")?;
        tx.commit().context("commit queued-prompt claim")?;
        Ok(Some(prompt))
    } else {
        tx.commit().context("commit empty queued-prompt claim")?;
        Ok(None)
    }
}

fn queue_permission_decision_record(
    db_path: &Path,
    session_id: &str,
    request_id: &str,
    option_id: &str,
) -> Result<()> {
    init_db(db_path)?;
    let conn = open_db(db_path)?;
    conn.execute(
        "insert into permission_decisions (session_id, request_id, option_id, created_at)
        values (?1, ?2, ?3, ?4)",
        params![session_id, request_id, option_id, now_rfc3339()],
    )
    .context("insert permission decision")?;
    Ok(())
}

fn claim_permission_decision_record(
    db_path: &Path,
    session_id: &str,
) -> Result<Option<PermissionDecisionRecord>> {
    init_db(db_path)?;
    let mut conn = open_db(db_path)?;
    let tx = conn
        .transaction()
        .context("begin permission-decision transaction")?;
    let decision = {
        let mut stmt = tx
            .prepare(
                "select id, session_id, request_id, option_id, created_at
                from permission_decisions
                where session_id = ?1
                order by id asc
                limit 1",
            )
            .context("prepare permission-decision claim query")?;
        stmt.query_row(params![session_id], |row| {
            Ok(PermissionDecisionRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                request_id: row.get(2)?,
                option_id: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()
        .context("load permission decision to claim")?
    };
    if let Some(decision) = decision {
        tx.execute(
            "delete from permission_decisions where id = ?1",
            params![decision.id],
        )
        .context("delete claimed permission decision")?;
        tx.commit().context("commit permission-decision claim")?;
        Ok(Some(decision))
    } else {
        tx.commit()
            .context("commit empty permission-decision claim")?;
        Ok(None)
    }
}

async fn send_snapshot(
    client: reqwest::Client,
    token: Option<Arc<String>>,
    snapshot: SessionRecord,
) -> Result<()> {
    let mut request = client.post(REMOTE_CONTROL_UPSERT_URL).json(&snapshot);
    if let Some(token) = token {
        request = request.bearer_auth(token.as_str());
    }
    request
        .send()
        .await
        .context("send remote-control update")?
        .error_for_status()
        .context("remote-control server returned an error")?;
    Ok(())
}

async fn send_disconnect(
    client: reqwest::Client,
    token: Option<Arc<String>>,
    session_id: &str,
) -> Result<()> {
    let encoded_session_id =
        url::form_urlencoded::byte_serialize(session_id.as_bytes()).collect::<String>();
    let mut request = client.delete(format!("{REMOTE_CONTROL_UPSERT_URL}/{encoded_session_id}"));
    if let Some(token) = token {
        request = request.bearer_auth(token.as_str());
    }
    request
        .send()
        .await
        .context("send remote-control disconnect")?
        .error_for_status()
        .context("remote-control disconnect returned an error")?;
    Ok(())
}

async fn claim_remote_prompt(
    client: reqwest::Client,
    token: Option<Arc<String>>,
    session_id: &str,
) -> Result<Option<QueuedPrompt>> {
    let mut request = client
        .post("https://localhost:11921/api/queued-prompts/claim")
        .json(&ClaimQueuedPromptRequest {
            session_id: session_id.to_string(),
        });
    if let Some(token) = token {
        request = request.bearer_auth(token.as_str());
    }
    let response = request
        .send()
        .await
        .context("claim remote queued prompt")?
        .error_for_status()
        .context("remote queued-prompt claim returned an error")?;
    response
        .json::<Option<QueuedPrompt>>()
        .await
        .context("decode claimed remote queued prompt")
}

async fn claim_remote_permission_decision(
    client: reqwest::Client,
    token: Option<Arc<String>>,
    session_id: &str,
) -> Result<Option<PermissionDecisionRecord>> {
    let mut request = client
        .post("https://localhost:11921/api/permission-decisions/claim")
        .json(&ClaimPermissionDecisionRequest {
            session_id: session_id.to_string(),
        });
    if let Some(token) = token {
        request = request.bearer_auth(token.as_str());
    }
    let response = request
        .send()
        .await
        .context("claim remote permission decision")?
        .error_for_status()
        .context("remote permission-decision claim returned an error")?;
    response
        .json::<Option<PermissionDecisionRecord>>()
        .await
        .context("decode claimed remote permission decision")
}

/// Stable machine-readable id for a permission option kind, used by the
/// remote viewer to style allow/reject buttons.
fn permission_option_kind_id(kind: PermissionOptionKind) -> &'static str {
    use PermissionOptionKind as K;
    match kind {
        K::AllowOnce => "allow_once",
        K::AllowAlways => "allow_always",
        K::RejectOnce => "reject_once",
        K::RejectAlways => "reject_always",
        _ => "other",
    }
}

fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => "[image]".to_string(),
        ContentBlock::Audio(_) => "[audio]".to_string(),
        ContentBlock::ResourceLink(link) => format!("[link {}]", link.uri),
        ContentBlock::Resource(_) => "[resource]".to_string(),
        _ => "[unknown content]".to_string(),
    }
}

fn format_tool_call(title: &str, content: &[ToolCallContent]) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ToolCallContent::Content(block) => parts.push(content_block_text(&block.content)),
            ToolCallContent::Diff(diff) => parts.push(format!("diff: {}", diff.path.display())),
            ToolCallContent::Terminal(terminal) => {
                parts.push(format!("terminal: {}", terminal.terminal_id))
            }
            _ => parts.push("unsupported tool content".to_string()),
        }
    }

    if parts.is_empty() {
        title.to_string()
    } else {
        format!("{}\n\n{}", title, parts.join("\n\n"))
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn connected_session_cutoff_rfc3339() -> String {
    rfc3339_before(CONNECTED_SESSION_TTL)
}

fn rfc3339_before(age: Duration) -> String {
    (OffsetDateTime::now_utc() - time::Duration::seconds(age.as_secs() as i64))
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{PermissionOption, ToolCallUpdate, ToolCallUpdateFields};
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    use crate::event::PermissionDecision;

    /// Build a `PermissionPrompt` and keep the original responder receiver
    /// so tests can assert what decision was forwarded to the runtime.
    fn permission_prompt(
        call_id: &str,
    ) -> (
        PermissionPrompt,
        tokio::sync::oneshot::Receiver<PermissionDecision>,
    ) {
        let (responder, rx) = tokio::sync::oneshot::channel();
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(call_id.to_string(), ToolCallUpdateFields::default()),
            options: vec![
                PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
            responder,
        };
        (prompt, rx)
    }

    #[test]
    fn tracker_counts_user_prompts_and_agent_replies() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::ContentChunk::new(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hi"),
                ),
            ),
        ));
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::ContentChunk::new(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" again"),
                ),
            ),
        ));

        assert_eq!(state.total_messages, 2);
    }

    #[test]
    fn tracker_records_transcript_history() {
        let mut state = TrackerState::new("proj".to_string(), "agent".to_string());
        state.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });
        state.observe_command(&UiCommand::SendPrompt {
            text: "hello".to_string(),
            images: Vec::new(),
        });
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::ContentChunk::new(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new("hi"),
                ),
            ),
        ));
        state.observe_session_update(&SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::ContentChunk::new(
                agent_client_protocol::schema::ContentBlock::Text(
                    agent_client_protocol::schema::TextContent::new(" there"),
                ),
            ),
        ));

        let snapshot = state.snapshot().expect("snapshot");
        assert_eq!(snapshot.transcript.len(), 2);
        assert_eq!(snapshot.transcript[0].kind, "user");
        assert_eq!(snapshot.transcript[0].text, "hello");
        assert!(!snapshot.transcript[0].timestamp.is_empty());
        assert_eq!(snapshot.transcript[1].kind, "agent");
        assert_eq!(snapshot.transcript[1].text, "hi there");
        assert!(!snapshot.transcript[1].timestamp.is_empty());
    }

    #[test]
    fn sqlite_upsert_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let session = SessionRecord {
            session_id: "sess-1".to_string(),
            name: "demo".to_string(),
            start_time: "2026-06-03T10:00:00Z".to_string(),
            last_update: "2026-06-03T10:00:20Z".to_string(),
            total_messages: 4,
            project: "mjolnir".to_string(),
            agent: "anvil".to_string(),
            transcript: vec![
                TranscriptEntry {
                    kind: "user".to_string(),
                    text: "hello".to_string(),
                    timestamp: "2026-06-03T10:00:05Z".to_string(),
                },
                TranscriptEntry {
                    kind: "agent".to_string(),
                    text: "hi".to_string(),
                    timestamp: "2026-06-03T10:00:06Z".to_string(),
                },
            ],
            queued_prompt_count: 0,
            pending_permissions: Vec::new(),
        };

        upsert_session_record(&db_path, &session).expect("insert");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                total_messages: 6,
                last_update: "2026-06-03T10:00:40Z".to_string(),
                transcript: vec![
                    TranscriptEntry {
                        kind: "user".to_string(),
                        text: "hello".to_string(),
                        timestamp: "2026-06-03T10:00:05Z".to_string(),
                    },
                    TranscriptEntry {
                        kind: "agent".to_string(),
                        text: "hi there".to_string(),
                        timestamp: "2026-06-03T10:00:06Z".to_string(),
                    },
                ],
                ..session.clone()
            },
        )
        .expect("update");

        let sessions = load_session_records(&db_path).expect("load");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "demo");
        assert_eq!(sessions[0].total_messages, 6);
        assert_eq!(sessions[0].start_time, "2026-06-03T10:00:00Z");
        assert_eq!(sessions[0].last_update, "2026-06-03T10:00:40Z");
        assert_eq!(sessions[0].transcript.len(), 2);
        assert_eq!(sessions[0].transcript[0].kind, "user");
        assert_eq!(sessions[0].transcript[0].text, "hello");
        assert_eq!(sessions[0].transcript[1].kind, "agent");
        assert_eq!(sessions[0].transcript[1].text, "hi there");
    }

    #[test]
    fn connected_session_listing_excludes_disconnected_and_stale_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let fresh = now_rfc3339();
        let active = SessionRecord {
            session_id: "sess-active".to_string(),
            name: "active".to_string(),
            start_time: fresh.clone(),
            last_update: fresh.clone(),
            total_messages: 1,
            project: "mjolnir".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            pending_permissions: Vec::new(),
        };
        let disconnected = SessionRecord {
            session_id: "sess-disconnected".to_string(),
            name: "disconnected".to_string(),
            ..active.clone()
        };
        let stale = SessionRecord {
            session_id: "sess-stale".to_string(),
            name: "stale".to_string(),
            start_time: "1970-01-01T00:00:00Z".to_string(),
            last_update: "1970-01-01T00:00:00Z".to_string(),
            ..active.clone()
        };

        upsert_session_record(&db_path, &active).expect("insert active");
        upsert_session_record(&db_path, &disconnected).expect("insert disconnected");
        upsert_session_record(&db_path, &stale).expect("insert stale");
        disconnect_session_record(&db_path, "sess-disconnected").expect("disconnect");

        let connected =
            load_connected_session_records(&db_path, &connected_session_cutoff_rfc3339())
                .expect("load connected");
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].session_id, "sess-active");

        let all = load_session_records(&db_path).expect("load all");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn queued_prompts_round_trip_and_claim_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_prompt_record(&db_path, "sess-1", "first").expect("queue first");
        queue_prompt_record(&db_path, "sess-1", "second").expect("queue second");
        queue_prompt_record(&db_path, "sess-2", "other").expect("queue other");

        let sess_1 = load_queued_prompts(&db_path, "sess-1").expect("load sess-1");
        assert_eq!(sess_1.len(), 2);
        assert_eq!(sess_1[0].text, "first");
        assert_eq!(sess_1[1].text, "second");

        let claimed = claim_queued_prompt_record(&db_path, "sess-1")
            .expect("claim first")
            .expect("prompt");
        assert_eq!(claimed.text, "first");

        let remaining = load_queued_prompts(&db_path, "sess-1").expect("load remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "second");

        let second = claim_queued_prompt_record(&db_path, "sess-1")
            .expect("claim second")
            .expect("prompt");
        assert_eq!(second.text, "second");
        assert!(
            claim_queued_prompt_record(&db_path, "sess-1")
                .expect("claim empty")
                .is_none()
        );

        let other = load_queued_prompts(&db_path, "sess-2").expect("load sess-2");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].text, "other");
    }

    /// Insert a queue row with an explicit `created_at`, bypassing the
    /// public helpers that always stamp "now".
    fn insert_decision_at(db_path: &Path, session_id: &str, created_at: &str) {
        let conn = open_db(db_path).expect("open db");
        conn.execute(
            "insert into permission_decisions (session_id, request_id, option_id, created_at)
            values (?1, 'call-old', 'allow', ?2)",
            params![session_id, created_at],
        )
        .expect("insert decision");
    }

    fn session_named(session_id: &str, last_update: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_string(),
            name: session_id.to_string(),
            start_time: "2026-06-10T08:00:00Z".to_string(),
            last_update: last_update.to_string(),
            total_messages: 1,
            project: "proj".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            pending_permissions: Vec::new(),
        }
    }

    #[test]
    fn prune_keeps_decisions_for_live_sessions_and_drops_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(&db_path, &session_named("sess-live", &now)).expect("live");
        upsert_session_record(&db_path, &session_named("sess-disconnected", &now))
            .expect("disconnected");
        disconnect_session_record(&db_path, "sess-disconnected").expect("disconnect");
        upsert_session_record(
            &db_path,
            &session_named("sess-stale", "1970-01-01T00:00:00Z"),
        )
        .expect("stale");

        queue_permission_decision_record(&db_path, "sess-live", "call-1", "allow")
            .expect("live decision");
        queue_permission_decision_record(&db_path, "sess-disconnected", "call-2", "allow")
            .expect("disconnected decision");
        queue_permission_decision_record(&db_path, "sess-stale", "call-3", "allow")
            .expect("stale decision");
        queue_permission_decision_record(&db_path, "sess-ghost", "call-4", "allow")
            .expect("ghost decision");
        // Even a live session's decision dies once it outlives the age cap.
        insert_decision_at(&db_path, "sess-live", "1970-01-01T00:00:00Z");

        let counts = prune_stale_records(&db_path, None).expect("prune");
        assert_eq!(counts.prompts, 0);
        assert_eq!(counts.decisions, 4);

        let kept = claim_permission_decision_record(&db_path, "sess-live")
            .expect("claim live")
            .expect("live decision kept");
        assert_eq!(kept.request_id, "call-1");
        for session in ["sess-live", "sess-disconnected", "sess-stale", "sess-ghost"] {
            assert!(
                claim_permission_decision_record(&db_path, session)
                    .expect("claim")
                    .is_none(),
                "no decisions should remain for {session}"
            );
        }
    }

    #[test]
    fn prune_drops_only_ancient_queued_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(&db_path, &session_named("sess-1", &now)).expect("session");
        disconnect_session_record(&db_path, "sess-1").expect("disconnect");

        // A prompt queued for a disconnected session must survive pruning
        // so `mj resume` can still claim it...
        queue_prompt_record(&db_path, "sess-1", "run after resume").expect("queue fresh");
        // ...but an ancient one is dead weight.
        let conn = open_db(&db_path).expect("open db");
        conn.execute(
            "insert into queued_prompts (session_id, text, created_at)
            values ('sess-1', 'forgotten', '1970-01-01T00:00:00Z')",
            [],
        )
        .expect("insert ancient prompt");
        drop(conn);

        let counts = prune_stale_records(&db_path, None).expect("prune");
        assert_eq!(counts.prompts, 1);

        let remaining = load_queued_prompts(&db_path, "sess-1").expect("load");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "run after resume");
    }

    #[test]
    fn prune_expires_disconnected_session_history_with_its_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();
        let history_ttl = Duration::from_secs(30 * 24 * 60 * 60);

        // Recent disconnected session: history kept.
        upsert_session_record(&db_path, &session_named("sess-recent", &now)).expect("recent");
        disconnect_session_record(&db_path, "sess-recent").expect("disconnect recent");
        // Ancient disconnected session: history and its prompts deleted.
        upsert_session_record(
            &db_path,
            &session_named("sess-ancient", "1970-01-01T00:00:00Z"),
        )
        .expect("ancient");
        disconnect_session_record(&db_path, "sess-ancient").expect("disconnect ancient");
        queue_prompt_record(&db_path, "sess-ancient", "never ran").expect("queue prompt");

        // With history pruning disabled nothing is touched...
        let counts = prune_stale_records(&db_path, None).expect("prune disabled");
        assert_eq!(counts.sessions, 0);
        assert_eq!(load_session_records(&db_path).expect("load all").len(), 2);

        // ...with a TTL only the expired session (and its prompts) goes.
        let counts = prune_stale_records(&db_path, Some(history_ttl)).expect("prune");
        assert_eq!(counts.sessions, 1);
        assert_eq!(counts.prompts, 1);
        let remaining = load_session_records(&db_path).expect("load remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_id, "sess-recent");
        assert!(
            load_queued_prompts(&db_path, "sess-ancient")
                .expect("load prompts")
                .is_empty()
        );
    }

    #[test]
    fn disconnect_clears_permission_decisions_but_keeps_queued_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let now = now_rfc3339();

        upsert_session_record(&db_path, &session_named("sess-1", &now)).expect("session");
        queue_permission_decision_record(&db_path, "sess-1", "call-1", "allow")
            .expect("queue decision");
        queue_prompt_record(&db_path, "sess-1", "next task").expect("queue prompt");

        disconnect_session_record(&db_path, "sess-1").expect("disconnect");

        assert!(
            claim_permission_decision_record(&db_path, "sess-1")
                .expect("claim decision")
                .is_none(),
            "disconnect must drop queued permission decisions"
        );
        let prompts = load_queued_prompts(&db_path, "sess-1").expect("load prompts");
        assert_eq!(prompts.len(), 1, "queued prompts must survive disconnect");
    }

    #[tokio::test]
    async fn intercept_publishes_pending_permission_and_clears_on_answer() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, rx) = permission_prompt("call-1");
        let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert_eq!(snapshot.pending_permissions.len(), 1);
        let pending = &snapshot.pending_permissions[0];
        assert_eq!(pending.request_id, "call-1");
        assert_eq!(pending.options.len(), 2);
        assert_eq!(pending.options[0].option_id, "allow");
        assert_eq!(pending.options[0].label, "Allow");
        assert_eq!(pending.options[0].kind, "allow_once");
        assert_eq!(pending.options[1].kind, "reject_once");

        // Answering through the wrapped responder forwards the decision to
        // the original (runtime) receiver and retracts the pending entry
        // before the forward, so the snapshot is already clean here.
        let UiEvent::PermissionRequest(wrapped) = event else {
            panic!("intercept must preserve the event kind");
        };
        wrapped
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("wrapped responder open");
        match rx.await {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("expected forwarded decision, got {other:?}"),
        }
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(snapshot.pending_permissions.is_empty());
    }

    #[test]
    fn upsert_rejects_snapshots_older_than_the_stored_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        // A "pending permission" snapshot arrives late, after the cleared
        // snapshot with a newer last_update was already stored.
        let cleared = SessionRecord {
            pending_permissions: Vec::new(),
            ..session_named("sess-1", "2026-06-10T10:00:02Z")
        };
        let stale_pending = SessionRecord {
            pending_permissions: vec![PendingPermissionRecord {
                request_id: "call-1".to_string(),
                title: "run something".to_string(),
                options: Vec::new(),
                requested_at: "2026-06-10T10:00:01Z".to_string(),
            }],
            ..session_named("sess-1", "2026-06-10T10:00:01Z")
        };

        upsert_session_record(&db_path, &cleared).expect("store newer");
        upsert_session_record(&db_path, &stale_pending).expect("late stale write");

        let loaded = load_session_records(&db_path).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_update, "2026-06-10T10:00:02Z");
        assert!(
            loaded[0].pending_permissions.is_empty(),
            "a stale snapshot must not resurrect a cleared permission"
        );

        // An equal-or-newer snapshot still updates the row.
        let newer = SessionRecord {
            total_messages: 9,
            ..session_named("sess-1", "2026-06-10T10:00:03Z")
        };
        upsert_session_record(&db_path, &newer).expect("store newest");
        let loaded = load_session_records(&db_path).expect("reload");
        assert_eq!(loaded[0].total_messages, 9);
    }

    #[tokio::test]
    async fn intercept_is_a_passthrough_without_a_ui_event_channel() {
        // Headless trackers cannot apply remote decisions, so they must not
        // advertise pending permissions: the prompt passes through with its
        // original responder and the snapshot stays clean.
        let tracker = RemoteSessionTracker {
            publish_permissions: false,
            ..RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string())
        };
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, rx) = permission_prompt("call-1");
        let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));

        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(
            snapshot.pending_permissions.is_empty(),
            "headless sessions must not publish approval UI"
        );

        // The responder is the original one: answering it resolves the
        // runtime receiver directly, with no wrapper task involved.
        let UiEvent::PermissionRequest(prompt) = event else {
            panic!("intercept must preserve the event kind");
        };
        prompt
            .responder
            .send(PermissionDecision::Selected("allow".to_string()))
            .expect("responder open");
        match rx.await {
            Ok(PermissionDecision::Selected(id)) => assert_eq!(id, "allow"),
            other => panic!("expected direct decision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn intercept_clears_pending_permission_when_prompt_is_dropped() {
        let tracker =
            RemoteSessionTracker::new_disconnected("proj".to_string(), "agent".to_string());
        tracker.observe_event(&UiEvent::SessionStarted {
            session_id: "sess-1".to_string(),
            resumed: false,
        });

        let (prompt, rx) = permission_prompt("call-1");
        let event = tracker.intercept_event(UiEvent::PermissionRequest(prompt));
        // The UI dropped the prompt without answering (e.g. cancel-all on
        // shutdown). The runtime sees the cancel and the entry is retracted.
        drop(event);
        assert!(rx.await.is_err(), "drop must propagate as a closed channel");
        let snapshot = tracker
            .state
            .lock()
            .expect("state")
            .snapshot()
            .expect("snapshot");
        assert!(snapshot.pending_permissions.is_empty());
    }

    #[test]
    fn pending_permissions_round_trip_through_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        let pending = PendingPermissionRecord {
            request_id: "call-1".to_string(),
            title: "run `cargo test`".to_string(),
            options: vec![PermissionOptionRecord {
                option_id: "allow".to_string(),
                label: "Allow".to_string(),
                kind: "allow_once".to_string(),
            }],
            requested_at: "2026-06-10T10:00:00Z".to_string(),
        };
        let session = SessionRecord {
            session_id: "sess-1".to_string(),
            name: "demo".to_string(),
            start_time: "2026-06-10T10:00:00Z".to_string(),
            last_update: "2026-06-10T10:00:20Z".to_string(),
            total_messages: 1,
            project: "mjolnir".to_string(),
            agent: "anvil".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            pending_permissions: vec![pending.clone()],
        };

        upsert_session_record(&db_path, &session).expect("insert");
        let loaded = load_session_records(&db_path).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pending_permissions, vec![pending]);

        // The next snapshot without the permission retracts it.
        upsert_session_record(
            &db_path,
            &SessionRecord {
                pending_permissions: Vec::new(),
                ..session
            },
        )
        .expect("update");
        let loaded = load_session_records(&db_path).expect("reload");
        assert!(loaded[0].pending_permissions.is_empty());
    }

    #[test]
    fn permission_decisions_queue_and_claim_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");

        queue_permission_decision_record(&db_path, "sess-1", "call-1", "allow")
            .expect("queue first");
        queue_permission_decision_record(&db_path, "sess-1", "call-2", "reject")
            .expect("queue second");
        queue_permission_decision_record(&db_path, "sess-2", "call-9", "allow")
            .expect("queue other session");

        let first = claim_permission_decision_record(&db_path, "sess-1")
            .expect("claim first")
            .expect("decision");
        assert_eq!(first.request_id, "call-1");
        assert_eq!(first.option_id, "allow");

        let second = claim_permission_decision_record(&db_path, "sess-1")
            .expect("claim second")
            .expect("decision");
        assert_eq!(second.request_id, "call-2");
        assert_eq!(second.option_id, "reject");

        assert!(
            claim_permission_decision_record(&db_path, "sess-1")
                .expect("claim empty")
                .is_none()
        );

        let other = claim_permission_decision_record(&db_path, "sess-2")
            .expect("claim other")
            .expect("decision");
        assert_eq!(other.request_id, "call-9");
    }

    #[tokio::test]
    async fn permission_decision_endpoints_enforce_token_and_validate_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let app = build_router(db_path, token.clone(), "123456".to_string());

        let decision_body = |request_id: &str, option_id: &str| {
            serde_json::to_vec(&QueuePermissionDecisionRequest {
                session_id: "sess-1".to_string(),
                request_id: request_id.to_string(),
                option_id: option_id.to_string(),
            })
            .expect("decision json")
        };

        // Without the bearer token the decision is rejected.
        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(decision_body("call-1", "allow")))
                    .expect("request"),
            )
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // Blank fields are rejected even with a valid token.
        let blank = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(decision_body("call-1", "   ")))
                    .expect("request"),
            )
            .await
            .expect("send blank option");
        assert_eq!(blank.status(), StatusCode::BAD_REQUEST);

        // A valid decision is accepted, then claimed back exactly once.
        let accepted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(decision_body("call-1", "allow")))
                    .expect("request"),
            )
            .await
            .expect("send decision");
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);

        let claim_body = serde_json::to_vec(&ClaimPermissionDecisionRequest {
            session_id: "sess-1".to_string(),
        })
        .expect("claim json");
        let claimed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body.clone()))
                    .expect("request"),
            )
            .await
            .expect("claim decision");
        assert_eq!(claimed.status(), StatusCode::OK);
        let claimed: Option<PermissionDecisionRecord> = serde_json::from_slice(
            &claimed
                .into_body()
                .collect()
                .await
                .expect("claim body")
                .to_bytes(),
        )
        .expect("claim json");
        let claimed = claimed.expect("a decision was queued");
        assert_eq!(claimed.request_id, "call-1");
        assert_eq!(claimed.option_id, "allow");

        let empty = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/permission-decisions/claim")
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(claim_body))
                    .expect("request"),
            )
            .await
            .expect("claim again");
        assert_eq!(empty.status(), StatusCode::OK);
        let empty: Option<PermissionDecisionRecord> = serde_json::from_slice(
            &empty
                .into_body()
                .collect()
                .await
                .expect("empty claim body")
                .to_bytes(),
        )
        .expect("empty claim json");
        assert!(empty.is_none());
    }

    #[test]
    fn token_matches_requires_exact_bearer() {
        assert!(token_matches("secret", Some("secret")));
        assert!(!token_matches("secret", Some("wrong")));
        assert!(!token_matches("secret", Some("secre")));
        assert!(!token_matches("secret", None));
    }

    #[test]
    fn cookie_matches_requires_exact_session_cookie() {
        assert!(cookie_matches(
            Some("foo=bar; mj_remote_session=secret; theme=dark"),
            SESSION_COOKIE_NAME,
            "secret"
        ));
        assert!(!cookie_matches(
            Some("foo=bar; mj_remote_session=wrong"),
            SESSION_COOKIE_NAME,
            "secret"
        ));
        assert!(!cookie_matches(
            Some("foo=bar; other=secret"),
            SESSION_COOKIE_NAME,
            "secret"
        ));
        assert!(!cookie_matches(None, SESSION_COOKIE_NAME, "secret"));
    }

    #[test]
    fn server_listen_config_defaults_to_localhost() {
        assert_eq!(
            server_listen_config(None).expect("config"),
            ServerListenConfig {
                bind_addr: REMOTE_CONTROL_LOCAL_ADDR.to_string(),
                viewer_host: "localhost".to_string(),
            }
        );
    }

    #[test]
    fn server_listen_config_uses_public_hostname() {
        assert_eq!(
            server_listen_config(Some("example.com")).expect("config"),
            ServerListenConfig {
                bind_addr: REMOTE_CONTROL_PUBLIC_ADDR.to_string(),
                viewer_host: "example.com".to_string(),
            }
        );
    }

    #[test]
    fn server_listen_config_treats_blank_hostname_as_localhost() {
        assert_eq!(
            server_listen_config(Some("   ")).expect("config"),
            server_listen_config(None).expect("config")
        );
    }

    #[test]
    fn normalize_requested_hostname_trims_and_drops_blank_values() {
        assert_eq!(
            normalize_requested_hostname(Some("  example.com  ")).as_deref(),
            Some("example.com")
        );
        assert_eq!(normalize_requested_hostname(Some("   ")), None);
        assert_eq!(normalize_requested_hostname(None), None);
    }

    #[test]
    fn bind_server_listener_reports_address_in_use() {
        let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy port");
        let bind_addr = occupied.local_addr().expect("listener addr").to_string();

        let err = bind_server_listener(&bind_addr).expect_err("second bind should fail");
        let message = format!("{err:#}");
        assert!(message.contains(&bind_addr), "unexpected error: {message}");
        assert!(
            message.contains("already running"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn viewer_code_is_six_digits() {
        let code = generate_viewer_code().expect("code");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|ch| ch.is_ascii_digit()));
    }

    fn test_state() -> ServerState {
        ServerState {
            db_path: Arc::new(PathBuf::from("unused.sqlite3")),
            token: Arc::new("integration-token".to_string()),
            viewer_code: Arc::new("123456".to_string()),
            sessions: Arc::new(Mutex::new(HashSet::new())),
            code_guard: Arc::new(Mutex::new(CodeAuthGuard::default())),
        }
    }

    #[test]
    fn viewer_code_locks_out_after_repeated_failures() {
        let state = test_state();

        // Each wrong code is rejected as unauthorized until the lockout trips.
        for _ in 0..MAX_VIEWER_CODE_ATTEMPTS {
            let err = create_code_session_response(&state, "000000", StatusCode::NO_CONTENT)
                .expect_err("wrong code rejected");
            assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        }

        // Once locked, further attempts are throttled — even the correct code.
        let throttled = create_code_session_response(&state, "000000", StatusCode::NO_CONTENT)
            .expect_err("locked out");
        assert_eq!(throttled.0, StatusCode::TOO_MANY_REQUESTS);
        let correct_but_locked =
            create_code_session_response(&state, "123456", StatusCode::NO_CONTENT)
                .expect_err("correct code still locked");
        assert_eq!(correct_but_locked.0, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn correct_viewer_code_resets_failure_counter() {
        let state = test_state();
        for _ in 0..(MAX_VIEWER_CODE_ATTEMPTS - 1) {
            let _ = create_code_session_response(&state, "000000", StatusCode::NO_CONTENT);
        }
        // A success before the threshold clears the counter so we never lock out.
        create_code_session_response(&state, "123456", StatusCode::NO_CONTENT).expect("unlock");
        assert_eq!(state.code_guard.lock().expect("guard").failures, 0);
    }

    #[test]
    fn issuing_and_clearing_a_session_revokes_the_cookie() {
        let state = test_state();
        let response =
            issue_session_cookie(&state, StatusCode::NO_CONTENT).expect("issue session cookie");
        let set_cookie = response
            .headers()
            .get(SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .expect("set-cookie str");
        let value = cookie_value(Some(set_cookie), SESSION_COOKIE_NAME)
            .expect("session cookie value")
            .to_string();

        // The freshly minted id is a tracked, valid session.
        assert!(state.sessions.lock().expect("sessions").contains(&value));

        // Logout removes exactly that id, so the cookie no longer authorizes.
        state.sessions.lock().expect("sessions").remove(&value);
        assert!(!state.sessions.lock().expect("sessions").contains(&value));
    }

    #[test]
    fn issued_session_ids_are_unique_per_unlock() {
        let state = test_state();
        for _ in 0..3 {
            issue_session_cookie(&state, StatusCode::NO_CONTENT).expect("issue");
        }
        assert_eq!(state.sessions.lock().expect("sessions").len(), 3);
    }

    #[test]
    fn ensure_token_persists_and_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token");

        let first = ensure_token(&token_path).expect("generate");
        assert!(!first.is_empty());
        let second = ensure_token(&token_path).expect("reload");
        assert_eq!(first, second);
    }

    #[test]
    fn remote_qr_login_url_encodes_query_token() {
        assert_eq!(
            remote_qr_login_url("localhost", "abc123"),
            "https://localhost:11921/auth/login?token=abc123"
        );
        assert_eq!(
            remote_qr_login_url("example.com", "a+b/c=="),
            "https://example.com:11921/auth/login?token=a%2Bb%2Fc%3D%3D"
        );
    }

    #[test]
    fn ensure_server_paths_reuses_stable_cert_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ensure_server_paths_in(dir.path(), Some("example.com")).expect("paths");
        assert!(paths.cert_path.ends_with("cert.pem"));
        assert!(paths.key_path.ends_with("key.pem"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cert-hostname")).expect("read hostname"),
            "example.com"
        );
    }

    #[test]
    fn ensure_server_paths_treats_blank_hostname_as_localhost() {
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_server_paths_in(dir.path(), Some("   ")).expect("paths");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cert-hostname")).expect("read hostname"),
            "localhost"
        );
    }

    #[test]
    fn render_login_qr_produces_visible_blocks() {
        let rendered = render_login_qr("https://localhost:11921/#token=test").expect("qr");
        assert!(rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'));
        assert!(rendered.contains('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_token_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token");
        ensure_token(&token_path).expect("generate");
        let mode = std::fs::metadata(&token_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // End-to-end check of the security-critical path: the ring CryptoProvider,
    // TLS served from a self-signed certificate that the client pins, and bearer
    // token enforcement on both endpoints.
    #[tokio::test]
    async fn server_enforces_token_over_pinned_tls() {
        install_crypto_provider();
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cert =
            generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
                .expect("cert");
        std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).expect("write key");

        let db_path = dir.path().join("sessions.sqlite3");
        init_db(&db_path).expect("init db");
        let token = "integration-token".to_string();
        let viewer_code = "123456".to_string();
        let app = build_router(db_path, token.clone(), viewer_code.clone());

        let _client = build_client(&cert_path).expect("pinned client");
        let base = "https://127.0.0.1:11921";
        let record_time = now_rfc3339();
        let record = SessionRecord {
            session_id: "sess-int".to_string(),
            name: "demo".to_string(),
            start_time: record_time.clone(),
            last_update: record_time,
            total_messages: 1,
            project: "proj".to_string(),
            agent: "agent".to_string(),
            transcript: Vec::new(),
            queued_prompt_count: 0,
            pending_permissions: Vec::new(),
        };

        // Without the bearer token the write is rejected.
        let unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("{base}/api/sessions"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&record).expect("record json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        // With the token the record is accepted and then listed back.
        let accepted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("{base}/api/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&record).expect("record json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("send authenticated");
        assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);

        let listed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("list request");
        assert_eq!(listed.status(), reqwest::StatusCode::OK);
        let listed: Vec<SessionRecord> = serde_json::from_slice(
            &listed
                .into_body()
                .collect()
                .await
                .expect("read body")
                .to_bytes(),
        )
        .expect("list json");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, "sess-int");

        let viewer = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/?token={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("viewer request");
        assert_eq!(viewer.status(), reqwest::StatusCode::OK);
        let viewer = String::from_utf8(
            viewer
                .into_body()
                .collect()
                .await
                .expect("viewer body")
                .to_bytes()
                .to_vec(),
        )
        .expect("viewer utf8");
        assert!(viewer.contains("Unlock Remote Sessions"));
        assert!(!viewer.contains(&token));

        let live_listed_via_query = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions?token={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list via query token");
        assert_eq!(live_listed_via_query.status(), reqwest::StatusCode::OK);
        let live_listed_via_query: Vec<SessionRecord> = serde_json::from_slice(
            &live_listed_via_query
                .into_body()
                .collect()
                .await
                .expect("live list via query token body")
                .to_bytes(),
        )
        .expect("live list via query token json");
        assert_eq!(live_listed_via_query.len(), 1);

        let bootstrap = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/auth/login?token={token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("bootstrap login request");
        assert_eq!(bootstrap.status(), reqwest::StatusCode::SEE_OTHER);
        assert_eq!(
            bootstrap
                .headers()
                .get(axum::http::header::LOCATION)
                .expect("location header"),
            "/"
        );
        let bootstrap_cookie = bootstrap
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("bootstrap set-cookie header")
            .to_str()
            .expect("bootstrap set-cookie str")
            .to_string();
        assert!(bootstrap_cookie.contains(SESSION_COOKIE_NAME));

        let viewer_sessions_unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("viewer sessions unauthenticated request");
        assert_eq!(
            viewer_sessions_unauthorized.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let auth_response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("{base}/auth/session"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&SessionAuthRequest {
                            code: viewer_code.clone(),
                        })
                        .expect("auth json"),
                    ))
                    .expect("request"),
            )
            .await
            .expect("viewer auth request");
        assert_eq!(auth_response.status(), reqwest::StatusCode::NO_CONTENT);
        let session_cookie = auth_response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("set-cookie header")
            .to_str()
            .expect("set-cookie str")
            .to_string();
        assert!(session_cookie.contains("HttpOnly"));
        assert!(session_cookie.contains("Secure"));
        assert!(session_cookie.contains("SameSite=Strict"));
        assert!(session_cookie.contains(SESSION_COOKIE_NAME));
        // Only the cookie value is needed to replay the session; keep it so the
        // logout step below can prove the same cookie is revoked server-side.
        let session_cookie_value = cookie_value(Some(&session_cookie), SESSION_COOKIE_NAME)
            .expect("session cookie value")
            .to_string();

        let live_listed_via_cookie = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(axum::http::header::COOKIE, session_cookie.clone())
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list via cookie");
        assert_eq!(live_listed_via_cookie.status(), reqwest::StatusCode::OK);
        let live_listed_via_cookie: Vec<SessionRecord> = serde_json::from_slice(
            &live_listed_via_cookie
                .into_body()
                .collect()
                .await
                .expect("live list via cookie body")
                .to_bytes(),
        )
        .expect("live list via cookie json");
        assert_eq!(live_listed_via_cookie.len(), 1);
        assert_eq!(live_listed_via_cookie[0].session_id, "sess-int");

        let logout = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("{base}/auth/session"))
                    .header(axum::http::header::COOKIE, session_cookie.clone())
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("logout request");
        assert_eq!(logout.status(), reqwest::StatusCode::NO_CONTENT);

        // The cookie is revoked server-side: replaying the very same cookie now
        // fails, so logout is not merely cosmetic client-side cookie clearing.
        let live_after_logout = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(
                        axum::http::header::COOKIE,
                        format!("{SESSION_COOKIE_NAME}={session_cookie_value}"),
                    )
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live after logout request");
        assert_eq!(
            live_after_logout.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let live_unauthorized = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live unauthenticated request");
        assert_eq!(
            live_unauthorized.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let live_listed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list request");
        assert_eq!(live_listed.status(), reqwest::StatusCode::OK);
        let live_listed: Vec<SessionRecord> = serde_json::from_slice(
            &live_listed
                .into_body()
                .collect()
                .await
                .expect("live list body")
                .to_bytes(),
        )
        .expect("live list json");
        assert_eq!(live_listed.len(), 1);
        assert_eq!(live_listed[0].session_id, "sess-int");

        let disconnected = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("{base}/api/sessions/{}", record.session_id))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("disconnect request");
        assert_eq!(disconnected.status(), reqwest::StatusCode::NO_CONTENT);

        let historical_after_disconnect = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("historical list request");
        assert_eq!(
            historical_after_disconnect.status(),
            reqwest::StatusCode::OK
        );
        let historical_after_disconnect: Vec<SessionRecord> = serde_json::from_slice(
            &historical_after_disconnect
                .into_body()
                .collect()
                .await
                .expect("historical list body")
                .to_bytes(),
        )
        .expect("historical list json");
        assert_eq!(historical_after_disconnect.len(), 1);
        assert_eq!(historical_after_disconnect[0].session_id, "sess-int");

        let live_after_disconnect = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("{base}/live/sessions"))
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("live list after disconnect request");
        assert_eq!(live_after_disconnect.status(), reqwest::StatusCode::OK);
        let live_after_disconnect: Vec<SessionRecord> = serde_json::from_slice(
            &live_after_disconnect
                .into_body()
                .collect()
                .await
                .expect("live list after disconnect body")
                .to_bytes(),
        )
        .expect("live list after disconnect json");
        assert!(live_after_disconnect.is_empty());
    }
}
