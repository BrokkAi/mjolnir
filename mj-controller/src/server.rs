//! Daemon-owned, phone-oriented control surface for Hel.
//!
//! The server deliberately owns no controller business logic. It publishes a
//! redacted projection of controller state and forwards validated, typed
//! actions through a channel supplied by the controller.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Component, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result as AnyResult};
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY as CONTENT_SECURITY_POLICY_HEADER, CONTENT_TYPE, COOKIE,
    HeaderValue, LOCATION, REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Semaphore;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use mj_core::attachment::{AttachmentRef, AttachmentStore, MAX_IMAGE_BYTES, MAX_IMAGES};
use mj_core::config::{Config, TargetTemplate, project_history_host, validate_id};
use mj_core::elicitation::{ElicitationRequest, ElicitationResponse, MAX_ELICITATION_BYTES};
use mj_core::path_completion::{CompletionHost, CompletionKind, PathCompletion};
use mj_core::refusal::{Refusal, RefusalKind};
use mj_core::state::{
    MoveOperation, MovePhase, MovePreparation, MoveSelection, MoveSessionRequest,
    ProjectSourceIdentity, SessionResourceAllocation, SessionState, SessionTransitionKind,
    State as AppState,
};

use crate::targets::AdditionalMount;

use crate::dictation::{
    DictationError, DictationOperation, DictationRequest, DictationResponse, MAX_AUDIO_BYTES,
    validate_wav,
};
use crate::image::optimize_image;

pub mod api;

pub use api::{
    ApiFailure, ApiSession, PromptRequest, PromptResponse, SessionListResponse,
    StartSessionRequest, StartSessionResponse, SubagentBackend, WaitOutcome, WaitRequest,
    WaitResponse, api_token_path, load_or_create_api_token, map_stop_reason, resolve_wait,
};

pub use mj_client::web::{
    BrowserDiffStat, BrowserTranscript, BrowserTranscriptEntry, WebListenerProcess,
    WebViewerAccess, WebViewerRecovery,
};

// Keep all control surfaces on the same queue vocabulary. The resume flow
// used to define a private copy here, which made a move request impossible to
// pass through the web and daemon boundaries without lossy conversion.
pub use mj_core::state::ResumeQueueDisposition;

/// Select the process-wide rustls provider before any TLS configuration is built.
///
/// Dependency feature unification can enable both rustls providers. Rustls
/// deliberately refuses to guess in that case, so each executable that links
/// the controller installs the ring provider at process startup. A provider
/// installed even earlier is already sufficient and remains in place.
pub fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub const COOKIE_NAME: &str = "hel_viewer_session";
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const EPHEMERAL_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_CODE_FAILURES: u32 = 5;
const CODE_LOCKOUT_BASE: Duration = Duration::from_secs(30);
const CODE_LOCKOUT_CAP: Duration = Duration::from_secs(60 * 60);
const MAX_TITLE_CHARS: usize = 120;
const MAX_PROMPT_CHARS: usize = 64 * 1024;
/// How many repositories one dirty-worktree acknowledgement may name. A bundle
/// with more repositories than this than has bigger problems than the phone.
const MAX_DIRTY_ACKNOWLEDGEMENTS: usize = 32;
/// The largest draft a phone may store. A composer is for a prompt, and a
/// prompt this size has other problems; the bound exists so one viewer cannot
/// fill the daemon's database with text it never sent.
const MAX_DRAFT_BYTES: usize = 64 * 1024;
/// How many prompt-history matches one search returns. Public because the
/// controller loop performs the search and must use the same bound the phone
/// was promised.
pub const MAX_HISTORY_MATCHES: usize = 40;
/// Image prompts need far more room than any other phone request. Browser
/// uploads are base64-encoded, so two ordinary photographs already exceed the
/// general body limit even when each one fits it. The larger bound therefore
/// stays scoped to the action route that carries prompts.
const MAX_PROMPT_BODY_BYTES: usize = 32 * 1024 * 1024;
/// A browser uploads one source image at a time. The image optimizer has its
/// own decoded-allocation bound; this is the HTTP envelope bound before that
/// work starts.
const MAX_ATTACHMENT_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Keep two browser uploads/transcriptions in flight. The permit is acquired
/// before reading the request body, so an overloaded client is rejected
/// without accepting megabytes that cannot be processed yet.
const MAX_CONCURRENT_DICTATIONS: usize = 2;
/// Keep this in sync with the prompt admission bound and the browser composer.
pub const MAX_PROMPT_IMAGES: usize = MAX_IMAGES;
const COOKIE_KEY_BYTES: usize = 32;
const COOKIE_KEY_FILE: &str = "phone-cookie-key";

