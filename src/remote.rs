//! Lightweight remote-control server and client for managing mj fleets.
//!
//! The server is intentionally small: clients connect outbound over
//! self-signed mutual TLS, use a shared token for bootstrap/admin auth,
//! heartbeat, poll for work, and report results. This is the foundation for
//! hosted fleet management without coupling the core TUI to a specific cloud
//! service.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::headless::PermissionMode;
use crate::version::MJOLNIR_VERSION;

const MAX_HTTP_BODY: usize = 1024 * 1024;
const REMOTE_PAIRING_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 15;
const REMOTE_JOB_TIMEOUT_SECS: u64 = 30 * 60;

pub struct ClientConfig {
    pub pairing_uri: Option<String>,
    pub join_code: Option<String>,
    pub ca_sha256: Option<String>,
    pub server: Option<String>,
    pub token: Option<String>,
    pub ca_cert: Option<PathBuf>,
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub poll_interval: Duration,
    pub discovery_addr: SocketAddr,
    pub discovery_timeout: Duration,
}

pub struct SubmitPromptConfig {
    pub server: String,
    pub token: String,
    pub ca_cert: Option<PathBuf>,
    pub client_id: String,
    pub prompt: String,
    pub permission_mode: PermissionMode,
    pub wait: bool,
}

