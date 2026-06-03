use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rusqlite::{Connection, OptionalExtension, params};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig as RustlsServerConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;

pub const DEFAULT_PORT: u16 = 11399;
const ADMIN_COOKIE: &str = "mj_remote_session";
const SESSION_TTL_SECONDS: i64 = 60 * 60 * 24 * 30;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub reset_login_token: bool,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    certs: Arc<CertPaths>,
}

#[derive(Clone)]
struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug)]
struct CertPaths {
    ca_cert: PathBuf,
    ca_key: PathBuf,
    server_cert: PathBuf,
    server_key: PathBuf,
}

#[derive(Debug, Clone)]
struct RemoteConnectInfo {
    client_fingerprint_sha256: Option<String>,
}

struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

pub async fn run_server(config: ServerConfig) -> Result<()> {
    let remote_dir = remote_data_dir()?;
    std::fs::create_dir_all(&remote_dir)
        .with_context(|| format!("create {}", remote_dir.display()))?;

    let db_path = remote_dir.join("remote.db");
    let store = Store::open(db_path)?;
    let new_token = store.ensure_admin_token(config.reset_login_token)?;

    let certs = Arc::new(CertPaths {
        ca_cert: remote_dir.join("ca-cert.pem"),
        ca_key: remote_dir.join("ca-key.pem"),
        server_cert: remote_dir.join("server-cert.pem"),
        server_key: remote_dir.join("server-key.pem"),
    });
    ensure_ca(&certs)?;
    ensure_server_cert(&certs)?;

    let tls_config = Arc::new(load_tls_config(&certs)?);
    let addr = SocketAddr::new(config.bind, config.port);
    let tcp = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let local_addr = tcp.local_addr().context("local address")?;

    if let Some(token) = new_token {
        println!("Remote control server initialized.");
        println!("Admin URL: https://{local_addr}");
        println!();
        println!("Initial admin login token:");
        println!("  {token}");
        println!();
    } else {
        println!("Remote control server listening at https://{local_addr}");
    }

    let state = AppState { store, certs };
    let app = router(state).into_make_service_with_connect_info::<RemoteConnectInfo>();
    let listener = TlsListener {
        inner: tcp,
        acceptor: TlsAcceptor::from(tls_config),
    };

    axum::serve(listener, app)
        .await
        .context("serve remote server")
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/machines", get(api_machines))
        .route("/api/machines/{id}/approve", post(api_approve_machine))
        .route("/api/machines/{id}/reject", post(api_reject_machine))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{id}/events", get(api_session_events))
        .route("/api/sessions/{id}/prompts", post(api_submit_prompt))
        .route("/client/enroll", post(client_enroll))
        .route("/client/enroll/{id}", get(client_enroll_status))
        .route("/client/heartbeat", post(client_heartbeat))
        .route("/client/sessions", post(client_register_session))
        .route("/client/sessions/{id}/events", post(client_push_event))
        .route(
            "/client/sessions/{id}/prompts/next",
            get(client_next_prompt),
        )
        .route(
            "/client/prompts/{id}/complete",
            post(client_prompt_complete),
        )
        .route("/client/prompts/{id}/fail", post(client_prompt_fail))
        .with_state(state)
}

impl<'a> Connected<axum::serve::IncomingStream<'a, TlsListener>> for RemoteConnectInfo {
    fn connect_info(stream: axum::serve::IncomingStream<'a, TlsListener>) -> Self {
        stream.remote_addr().clone()
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = RemoteConnectInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, peer_addr) = match self.inner.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!("remote tcp accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };
            let tls_stream = match self.acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::warn!("remote tls accept failed from {peer_addr}: {e}");
                    continue;
                }
            };
            let fingerprint = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|cert| certificate_fingerprint_sha256(cert.as_ref()));
            return (
                tls_stream,
                RemoteConnectInfo {
                    client_fingerprint_sha256: fingerprint,
                },
            );
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(RemoteConnectInfo {
            client_fingerprint_sha256: None,
        })
    }
}

