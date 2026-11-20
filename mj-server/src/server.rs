//! Daemon-owned, phone-oriented control surface for Hel.
//!
//! The server deliberately owns no controller business logic. It publishes a
//! redacted projection of controller state and forwards validated, typed
//! actions through a channel supplied by the controller.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Component, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result as AnyResult};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_SECURITY_POLICY as CONTENT_SECURITY_POLICY_HEADER, CONTENT_TYPE, COOKIE, ETAG,
    HeaderValue, IF_NONE_MATCH, LOCATION, REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
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
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use mj_core::attachment::{AttachmentRef, AttachmentStore, MAX_IMAGE_BYTES, MAX_IMAGES};
use mj_core::config::{Config, TargetTemplate, project_history_host, validate_id};
use mj_core::elicitation::{ElicitationRequest, ElicitationResponse, MAX_ELICITATION_BYTES};
use mj_core::path_completion::{CompletionHost, CompletionKind, PathCompletion};
use mj_core::refusal::{Refusal, RefusalKind};
use mj_core::state::{
    HostContainerSize, MovePreparation, MoveSelection, MoveSessionRequest, ProjectSourceIdentity,
    ResourceAllocationKind, SessionResourceAllocation, SessionState, SessionTransitionKind,
    State as AppState,
};

use mj_controller::targets::AdditionalMount;

use mj_controller::image::optimize_image;

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

pub const COOKIE_NAME: &str = "hel_viewer_session";
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const EPHEMERAL_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_CODE_FAILURES: u32 = 5;
const CODE_LOCKOUT_BASE: Duration = Duration::from_secs(30);
const CODE_LOCKOUT_CAP: Duration = Duration::from_secs(60 * 60);
const MAX_TITLE_CHARS: usize = 120;
/// The largest body of an API request that carries prompt or message text:
/// the ordinary allowance plus the largest prompt the worker's relay accepts
/// ([`mj_core::relay::RELAY_COMMAND_BYTE_BUDGET`], measured as serde_json
/// writes it). JSON lets a client spell any character as a `\u` escape, six
/// bytes for a one-byte ASCII character, so a client's body can be six times
/// what serde_json writes. Any prompt the worker could take therefore reaches
/// the validator, and one it could not is refused with the validator's
/// reason rather than a bare 413 (#1556).
const MAX_PROMPT_TEXT_BODY_BYTES: usize =
    MAX_BODY_BYTES + 6 * mj_core::relay::RELAY_COMMAND_BYTE_BUDGET;
/// How many repositories one dirty-worktree acknowledgement may name. A bundle
/// with more repositories than this than has bigger problems than the phone.
const MAX_DIRTY_ACKNOWLEDGEMENTS: usize = 32;
/// The largest draft a phone may store: as large as the largest prompt the
/// worker's relay takes, so any prompt being composed can be saved (#1556).
/// The bound exists so one viewer cannot fill the daemon's database with text
/// it never sent.
const MAX_DRAFT_BYTES: usize = mj_core::relay::RELAY_COMMAND_BYTE_BUDGET;
/// The longest shell command a phone may run, unchanged by #1556.
const MAX_SHELL_COMMAND_CHARS: usize = 64 * 1024;
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
/// Keep this in sync with the prompt admission bound and the browser composer.
pub const MAX_PROMPT_IMAGES: usize = MAX_IMAGES;

/// How long stored viewer state outlives its last use.
///
/// It matches the session cookie's own lifetime: state keyed to an identity
/// that can no longer authenticate has nothing left to belong to.
pub const fn default_session_ttl() -> Duration {
    DEFAULT_SESSION_TTL
}

/// Options for the daemon's phone service.
///
/// `ServerOptions::new` generates both the six-digit viewer code and an
/// ephemeral cookie key. A caller that wants cookies to survive server
/// restarts loads its key and logout records with `load_cookie_credentials`.
/// `set_cookie_key` installs a key for callers managing storage themselves. The
/// key and viewer code are intentionally omitted from `Debug` output.
#[derive(Clone)]
pub struct ServerOptions {
    pub bind: SocketAddr,
    pub snapshot_rx: watch::Receiver<ViewerSnapshot>,
    pub conversation_rx:
        watch::Receiver<mj_core::snapshot_map::SnapshotMap<String, BrowserTranscript>>,
    pub action_tx: mpsc::Sender<ControllerRequest>,
    pub bundle_tx: mpsc::Sender<BundleRequest>,
    pub receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    pub preflight_tx: mpsc::Sender<PreflightRequest>,
    pub move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    pub client_state_tx: mpsc::Sender<ClientStateRequest>,
    /// Dedicated bounded path for stopping one live background task. This is
    /// deliberately separate from [`ControllerAction`]: stopping a provider
    /// task does not occupy the controller's action admission slot.
    background_task_stop_tx: mpsc::Sender<BackgroundTaskStopRequest>,
    pub shutdown: CancellationToken,
    pub image_builds: mj_controller::session_images::ImageBuilds,
    pub session_ttl: Duration,
    /// Keep this enabled for direct HTTPS or an HTTPS reverse proxy. It may be
    /// disabled only for an explicitly trusted HTTP development endpoint.
    pub secure_cookie: bool,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
    viewer_code: String,
    login_token: String,
    cookie_key: Vec<u8>,
    viewer_revocations: Arc<ViewerRevocations>,
    viewer_history: viewer_feed::ViewerHistoryHandle,
    api_token: String,
    subagent: Option<Arc<dyn api::SubagentBackend>>,
    /// Where the remembered fast-start preferences live. Injected rather than
    /// resolved from the process environment inside a handler, so the
    /// documented API answers from its own state and tests never read the
    /// developer's real configuration.
    preferences_path: PathBuf,
    /// Checks the engine behind each local container target for the launch
    /// options route. Tests replace it so the answer does not depend on the
    /// engines the test host has.
    worker_source_check: Option<WorkerSourceCheck>,
    /// The daemon's upgrade admission. A closed gate refuses new requests, and
    /// tells the long requests a shutdown ends that the daemon is being
    /// replaced, so their clients follow it. Tests give the server its own.
    upgrade_gate: Arc<mj_controller::upgrade::Gate>,
}