#[derive(Debug, Clone)]
struct PairingConfig {
    server: String,
    token: String,
    ca_cert_pem: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PairingSeed {
    server: Option<String>,
    token: Option<String>,
    join_code: Option<String>,
    ca_sha256: Option<String>,
}

struct ResolvePairingInput<'a> {
    pairing_uri: Option<&'a str>,
    join_code: Option<String>,
    ca_sha256: Option<String>,
    server: Option<String>,
    token: Option<String>,
    ca_cert: Option<&'a std::path::Path>,
    discovery_addr: SocketAddr,
    discovery_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClientRecord {
    id: String,
    name: String,
    cwd: String,
    version: String,
    status: ClientStatus,
    last_seen_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_cert_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientStatus {
    Idle,
    Running,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobRecord {
    id: String,
    client_id: String,
    prompt: String,
    permission_mode: String,
    status: JobStatus,
    created_epoch: u64,
    updated_epoch: u64,
    result: Option<JobResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobResult {
    ok: bool,
    output: Option<serde_json::Value>,
    stdout: String,
    stderr: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RemoteServerState {
    clients: HashMap<String, ClientRecord>,
    jobs: HashMap<String, JobRecord>,
    queue: VecDeque<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    name: String,
    cwd: String,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterResponse {
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca_cert_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_cert_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_pem: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HeartbeatRequest {
    client_id: String,
    status: ClientStatusWire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientStatusWire {
    Idle,
    Running,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClientsResponse {
    clients: Vec<ClientRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateJobRequest {
    client_id: String,
    prompt: String,
    permission_mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CreateJobResponse {
    job_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PollJobResponse {
    job: Option<JobAssignment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobAssignment {
    id: String,
    prompt: String,
    permission_mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JobResultRequest {
    client_id: String,
    job_id: String,
    result: JobResult,
}

#[derive(Debug, Serialize, Deserialize)]
struct JobStatusResponse {
    id: String,
    client_id: String,
    status: JobStatus,
    result: Option<JobResult>,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    client_cert_sha256: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveryRequest {
    protocol: String,
    join_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveryResponse {
    protocol: String,
    join_code: String,
    server: String,
    ca_cert_pem: String,
    ca_sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedRemoteConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client: Option<SavedRemoteClient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedRemoteClient {
    server: String,
    token: String,
    client_id: String,
    ca_cert_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
    name: String,
    cwd: String,
    paired_at_epoch: u64,
    expires_at_epoch: u64,
}

impl SavedRemoteConfig {
    fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
    }

    fn from_pairing(
        pairing: PairingConfig,
        client_id: String,
        client_cert_pem: String,
        client_key_pem: String,
        name: String,
        cwd: String,
    ) -> Result<Self> {
        let paired_at_epoch = now_epoch();
        let ca_cert_pem = pairing
            .ca_cert_pem
            .ok_or_else(|| anyhow!("remote server did not provide a CA certificate"))?;
        Ok(Self {
            client: Some(SavedRemoteClient {
                server: pairing.server,
                token: pairing.token,
                client_id,
                ca_cert_pem,
                client_cert_pem,
                client_key_pem,
                name,
                cwd,
                paired_at_epoch,
                expires_at_epoch: paired_at_epoch + REMOTE_PAIRING_TTL_SECS,
            }),
        })
    }

    fn save(&self, path: &std::path::Path) -> Result<()> {
        write_toml_private(path, self)
    }

    fn active_client(&self) -> Result<SavedRemoteClient> {
        let client = self.client.clone().ok_or_else(|| {
            anyhow!(
                "no remote client pairing found; run `mj remote join --code <code>` once to pair this node"
            )
        })?;
        if now_epoch() >= client.expires_at_epoch {
            bail!(
                "remote client pairing expired; run `mj remote join --code <code>` again to refresh it"
            );
        }
        Ok(client)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedServerState {
    token: String,
    join_code: String,
    tls: TlsMaterial,
    #[serde(default)]
    state: RemoteServerState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TlsMaterial {
    ca_cert_pem: String,
    ca_key_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
}

struct GeneratedClientCertificate {
    cert_pem: String,
    key_pem: String,
}

struct RemoteServerStore {
    path: PathBuf,
    token: String,
    join_code: String,
    tls: TlsMaterial,
    state: RemoteServerState,
}

impl RemoteServerStore {
    fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            let tls = generate_tls_material()?;
            return Ok(Self {
                path: path.to_path_buf(),
                token: random_token(24),
                join_code: random_join_code(),
                tls,
                state: RemoteServerState::default(),
            });
        }
        let body =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let persisted: PersistedServerState =
            toml::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            token: persisted.token,
            join_code: persisted.join_code,
            tls: persisted.tls,
            state: persisted.state,
        })
    }

    fn save(&self) -> Result<()> {
        write_toml_private(
            &self.path,
            &PersistedServerState {
                token: self.token.clone(),
                join_code: self.join_code.clone(),
                tls: self.tls.clone(),
                state: self.state.clone(),
            },
        )
    }

    fn register(
        &mut self,
        body: RegisterRequest,
        client_cert_sha256: Option<String>,
    ) -> Result<RegisterResponse> {
        let response = self.state.register(body, client_cert_sha256, &self.tls)?;
        self.save()?;
        Ok(response)
    }

    fn heartbeat(
        &mut self,
        body: HeartbeatRequest,
        client_cert_sha256: Option<String>,
    ) -> Result<()> {
        self.state
            .verify_client_cert(&body.client_id, client_cert_sha256.as_deref())?;
        self.state.heartbeat(body)?;
        self.save()
    }

    fn clients_response(&mut self) -> ClientsResponse {
        self.state.clients_response()
    }

    fn create_job(&mut self, body: CreateJobRequest) -> Result<CreateJobResponse> {
        let response = self.state.create_job(body)?;
        self.save()?;
        Ok(response)
    }

    fn poll_job(
        &mut self,
        client_id: &str,
        client_cert_sha256: Option<String>,
    ) -> Result<PollJobResponse> {
        self.state
            .verify_client_cert(client_id, client_cert_sha256.as_deref())?;
        let response = self.state.poll_job(client_id)?;
        self.save()?;
        Ok(response)
    }

    fn complete_job(
        &mut self,
        body: JobResultRequest,
        client_cert_sha256: Option<String>,
    ) -> Result<()> {
        self.state
            .verify_client_cert(&body.client_id, client_cert_sha256.as_deref())?;
        self.state.complete_job(body)?;
        self.save()
    }

    fn job_status(&mut self, job_id: &str) -> Result<JobStatusResponse> {
        if self.state.expire_stale_running_jobs(now_epoch()) {
            self.save()?;
        }
        self.state.job_status(job_id)
    }
}

pub async fn run_server(
    bind: SocketAddr,
    token: Option<String>,
    join_code: Option<String>,
    discovery_bind: SocketAddr,
    state_file: Option<PathBuf>,
) -> Result<()> {
    let state_path = state_file.unwrap_or_else(default_remote_server_state_path);
    let mut store = RemoteServerStore::load(&state_path)
        .with_context(|| format!("load {}", state_path.display()))?;
    if let Some(token) = token {
        store.token = token;
    }
    if let Some(join_code) = join_code {
        store.join_code = join_code;
    }
    store
        .save()
        .with_context(|| format!("save {}", state_path.display()))?;
    let token = store.token.clone();
    let join_code = store.join_code.clone();
    let ca_cert_pem = store.tls.ca_cert_pem.clone();
    let tls_acceptor = Arc::new(tls_acceptor(&store.tls)?);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind remote server on {bind}"))?;
    let local_addr = listener.local_addr().context("remote server local addr")?;
    let pairing_uri = pairing_uri_for_code(&join_code, &token, &ca_cert_pem)?;
    let state = Arc::new(Mutex::new(store));

    println!("mj remote server listening on https://{local_addr}");
    println!("server state: {}", state_path.display());
    println!("tls: self-signed mutual TLS enabled");
    println!("join code: {join_code}");
    println!("pairing uri: {pairing_uri}");
    println!("QR payload: {pairing_uri}");
    println!("manual fallback token: {token}");
    println!("CA SHA-256: {}", ca_sha256_from_pem(&ca_cert_pem)?);

    tokio::spawn(run_discovery_responder(
        discovery_bind,
        local_addr,
        join_code,
        ca_cert_pem,
    ));

    loop {
        let (stream, _) = listener.accept().await.context("accept remote client")?;
        let state = Arc::clone(&state);
        let tls_acceptor = Arc::clone(&tls_acceptor);
        tokio::spawn(async move {
            if let Err(e) = handle_tls_connection(stream, state, tls_acceptor).await {
                tracing::warn!("remote request failed: {e:#}");
            }
        });
    }
}

pub async fn run_client(cfg: ClientConfig) -> Result<()> {
    let name = cfg.name.unwrap_or_else(default_client_name);
    let cwd = cfg.cwd.canonicalize().unwrap_or(cfg.cwd);
    let explicit_pairing = cfg.pairing_uri.is_some()
        || cfg.join_code.is_some()
        || cfg.server.is_some()
        || cfg.token.is_some();
    let saved_config_path = default_remote_config_path();
    let saved_config = SavedRemoteConfig::load(&saved_config_path)
        .with_context(|| format!("load {}", saved_config_path.display()))?;
    let saved_client = if explicit_pairing {
        None
    } else {
        Some(saved_config.active_client()?)
    };
    let pairing = if let Some(saved) = saved_client.as_ref() {
        PairingConfig {
            server: saved.server.clone(),
            token: saved.token.clone(),
            ca_cert_pem: Some(saved.ca_cert_pem.clone()),
        }
    } else {
        resolve_pairing(ResolvePairingInput {
            pairing_uri: cfg.pairing_uri.as_deref(),
            join_code: cfg.join_code,
            ca_sha256: cfg.ca_sha256,
            server: cfg.server,
            token: cfg.token,
            ca_cert: cfg.ca_cert.as_deref(),
            discovery_addr: cfg.discovery_addr,
            discovery_timeout: cfg.discovery_timeout,
        })
        .await?
    };
    let http = reqwest_client(
        &pairing,
        saved_client.as_ref().map(|client| {
            (
                client.client_cert_pem.as_str(),
                client.client_key_pem.as_str(),
            )
        }),
    )?;
    let register = RegisterRequest {
        client_id: saved_client.as_ref().map(|client| client.client_id.clone()),
        name: name.clone(),
        cwd: cwd.display().to_string(),
        version: MJOLNIR_VERSION.to_string(),
    };
    let registered: RegisterResponse =
        post_json(&http, &pairing, "/api/register", &register).await?;

    println!(
        "registered remote client {} with {}",
        registered.client_id, pairing.server
    );
    let client_cert_pem = registered
        .client_cert_pem
        .or_else(|| {
            saved_client
                .as_ref()
                .map(|client| client.client_cert_pem.clone())
        })
        .ok_or_else(|| anyhow!("remote server did not issue a client certificate"))?;
    let client_key_pem = registered
        .client_key_pem
        .or_else(|| {
            saved_client
                .as_ref()
                .map(|client| client.client_key_pem.clone())
        })
        .ok_or_else(|| anyhow!("remote server did not issue a client private key"))?;
    let saved = SavedRemoteConfig::from_pairing(
        pairing.clone(),
        registered.client_id.clone(),
        client_cert_pem,
        client_key_pem,
        name,
        cwd.display().to_string(),
    )?;
    saved
        .save(&saved_config_path)
        .with_context(|| format!("save {}", saved_config_path.display()))?;
    let saved_client = saved
        .client
        .as_ref()
        .ok_or_else(|| anyhow!("saved remote client missing after pairing"))?;
    let http = reqwest_client(
        &pairing,
        Some((
            saved_client.client_cert_pem.as_str(),
            saved_client.client_key_pem.as_str(),
        )),
    )?;

    loop {
        send_heartbeat(
            &http,
            &pairing,
            &registered.client_id,
            ClientStatusWire::Idle,
        )
        .await?;
        let poll: PollJobResponse = get_json(
            &http,
            &pairing,
            &format!(
                "/api/jobs/poll?client_id={}",
                percent_encode(&registered.client_id)
            ),
        )
        .await?;

        if let Some(job) = poll.job {
            send_heartbeat(
                &http,
                &pairing,
                &registered.client_id,
                ClientStatusWire::Running,
            )
            .await?;
            println!("running remote job {}", job.id);
            let result = execute_job(&cwd, &job).await;
            post_json::<_, serde_json::Value>(
                &http,
                &pairing,
                "/api/jobs/result",
                &JobResultRequest {
                    client_id: registered.client_id.clone(),
                    job_id: job.id,
                    result,
                },
            )
            .await?;
        }

        tokio::time::sleep(cfg.poll_interval.max(Duration::from_secs(1))).await;
    }
}

pub async fn list_clients(
    server: &str,
    token: &str,
    ca_cert: Option<&std::path::Path>,
) -> Result<()> {
    let pairing = PairingConfig {
        server: normalize_server_url(server),
        token: token.to_string(),
        ca_cert_pem: load_ca_for_server(server, ca_cert)?,
    };
    let http = reqwest_client(&pairing, None)?;
    let response: ClientsResponse = get_json(&http, &pairing, "/api/clients").await?;
    if response.clients.is_empty() {
        println!("no remote clients registered");
        return Ok(());
    }
    for client in response.clients {
        println!(
            "{}  {:?}  {}  {}  {}",
            client.id, client.status, client.name, client.cwd, client.version
        );
    }
    Ok(())
}

pub async fn submit_prompt(cfg: SubmitPromptConfig) -> Result<()> {
    if cfg.prompt.trim().is_empty() {
        bail!("empty prompt");
    }

    let pairing = PairingConfig {
        server: normalize_server_url(&cfg.server),
        token: cfg.token,
        ca_cert_pem: load_ca_for_server(&cfg.server, cfg.ca_cert.as_deref())?,
    };
    let http = reqwest_client(&pairing, None)?;
    let created: CreateJobResponse = post_json(
        &http,
        &pairing,
        "/api/jobs",
        &CreateJobRequest {
            client_id: cfg.client_id,
            prompt: cfg.prompt,
            permission_mode: permission_mode_label(cfg.permission_mode).to_string(),
        },
    )
    .await?;
    println!("queued remote job {}", created.job_id);

    if !cfg.wait {
        return Ok(());
    }

    loop {
        let status: JobStatusResponse = get_json(
            &http,
            &pairing,
            &format!(
                "/api/jobs/status?job_id={}",
                percent_encode(&created.job_id)
            ),
        )
        .await?;
        match status.status {
            JobStatus::Queued | JobStatus::Running => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            JobStatus::Completed | JobStatus::Failed => {
                print_job_result(status.result.as_ref());
                return if matches!(status.status, JobStatus::Completed) {
                    Ok(())
                } else {
                    Err(anyhow!("remote job failed"))
                };
            }
        }
    }
}

async fn run_discovery_responder(
    bind: SocketAddr,
    http_addr: SocketAddr,
    join_code: String,
    ca_cert_pem: String,
) {
    if let Err(e) = run_discovery_responder_inner(bind, http_addr, join_code, ca_cert_pem).await {
        tracing::warn!("remote discovery responder failed: {e:#}");
    }
}

async fn run_discovery_responder_inner(
    bind: SocketAddr,
    http_addr: SocketAddr,
    join_code: String,
    ca_cert_pem: String,
) -> Result<()> {
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("bind remote discovery on {bind}"))?;
    println!("remote discovery listening on udp://{bind}");

    let mut buf = [0_u8; 2048];
    loop {
        let (len, peer) = socket.recv_from(&mut buf).await.context("read discovery")?;
        let Ok(request) = serde_json::from_slice::<DiscoveryRequest>(&buf[..len]) else {
            continue;
        };
        if request.protocol != "mj-remote-discovery-v1" || request.join_code != join_code {
            continue;
        }
        let server = server_base_url_for_peer(http_addr, peer);
        let response = serde_json::to_vec(&DiscoveryResponse {
            protocol: "mj-remote-discovery-v1".to_string(),
            join_code: join_code.clone(),
            server,
            ca_cert_pem: ca_cert_pem.clone(),
            ca_sha256: ca_sha256_from_pem(&ca_cert_pem).context("hash remote CA certificate")?,
        })
        .context("serialize discovery response")?;
        socket
            .send_to(&response, peer)
            .await
            .context("send discovery response")?;
    }
}

async fn handle_tls_connection(
    stream: TcpStream,
    state: Arc<Mutex<RemoteServerStore>>,
    tls_acceptor: Arc<TlsAcceptor>,
) -> Result<()> {
    let stream = tls_acceptor.accept(stream).await.context("tls handshake")?;
    let client_cert_sha256 = peer_cert_sha256(&stream);
    handle_connection_stream(stream, state, client_cert_sha256).await
}

async fn handle_connection_stream<S>(
    mut stream: S,
    state: Arc<Mutex<RemoteServerStore>>,
    client_cert_sha256: Option<String>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = tokio::time::timeout(
        Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS),
        read_http_request(&mut stream, client_cert_sha256),
    )
    .await
    .context("timed out reading remote request")??;
    let response = match route_request(request, state) {
        Ok(response) => response,
        Err(e) => json_response(
            500,
            &serde_json::json!({
                "error": e.to_string(),
            }),
        )?,
    };
    stream
        .write_all(&response)
        .await
        .context("write response")?;
    Ok(())
}

fn route_request(request: HttpRequest, state: Arc<Mutex<RemoteServerStore>>) -> Result<Vec<u8>> {
    if request.method == "GET" && request.path == "/health" {
        return json_response(200, &serde_json::json!({"ok": true}));
    }

    let mut store = state.lock().expect("remote state");

    if !authorized(&request, &store.token) {
        return json_response(401, &serde_json::json!({"error": "unauthorized"}));
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/api/register") => {
            let body: RegisterRequest = serde_json::from_slice(&request.body)?;
            let response = store.register(body, request.client_cert_sha256)?;
            json_response(200, &response)
        }
        ("POST", "/api/heartbeat") => {
            let body: HeartbeatRequest = serde_json::from_slice(&request.body)?;
            store.heartbeat(body, request.client_cert_sha256)?;
            json_response(200, &serde_json::json!({"ok": true}))
        }
        ("GET", "/api/clients") => {
            let response = store.clients_response();
            json_response(200, &response)
        }
        ("POST", "/api/jobs") => {
            let body: CreateJobRequest = serde_json::from_slice(&request.body)?;
            let response = store.create_job(body)?;
            json_response(200, &response)
        }
        ("GET", "/api/jobs/poll") => {
            let client_id = query_param(&request, "client_id")?.to_string();
            let response = store.poll_job(&client_id, request.client_cert_sha256)?;
            json_response(200, &response)
        }
        ("POST", "/api/jobs/result") => {
            let body: JobResultRequest = serde_json::from_slice(&request.body)?;
            store.complete_job(body, request.client_cert_sha256)?;
            json_response(200, &serde_json::json!({"ok": true}))
        }
        ("GET", "/api/jobs/status") => {
            let job_id = query_param(&request, "job_id")?;
            let response = store.job_status(job_id)?;
            json_response(200, &response)
        }
        _ => json_response(404, &serde_json::json!({"error": "not found"})),
    }
}

impl RemoteServerState {
    fn register(
        &mut self,
        body: RegisterRequest,
        client_cert_sha256: Option<String>,
        tls: &TlsMaterial,
    ) -> Result<RegisterResponse> {
        let requested_client_id = body.client_id.filter(|id| !id.trim().is_empty());
        if let Some(client_id) = requested_client_id.as_deref()
            && let Some(existing) = self.clients.get(client_id)
            && let Some(expected) = existing.client_cert_sha256.as_deref()
        {
            let Some(actual) = client_cert_sha256.as_deref() else {
                bail!("client {client_id} must present its saved client certificate");
            };
            if actual != expected {
                bail!("client certificate does not match client {client_id}");
            }
        }
        let client_id = requested_client_id.unwrap_or_else(|| unique_id("client", &body.name));
        let issue_cert = client_cert_sha256.is_none()
            || self
                .clients
                .get(&client_id)
                .and_then(|client| client.client_cert_sha256.as_ref())
                .is_none();
        let issued = if issue_cert {
            Some(generate_client_certificate(tls, &client_id)?)
        } else {
            None
        };
        let client_cert_sha256 = client_cert_sha256.or_else(|| {
            issued
                .as_ref()
                .map(|cert| cert_sha256_from_pem(&cert.cert_pem).expect("generated cert parses"))
        });
        self.clients.insert(
            client_id.clone(),
            ClientRecord {
                id: client_id.clone(),
                name: body.name,
                cwd: body.cwd,
                version: body.version,
                status: ClientStatus::Idle,
                last_seen_epoch: now_epoch(),
                client_cert_sha256,
            },
        );
        Ok(RegisterResponse {
            client_id,
            ca_cert_pem: Some(tls.ca_cert_pem.clone()),
            client_cert_pem: issued.as_ref().map(|cert| cert.cert_pem.clone()),
            client_key_pem: issued.map(|cert| cert.key_pem),
        })
    }

    fn verify_client_cert(&self, client_id: &str, actual: Option<&str>) -> Result<()> {
        let client = self
            .clients
            .get(client_id)
            .ok_or_else(|| anyhow!("unknown client {client_id}"))?;
        let Some(expected) = client.client_cert_sha256.as_deref() else {
            return Ok(());
        };
        let Some(actual) = actual else {
            bail!("client {client_id} must present its client certificate");
        };
        if actual != expected {
            bail!("client certificate does not match client {client_id}");
        }
        Ok(())
    }

    fn heartbeat(&mut self, body: HeartbeatRequest) -> Result<()> {
        let client = self
            .clients
            .get_mut(&body.client_id)
            .ok_or_else(|| anyhow!("unknown client {}", body.client_id))?;
        client.status = match body.status {
            ClientStatusWire::Idle => ClientStatus::Idle,
            ClientStatusWire::Running => ClientStatus::Running,
        };
        client.last_seen_epoch = now_epoch();
        Ok(())
    }

    fn clients_response(&mut self) -> ClientsResponse {
        let now = now_epoch();
        let mut clients: Vec<_> = self
            .clients
            .values()
            .cloned()
            .map(|mut client| {
                if now.saturating_sub(client.last_seen_epoch) > 30 {
                    client.status = ClientStatus::Offline;
                }
                client
            })
            .collect();
        clients.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        ClientsResponse { clients }
    }

    fn create_job(&mut self, body: CreateJobRequest) -> Result<CreateJobResponse> {
        if !self.clients.contains_key(&body.client_id) {
            bail!("unknown client {}", body.client_id);
        }
        if body.prompt.trim().is_empty() {
            bail!("empty prompt");
        }
        let job_id = unique_id("job", &body.client_id);
        let now = now_epoch();
        self.jobs.insert(
            job_id.clone(),
            JobRecord {
                id: job_id.clone(),
                client_id: body.client_id,
                prompt: body.prompt,
                permission_mode: body.permission_mode,
                status: JobStatus::Queued,
                created_epoch: now,
                updated_epoch: now,
                result: None,
            },
        );
        self.queue.push_back(job_id.clone());
        Ok(CreateJobResponse { job_id })
    }

    fn poll_job(&mut self, client_id: &str) -> Result<PollJobResponse> {
        if !self.clients.contains_key(client_id) {
            bail!("unknown client {client_id}");
        }
        let Some(index) = self.queue.iter().position(|job_id| {
            self.jobs.get(job_id).is_some_and(|job| {
                job.client_id == client_id && matches!(job.status, JobStatus::Queued)
            })
        }) else {
            return Ok(PollJobResponse { job: None });
        };
        let job_id = self.queue.remove(index).expect("job id in queue");
        let job = self.jobs.get_mut(&job_id).expect("queued job exists");
        job.status = JobStatus::Running;
        job.updated_epoch = now_epoch();
        Ok(PollJobResponse {
            job: Some(JobAssignment {
                id: job.id.clone(),
                prompt: job.prompt.clone(),
                permission_mode: job.permission_mode.clone(),
            }),
        })
    }

    fn complete_job(&mut self, body: JobResultRequest) -> Result<()> {
        let job = self
            .jobs
            .get_mut(&body.job_id)
            .ok_or_else(|| anyhow!("unknown job {}", body.job_id))?;
        if job.client_id != body.client_id {
            bail!(
                "job {} is not assigned to client {}",
                body.job_id,
                body.client_id
            );
        }
        job.status = if body.result.ok {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        job.updated_epoch = now_epoch();
        job.result = Some(body.result);
        if let Some(client) = self.clients.get_mut(&body.client_id) {
            client.status = ClientStatus::Idle;
            client.last_seen_epoch = now_epoch();
        }
        Ok(())
    }

    fn expire_stale_running_jobs(&mut self, now: u64) -> bool {
        let mut changed = false;
        for job in self.jobs.values_mut() {
            if matches!(job.status, JobStatus::Running)
                && now.saturating_sub(job.updated_epoch) >= REMOTE_JOB_TIMEOUT_SECS
            {
                job.status = JobStatus::Failed;
                job.updated_epoch = now;
                job.result = Some(JobResult {
                    ok: false,
                    output: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!(
                        "remote job timed out after {REMOTE_JOB_TIMEOUT_SECS} seconds"
                    )),
                });
                changed = true;
            }
        }
        changed
    }

    fn job_status(&mut self, job_id: &str) -> Result<JobStatusResponse> {
        self.expire_stale_running_jobs(now_epoch());
        let job = self
            .jobs
            .get(job_id)
            .ok_or_else(|| anyhow!("unknown job {job_id}"))?;
        Ok(JobStatusResponse {
            id: job.id.clone(),
            client_id: job.client_id.clone(),
            status: job.status.clone(),
            result: job.result.clone(),
        })
    }
}

async fn execute_job(cwd: &std::path::Path, job: &JobAssignment) -> JobResult {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            return JobResult {
                ok: false,
                output: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("resolve current executable: {e}")),
            };
        }
    };
    let output = tokio::process::Command::new(exe)
        .arg("--no-update-check")
        .arg("--cwd")
        .arg(cwd)
        .arg(format!("--print={}", job.prompt))
        .arg("--output-format")
        .arg("json")
        .arg("--permission-mode")
        .arg(&job.permission_mode)
        .output()
        .await;

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let parsed = serde_json::from_str(&stdout).ok();
            JobResult {
                ok: output.status.success(),
                output: parsed,
                stdout,
                stderr,
                error: if output.status.success() {
                    None
                } else {
                    Some(format!("mj --print exited with {}", output.status))
                },
            }
        }
        Err(e) => JobResult {
            ok: false,
            output: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("run mj --print: {e}")),
        },
    }
}

async fn send_heartbeat(
    http: &reqwest::Client,
    pairing: &PairingConfig,
    client_id: &str,
    status: ClientStatusWire,
) -> Result<()> {
    post_json::<_, serde_json::Value>(
        http,
        pairing,
        "/api/heartbeat",
        &HeartbeatRequest {
            client_id: client_id.to_string(),
            status,
        },
    )
    .await?;
    Ok(())
}

async fn post_json<T, R>(
    http: &reqwest::Client,
    pairing: &PairingConfig,
    path: &str,
    body: &T,
) -> Result<R>
where
    T: Serialize + ?Sized,
    R: for<'de> Deserialize<'de>,
{
    let response = http
        .post(api_url(&pairing.server, path))
        .bearer_auth(&pairing.token)
        .json(body)
        .send()
        .await
        .context("remote POST")?;
    decode_response(response).await
}

async fn get_json<R>(http: &reqwest::Client, pairing: &PairingConfig, path: &str) -> Result<R>
where
    R: for<'de> Deserialize<'de>,
{
    let response = http
        .get(api_url(&pairing.server, path))
        .bearer_auth(&pairing.token)
        .send()
        .await
        .context("remote GET")?;
    decode_response(response).await
}

async fn decode_response<R>(response: reqwest::Response) -> Result<R>
where
    R: for<'de> Deserialize<'de>,
{
    let status = response.status();
    let body = response.text().await.context("read remote response")?;
    if !status.is_success() {
        bail!("remote server returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parse remote response: {body}"))
}

async fn read_http_request<S>(
    stream: &mut S,
    client_cert_sha256: Option<String>,
) -> Result<HttpRequest>
where
    S: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 1024];
        let n = stream.read(&mut chunk).await.context("read request")?;
        if n == 0 {
            bail!("connection closed before request headers");
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > MAX_HTTP_BODY {
            bail!("request too large");
        }
        if let Some(pos) = find_header_end(&bytes) {
            break pos;
        }
    };

    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("missing method"))?
        .to_string();
    let target = parts.next().ok_or_else(|| anyhow!("missing target"))?;
    let (path, query) = split_target(target);
    let mut header_map = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            header_map.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length = header_map
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_HTTP_BODY {
        bail!("request body too large");
    }

    let body_start = header_end + 4;
    while bytes.len().saturating_sub(body_start) < content_length {
        let mut chunk = [0_u8; 1024];
        let n = stream.read(&mut chunk).await.context("read body")?;
        if n == 0 {
            bail!("connection closed before request body");
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let body = bytes[body_start..body_start + content_length].to_vec();

    Ok(HttpRequest {
        method,
        path,
        query,
        headers: header_map,
        body,
        client_cert_sha256,
    })
}

fn json_response<T: Serialize>(status: u16, value: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value).context("serialize response")?;
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let mut response = headers.into_bytes();
    response.extend_from_slice(&body);
    Ok(response)
}

fn authorized(request: &HttpRequest, token: &str) -> bool {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == token)
}

async fn resolve_pairing(input: ResolvePairingInput<'_>) -> Result<PairingConfig> {
    let ResolvePairingInput {
        pairing_uri,
        join_code,
        ca_sha256,
        server,
        token,
        ca_cert,
        discovery_addr,
        discovery_timeout,
    } = input;
    let from_uri = pairing_uri.map(parse_pairing_uri).transpose()?;
    let server = server.or_else(|| from_uri.as_ref().and_then(|cfg| cfg.server.clone()));
    let token = token.or_else(|| from_uri.as_ref().and_then(|cfg| cfg.token.clone()));
    let ca_sha256 = ca_sha256.or_else(|| from_uri.as_ref().and_then(|cfg| cfg.ca_sha256.clone()));
    let join_code = join_code.or_else(|| from_uri.and_then(|cfg| cfg.join_code));
    if let (Some(server), Some(token)) = (server.clone(), token.clone()) {
        return Ok(PairingConfig {
            server: normalize_server_url(&server),
            token,
            ca_cert_pem: load_ca_for_server(&server, ca_cert)?,
        });
    }
    let Some(join_code) = join_code else {
        bail!("missing remote pairing; pass --code or --server and --token");
    };
    let Some(ca_sha256) = ca_sha256 else {
        bail!("automatic discovery requires a pairing URI from `mj remote server` or --ca-sha256");
    };
    let Some(token) = token else {
        bail!("automatic discovery requires a pairing URI from `mj remote server` or --token");
    };
    let discovered =
        discover_pairing(&join_code, &ca_sha256, discovery_addr, discovery_timeout).await?;
    Ok(PairingConfig {
        server: normalize_server_url(&discovered.server),
        token,
        ca_cert_pem: discovered.ca_cert_pem,
    })
}

fn parse_pairing_uri(value: &str) -> Result<PairingSeed> {
    let query = value
        .strip_prefix("mj+remote://join?")
        .ok_or_else(|| anyhow!("invalid pairing URI"))?;
    let query = parse_query(query);
    Ok(PairingSeed {
        server: query
            .get("server")
            .map(|server| normalize_server_url(server)),
        token: query.get("token").cloned(),
        join_code: query.get("code").cloned(),
        ca_sha256: query
            .get("ca_sha256")
            .or_else(|| query.get("fingerprint"))
            .map(|value| normalize_fingerprint(value)),
    })
}

async fn discover_pairing(
    join_code: &str,
    expected_ca_sha256: &str,
    discovery_addr: SocketAddr,
    timeout: Duration,
) -> Result<PairingConfig> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("bind discovery client")?;
    socket
        .set_broadcast(true)
        .context("enable discovery broadcast")?;
    let request = serde_json::to_vec(&DiscoveryRequest {
        protocol: "mj-remote-discovery-v1".to_string(),
        join_code: join_code.to_string(),
    })
    .context("serialize discovery request")?;
    socket
        .send_to(&request, discovery_addr)
        .await
        .context("send discovery request")?;

    let mut buf = [0_u8; 2048];
    let recv = tokio::time::timeout(timeout, socket.recv_from(&mut buf))
        .await
        .context("timed out discovering remote server")??;
    let (len, _) = recv;
    let response: DiscoveryResponse =
        serde_json::from_slice(&buf[..len]).context("parse discovery response")?;
    if response.protocol != "mj-remote-discovery-v1" {
        bail!("invalid discovery response");
    }
    if response.join_code != join_code {
        bail!("discovery response did not match requested join code");
    }
    let actual_ca_sha256 = ca_sha256_from_pem(&response.ca_cert_pem)?;
    if normalize_fingerprint(expected_ca_sha256) != actual_ca_sha256 {
        bail!("discovery response CA fingerprint did not match pairing URI");
    }
    if normalize_fingerprint(&response.ca_sha256) != actual_ca_sha256 {
        bail!("discovery response CA fingerprint did not match certificate");
    }
    Ok(PairingConfig {
        server: response.server,
        token: String::new(),
        ca_cert_pem: Some(response.ca_cert_pem),
    })
}

fn pairing_uri_for_code(join_code: &str, token: &str, ca_cert_pem: &str) -> Result<String> {
    Ok(format!(
        "mj+remote://join?code={}&token={}&ca_sha256={}",
        percent_encode(join_code),
        percent_encode(token),
        ca_sha256_from_pem(ca_cert_pem)?
    ))
}

fn server_base_url(bind: SocketAddr) -> String {
    let host = if bind.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bind.ip().to_string()
    };
    format!("https://{}:{}", host, bind.port())
}

fn server_base_url_for_peer(http_addr: SocketAddr, peer: SocketAddr) -> String {
    if !http_addr.ip().is_unspecified() {
        return server_base_url(http_addr);
    }
    let host = local_ip_for_peer(peer)
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    format!("https://{}:{}", host, http_addr.port())
}

fn reqwest_client(
    pairing: &PairingConfig,
    identity: Option<(&str, &str)>,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(ca_cert_pem) = pairing.ca_cert_pem.as_deref() {
        let ca = reqwest::Certificate::from_pem(ca_cert_pem.as_bytes())
            .context("parse remote CA certificate")?;
        builder = builder
            .add_root_certificate(ca)
            .danger_accept_invalid_hostnames(true);
    }
    if let Some((cert_pem, key_pem)) = identity {
        let mut combined = String::new();
        combined.push_str(cert_pem);
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(key_pem);
        let identity =
            reqwest::Identity::from_pem(combined.as_bytes()).context("parse client identity")?;
        builder = builder.identity(identity);
    }
    builder.build().context("build remote HTTP client")
}

fn load_ca_for_server(server: &str, ca_cert: Option<&std::path::Path>) -> Result<Option<String>> {
    if let Some(path) = ca_cert {
        return std::fs::read_to_string(path)
            .with_context(|| format!("read CA certificate {}", path.display()))
            .map(Some);
    }
    load_saved_ca_for_server(server)
}

fn load_saved_ca_for_server(server: &str) -> Result<Option<String>> {
    let path = default_remote_config_path();
    let config = SavedRemoteConfig::load(&path)?;
    let normalized = normalize_server_url(server);
    Ok(config.client.and_then(|client| {
        if normalize_server_url(&client.server) == normalized {
            Some(client.ca_cert_pem)
        } else {
            None
        }
    }))
}

fn local_ip_for_peer(peer: SocketAddr) -> Option<SocketAddr> {
    let bind_addr = if peer.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = std::net::UdpSocket::bind(bind_addr).ok()?;
    socket.connect(peer).ok()?;
    socket.local_addr().ok()
}

fn default_remote_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("remote.toml")
}