impl Store {
    fn open(path: PathBuf) -> Result<Self> {
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS admin_sessions (
                id TEXT PRIMARY KEY,
                token_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS machines (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('pending', 'approved', 'rejected')),
                csr_pem TEXT NOT NULL,
                cert_pem TEXT,
                cert_fingerprint_sha256 TEXT,
                created_at INTEGER NOT NULL,
                approved_at INTEGER,
                rejected_at INTEGER,
                last_seen_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS client_sessions (
                id TEXT PRIMARY KEY,
                machine_id TEXT NOT NULL,
                cwd TEXT,
                agent_label TEXT,
                status TEXT NOT NULL CHECK(status IN ('active', 'idle', 'processing', 'closed')),
                created_at INTEGER NOT NULL,
                last_seen_at INTEGER,
                closed_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS session_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS queued_prompts (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                text TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('queued', 'delivered', 'completed', 'failed')),
                created_at INTEGER NOT NULL,
                delivered_at INTEGER,
                completed_at INTEGER,
                error TEXT
            );
            "#,
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow!("remote database lock poisoned"))
    }

    fn ensure_admin_token(&self, reset: bool) -> Result<Option<String>> {
        let existing = self.get_setting("admin_token_hash")?;
        if existing.is_some() && !reset {
            return Ok(None);
        }
        let token = format!("mj_{}", random_secret());
        self.set_setting("admin_token_hash", &hash_secret(&token))?;
        Ok(Some(token))
    }

    fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(Into::into)
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn validate_admin_token(&self, token: &str) -> Result<bool> {
        Ok(self
            .get_setting("admin_token_hash")?
            .is_some_and(|stored| stored == hash_secret(token)))
    }