/// The header, valued `pending`, that marks a 503 as "this daemon is being
/// replaced by an automatic upgrade; ask the next one". The request was not
/// accepted, or was a read the shutdown ended, so asking again repeats nothing.
pub const UPGRADE_HEADER: &str = "x-mj-upgrade";

/// Answers, for a target id, why no worker binary could serve a session there.
/// It blocks (a bare SSH host is asked its platform), so callers run it off
/// the async runtime.
pub type WorkerSourceCheck = Arc<dyn Fn(&str, Option<&str>) -> Option<String> + Send + Sync>;

/// Typed request channels served by the authenticated HTTP surface.
pub struct ServerRequests {
    pub action_tx: mpsc::Sender<ControllerRequest>,
    pub bundle_tx: mpsc::Sender<BundleRequest>,
    pub receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    pub preflight_tx: mpsc::Sender<PreflightRequest>,
    pub move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    pub client_state_tx: mpsc::Sender<ClientStateRequest>,
}

impl ServerOptions {
    pub fn new(
        bind: SocketAddr,
        snapshot_rx: watch::Receiver<ViewerSnapshot>,
        conversation_rx: watch::Receiver<
            mj_core::snapshot_map::SnapshotMap<String, BrowserTranscript>,
        >,
        requests: ServerRequests,
    ) -> AnyResult<Self> {
        let cookie_key = generate_cookie_key()?.to_vec();
        let viewer_history = viewer_feed::ViewerHistoryHandle::new()?;
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
            background_task_stop_tx: mpsc::channel(1).0,
            shutdown: CancellationToken::new(),
            image_builds: mj_controller::session_images::ImageBuilds::default(),
            session_ttl: DEFAULT_SESSION_TTL,
            secure_cookie: true,
            tls_config: None,
            viewer_code: generate_viewer_code()?,
            login_token: derive_login_token(&cookie_key),
            cookie_key,
            viewer_revocations: Arc::new(ViewerRevocations::default()),
            viewer_history,
            // An empty token authenticates nothing: the daemon installs the
            // persisted one, and a server without it serves the viewer only.
            api_token: String::new(),
            subagent: None,
            preferences_path: mj_core::go::GoPreferences::path(),
            worker_source_check: None,
            upgrade_gate: mj_controller::upgrade::gate().clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_upgrade_gate(&mut self, gate: Arc<mj_controller::upgrade::Gate>) {
        self.upgrade_gate = gate;
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
    /// out and revokes QR login URLs. This does not load durable logout records;
    /// use `load_cookie_credentials` for the daemon's persisted credentials.
    pub fn set_cookie_key(&mut self, key: Vec<u8>) -> AnyResult<()> {
        anyhow::ensure!(
            key.len() >= COOKIE_KEY_BYTES,
            "cookie signing key must be at least {COOKIE_KEY_BYTES} bytes"
        );
        self.login_token = derive_login_token(&key);
        self.cookie_key = key;
        Ok(())
    }

    /// Load the signing key and durable logout records off the async runtime.
    pub async fn load_cookie_credentials(&mut self, path: PathBuf) -> AnyResult<()> {
        let (key, revocations) = mj_core::runtime::spawn_blocking(move || {
            let key = load_or_create_cookie_key(&path)?;
            let revocations =
                ViewerRevocations::load(path.with_file_name("phone-cookie-revocations.json"))?;
            Ok::<_, anyhow::Error>((key, revocations))
        })
        .await
        .context("load viewer credentials task")??;
        self.set_cookie_key(key)?;
        self.viewer_revocations = Arc::new(revocations);
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

    /// Refuse launches, resumes and moves onto a target the check reports as
    /// having no worker binary. Without it every target is admitted.
    pub fn set_worker_source_check(&mut self, check: WorkerSourceCheck) {
        self.worker_source_check = Some(check);
    }

    /// Read the remembered default from a specific preferences file instead
    /// of the user's own. Tests point this at a temporary path.
    pub fn set_preferences_path(&mut self, path: PathBuf) {
        self.preferences_path = path;
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
    let image_builds = options.image_builds.clone();
    let app = router(options);
    mj_core::diagnostic_stdout!("Mjolnir viewer code: {viewer_code}");
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
    let result = tokio::select! {
        result = &mut serve => result,
        _ = shutdown.cancelled() => {
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(2)));
            serve.await
        }
    }
    .with_context(|| format!("serve web viewer on {bind}"));
    image_builds.finish().await;
    result
}

mod viewer_feed;
mod viewer_types;
pub mod viewer_wire;
pub use viewer_types::*;
mod actions;
pub use actions::*;
mod routes;
use routes::*;
mod handlers;
pub use handlers::validate_new_preflight;
use handlers::*;
mod validation;
pub use validation::phone_prompt_blocks;
use validation::*;
mod errors;
use errors::*;
mod auth;
use auth::*;
pub(crate) use mj_controller::signing_key::{COOKIE_KEY_BYTES, generate_cookie_key};
pub use mj_controller::signing_key::{
    cookie_key_path, install_rustls_crypto_provider, load_or_create_cookie_key,
};
pub use mj_controller::wait_policy::ViewerLifecycleCategory;
mod assets;
use assets::*;
mod config_view;
pub use config_view::*;
mod settings;
use settings::*;

#[cfg(test)]
mod tests;