fn default_remote_server_state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("remote-server.toml")
}

fn write_toml_private<T: Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(value).context("serialize remote config")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    set_private_file_permissions(&tmp)?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn generate_tls_material() -> Result<TlsMaterial> {
    let ca_key = KeyPair::generate().context("generate remote CA key")?;
    let mut ca_params = CertificateParams::new(vec!["mjolnir remote CA".to_string()])
        .context("create CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_cert = ca_params
        .clone()
        .self_signed(&ca_key)
        .context("self-sign remote CA")?;

    let server_key = KeyPair::generate().context("generate remote server key")?;
    let mut server_params = CertificateParams::new(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        "mj-remote".to_string(),
    ])
    .context("create server cert params")?;
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .context("sign remote server cert")?;

    Ok(TlsMaterial {
        ca_cert_pem: ca_cert.pem(),
        ca_key_pem: ca_key.serialize_pem(),
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
    })
}

fn generate_client_certificate(
    tls: &TlsMaterial,
    client_id: &str,
) -> Result<GeneratedClientCertificate> {
    let ca_key = KeyPair::from_pem(&tls.ca_key_pem).context("parse remote CA key")?;
    let ca_params =
        CertificateParams::from_ca_cert_pem(&tls.ca_cert_pem).context("parse remote CA cert")?;
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("reconstruct remote CA certificate")?;

    let client_key = KeyPair::generate().context("generate remote client key")?;
    let mut client_params =
        CertificateParams::new(vec![client_id.to_string()]).context("create client cert params")?;
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .context("sign remote client cert")?;
    Ok(GeneratedClientCertificate {
        cert_pem: client_cert.pem(),
        key_pem: client_key.serialize_pem(),
    })
}