    fn create_admin_session(&self) -> Result<String> {
        let session = random_secret();
        let now = now_ts();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO admin_sessions(id, token_hash, created_at, expires_at) VALUES(?1, ?2, ?3, ?4)",
            params![Uuid::new_v4().to_string(), hash_secret(&session), now, now + SESSION_TTL_SECONDS],
        )?;
        Ok(session)
    }

    fn validate_admin_session(&self, session: &str) -> Result<bool> {
        let now = now_ts();
        let conn = self.lock()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM admin_sessions WHERE token_hash = ?1 AND expires_at > ?2",
                params![hash_secret(session), now],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn delete_admin_session(&self, session: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM admin_sessions WHERE token_hash = ?1",
            [hash_secret(session)],
        )?;
        Ok(())
    }

    fn list_machines(&self) -> Result<Vec<MachineDto>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, status, cert_fingerprint_sha256, created_at, approved_at, rejected_at, last_seen_at \
             FROM machines ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MachineDto {
                id: row.get(0)?,
                name: row.get(1)?,
                status: row.get(2)?,
                cert_fingerprint_sha256: row.get(3)?,
                created_at: row.get(4)?,
                approved_at: row.get(5)?,
                rejected_at: row.get(6)?,
                last_seen_at: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn create_enrollment(&self, name: &str, csr_pem: &str) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO machines(id, name, status, csr_pem, created_at) VALUES(?1, ?2, 'pending', ?3, ?4)",
            params![id, name, csr_pem, now_ts()],
        )?;
        Ok(id)
    }

    fn enrollment_status(&self, id: &str) -> Result<Option<EnrollmentStatus>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT status, cert_pem FROM machines WHERE id = ?1",
            [id],
            |row| {
                Ok(EnrollmentStatus {
                    status: row.get(0)?,
                    cert_pem: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    fn approve_machine(&self, id: &str, cert_pem: &str, fingerprint: &str) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE machines SET status = 'approved', cert_pem = ?1, cert_fingerprint_sha256 = ?2, approved_at = ?3, rejected_at = NULL WHERE id = ?4 AND status = 'pending'",
            params![cert_pem, fingerprint, now_ts(), id],
        )?;
        Ok(changed > 0)
    }

    fn reject_machine(&self, id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE machines SET status = 'rejected', rejected_at = ?1 WHERE id = ?2 AND status != 'approved'",
            params![now_ts(), id],
        )?;
        Ok(changed > 0)
    }

    fn csr_for_machine(&self, id: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row("SELECT csr_pem FROM machines WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(Into::into)
    }

    fn approved_machine_by_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<MachineIdentity>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id FROM machines WHERE cert_fingerprint_sha256 = ?1 AND status = 'approved'",
            [fingerprint],
            |row| Ok(MachineIdentity { id: row.get(0)? }),
        )
        .optional()
        .map_err(Into::into)
    }

    fn touch_machine(&self, machine_id: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE machines SET last_seen_at = ?1 WHERE id = ?2",
            params![now_ts(), machine_id],
        )?;
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<ClientSessionDto>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, machine_id, cwd, agent_label, status, created_at, last_seen_at, closed_at \
             FROM client_sessions ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ClientSessionDto {
                id: row.get(0)?,
                machine_id: row.get(1)?,
                cwd: row.get(2)?,
                agent_label: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
                last_seen_at: row.get(6)?,
                closed_at: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn create_session(
        &self,
        machine_id: &str,
        cwd: Option<&str>,
        agent_label: Option<&str>,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_ts();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO client_sessions(id, machine_id, cwd, agent_label, status, created_at, last_seen_at) VALUES(?1, ?2, ?3, ?4, 'active', ?5, ?5)",
            params![id, machine_id, cwd, agent_label, now],
        )?;
        Ok(id)
    }

    fn session_belongs_to_machine(&self, session_id: &str, machine_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM client_sessions WHERE id = ?1 AND machine_id = ?2",
                params![session_id, machine_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn push_event(&self, session_id: &str, kind: &str, text: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO session_events(session_id, kind, text, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![session_id, kind, text, now_ts()],
        )?;
        conn.execute(
            "UPDATE client_sessions SET last_seen_at = ?1 WHERE id = ?2",
            params![now_ts(), session_id],
        )?;
        Ok(())
    }

    fn session_events(&self, session_id: &str) -> Result<Vec<SessionEventDto>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, kind, text, created_at FROM session_events WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(SessionEventDto {
                id: row.get(0)?,
                session_id: row.get(1)?,
                kind: row.get(2)?,
                text: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn queue_prompt(&self, session_id: &str, text: &str) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO queued_prompts(id, session_id, text, status, created_at) VALUES(?1, ?2, ?3, 'queued', ?4)",
            params![id, session_id, text, now_ts()],
        )?;
        drop(conn);
        self.push_event(session_id, "remote_prompt", text)?;
        Ok(id)
    }

    fn next_prompt(&self, session_id: &str) -> Result<Option<PromptDto>> {
        let conn = self.lock()?;
        let prompt: Option<PromptDto> = conn
            .query_row(
                "SELECT id, text FROM queued_prompts WHERE session_id = ?1 AND status = 'queued' ORDER BY created_at ASC LIMIT 1",
                [session_id],
                |row| Ok(PromptDto { id: row.get(0)?, text: row.get(1)? }),
            )
            .optional()?;
        if let Some(prompt) = prompt.as_ref() {
            conn.execute(
                "UPDATE queued_prompts SET status = 'delivered', delivered_at = ?1 WHERE id = ?2",
                params![now_ts(), prompt.id],
            )?;
        }
        Ok(prompt)
    }

    fn complete_prompt(&self, prompt_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE queued_prompts SET status = 'completed', completed_at = ?1 WHERE id = ?2",
            params![now_ts(), prompt_id],
        )?;
        Ok(changed > 0)
    }

    fn fail_prompt(&self, prompt_id: &str, error: &str) -> Result<bool> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE queued_prompts SET status = 'failed', error = ?1, completed_at = ?2 WHERE id = ?3",
            params![error, now_ts(), prompt_id],
        )?;
        Ok(changed > 0)
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn api_login(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Response {
    match state.store.validate_admin_token(&req.token) {
        Ok(true) => match state.store.create_admin_session() {
            Ok(session) => (
                StatusCode::OK,
                [(header::SET_COOKIE, admin_cookie(&session))],
                Json(OkResponse { ok: true }),
            )
                .into_response(),
            Err(e) => server_error(e),
        },
        Ok(false) => (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new("invalid token")),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

async fn api_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(session) = cookie_value(&headers, ADMIN_COOKIE)
        && let Err(e) = state.store.delete_admin_session(&session)
    {
        return server_error(e);
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!("{ADMIN_COOKIE}=; Max-Age=0; Path=/; HttpOnly; Secure; SameSite=Lax"),
        )],
        Json(OkResponse { ok: true }),
    )
        .into_response()
}