/// How long stored viewer state outlives its last use.
///
/// It matches the session cookie's own lifetime: state keyed to an identity
/// that can no longer authenticate has nothing left to belong to.
pub const fn default_session_ttl() -> Duration {
    DEFAULT_SESSION_TTL
}

pub fn cookie_key_path() -> PathBuf {
    mj_core::config::data_dir().join(COOKIE_KEY_FILE)
}

/// Load the phone cookie signing key, creating it on first use.
///
/// Session cookies are stateless, so this file is the only thing that keeps a
/// signed-in phone signed in across daemon restarts. Deleting it is
/// therefore the explicit sign-everyone-out gesture: the next start writes a
/// new key and every outstanding cookie stops validating. A missing file is
/// ordinary first use; an unreadable or too-short one is replaced loudly,
/// because refusing to start would be a worse answer than asking phones to
/// enter the viewer code again.
pub fn load_or_create_cookie_key(path: &std::path::Path) -> AnyResult<Vec<u8>> {
    match std::fs::read(path) {
        Ok(key) if key.len() >= COOKIE_KEY_BYTES => return Ok(key),
        Ok(key) => tracing::warn!(
            path = %path.display(),
            bytes = key.len(),
            "phone cookie key is shorter than {COOKIE_KEY_BYTES} bytes; generating a new key signs every phone out"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            "could not read the phone cookie key ({error}); generating a new key signs every phone out"
        ),
    }
    let key = generate_cookie_key()?;
    mj_core::config::atomic_write(path, &key)
        .with_context(|| format!("persist Mjolnir phone cookie key {}", path.display()))?;
    Ok(key.to_vec())
}

/// Options for the daemon's phone service.
///
/// `ServerOptions::new` generates both the six-digit viewer code and an
/// ephemeral cookie key. A caller that wants cookies to survive server
/// restarts installs a persisted key with `set_cookie_key`, which
/// `load_or_create_cookie_key` reads from its private Hel data directory. The
/// key and viewer code are intentionally omitted from `Debug` output.
#[derive(Clone)]
pub struct ServerOptions {
    pub bind: SocketAddr,
    pub snapshot_rx: watch::Receiver<ViewerSnapshot>,
    pub conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    pub action_tx: mpsc::Sender<ControllerRequest>,
    pub bundle_tx: mpsc::Sender<BundleRequest>,
    pub receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    pub preflight_tx: mpsc::Sender<PreflightRequest>,
    pub move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    pub client_state_tx: mpsc::Sender<ClientStateRequest>,
    pub dictation_tx: mpsc::Sender<DictationRequest>,
    /// Dedicated bounded path for stopping one live background task. This is
    /// deliberately separate from [`ControllerAction`]: stopping a provider
    /// task does not occupy the controller's action admission slot.
    background_task_stop_tx: mpsc::Sender<BackgroundTaskStopRequest>,
    pub shutdown: CancellationToken,
    pub session_ttl: Duration,
    /// Keep this enabled for direct HTTPS or an HTTPS reverse proxy. It may be
    /// disabled only for an explicitly trusted HTTP development endpoint.
    pub secure_cookie: bool,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
    viewer_code: String,
    login_token: String,
    cookie_key: Vec<u8>,
    api_token: String,
    subagent: Option<Arc<dyn api::SubagentBackend>>,
}

/// Typed request channels served by the authenticated HTTP surface.
pub struct ServerRequests {
    pub action_tx: mpsc::Sender<ControllerRequest>,
    pub bundle_tx: mpsc::Sender<BundleRequest>,
    pub receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    pub preflight_tx: mpsc::Sender<PreflightRequest>,
    pub move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    pub client_state_tx: mpsc::Sender<ClientStateRequest>,
    pub dictation_tx: mpsc::Sender<DictationRequest>,
}