fn tls_acceptor(tls: &TlsMaterial) -> Result<TlsAcceptor> {
    let mut roots = RootCertStore::empty();
    roots
        .add(certificate_der_from_pem(&tls.ca_cert_pem)?)
        .context("trust remote CA for client certificates")?;
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .context("build client certificate verifier")?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            certificates_from_pem(&tls.server_cert_pem)?,
            private_key_from_pem(&tls.server_key_pem)?,
        )
        .context("build TLS server config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn peer_cert_sha256(stream: &TlsStream<TcpStream>) -> Option<String> {
    stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| hex_encode(&Sha256::digest(cert.as_ref())))
}

fn cert_sha256_from_pem(pem: &str) -> Result<String> {
    Ok(hex_encode(&Sha256::digest(
        certificate_der_from_pem(pem)?.as_ref(),
    )))
}

fn ca_sha256_from_pem(pem: &str) -> Result<String> {
    cert_sha256_from_pem(pem)
}

fn certificates_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    Ok(pem_blocks(pem, "CERTIFICATE")?
        .into_iter()
        .map(CertificateDer::from)
        .collect())
}

fn certificate_der_from_pem(pem: &str) -> Result<CertificateDer<'static>> {
    pem_blocks(pem, "CERTIFICATE")?
        .into_iter()
        .next()
        .map(CertificateDer::from)
        .ok_or_else(|| anyhow!("missing certificate PEM block"))
}