async fn api_machines(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(err) = require_admin(&state, &headers) {
        return err.into_response();
    }
    json_result(state.store.list_machines())
}

async fn api_approve_machine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = require_admin(&state, &headers) {
        return err.into_response();
    }
    let csr_pem = match state.store.csr_for_machine(&id) {
        Ok(Some(csr)) => csr,
        Ok(None) => return not_found("machine not found"),
        Err(e) => return server_error(e),
    };
    let signed = match sign_client_csr(&state.certs, &csr_pem) {
        Ok(signed) => signed,
        Err(e) => return server_error(e),
    };
    match state
        .store
        .approve_machine(&id, &signed.cert_pem, &signed.fingerprint_sha256)
    {
        Ok(true) => Json(signed).into_response(),
        Ok(false) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("machine is not pending")),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

async fn api_reject_machine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = require_admin(&state, &headers) {
        return err.into_response();
    }
    match state.store.reject_machine(&id) {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => not_found("machine not found or already approved"),
        Err(e) => server_error(e),
    }
}

async fn api_sessions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(err) = require_admin(&state, &headers) {
        return err.into_response();
    }
    json_result(state.store.list_sessions())
}

async fn api_session_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = require_admin(&state, &headers) {
        return err.into_response();
    }
    json_result(state.store.session_events(&id))
}

async fn api_submit_prompt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<PromptSubmitRequest>,
) -> Response {
    if let Err(err) = require_admin(&state, &headers) {
        return err.into_response();
    }
    if req.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("prompt is empty")),
        )
            .into_response();
    }
    match state.store.queue_prompt(&id, req.text.trim()) {
        Ok(prompt_id) => Json(PromptQueuedResponse { id: prompt_id }).into_response(),
        Err(e) => server_error(e),
    }
}

async fn client_enroll(State(state): State<AppState>, Json(req): Json<EnrollRequest>) -> Response {
    if req.machine_name.trim().is_empty() || req.csr_pem.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("machine_name and csr_pem are required")),
        )
            .into_response();
    }
    if let Err(e) = CertificateSigningRequestParams::from_pem(&req.csr_pem) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(format!("invalid csr: {e}"))),
        )
            .into_response();
    }
    match state
        .store
        .create_enrollment(req.machine_name.trim(), &req.csr_pem)
    {
        Ok(machine_id) => Json(EnrollResponse {
            machine_id,
            status: "pending".to_string(),
        })
        .into_response(),
        Err(e) => server_error(e),
    }
}

async fn client_enroll_status(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.enrollment_status(&id) {
        Ok(Some(status)) => Json(status).into_response(),
        Ok(None) => not_found("enrollment not found"),
        Err(e) => server_error(e),
    }
}

async fn client_heartbeat(
    State(state): State<AppState>,
    ConnectInfo(info): ConnectInfo<RemoteConnectInfo>,
) -> Response {
    let machine = match require_machine(&state, &info) {
        Ok(machine) => machine,
        Err(err) => return err.into_response(),
    };
    match state.store.touch_machine(&machine.id) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => server_error(e),
    }
}

async fn client_register_session(
    State(state): State<AppState>,
    ConnectInfo(info): ConnectInfo<RemoteConnectInfo>,
    Json(req): Json<RegisterSessionRequest>,
) -> Response {
    let machine = match require_machine(&state, &info) {
        Ok(machine) => machine,
        Err(err) => return err.into_response(),
    };
    match state
        .store
        .create_session(&machine.id, req.cwd.as_deref(), req.agent_label.as_deref())
    {
        Ok(session_id) => Json(RegisterSessionResponse { session_id }).into_response(),
        Err(e) => server_error(e),
    }
}