impl ServerOptions {
    pub fn new(
        bind: SocketAddr,
        snapshot_rx: watch::Receiver<ViewerSnapshot>,
        conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
        requests: ServerRequests,
    ) -> AnyResult<Self> {
        Ok(Self {
            bind,
            snapshot_rx,
            conversation_rx,
            action_tx: requests.action_tx,
            bundle_tx: requests.bundle_tx,
            receipt_tx: requests.receipt_tx,
            preflight_tx: requests.preflight_tx,
            move_preparation_tx: requests.move_preparation_tx,
            client_state_tx: requests.client_state_tx,
            dictation_tx: requests.dictation_tx,
            background_task_stop_tx: mpsc::channel(1).0,
            shutdown: CancellationToken::new(),
            session_ttl: DEFAULT_SESSION_TTL,
            secure_cookie: true,
            tls_config: None,
            viewer_code: generate_viewer_code()?,
            login_token: generate_login_token()?,
            cookie_key: generate_cookie_key()?.to_vec(),
            // An empty token authenticates nothing: the daemon installs the
            // persisted one, and a server without it serves the viewer only.
            api_token: String::new(),
            subagent: None,
        })
    }

    pub fn viewer_code(&self) -> &str {
        &self.viewer_code
    }

    pub fn login_token(&self) -> &str {
        &self.login_token
    }

    /// Serve HTTPS directly using the supplied Rustls configuration. Hel's
    /// CLI can load its persisted certificate (including a Tailscale-issued
    /// certificate) and pass it here without coupling this module to disk.
    pub fn set_tls_config(&mut self, config: axum_server::tls_rustls::RustlsConfig) {
        self.tls_config = Some(config);
        self.secure_cookie = true;
    }

    /// Install a persisted signing key. Rotating this value signs every phone
    /// out without maintaining a server-side session database.
    pub fn set_cookie_key(&mut self, key: Vec<u8>) -> AnyResult<()> {
        anyhow::ensure!(
            key.len() >= COOKIE_KEY_BYTES,
            "cookie signing key must be at least {COOKIE_KEY_BYTES} bytes"
        );
        self.cookie_key = key;
        Ok(())
    }

    /// Install the controller's bounded background-task stop path.
    pub fn set_background_task_stop_tx(&mut self, tx: mpsc::Sender<BackgroundTaskStopRequest>) {
        self.background_task_stop_tx = tx;
    }

    /// Install the persisted bearer token for the `/api/v1` routes. Rotating
    /// it revokes every client that still holds the old one.
    pub fn set_api_token(&mut self, token: String) {
        self.api_token = token;
    }

    /// Install the daemon-side backend the `/api/v1` routes drive sessions
    /// through. Without it those routes answer 503.
    pub fn set_subagent_backend(&mut self, backend: Arc<dyn api::SubagentBackend>) {
        self.subagent = Some(backend);
    }

    #[cfg(test)]
    fn with_test_credentials(mut self, code: &str, key: &[u8]) -> Self {
        self.viewer_code = code.to_string();
        self.login_token = "test-login-token".into();
        self.cookie_key = key.to_vec();
        self.secure_cookie = false;
        self.api_token = "test-api-token".into();
        self
    }
}

/// Run the phone server until its shutdown token is cancelled.
///
/// This binds only the requested listener. It does not daemonize, provision a
/// target, or keep sessions alive: controller availability is required, just
/// like MJ's explicit remote-viewer model.
pub async fn run_server(options: ServerOptions) -> AnyResult<()> {
    let listener = tokio::net::TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("bind web viewer to {}", options.bind))?;
    run_server_on_listener(options, listener).await
}

/// Serve a reserved socket so readiness and advertised ports reflect a real listener.
pub async fn run_server_on_listener(
    options: ServerOptions,
    listener: tokio::net::TcpListener,
) -> AnyResult<()> {
    let mut options = options;
    let bind = listener.local_addr().context("read web viewer address")?;
    let shutdown = options.shutdown.clone();
    let viewer_code = options.viewer_code.clone();
    let tls_config = options.tls_config.take();
    let app = router(options);
    println!("Mjolnir viewer code: {viewer_code}");
    let listener = listener.into_std().context("prepare web viewer listener")?;
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    let serve = async move {
        if let Some(tls_config) = tls_config {
            axum_server::from_tcp_rustls(listener, tls_config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        } else {
            axum_server::from_tcp(listener)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
    };
    tokio::pin!(serve);
    tokio::select! {
        result = &mut serve => result,
        _ = shutdown.cancelled() => {
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(2)));
            serve.await
        }
    }
    .with_context(|| format!("serve web viewer on {bind}"))
}

mod viewer_types;
pub use viewer_types::*;
mod actions;
pub use actions::*;
mod routes;
use routes::*;
mod handlers;
use handlers::*;
mod validation;
use validation::*;
mod errors;
use errors::*;
mod auth;
pub use auth::*;
mod assets;
use assets::*;
mod config_view;
pub use config_view::*;

#[cfg(test)]
mod tests;
