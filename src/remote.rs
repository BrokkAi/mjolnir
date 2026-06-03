//! Simple remote-control server and local session registration.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::SessionUpdate;
use anyhow::{Context, Result, anyhow};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use rcgen::generate_simple_self_signed;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::config::SelectedAgent;
use crate::event::{UiCommand, UiEvent};

const REMOTE_CONTROL_ADDR: &str = "127.0.0.1:11921";
const REMOTE_CONTROL_UPSERT_URL: &str = "https://localhost:11921/api/sessions";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
/// A `SessionRecord` is a handful of short strings; cap request bodies well
/// above that so a buggy or hostile local client cannot exhaust memory.
const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub name: String,
    pub start_time: String,
    pub last_update: String,
    pub total_messages: u64,
    pub project: String,
    pub agent: String,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionTracker {
    client: Option<reqwest::Client>,
    token: Option<Arc<String>>,
    state: Arc<Mutex<TrackerState>>,
    heartbeat: Arc<Mutex<Option<JoinHandle<()>>>>,
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
}

#[derive(Debug, Clone)]
struct ServerPaths {
    db_path: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    token_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ServerState {
    db_path: Arc<PathBuf>,
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
        }
    }

    fn observe_command(&mut self, command: &UiCommand) {
        if matches!(command, UiCommand::SendPrompt { .. }) {
            self.total_messages = self.total_messages.saturating_add(1);
            self.agent_message_open = false;
            self.touch();
        }
    }

    fn observe_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::SessionStarted { session_id, .. } => {
                let now = now_rfc3339();
                let first_start = self.session_id.is_none();
                self.session_id = Some(session_id.clone());
                if self.name.is_none() {
                    self.name = Some(session_id.clone());
                }
                if self.start_time.is_none() {
                    self.start_time = Some(now.clone());
                }
                self.last_update = Some(now);
                self.agent_message_open = false;
                first_start
            }
            UiEvent::SessionUpdate(update) => {
                self.observe_session_update(update);
                false
            }
            UiEvent::PromptDone { .. } | UiEvent::PromptFailed { .. } | UiEvent::Fatal(_) => {
                self.agent_message_open = false;
                self.touch();
                false
            }
            UiEvent::Connected { .. }
            | UiEvent::SessionConfigOptions { .. }
            | UiEvent::PermissionRequest(_)
            | UiEvent::Warning(_) => false,
        }
    }

    fn observe_session_update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(_) => {
                if !self.agent_message_open {
                    self.total_messages = self.total_messages.saturating_add(1);
                    self.agent_message_open = true;
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
        })
    }

    fn snapshot_with_heartbeat_touch(&mut self) -> Option<SessionRecord> {
        self.touch();
        self.snapshot()
    }

    fn touch(&mut self) {
        self.last_update = Some(now_rfc3339());
    }
}

impl RemoteSessionTracker {
    pub fn new(project: String, agent: String) -> Self {
        let dir = remote_control_dir();
        let token = read_token(&dir.join("token")).map(Arc::new);
        let client = build_client(&dir.join("cert.pem"));
        Self {
            client,
            token,
            state: Arc::new(Mutex::new(TrackerState::new(project, agent))),
            heartbeat: Arc::new(Mutex::new(None)),
        }
    }

    pub fn observe_command(&self, command: &UiCommand) {
        if let Ok(mut state) = self.state.lock() {
            state.observe_command(command);
        }
        self.spawn_flush();
    }

    pub fn observe_event(&self, event: &UiEvent) {
        let started = if let Ok(mut state) = self.state.lock() {
            state.observe_event(event)
        } else {
            false
        };
        if started {
            self.ensure_heartbeat();
        }
        self.spawn_flush();
    }

    pub async fn shutdown(&self) {
        let handle = self.heartbeat.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let snapshot = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.snapshot_with_heartbeat_touch());
        if let Some(snapshot) = snapshot
            && let Err(error) = send_snapshot(client, self.token.clone(), snapshot).await
        {
            debug!("final remote-control flush failed: {error:#}");
        }
    }

    fn ensure_heartbeat(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Ok(mut slot) = self.heartbeat.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let state = Arc::clone(&self.state);
        let token = self.token.clone();
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                let snapshot = state
                    .lock()
                    .ok()
                    .and_then(|mut state| state.snapshot_with_heartbeat_touch());
                let Some(snapshot) = snapshot else {
                    continue;
                };
                if let Err(error) = send_snapshot(client.clone(), token.clone(), snapshot).await {
                    debug!("remote-control heartbeat failed: {error:#}");
                }
            }
        }));
    }

    fn spawn_flush(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let snapshot = self.state.lock().ok().and_then(|state| state.snapshot());
        let Some(snapshot) = snapshot else {
            return;
        };
        let token = self.token.clone();
        tokio::spawn(async move {
            if let Err(error) = send_snapshot(client, token, snapshot).await {
                debug!("remote-control flush failed: {error:#}");
            }
        });
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

pub async fn run_server() -> Result<()> {
    install_crypto_provider();

    let paths = ensure_server_paths()?;
    init_db(&paths.db_path)?;
    let token = ensure_token(&paths.token_path)?;

    let app = build_router(paths.db_path.clone(), token);

    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert_path, &paths.key_path)
            .await
            .context("load remote-control TLS certificate")?;

    println!("Remote control listening on https://localhost:11921");
    axum_server::bind_rustls(REMOTE_CONTROL_ADDR.parse()?, tls_config)
        .serve(app.into_make_service())
        .await
        .with_context(|| {
            format!(
                "serve remote-control API on {REMOTE_CONTROL_ADDR} (is another `mj server` already running?)"
            )
        })
}