async fn client_push_event(
    State(state): State<AppState>,
    ConnectInfo(info): ConnectInfo<RemoteConnectInfo>,
    Path(id): Path<String>,
    Json(req): Json<PushEventRequest>,
) -> Response {
    let machine = match require_machine(&state, &info) {
        Ok(machine) => machine,
        Err(err) => return err.into_response(),
    };
    match state.store.session_belongs_to_machine(&id, &machine.id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("session does not belong to machine")),
            )
                .into_response();
        }
        Err(e) => return server_error(e),
    }
    match state.store.push_event(&id, &req.kind, &req.text) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => server_error(e),
    }
}

async fn client_next_prompt(
    State(state): State<AppState>,
    ConnectInfo(info): ConnectInfo<RemoteConnectInfo>,
    Path(id): Path<String>,
) -> Response {
    let machine = match require_machine(&state, &info) {
        Ok(machine) => machine,
        Err(err) => return err.into_response(),
    };
    match state.store.session_belongs_to_machine(&id, &machine.id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("session does not belong to machine")),
            )
                .into_response();
        }
        Err(e) => return server_error(e),
    }
    match state.store.next_prompt(&id) {
        Ok(Some(prompt)) => Json(prompt).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => server_error(e),
    }
}

async fn client_prompt_complete(
    State(state): State<AppState>,
    ConnectInfo(info): ConnectInfo<RemoteConnectInfo>,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = require_machine(&state, &info) {
        return err.into_response();
    }
    match state.store.complete_prompt(&id) {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => not_found("prompt not found"),
        Err(e) => server_error(e),
    }
}

async fn client_prompt_fail(
    State(state): State<AppState>,
    ConnectInfo(info): ConnectInfo<RemoteConnectInfo>,
    Path(id): Path<String>,
    Json(req): Json<PromptFailRequest>,
) -> Response {
    if let Err(err) = require_machine(&state, &info) {
        return err.into_response();
    }
    match state.store.fail_prompt(&id, &req.error) {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => not_found("prompt not found"),
        Err(e) => server_error(e),
    }
}

enum AuthError {
    LoginRequired,
    ClientCertificateRequired,
    ClientCertificateNotApproved,
    Internal(anyhow::Error),
}

impl AuthError {
    fn into_response(self) -> Response {
        match self {
            Self::LoginRequired => (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new("login required")),
            )
                .into_response(),
            Self::ClientCertificateRequired => (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new("client certificate required")),
            )
                .into_response(),
            Self::ClientCertificateNotApproved => (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("client certificate is not approved")),
            )
                .into_response(),
            Self::Internal(error) => server_error(error),
        }
    }
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> std::result::Result<(), AuthError> {
    let Some(session) = cookie_value(headers, ADMIN_COOKIE) else {
        return Err(AuthError::LoginRequired);
    };
    match state.store.validate_admin_session(&session) {
        Ok(true) => Ok(()),
        Ok(false) => Err(AuthError::LoginRequired),
        Err(e) => Err(AuthError::Internal(e)),
    }
}

fn require_machine(
    state: &AppState,
    info: &RemoteConnectInfo,
) -> std::result::Result<MachineIdentity, AuthError> {
    let Some(fingerprint) = info.client_fingerprint_sha256.as_deref() else {
        return Err(AuthError::ClientCertificateRequired);
    };
    match state.store.approved_machine_by_fingerprint(fingerprint) {
        Ok(Some(machine)) => Ok(machine),
        Ok(None) => Err(AuthError::ClientCertificateNotApproved),
        Err(e) => Err(AuthError::Internal(e)),
    }
}

fn ensure_ca(paths: &CertPaths) -> Result<()> {
    if paths.ca_cert.exists() && paths.ca_key.exists() {
        return Ok(());
    }
    let key = KeyPair::generate().context("generate remote CA key")?;
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let cert = params
        .self_signed(&key)
        .context("generate remote CA cert")?;
    std::fs::write(&paths.ca_cert, cert.pem())
        .with_context(|| format!("write {}", paths.ca_cert.display()))?;
    std::fs::write(&paths.ca_key, key.serialize_pem())
        .with_context(|| format!("write {}", paths.ca_key.display()))?;
    Ok(())
}