fn private_key_from_pem(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let key = pem_blocks(pem, "PRIVATE KEY")?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("missing private key PEM block"))?;
    Ok(PrivatePkcs8KeyDer::from(key).into())
}

fn pem_blocks(pem: &str, label: &str) -> Result<Vec<Vec<u8>>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut rest = pem;
    let mut blocks = Vec::new();
    while let Some(begin_index) = rest.find(&begin) {
        let after_begin = &rest[begin_index + begin.len()..];
        let Some(end_index) = after_begin.find(&end) else {
            bail!("unterminated {label} PEM block");
        };
        let body = after_begin[..end_index]
            .lines()
            .map(str::trim)
            .collect::<String>();
        blocks.push(
            base64::engine::general_purpose::STANDARD
                .decode(body)
                .with_context(|| format!("decode {label} PEM block"))?,
        );
        rest = &after_begin[end_index + end.len()..];
    }
    Ok(blocks)
}

#[cfg(unix)]
fn set_private_file_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("set permissions on {}", path.display()))
}

#[cfg(windows)]
fn set_private_file_permissions(path: &std::path::Path) -> Result<()> {
    let username = std::env::var("USERNAME").context("resolve current Windows user")?;
    let user = match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.trim().is_empty() => format!("{domain}\\{username}"),
        _ => username,
    };
    let grant = format!("{user}:F");
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(grant)
        .status()
        .with_context(|| format!("run icacls for {}", path.display()))?;
    if !status.success() {
        bail!("icacls failed for {} with {status}", path.display());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_private_file_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn normalize_server_url(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

fn api_url(server: &str, path: &str) -> String {
    format!("{}{}", normalize_server_url(server), path)
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    if let Some((path, query)) = target.split_once('?') {
        (path.to_string(), parse_query(query))
    } else {
        (target.to_string(), HashMap::new())
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

fn query_param<'a>(request: &'a HttpRequest, name: &str) -> Result<&'a str> {
    request
        .query
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing query parameter {name}"))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .flat_map(char::to_lowercase)
        .collect()
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn print_job_result(result: Option<&JobResult>) {
    let Some(result) = result else {
        return;
    };
    if let Some(output) = &result.output {
        if let Some(text) = output.get("result").and_then(serde_json::Value::as_str) {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            return;
        }
        println!(
            "{}",
            serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string())
        );
        return;
    }
    print!("{}", result.stdout);
    if !result.stderr.is_empty() {
        eprintln!("{}", result.stderr);
    }
    if let Some(error) = &result.error {
        eprintln!("{error}");
    }
}

fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

fn random_join_code() -> String {
    random_token(3).to_ascii_uppercase()
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    getrandom::fill(&mut buf).expect("secure OS random source");
    hex_encode(&buf)
}

fn unique_id(prefix: &str, seed: &str) -> String {
    let raw = format!(
        "{}:{}:{}:{}",
        prefix,
        seed,
        now_epoch_nanos(),
        random_token(8)
    );
    let digest = Sha256::digest(raw.as_bytes());
    format!("{prefix}-{}", hex_encode(&digest[..8]))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn default_client_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "mj-client".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tls() -> TlsMaterial {
        generate_tls_material().expect("tls material")
    }

    #[test]
    fn pairing_uri_roundtrips() {
        let tls = test_tls();
        let expected_ca_sha256 = ca_sha256_from_pem(&tls.ca_cert_pem).expect("hash");
        let uri = pairing_uri_for_code("ABC123", "secret-token", &tls.ca_cert_pem).expect("uri");
        let parsed = parse_pairing_uri(&uri).expect("parse");
        assert_eq!(parsed.join_code.as_deref(), Some("ABC123"));
        assert_eq!(parsed.token.as_deref(), Some("secret-token"));
        assert_eq!(
            parsed.ca_sha256.as_deref(),
            Some(expected_ca_sha256.as_str())
        );
        assert!(parsed.server.is_none());
    }

    #[tokio::test]
    async fn resolve_pairing_allows_cli_overrides() {
        let uri = "mj+remote://join?server=http%3A%2F%2Fold%3A1&token=old-token";
        let parsed = resolve_pairing(ResolvePairingInput {
            pairing_uri: Some(uri),
            join_code: None,
            ca_sha256: None,
            server: Some("http://new:2".to_string()),
            token: Some("new-token".to_string()),
            ca_cert: None,
            discovery_addr: "255.255.255.255:7338".parse().expect("addr"),
            discovery_timeout: Duration::from_millis(1),
        })
        .await
        .expect("resolve");
        assert_eq!(parsed.server, "http://new:2");
        assert_eq!(parsed.token, "new-token");
    }

    #[tokio::test]
    async fn resolve_pairing_rejects_discovery_without_ca_fingerprint() {
        let err = resolve_pairing(ResolvePairingInput {
            pairing_uri: None,
            join_code: Some("ABC123".to_string()),
            ca_sha256: None,
            server: None,
            token: None,
            ca_cert: None,
            discovery_addr: "255.255.255.255:7338".parse().expect("addr"),
            discovery_timeout: Duration::from_millis(1),
        })
        .await
        .expect_err("missing CA fingerprint should fail before discovery");
        assert!(err.to_string().contains("requires a pairing URI"));
    }

    #[tokio::test]
    async fn resolve_pairing_rejects_discovery_without_token() {
        let err = resolve_pairing(ResolvePairingInput {
            pairing_uri: None,
            join_code: Some("ABC123".to_string()),
            ca_sha256: Some("abc123".to_string()),
            server: None,
            token: None,
            ca_cert: None,
            discovery_addr: "255.255.255.255:7338".parse().expect("addr"),
            discovery_timeout: Duration::from_millis(1),
        })
        .await
        .expect_err("missing token should fail before discovery");
        assert!(err.to_string().contains("requires a pairing URI"));
    }

    #[test]
    fn load_ca_for_server_prefers_explicit_ca_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, "test-ca").expect("write ca");

        let ca = load_ca_for_server("https://example.test", Some(&path)).expect("load ca");
        assert_eq!(ca.as_deref(), Some("test-ca"));
    }

    #[test]
    fn server_base_url_for_peer_uses_bound_addr_when_specific() {
        let url = server_base_url_for_peer(
            "127.0.0.1:7337".parse().expect("http addr"),
            "127.0.0.1:9999".parse().expect("peer addr"),
        );
        assert_eq!(url, "https://127.0.0.1:7337");
    }

    #[test]
    fn server_state_queues_jobs_for_matching_client() {
        let mut state = RemoteServerState::default();
        let tls = test_tls();
        let registered = state
            .register(
                RegisterRequest {
                    client_id: None,
                    name: "desktop".to_string(),
                    cwd: "/tmp/project".to_string(),
                    version: "0.0.0".to_string(),
                },
                None,
                &tls,
            )
            .expect("register");
        let created = state
            .create_job(CreateJobRequest {
                client_id: registered.client_id.clone(),
                prompt: "summarize".to_string(),
                permission_mode: "default".to_string(),
            })
            .expect("create");

        let polled = state.poll_job(&registered.client_id).expect("poll");
        let job = polled.job.expect("job");
        assert_eq!(job.id, created.job_id);
        assert_eq!(job.prompt, "summarize");
    }

    #[test]
    fn server_state_reuses_persisted_client_id() {
        let mut state = RemoteServerState::default();
        let tls = test_tls();
        let registered = state
            .register(
                RegisterRequest {
                    client_id: Some("client-stable".to_string()),
                    name: "desktop".to_string(),
                    cwd: "/tmp/project".to_string(),
                    version: "0.0.0".to_string(),
                },
                None,
                &tls,
            )
            .expect("register");

        assert_eq!(registered.client_id, "client-stable");
        assert!(state.clients.contains_key("client-stable"));
    }

    #[test]
    fn saved_remote_config_rejects_expired_pairing() {
        let config = SavedRemoteConfig {
            client: Some(SavedRemoteClient {
                server: "http://127.0.0.1:7337".to_string(),
                token: "secret".to_string(),
                client_id: "client-stable".to_string(),
                ca_cert_pem: "ca".to_string(),
                client_cert_pem: "cert".to_string(),
                client_key_pem: "key".to_string(),
                name: "desktop".to_string(),
                cwd: "/tmp/project".to_string(),
                paired_at_epoch: 1,
                expires_at_epoch: 2,
            }),
        };

        assert!(config.active_client().is_err());
    }

    #[test]
    fn server_store_persists_token_join_code_and_clients() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remote-server.toml");
        let mut store = RemoteServerStore::load(&path).expect("load");
        store.token = "token-1".to_string();
        store.join_code = "ABC123".to_string();
        let registered = store
            .register(
                RegisterRequest {
                    client_id: Some("client-stable".to_string()),
                    name: "desktop".to_string(),
                    cwd: "/tmp/project".to_string(),
                    version: "0.0.0".to_string(),
                },
                None,
            )
            .expect("register");
        assert_eq!(registered.client_id, "client-stable");

        let loaded = RemoteServerStore::load(&path).expect("reload");
        assert_eq!(loaded.token, "token-1");
        assert_eq!(loaded.join_code, "ABC123");
        assert!(loaded.state.clients.contains_key("client-stable"));
    }

    #[test]
    fn completing_job_records_result() {
        let mut state = RemoteServerState::default();
        let tls = test_tls();
        let registered = state
            .register(
                RegisterRequest {
                    client_id: None,
                    name: "desktop".to_string(),
                    cwd: "/tmp/project".to_string(),
                    version: "0.0.0".to_string(),
                },
                None,
                &tls,
            )
            .expect("register");
        let created = state
            .create_job(CreateJobRequest {
                client_id: registered.client_id.clone(),
                prompt: "summarize".to_string(),
                permission_mode: "default".to_string(),
            })
            .expect("create");
        state.poll_job(&registered.client_id).expect("poll");
        state
            .complete_job(JobResultRequest {
                client_id: registered.client_id,
                job_id: created.job_id.clone(),
                result: JobResult {
                    ok: true,
                    output: Some(serde_json::json!({"result": "done"})),
                    stdout: String::new(),
                    stderr: String::new(),
                    error: None,
                },
            })
            .expect("complete");

        let status = state.job_status(&created.job_id).expect("status");
        assert!(matches!(status.status, JobStatus::Completed));
        assert_eq!(
            status
                .result
                .and_then(|result| result.output)
                .and_then(|output| output.get("result").cloned()),
            Some(serde_json::json!("done"))
        );
    }

    #[test]
    fn running_job_times_out_when_status_is_read() {
        let mut state = RemoteServerState::default();
        let tls = test_tls();
        let registered = state
            .register(
                RegisterRequest {
                    client_id: None,
                    name: "desktop".to_string(),
                    cwd: "/tmp/project".to_string(),
                    version: "0.0.0".to_string(),
                },
                None,
                &tls,
            )
            .expect("register");
        let created = state
            .create_job(CreateJobRequest {
                client_id: registered.client_id.clone(),
                prompt: "summarize".to_string(),
                permission_mode: "default".to_string(),
            })
            .expect("create");
        state.poll_job(&registered.client_id).expect("poll");
        let job = state.jobs.get_mut(&created.job_id).expect("job");
        job.updated_epoch = now_epoch().saturating_sub(REMOTE_JOB_TIMEOUT_SECS);

        let status = state.job_status(&created.job_id).expect("status");

        assert!(matches!(status.status, JobStatus::Failed));
        assert_eq!(
            status.result.and_then(|result| result.error),
            Some(format!(
                "remote job timed out after {REMOTE_JOB_TIMEOUT_SECS} seconds"
            ))
        );
    }
}