/// Install the ring CryptoProvider so we do not depend on aws-lc-rs (which needs
/// cmake + a C toolchain). reqwest and rcgen already pull ring in. Idempotent:
/// a second call is a no-op.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn build_router(db_path: PathBuf, token: String) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route("/api/sessions", post(upsert_session))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ServerState {
            db_path: Arc::new(db_path),
        })
}

/// Reject any request that does not carry the expected `Authorization: Bearer`
/// token. The loopback interface is reachable by every local user, so without
/// this any local process could read or overwrite the session registry.
async fn require_token(
    State(expected): State<Arc<String>>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, (StatusCode, String)> {
    let provided = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if token_matches(expected.as_str(), provided) {
        Ok(next.run(request).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))
    }
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

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn remote_control_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("remote-control")
}

fn ensure_server_paths() -> Result<ServerPaths> {
    let root = remote_control_dir();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create remote-control dir {}", root.display()))?;

    let cert_path = root.join("cert.pem");
    let key_path = root.join("key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let cert = generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ])
        .context("generate localhost self-signed certificate")?;
        std::fs::write(&cert_path, cert.cert.pem())
            .with_context(|| format!("write {}", cert_path.display()))?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())
            .with_context(|| format!("write {}", key_path.display()))?;
        restrict_permissions(&key_path)?;
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
            agent text not null
        );",
    )
    .context("create remote-control schema")?;
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
    conn.execute(
        "insert into sessions (
            session_id,
            name,
            start_time,
            last_update,
            total_messages,
            project,
            agent
        ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        on conflict(session_id) do update set
            name = excluded.name,
            start_time = sessions.start_time,
            last_update = excluded.last_update,
            total_messages = excluded.total_messages,
            project = excluded.project,
            agent = excluded.agent",
        params![
            session.session_id,
            session.name,
            session.start_time,
            session.last_update,
            total_messages,
            session.project,
            session.agent,
        ],
    )
    .context("upsert remote-control session")?;
    Ok(())
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
                agent
            from sessions
            order by last_update desc, session_id asc",
        )
        .context("prepare session query")?;
    let rows = stmt
        .query_map([], |row| {
            let total_messages: i64 = row.get(4)?;
            Ok(SessionRecord {
                session_id: row.get(0)?,
                name: row.get(1)?,
                start_time: row.get(2)?,
                last_update: row.get(3)?,
                total_messages: u64::try_from(total_messages).unwrap_or(0),
                project: row.get(5)?,
                agent: row.get(6)?,
            })
        })
        .context("query sessions")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("collect sessions")
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

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };

        upsert_session_record(&db_path, &session).expect("insert");
        upsert_session_record(
            &db_path,
            &SessionRecord {
                total_messages: 6,
                last_update: "2026-06-03T10:00:40Z".to_string(),
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
    }

    #[test]
    fn token_matches_requires_exact_bearer() {
        assert!(token_matches("secret", Some("secret")));
        assert!(!token_matches("secret", Some("wrong")));
        assert!(!token_matches("secret", Some("secre")));
        assert!(!token_matches("secret", None));
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
        let app = build_router(db_path, token.clone());

        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
            .await
            .expect("tls config");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .serve(app.into_make_service())
                .await
        });

        let client = build_client(&cert_path).expect("pinned client");
        let base = format!("https://127.0.0.1:{port}");
        let record = SessionRecord {
            session_id: "sess-int".to_string(),
            name: "demo".to_string(),
            start_time: "2026-06-03T10:00:00Z".to_string(),
            last_update: "2026-06-03T10:00:00Z".to_string(),
            total_messages: 1,
            project: "proj".to_string(),
            agent: "agent".to_string(),
        };

        // Without the bearer token the write is rejected.
        let unauthorized = client
            .post(format!("{base}/api/sessions"))
            .json(&record)
            .send()
            .await
            .expect("send unauthenticated");
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        // With the token the record is accepted and then listed back.
        let accepted = client
            .post(format!("{base}/api/sessions"))
            .bearer_auth(&token)
            .json(&record)
            .send()
            .await
            .expect("send authenticated");
        assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);

        let listed: Vec<SessionRecord> = client
            .get(format!("{base}/sessions"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("list request")
            .json()
            .await
            .expect("list json");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, "sess-int");

        server.abort();
    }
}