fn ensure_server_cert(paths: &CertPaths) -> Result<()> {
    if paths.server_cert.exists() && paths.server_key.exists() {
        return Ok(());
    }
    let ca_cert_pem = std::fs::read_to_string(&paths.ca_cert).context("read CA cert")?;
    let ca_key_pem = std::fs::read_to_string(&paths.ca_key).context("read CA key")?;
    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parse CA key")?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem).context("parse CA cert")?;
    let ca_cert = ca_params.self_signed(&ca_key).context("load CA cert")?;

    let server_key = KeyPair::generate().context("generate server key")?;
    let mut params =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])?;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let cert = params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .context("sign server cert")?;
    std::fs::write(&paths.server_cert, cert.pem())
        .with_context(|| format!("write {}", paths.server_cert.display()))?;
    std::fs::write(&paths.server_key, server_key.serialize_pem())
        .with_context(|| format!("write {}", paths.server_key.display()))?;
    Ok(())
}

fn load_tls_config(paths: &CertPaths) -> Result<RustlsServerConfig> {
    let ca_der = read_cert_chain(&paths.ca_cert)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("CA cert missing"))?;
    let cert_chain = read_cert_chain(&paths.server_cert)?;
    let key = read_private_key(&paths.server_key)?;

    let mut roots = RootCertStore::empty();
    roots.add(ca_der).context("add client CA root")?;
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .context("build client certificate verifier")?;

    RustlsServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(cert_chain, key)
        .context("build TLS server config")
}

fn sign_client_csr(paths: &CertPaths, csr_pem: &str) -> Result<SignedClientCert> {
    let ca_cert_pem = std::fs::read_to_string(&paths.ca_cert).context("read CA cert")?;
    let ca_key_pem = std::fs::read_to_string(&paths.ca_key).context("read CA key")?;
    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parse CA key")?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem).context("parse CA cert")?;
    let ca_cert = ca_params.self_signed(&ca_key).context("load CA cert")?;

    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).context("parse client CSR")?;
    csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    csr.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let cert = csr
        .signed_by(&ca_cert, &ca_key)
        .context("sign client CSR")?;
    let fingerprint_sha256 = certificate_fingerprint_sha256(cert.der());
    Ok(SignedClientCert {
        cert_pem: cert.pem(),
        fingerprint_sha256,
    })
}

fn read_cert_chain(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?,
    );
    rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("read certs {}", path.display()))
}

fn read_private_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?,
    );
    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("read private key {}", path.display()))?
        .ok_or_else(|| anyhow!("no private key in {}", path.display()))
}

fn remote_data_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("mj")
        .join("remote"))
}

fn certificate_fingerprint_sha256(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn admin_cookie(session: &str) -> String {
    format!(
        "{ADMIN_COOKIE}={session}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={SESSION_TTL_SECONDS}"
    )
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        let (key, value) = part.split_once('=')?;
        if key == name {
            return Some(value.to_string());
        }
    }
    None
}

fn json_result<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => server_error(e),
    }
}

fn server_error(error: impl std::fmt::Display) -> Response {
    tracing::error!("remote server error: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new("internal server error")),
    )
        .into_response()
}

fn not_found(message: impl Into<String>) -> Response {
    (StatusCode::NOT_FOUND, Json(ErrorResponse::new(message))).into_response()
}

#[derive(Deserialize)]
struct LoginRequest {
    token: String,
}

#[derive(Deserialize)]
struct EnrollRequest {
    machine_name: String,
    csr_pem: String,
}

#[derive(Deserialize)]
struct RegisterSessionRequest {
    cwd: Option<String>,
    agent_label: Option<String>,
}

#[derive(Deserialize)]
struct PushEventRequest {
    kind: String,
    text: String,
}

#[derive(Deserialize)]
struct PromptSubmitRequest {
    text: String,
}

#[derive(Deserialize)]
struct PromptFailRequest {
    error: String,
}

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

impl ErrorResponse {
    fn new(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
        }
    }
}

#[derive(Serialize)]
struct EnrollResponse {
    machine_id: String,
    status: String,
}

#[derive(Serialize)]
struct EnrollmentStatus {
    status: String,
    cert_pem: Option<String>,
}

#[derive(Serialize)]
struct SignedClientCert {
    cert_pem: String,
    fingerprint_sha256: String,
}

#[derive(Serialize)]
struct MachineDto {
    id: String,
    name: String,
    status: String,
    cert_fingerprint_sha256: Option<String>,
    created_at: i64,
    approved_at: Option<i64>,
    rejected_at: Option<i64>,
    last_seen_at: Option<i64>,
}

#[derive(Serialize)]
struct ClientSessionDto {
    id: String,
    machine_id: String,
    cwd: Option<String>,
    agent_label: Option<String>,
    status: String,
    created_at: i64,
    last_seen_at: Option<i64>,
    closed_at: Option<i64>,
}

#[derive(Serialize)]
struct SessionEventDto {
    id: i64,
    session_id: String,
    kind: String,
    text: String,
    created_at: i64,
}

#[derive(Serialize)]
struct PromptQueuedResponse {
    id: String,
}

#[derive(Serialize)]
struct RegisterSessionResponse {
    session_id: String,
}

#[derive(Serialize)]
struct PromptDto {
    id: String,
    text: String,
}

struct MachineIdentity {
    id: String,
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>mj remote</title>
<style>
:root { color-scheme: dark; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background:#101217; color:#e8ebf2; }
body { margin:0; }
header { padding:14px 18px; border-bottom:1px solid #2a2e3a; display:flex; align-items:center; justify-content:space-between; }
h1 { font-size:18px; margin:0; }
button { background:#5b7cff; color:white; border:0; border-radius:6px; padding:7px 10px; cursor:pointer; }
button.secondary { background:#303441; }
button.danger { background:#b84848; }
input, textarea { background:#171b24; color:#e8ebf2; border:1px solid #363b4a; border-radius:6px; padding:8px; }
textarea { width:100%; min-height:90px; box-sizing:border-box; }
main { display:grid; grid-template-columns: 320px 360px 1fr; gap:0; min-height:calc(100vh - 52px); }
section { border-right:1px solid #2a2e3a; padding:14px; overflow:auto; }
.card { border:1px solid #303645; border-radius:8px; padding:10px; margin:8px 0; background:#151923; }
.row { display:flex; gap:8px; align-items:center; flex-wrap:wrap; }
.muted { color:#99a1b3; font-size:12px; }
.fingerprint { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; word-break:break-all; }
.event { white-space:pre-wrap; border-left:3px solid #4e566c; padding:8px 10px; margin:8px 0; background:#151923; }
.event .kind { color:#9bb3ff; font-size:12px; text-transform:uppercase; }
#login { max-width:440px; margin:12vh auto; padding:24px; border:1px solid #303645; border-radius:10px; background:#151923; }
#app { display:none; }
.selected { outline:2px solid #5b7cff; }
</style>
</head>
<body>
<div id="login">
  <h1>mj remote login</h1>
  <p class="muted">Paste the admin login token printed by <code>mj server</code>.</p>
  <div class="row"><input id="token" type="password" placeholder="mj_..." style="flex:1"><button onclick="login()">Login</button></div>
  <p id="login-error" class="muted"></p>
</div>
<div id="app">
<header><h1>mj remote</h1><button class="secondary" onclick="logout()">Logout</button></header>
<main>
<section><h2>Machines</h2><div id="machines"></div></section>
<section><h2>Sessions</h2><div id="sessions"></div></section>
<section><h2>Thread</h2><div id="thread" class="muted">Select a session.</div><h3>Send prompt</h3><textarea id="prompt" placeholder="Prompt for selected session"></textarea><p><button onclick="sendPrompt()">Send prompt</button></p></section>
</main>
</div>
<script>
let selectedSession = null;
async function api(path, options = {}) {
  const res = await fetch(path, { credentials: 'same-origin', ...options });
  if (res.status === 401) { showLogin(); throw new Error('login required'); }
  if (!res.ok) { const body = await res.json().catch(() => ({})); throw new Error(body.error || res.statusText); }
  if (res.status === 204) return null;
  return await res.json();
}
function showLogin(){ document.getElementById('login').style.display='block'; document.getElementById('app').style.display='none'; }
function showApp(){ document.getElementById('login').style.display='none'; document.getElementById('app').style.display='block'; }
async function login(){
  const token = document.getElementById('token').value;
  try { await api('/api/login', { method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({token}) }); showApp(); refreshAll(); }
  catch(e){ document.getElementById('login-error').textContent = e.message; }
}
async function logout(){ await fetch('/api/logout', {method:'POST', credentials:'same-origin'}); showLogin(); }
async function refreshMachines(){
  const machines = await api('/api/machines'); showApp();
  document.getElementById('machines').innerHTML = machines.map(m => `<div class="card"><div><b>${esc(m.name)}</b> <span class="muted">${esc(m.status)}</span></div><div class="muted">${esc(m.id)}</div>${m.cert_fingerprint_sha256 ? `<div class="fingerprint muted">${esc(m.cert_fingerprint_sha256)}</div>` : ''}<div class="row">${m.status === 'pending' ? `<button onclick="approve('${m.id}')">Approve</button><button class="danger" onclick="rejectMachine('${m.id}')">Reject</button>` : ''}</div></div>`).join('');
}
async function refreshSessions(){
  const sessions = await api('/api/sessions'); showApp();
  document.getElementById('sessions').innerHTML = sessions.map(s => `<div class="card ${s.id===selectedSession?'selected':''}" onclick="selectSession('${s.id}')"><b>${esc(s.agent_label || '(agent)')}</b><div class="muted">${esc(s.cwd || '')}</div><div class="muted">${esc(s.status)} · ${esc(s.id)}</div></div>`).join('');
}
async function refreshThread(){
  if (!selectedSession) return;
  const events = await api(`/api/sessions/${selectedSession}/events`);
  document.getElementById('thread').innerHTML = events.map(e => `<div class="event"><div class="kind">${esc(e.kind)}</div>${esc(e.text)}</div>`).join('') || '<p class="muted">No events yet.</p>';
}
async function refreshAll(){ try { await refreshMachines(); await refreshSessions(); await refreshThread(); } catch(e) { console.log(e); } }
function selectSession(id){ selectedSession = id; refreshSessions(); refreshThread(); }
async function approve(id){ await api(`/api/machines/${id}/approve`, {method:'POST'}); refreshMachines(); }
async function rejectMachine(id){ await api(`/api/machines/${id}/reject`, {method:'POST'}); refreshMachines(); }
async function sendPrompt(){
  if (!selectedSession) return alert('Select a session first.');
  const text = document.getElementById('prompt').value;
  await api(`/api/sessions/${selectedSession}/prompts`, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({text})});
  document.getElementById('prompt').value=''; refreshThread();
}
function esc(s){ return String(s ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
refreshAll(); setInterval(refreshAll, 5000); setInterval(refreshThread, 2000);
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_secret_is_stable() {
        assert_eq!(hash_secret("abc"), hash_secret("abc"));
        assert_ne!(hash_secret("abc"), hash_secret("abcd"));
    }

    #[test]
    fn certificate_fingerprint_uses_lowercase_hex_sha256() {
        let digest = certificate_fingerprint_sha256(b"hello");
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn store_prompt_queue_delivers_once() {
        let conn = Connection::open_in_memory().expect("open");
        let store = Store {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate().expect("migrate");
        let session_id = store
            .create_session("machine", Some("/tmp"), Some("agent"))
            .expect("session");
        let prompt_id = store.queue_prompt(&session_id, "run tests").expect("queue");
        let prompt = store
            .next_prompt(&session_id)
            .expect("next")
            .expect("prompt");
        assert_eq!(prompt.id, prompt_id);
        assert_eq!(prompt.text, "run tests");
        assert!(
            store
                .next_prompt(&session_id)
                .expect("next again")
                .is_none()
        );
    }
}
