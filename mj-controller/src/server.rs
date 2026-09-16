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
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Semaphore;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use mj_core::attachment::{AttachmentRef, AttachmentStore, MAX_IMAGE_BYTES, MAX_IMAGES};
use mj_core::config::{Config, TargetTemplate, project_history_host, validate_id};
use mj_core::elicitation::{ElicitationRequest, ElicitationResponse, MAX_ELICITATION_BYTES};
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSnapshot {
    pub revision: u64,
    pub generated_at: String,
    /// Unix time in milliseconds, refreshed when serving the projection.
    /// Clients use this as the clock for live activity cards.
    #[serde(default)]
    pub server_time_ms: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<ViewerWorkspace>,
    pub sessions: Vec<ViewerSession>,
    pub profiles: Vec<ViewerProfile>,
    pub targets: Vec<ViewerTarget>,
    pub bundles: Vec<ViewerBundle>,
    /// The bounded part of `[review]` needed to report whether review is
    /// armed. Reviewer model and effort remain controller-private.
    #[serde(default)]
    pub review_config: ViewerReviewConfig,
    /// The global `[subagents] enabled` setting. The new-session form uses it
    /// as the default for its per-session sub-agent checkbox.
    #[serde(default)]
    pub subagents_enabled: bool,
    /// One entry per host or fleet that can be probed. Empty until the phone
    /// server's capacity poller has published a reading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity: Vec<ViewerTargetCapacity>,
    /// Recent failed launches, independent of provisional session rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_failures: Vec<ViewerLaunchFailure>,
}

/// Carries the launch failure's reason so a client can show why a session
/// never came up. The reason is the provisioning error chain, the same text
/// the session's `last_error` already publishes through `mj events`; it is not
/// the full local diagnostic file, which can hold credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewerLaunchFailure {
    /// Identifies the notice itself, so the browser can dismiss one. It is not
    /// a session id.
    pub id: String,
    pub workspace_id: String,
    /// The session the failed launch was for, when one had been published.
    /// Absent when the launch failed before any session record existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Why the launch failed, when the action recorded a reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ViewerSnapshot {
    /// Build the public projection. In particular, this never copies profile
    /// homes/environment, SSH hosts/keys, container environment, AWS details,
    /// concrete resource locators, native session IDs, or raw error strings.
    pub fn from_config_state(config: &Config, state: &AppState, revision: u64) -> Self {
        let sessions = state
            .sessions
            .values()
            .map(|session| {
                let incompatible = config
                    .targets
                    .keys()
                    .filter(|target_id| {
                        crate::controller::resume_compatibility(session, config, target_id).is_err()
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let lifecycle = ViewerLifecycleCategory::of(session.state);
                let source = session.project_source(config);
                let subagent = state.subagents.get(&session.id);
                let subagent_session_ids = state
                    .subagents
                    .values()
                    .filter(|child| child.parent_session_id == session.id)
                    .map(|child| child.child_session_id.clone())
                    .collect();
                ViewerSession {
                    capacity_retry: None,
                    id: session.id.clone(),
                    workspace_id: session.workspace_id.clone(),
                    title: session.display_title().to_owned(),
                    subagent_parent_id: subagent.map(|child| child.parent_session_id.clone()),
                    subagent_task_name: subagent.map(|child| child.task_name.clone()),
                    subagent_session_ids,
                    harness_kind: session.harness_kind.id().into(),
                    profile_id: session.last_profile.clone(),
                    bundle_id: session.bundle_id.clone(),
                    target_id: session.target_template_id.clone(),
                    state: session.state.as_str().into(),
                    created_at: session.created_at.clone(),
                    updated_at: session.updated_at.clone(),
                    has_error: session.last_error.is_some()
                        || session.configuration_issue(config).is_some(),
                    configuration_issue: session.configuration_issue(config),
                    // A session that failed to launch (or a close that left it
                    // dead) carries its reason here so a client need not open
                    // the local diagnostic to learn why.
                    launch_error: (session.state == SessionState::Error)
                        .then(|| session.last_error.clone())
                        .flatten(),
                    preview: Vec::new(),
                    queued_prompts: Vec::new(),
                    active_user_shells: Vec::new(),
                    background_tasks: Vec::new(),
                    pending_elicitations: Vec::new(),
                    conversation_available: false,
                    prompt_images_supported: false,
                    incompatible_resume_targets: incompatible.clone(),
                    compatible_resume_targets: config
                        .targets
                        .keys()
                        .filter(|target_id| !incompatible.contains(*target_id))
                        .cloned()
                        .collect(),
                    project_label: source.short,
                    project_key: project_key(&source.key),
                    display_location: session.project_target(config, &session.target_template_id),
                    lifecycle,
                    transitioning: session.state.transition_kind().is_some(),
                    latest_event_ordinal: 0,
                    last_activity_at_ms: None,
                    activity_details: None,
                    activity: String::new(),
                    operation: None,
                    move_recovery: None,
                    chat_phase: ViewerChatPhase::default(),
                    is_idle: false,
                    config_options: Vec::new(),
                    plan_mode_active: None,
                    turn_review: None,
                    available_commands: Vec::new(),
                    // What the durable record alone can justify. The phone server
                    // widens these once it knows whether the session manager holds
                    // the session and what the agent has advertised.
                    capabilities: ViewerSessionCapabilities {
                        open: false,
                        prompt: false,
                        run_shell: false,
                        cancel_turn: false,
                        cancel_operation: false,
                        stop: lifecycle.is_dashboard_visible(),
                        rename: true,
                        resume: !lifecycle.is_dashboard_visible(),
                        move_session: false,
                        set_config: false,
                        set_plan_mode: false,
                    },
                }
            })
            .collect();
        let profiles = config
            .enabled_profiles()
            .map(|(id, profile)| ViewerProfile {
                id: id.to_owned(),
                harness_kind: profile.kind.id().into(),
                quota: None,
            })
            .collect();
        let targets = config
            .targets
            .iter()
            .map(|(id, target)| ViewerTarget {
                id: id.clone(),
                kind: target.kind_name().into(),
                requires_project_directory: matches!(
                    target,
                    TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
                ),
                recent_project_directories: project_history_host(target)
                    .map(|host| {
                        state
                            .project_directories(host)
                            .iter()
                            .map(|directory| directory.to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        let bundles = config
            .bundles
            .iter()
            .map(|(id, bundle)| ViewerBundle {
                id: id.clone(),
                primary_repository: bundle.primary_repo.clone(),
                repositories: bundle
                    .repositories
                    .iter()
                    .map(|repository| ViewerRepository {
                        id: repository.id.clone(),
                        github: repository.github.clone(),
                        destination: repository.destination.to_string_lossy().into_owned(),
                    })
                    .collect(),
            })
            .collect();
        Self {
            revision,
            generated_at: now_unix().to_string(),
            server_time_ms: mj_core::clock::epoch_millis(),
            workspaces: Vec::new(),
            sessions,
            profiles,
            targets,
            bundles,
            review_config: ViewerReviewConfig {
                enabled: config.review.enabled,
                tier: config.review.tier.label().to_owned(),
                profile: config.review.profile.clone(),
            },
            subagents_enabled: config.subagents.enabled,
            capacity: Vec::new(),
            launch_failures: Vec::new(),
        }
    }
}

/// A stable, opaque grouping key for a project.
///
/// The controller's own project identity is a bundle, filesystem path, or Git
/// remote, and this projection publishes neither. A digest groups exactly as
/// well and says nothing: two sessions in the same project share a key, and a
/// key on its own reveals no source.
fn project_key(identity: &str) -> String {
    use sha2::Digest as _;
    let digest = Sha256::digest(identity.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<mj_core::relay::CapacityRetry>,
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    pub title: String,
    /// Parent ownership for a borrowed-target child session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_parent_id: Option<String>,
    /// The stable task label chosen by the parent when it spawned this child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_task_name: Option<String>,
    /// Direct children of this parent. Children are deliberately never nested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_session_ids: Vec<String>,
    pub harness_kind: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_id: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub has_error: bool,
    /// Public identifiers and repair guidance only; never raw runtime errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_issue: Option<String>,
    /// Why a launch failed, for a session that ended in the error state. This
    /// is the same provisioning error text `last_error` already publishes
    /// through `mj events`, surfaced here so `mj sessions`/`mj wait` can show
    /// the reason instead of a bare "failed to launch".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_error: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<ViewerQueuedPrompt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_user_shells: Vec<ViewerUserShell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<ViewerBackgroundTask>,
    /// Form questions the session is blocked on, published so a phone can
    /// answer them. These are the agent's own questions, already visible in
    /// the transcript, so they travel whole rather than redacted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<ElicitationRequest>,
    pub conversation_available: bool,
    /// Whether this session's agent advertised support for image content in
    /// prompts. The viewer offers the image controls only when it did, and the
    /// server refuses images for a session that did not.
    #[serde(default)]
    pub prompt_images_supported: bool,
    /// Target ids this session cannot resume on. Only the ids travel: the
    /// controller's reasons name project paths and SSH hosts, which this
    /// projection deliberately keeps on the controller.
    ///
    /// Retained beside `compatible_resume_targets` so a viewer cached from
    /// before that field existed keeps working through a deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incompatible_resume_targets: Vec<String>,
    /// Target ids this session can resume on, so the browser never has to
    /// subtract one set from another to find out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatible_resume_targets: Vec<String>,
    /// The canonical short source label for this session: a bundle name, path
    /// leaf, or repository name, never a source path itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_label: String,
    /// A stable key for grouping sessions by project. The controller's own
    /// source identity stays private, so what travels is a digest of it:
    /// enough to group by, and nothing to read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    /// The configured target's human-facing project location. This is the
    /// same target projection the terminal uses while a session is running.
    #[serde(default)]
    pub display_location: String,
    pub lifecycle: ViewerLifecycleCategory,
    /// A lifecycle transition temporarily owns this session's conversation.
    /// This remains separate from the coarse lifecycle category so Move can
    /// hide the old transcript while its durable record is still `Running`.
    #[serde(default)]
    pub transitioning: bool,
    /// How far the controller's projection of this session has advanced. A
    /// phone compares it against its own read frontier to know what is unread,
    /// without fetching a transcript to find out.
    #[serde(default)]
    pub latest_event_ordinal: u64,
    /// Durable relay receipt watermark from the materialized projection.
    /// It remains absent when the background snapshot pipeline has not yet
    /// delivered a projection for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<i64>,
    /// Structured live activity, absent when no operational relay snapshot is
    /// available for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_details: Option<ViewerActivityDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<ViewerOperation>,
    /// Safe recovery choices for a failed or cancelled Move. Diagnostics and
    /// checkpoint paths remain on the controller; this contains only the
    /// settings a person may choose again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_recovery: Option<ViewerMoveRecovery>,
    #[serde(default)]
    pub chat_phase: ViewerChatPhase,
    /// Known live activity is idle: no foreground turn, tool, or background work.
    /// Missing operational state must not be presented as confirmed idle.
    #[serde(default)]
    pub is_idle: bool,
    /// What this session is doing, in the words the dashboard row uses:
    /// `Turn 43m36s  Step 12s`, `BG 43m36s`, or `[idle]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// The settings the harness advertised, with the values it accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<ViewerConfigOption>,
    /// Whether plan mode is on, or `None` when this harness has no plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_active: Option<bool>,
    /// The review the daemon is running for this session, if any. A phone
    /// renders the same review the terminal does and resolves it the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_review: Option<ViewerTurnReview>,
    /// The Mjolnir commands this session accepts, published rather than hardcoded
    /// in the browser: a command list kept in two places is a command list that
    /// drifts, which is how `/review` went missing from the phone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_commands: Vec<ViewerMjCommand>,
    pub capabilities: ViewerSessionCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerMoveRecovery {
    pub operation_id: String,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub destination_profile_id: String,
    pub destination_target_template_id: String,
    pub phase: String,
    pub queue: String,
    pub clear_resource_allocation: bool,
    /// The source settings are retained so Resume cannot silently inherit a
    /// partially converted destination record after a failed Move.
    #[serde(default)]
    pub source_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub source_resource_allocation: Option<SessionResourceAllocation>,
    /// The exact destination settings are needed when a queue admission
    /// checkpoint pins retry to the already-provisioned destination.
    #[serde(default)]
    pub destination_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub destination_resource_allocation: Option<SessionResourceAllocation>,
    pub checkpoint_retained: bool,
    pub destination_ready: bool,
    pub queue_admission_started: bool,
    pub queue_admission_finished: bool,
}

impl ViewerMoveRecovery {
    #[must_use]
    pub fn from_operation(operation: &MoveOperation) -> Option<Self> {
        if matches!(operation.phase, MovePhase::Completed) {
            return None;
        }
        Some(Self {
            operation_id: operation.operation_id.clone(),
            source_profile_id: operation.source_profile_id.clone(),
            source_target_template_id: operation.source_target_template_id.clone(),
            destination_profile_id: operation.selection.profile_id.clone().unwrap_or_default(),
            destination_target_template_id: operation
                .selection
                .target_template_id
                .clone()
                .unwrap_or_default(),
            phase: match operation.phase {
                MovePhase::Preparing => "preparing",
                MovePhase::ClosingSource => "closing_source",
                MovePhase::ResumingDestination => "resuming_destination",
                MovePhase::StartingQueue => "starting_queue",
                MovePhase::Completed => "completed",
                MovePhase::Failed => "failed",
                MovePhase::Cancelled => "cancelled",
            }
            .into(),
            queue: match operation.queue {
                ResumeQueueDisposition::Start => "start",
                ResumeQueueDisposition::Discard => "discard",
            }
            .into(),
            clear_resource_allocation: operation.selection.clear_resource_allocation,
            source_additional_mounts: operation.source_additional_mounts.clone(),
            source_resource_allocation: operation.source_resource_allocation.clone(),
            destination_additional_mounts: operation
                .selection
                .additional_mounts
                .clone()
                .unwrap_or_default(),
            destination_resource_allocation: operation.selection.resource_allocation.clone(),
            checkpoint_retained: operation.checkpoint.is_some(),
            destination_ready: operation.destination_target.is_some()
                && operation.destination_native_session_id.is_some(),
            queue_admission_started: operation.queue_admission_started,
            queue_admission_finished: operation.queue_admission_finished,
        })
    }
}

impl ViewerSession {
    /// Apply a resolved controller source while keeping paths and remotes out
    /// of the public projection.
    pub fn set_project_source(&mut self, source: &ProjectSourceIdentity) {
        self.project_label = source.short.clone();
        self.project_key = project_key(&source.key);
    }
}

// One wire representation for the UI and native API activity facts.
pub use crate::database::{
    ApiActivityDetails as ViewerActivityDetails, ApiActivityKind as ViewerActivityKind,
};

/// One Mjolnir command a phone may offer for this session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerMjCommand {
    pub name: String,
    pub description: String,
    /// Whether Mjolnir handles this command locally or forwards it to the
    /// active agent.
    pub source: ViewerCommandSource,
    /// What the argument is called, when the command takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewerCommandSource {
    Mj,
    Agent,
}

/// Public review configuration: exactly what `/review status` needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewConfig {
    pub enabled: bool,
    pub tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// A turn review as a phone renders it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTurnReview {
    /// `quick` or `extended`.
    pub tier: String,
    /// What the review is doing, in one line.
    pub status: String,
    /// One row per reviewing agent: its label and where it has got to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<ViewerReviewRole>,
    /// Present once the review has reached a verdict the user must answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ViewerReviewVerdict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewRole {
    pub label: String,
    /// `pending`, `running`, `done`, `findings`, or `failed`.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewVerdict {
    /// `clean`, `findings`, or `failed`.
    pub kind: String,
    /// The findings, or the failure's reason.
    pub text: String,
    /// The resolutions this verdict accepts: `forward`, `dismiss`, `cancel`.
    /// A phone shows the rest disabled rather than hiding them, so the buttons
    /// do not move under a thumb.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
}

impl ViewerTurnReview {
    /// The phone's view of one review the daemon is running.
    #[must_use]
    pub fn from_runtime(review: &crate::review_host::RuntimeReviewView) -> Self {
        Self {
            tier: review.tier.label().to_owned(),
            status: review.status.clone(),
            roles: review
                .roles
                .iter()
                .map(|role| ViewerReviewRole {
                    label: role.label.clone(),
                    state: role.state.label().to_owned(),
                })
                .collect(),
            verdict: review.verdict.as_ref().map(|verdict| ViewerReviewVerdict {
                kind: match verdict.kind {
                    crate::review_host::VerdictKind::Clean => "clean",
                    crate::review_host::VerdictKind::Findings => "findings",
                    crate::review_host::VerdictKind::Failed => "failed",
                }
                .to_owned(),
                text: verdict.text.clone(),
                allowed: verdict
                    .allowed
                    .iter()
                    .filter_map(resolution_name)
                    .map(str::to_owned)
                    .collect(),
            }),
        }
    }
}

/// The wire name of one resolution, shared by the projection and the action
/// that performs it, so a button's name is the name the server accepts.
#[must_use]
pub fn resolution_name(resolution: &mj_core::review::driver::Resolution) -> Option<&'static str> {
    match resolution {
        mj_core::review::driver::Resolution::Forwarded => Some("forward"),
        mj_core::review::driver::Resolution::Dismissed => Some("dismiss"),
        mj_core::review::driver::Resolution::Cancelled => Some("cancel"),
        // Not resolutions a surface asks for: the review reaches these itself.
        mj_core::review::driver::Resolution::NothingToReview
        | mj_core::review::driver::Resolution::CoverageStarted => None,
    }
}

/// The resolution a phone's button asked for.
#[must_use]
pub fn resolution_from_name(name: &str) -> Option<mj_core::review::driver::Resolution> {
    match name {
        "forward" => Some(mj_core::review::driver::Resolution::Forwarded),
        "dismiss" => Some(mj_core::review::driver::Resolution::Dismissed),
        "cancel" => Some(mj_core::review::driver::Resolution::Cancelled),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerWorkspace {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQueuedPrompt {
    pub id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerUserShell {
    pub id: String,
    pub command: String,
    pub started_at_ms: Option<i64>,
}

/// One command the active agent left running in the background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerBackgroundTask {
    pub id: String,
    pub command: String,
    pub started_at_ms: i64,
    pub can_stop: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerProfile {
    pub id: String,
    pub harness_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<ViewerQuota>,
}

/// One usage window a harness reports, such as a weekly or five-hour limit.
///
/// `percent_used` is the figure a person acts on, so it travels as a number
/// rather than inside a sentence. The controller computes headroom; this is
/// its complement, because a bar fills as a limit is consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQuotaWindow {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Whether this window is on course to run out before it resets. The
    /// controller already computes this; a phone should not have to.
    pub projects_exhaustion_before_reset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQuota {
    /// One-line rendering, kept so a viewer cached from before the structured
    /// windows existed keeps working. The Quota page renders `windows`.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<ViewerQuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    pub stale: bool,
    /// When the reading was taken. A pulled view delivered by push cannot be
    /// told from a current one without its age, so this is not optional.
    #[serde(default)]
    pub refreshed_at_epoch_seconds: u64,
    /// Error state only. Raw vendor errors may contain paths or account data
    /// and remain on the controller.
    pub has_error: bool,
}

/// What one host or fleet has, and how fresh the reading is.
///
/// Every field that carries a reading is optional, and `sampled_at_epoch_seconds`
/// is present whenever any of them is: a reading without its age cannot be
/// told from a stale one, which is exactly the case where it matters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTargetCapacity {
    pub id: String,
    /// The host or fleet as a person names it. Never a locator, an address or
    /// a full path.
    pub label: String,
    pub target_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_cores: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    /// How many machines a fleet is running. Absent for a plain host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_machines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at_epoch_seconds: Option<u64>,
    pub refreshing: bool,
    pub stale: bool,
    /// Whether the last probe failed. The probe's own message names hosts and
    /// commands, so it stays on the controller.
    pub has_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTarget {
    pub id: String,
    pub kind: String,
    pub requires_project_directory: bool,
    /// Recent raw project directories for this target's physical host. Managed
    /// targets intentionally publish an empty list because they select a
    /// configured bundle rather than a host checkout.
    #[serde(default)]
    pub recent_project_directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerBundle {
    pub id: String,
    pub primary_repository: String,
    pub repositories: Vec<ViewerRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerRepository {
    pub id: String,
    pub github: Option<String>,
    pub destination: String,
}

/// What a phone may do with one session, as the controller sees it.
///
/// The viewer renders a control because a flag here is true, and for no other
/// reason. Deciding legality in the browser means copying controller policy
/// into JavaScript, where it drifts silently: the browser cannot know that a
/// session is unmanaged, that a lifecycle operation holds it, or that the
/// harness never advertised the option a control would change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSessionCapabilities {
    pub open: bool,
    pub prompt: bool,
    pub run_shell: bool,
    /// Cancel the turn the agent is working on now, leaving the session alive.
    pub cancel_turn: bool,
    /// Cancel the provision, resume or stop currently running.
    pub cancel_operation: bool,
    pub stop: bool,
    pub rename: bool,
    pub resume: bool,
    /// Prepare and confirm a daemon-owned move to a compatible profile or
    /// target. The browser must never compose Stop and Resume itself.
    #[serde(default)]
    pub move_session: bool,
    pub set_config: bool,
    pub set_plan_mode: bool,
}

/// The small set of states a phone reasons about, alongside the precise state.
///
/// A phone groups and filters by this; it shows the precise `state` string as
/// the word it prints. Collapsing here rather than in the browser keeps one
/// definition of "live" in the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerLifecycleCategory {
    Live,
    Starting,
    Stopping,
    Stopped,
    Failed,
}

impl ViewerLifecycleCategory {
    const fn of(state: SessionState) -> Self {
        match state {
            SessionState::Provisioning => Self::Starting,
            SessionState::Running | SessionState::Disconnected | SessionState::Checkpointing => {
                Self::Live
            }
            SessionState::Closing | SessionState::Destroying => Self::Stopping,
            SessionState::Stopped => Self::Stopped,
            SessionState::Lost | SessionState::Error | SessionState::DestroyedWithDataLoss => {
                Self::Failed
            }
        }
    }

    /// Whether this session belongs on the dashboard. Stopped and failed
    /// sessions belong to the resume flow instead, which is where a person can
    /// do something about them.
    pub const fn is_dashboard_visible(self) -> bool {
        matches!(self, Self::Live | Self::Starting | Self::Stopping)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerOperationKind {
    Create,
    Resume,
    Move,
    Stop,
    Destroy,
    Cleanup,
    Checkpoint,
}

impl ViewerOperationKind {
    pub const fn transition_kind(self) -> Option<SessionTransitionKind> {
        match self {
            Self::Create => Some(SessionTransitionKind::Starting),
            Self::Resume => Some(SessionTransitionKind::Resuming),
            Self::Move => Some(SessionTransitionKind::Moving),
            Self::Stop => Some(SessionTransitionKind::Stopping),
            Self::Destroy | Self::Cleanup => Some(SessionTransitionKind::Destroying),
            // Checkpointing is an ordinary live-session operation. It must
            // not replace a readable conversation with a placeholder.
            Self::Checkpoint => None,
        }
    }
}

/// One stage of a running operation, with the clock it started on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerOperationStage {
    pub label: String,
    pub started_at_epoch_seconds: u64,
}

/// A provision, resume, stop or checkpoint the controller is running now.
///
/// A phone that asked for one of these got `202 Accepted` and an identifier
/// rather than a result, because the work outlives the request. This is how it
/// finds out what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerOperation {
    pub id: String,
    pub session_id: String,
    pub kind: ViewerOperationKind,
    pub started_at_epoch_seconds: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<ViewerOperationStage>,
    /// Controller-authored and already meant for a person to read, unlike the
    /// error text this projection keeps on the controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    pub cancellable: bool,
}

/// What the agent is doing, mirroring `RelayExecutionState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerChatPhase {
    #[default]
    Idle,
    Running,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One setting the harness advertised, with the values it will accept.
///
/// The browser completes `/model` and `/effort` from this rather than from a
/// list of its own, so a harness that offers something new needs no viewer
/// change, and a viewer can never offer a value the harness would refuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerConfigOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub choices: Vec<ViewerConfigChoice>,
}

/// The complete set of operations a phone may ask the controller to perform.
/// Secret/config editing is intentionally not representable here, and the one
/// destructive variant, `ForceClose`, is not representable on the wire: it is
/// `#[serde(skip)]` so only in-process callers such as the HTTP API can build
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControllerAction {
    New {
        #[serde(default)]
        create_managed_worktree: Option<bool>,
        /// None follows the global `[subagents] enabled` setting.
        #[serde(default)]
        mjolnir_subagents: Option<bool>,
        /// Which workspace the session belongs to. Optional on the wire so a
        /// viewer cached from before workspaces reached the phone still parses,
        /// but a controller holding more than one workspace refuses an empty
        /// one rather than guessing.
        #[serde(default)]
        workspace_id: String,
        profile_id: String,
        bundle_id: String,
        target_id: String,
        /// Absent means "derive it", which is what the terminal does.
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        project_directory: Option<PathBuf>,
        /// The repositories the person was shown as having uncommitted changes
        /// and chose to launch over anyway.
        ///
        /// This names them rather than being a bare yes, so an acknowledgement
        /// cannot be replayed against a set the person never saw: if a
        /// different repository has gone dirty since the preflight, the launch
        /// stops and asks again.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dirty_ack: Vec<String>,
    },
    /// Give a session a new title. The terminal calls this a rename.
    Rename {
        session_id: String,
        title: String,
    },
    /// Stop the turn the agent is working on, leaving the session alive. This
    /// is not `Cancel`, which stops a provision, resume or stop.
    CancelTurn {
        session_id: String,
    },
    /// Change one setting the harness advertised, such as `model` or `effort`.
    SetConfig {
        session_id: String,
        key: String,
        value: String,
    },
    /// Turn plan mode on or off. The harness decides how, which is why this
    /// carries an intent rather than a mode id.
    SetPlanMode {
        session_id: String,
        active: bool,
    },
    RefreshQuota {
        profile_id: String,
    },
    RefreshCapacity {
        target_id: String,
    },
    Resume {
        session_id: String,
        workspace_id: String,
        profile_id: String,
        target_id: String,
        queue: ResumeQueueDisposition,
        /// A failed Move supplies the settings recorded before source
        /// teardown. Ordinary Resume requests leave these absent and retain
        /// the historical inheritance behavior.
        #[serde(default)]
        additional_mounts: Option<Vec<AdditionalMount>>,
        #[serde(default)]
        resource_allocation: Option<SessionResourceAllocation>,
    },
    /// Confirm a previously prepared move. Preparation is a separate
    /// authenticated request so changing the destination cannot be smuggled
    /// into a confirmation from an older browser form.
    Move {
        request: MoveSessionRequest,
    },
    Open {
        session_id: String,
    },
    Prompt {
        session_id: String,
        text: String,
        /// Images to send with the prompt. The controller turns each one into
        /// the ACP image content block its prompt path already speaks.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ViewerPromptImage>,
    },
    RunShell {
        session_id: String,
        command: String,
    },
    CancelShell {
        session_id: String,
        shell_command_id: String,
    },
    Close {
        session_id: String,
    },
    /// Destroy a session without checkpointing it: the live target is torn
    /// down, the recovery archive is removed, and sub-agent children are
    /// destroyed first. This is irreversible.
    ///
    /// Skipped by serde on purpose. The browser viewer posts this enum to
    /// `/actions`, so a wire request must never be able to name this variant;
    /// it is reachable only from the HTTP API, which builds it in process.
    #[serde(skip)]
    ForceClose {
        session_id: String,
    },
    Cancel {
        session_id: String,
    },
    /// Review the turn this session just finished.
    StartReview {
        session_id: String,
    },
    /// Forward the findings, dismiss them, or cancel the open review.
    ResolveReview {
        session_id: String,
        /// `forward`, `dismiss`, or `cancel`.
        resolution: String,
    },
    RemoveQueuedPrompt {
        session_id: String,
        queue_id: String,
    },
    /// Answer one of the session's pending form questions.
    RespondElicitation {
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    },
}

/// One image a phone attached to a prompt. Legacy callers may send inline
/// base64 data; the server normalizes it into an attachment before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerPromptImage {
    /// Legacy inline image bytes. New browser uploads and normalized inline
    /// prompts carry an attachment reference and leave this empty.
    #[serde(default)]
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// Session-scoped, immutable image bytes. The worker resolves this just
    /// before dispatch, keeping browser actions and durable commands small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentRef>,
}

/// The controller's answer to one phone action.
///
/// The answer means "accepted", not "finished": provisioning, resume and close
/// run for minutes, and a phone on a mobile network drops a request held open
/// that long. How the action then goes travels in snapshots — session state,
/// queued prompts, transcripts, and `has_error`.
///
/// Only the outcome crosses this boundary. The controller's own failure text
/// names profile homes, project paths and SSH hosts, so it stays on the
/// controller and the phone gets a fixed message it can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Admitted and now running; watch the snapshot for what happens next.
    ///
    /// A `new` action carries the published session id, which is the only way
    /// its caller learns what it just created.
    Accepted { session_id: Option<String> },
    /// The controller already runs as many phone actions as it allows.
    Busy,
    /// This session already has an operation running.
    SessionBusy,
    /// A cancel found no operation to cancel.
    NotCancellable,
    /// The controller could not start the action at all.
    Failed,
}

impl ActionOutcome {
    /// Admitted, with no session id to report.
    pub const fn accepted() -> Self {
        Self::Accepted { session_id: None }
    }

    /// The published session id, when this outcome carries one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Accepted { session_id } => session_id.as_deref(),
            _ => None,
        }
    }

    /// The reply an outcome owes the phone, or `None` when it was accepted.
    fn rejection(&self) -> Option<ApiError> {
        match self {
            Self::Accepted { .. } => None,
            Self::Busy => Some(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "the controller is at its concurrent action limit; retry shortly",
            )),
            Self::SessionBusy => Some(ApiError::new(
                StatusCode::CONFLICT,
                "another operation is already running for this session",
            )),
            Self::NotCancellable => Some(ApiError::new(
                StatusCode::CONFLICT,
                "the session has no cancellable operation",
            )),
            Self::Failed => Some(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the controller could not start this action",
            )),
        }
    }
}

#[derive(Debug)]
pub struct ControllerRequest {
    pub action: ControllerAction,
    pub reply: tokio::sync::oneshot::Sender<ActionOutcome>,
}

/// A phone request to create or reuse a quick project bundle. This has its
/// own channel because bundle creation returns a durable id and must publish a
/// config snapshot before the HTTP request can succeed; [`ControllerAction`]
/// intentionally carries only action admission outcomes.
#[derive(Debug)]
pub struct BundleRequest {
    pub source: String,
    pub reply: tokio::sync::oneshot::Sender<Result<String, BundleFailure>>,
}

/// Safe failure classes for bundle creation. Detailed controller errors stay
/// in daemon logs; a browser only needs to know whether to fix its source or
/// report a server-side failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleFailure {
    InvalidSource,
    Controller,
}

/// A phone acknowledging how far it has read a conversation.
///
/// This deliberately is not a `ControllerAction`: the viewer posts it after
/// every conversation fetch, and a fetch follows every revision. Routing it
/// through the action pipeline made each receipt reload the controller, bump
/// the revision and broadcast a snapshot, which triggered the next fetch, so
/// viewer and controller never went quiet; it also consumed the session's
/// single action slot, intermittently rejecting real actions. A receipt
/// therefore travels on its own channel and only persists one cursor field.
/// A phone asking whether a session it is about to create would launch
/// cleanly, and which network sources it will use first.
///
/// This is not a `ControllerAction`: it starts nothing, it takes no session
/// slot, and it must answer before the person has decided anything. It also
/// needs the controller, because resolving a local repository's configured
/// remotes is a fact about the disk rather than about the projection.
///
/// Resume preflights share this channel, and so the concurrency cap on it,
/// because they do the same kind of work on the same disk.
#[derive(Debug)]
pub enum PreflightRequest {
    New(NewPreflightRequest),
    Resume(ResumePreflightRequest),
}

#[derive(Debug)]
pub struct NewPreflightRequest {
    pub bundle_id: String,
    pub target_id: String,
    pub project_directory: Option<PathBuf>,
    pub remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    pub reply: tokio::sync::oneshot::Sender<Result<PreflightNew, PreflightFailure>>,
}

/// A resume preflight for one stopped session and one destination target. It
/// travels on the same channel and under the same concurrency cap as the
/// new-session preflight because it does the same kind of work: reading a
/// working tree and asking a remote about itself.
#[derive(Debug)]
pub struct ResumePreflightRequest {
    pub session_id: String,
    pub target_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<PreflightResume, PreflightFailure>>,
}

/// What a resume preflight found.
///
/// `Ready` covers every resume that changes nothing about where repository
/// content comes from. `ConvertingRawCheckout` means this resume moves a
/// local checkout into an isolated workspace, and carries the preview the
/// person has to confirm. `Unavailable` reports why the conversion cannot be
/// planned, in the plan's own words, because that message says what to do
/// about it (add a remote, commit a submodule) and the browser has no other
/// way to learn it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PreflightResume {
    Ready,
    ConvertingRawCheckout {
        preview: Box<mj_core::state::RawConversionPreview>,
    },
    Unavailable {
        detail: String,
    },
}

/// A move preparation is intentionally separate from action admission. It
/// performs read-only compatibility checks and returns the exact fingerprint
/// the later confirmation must echo; it never interrupts the source session.
#[derive(Debug)]
pub struct MovePreparationRequest {
    pub selection: MoveSelection,
    pub reply: tokio::sync::oneshot::Sender<Result<MovePreparation, String>>,
}

/// A preflight can fail because the requested bare directory is unusable, an
/// isolated repository lacks a usable network source, or the controller-side
/// check itself could not complete. The HTTP surface keeps those outcomes
/// distinct without carrying filesystem, Git, or SSH details to the phone.
#[derive(Debug)]
pub enum PreflightFailure {
    Validation,
    /// A configured isolated-session repository cannot be used as a network
    /// source. The detail is safe for the phone and tells the person how to
    /// choose the supported raw-local path instead.
    InvalidRepository(String),
    Controller(String),
}

/// One configured repository's network clone and publication destinations.
/// URLs have already been passed through the shared display sanitizer before
/// they reach a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightRepository {
    pub id: String,
    pub fetch_url: String,
    pub default_branch: String,
    pub push_urls: Vec<String>,
}

/// What a preflight found. Isolated sessions expose their complete network
/// source plan so the person can review it before creation. Raw-local targets
/// leave the plan empty because they use the selected checkout directly;
/// isolated targets set `local_changes_excluded` to make the copy boundary
/// explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightNew {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub managed_worktree: mj_core::state::ManagedWorktreeOptions,
    #[serde(default)]
    pub remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    #[serde(default)]
    pub dirty_repositories: Vec<String>,
    #[serde(default)]
    pub remote_repositories: Vec<PreflightRepository>,
    pub local_changes_excluded: bool,
}

/// What a phone asks about, or stores against, its own identity.
///
/// These travel on their own channel rather than as actions, for the reason a
/// read receipt does: they are frequent, they start nothing, and routing them
/// through the action pipeline would consume the session's single action slot
/// and reload the controller on every keystroke.
#[derive(Debug)]
pub enum ClientStateRequest {
    Read {
        client_id: String,
        session_id: String,
        reply: tokio::sync::oneshot::Sender<Result<ViewerClientState, String>>,
    },
    SaveDraft {
        client_id: String,
        session_id: String,
        draft: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    MarkWorkspaceRead {
        client_id: String,
        workspace_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    History {
        session_id: String,
        query: String,
        scope: String,
        reply: tokio::sync::oneshot::Sender<Result<ViewerPromptHistory, String>>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerClientState {
    pub draft: String,
    pub through_event_ordinal: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerPromptHistory {
    pub entries: Vec<String>,
    /// Whether the search stopped before it ran out of history, so a phone can
    /// say the answer is partial rather than presenting it as complete.
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ReadReceiptRequest {
    pub client_id: String,
    pub session_id: String,
    pub through: u64,
    pub reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// A phone request to stop one currently projected background task.
///
/// This is intentionally not a [`ControllerAction`]. The request is already
/// validated against the current operational snapshot by the HTTP handler,
/// then the controller resolves the live session handle and waits for the
/// provider acknowledgement in a supervised task.
#[derive(Debug)]
pub struct BackgroundTaskStopRequest {
    pub session_id: String,
    pub background_task_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<(), BackgroundTaskStopFailure>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStopFailure {
    /// The session manager could not resolve the live session handle.
    SessionUnavailable,
    /// The provider or relay rejected the stop request.
    Provider,
    /// The stop task itself failed before reaching the provider.
    Internal,
}

#[derive(Clone)]
struct ServerState {
    snapshot_rx: watch::Receiver<ViewerSnapshot>,
    conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    action_tx: mpsc::Sender<ControllerRequest>,
    bundle_tx: mpsc::Sender<BundleRequest>,
    receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    preflight_tx: mpsc::Sender<PreflightRequest>,
    move_preparation_tx: mpsc::Sender<MovePreparationRequest>,
    client_state_tx: mpsc::Sender<ClientStateRequest>,
    dictation_tx: mpsc::Sender<DictationRequest>,
    background_task_stop_tx: mpsc::Sender<BackgroundTaskStopRequest>,
    dictation_permits: Arc<Semaphore>,
    dictation_probe_permits: Arc<Semaphore>,
    shutdown: CancellationToken,
    viewer_code: Arc<str>,
    login_token: Arc<str>,
    cookie_key: Arc<[u8]>,
    session_ttl: Duration,
    secure_cookie: bool,
    code_guard: Arc<Mutex<CodeGuard>>,
    api_token: Arc<str>,
    subagent: Option<Arc<dyn api::SubagentBackend>>,
}

/// Online-guessing defence for the deliberately small viewer code.
///
/// Five wrong codes lock the endpoint, and each further lockout lasts twice as
/// long as the one before it, up to an hour. The escalation count survives an
/// expired lockout, so a script cannot recover its full allowance by waiting;
/// a correct code clears the whole history, so one mistyped digit still costs
/// at most a single short wait.
#[derive(Debug, Default)]
struct CodeGuard {
    failures: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
}

impl CodeGuard {
    fn locked_at(&mut self, now: Instant) -> bool {
        match self.locked_until {
            Some(until) if now < until => true,
            Some(_) => {
                // The wait is served: allow a fresh run of attempts, but keep
                // the escalation history that makes the next wait longer.
                self.locked_until = None;
                self.failures = 0;
                false
            }
            None => false,
        }
    }

    fn record_failure_at(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures < MAX_CODE_FAILURES {
            return;
        }
        self.failures = 0;
        self.lockouts = self.lockouts.saturating_add(1);
        self.locked_until = Some(now + code_lockout(self.lockouts));
    }
}

/// Doubling backoff, capped so the owner of a locked-out server is never shut
/// out for longer than it takes to notice.
fn code_lockout(lockouts: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(lockouts.saturating_sub(1))
        .unwrap_or(u32::MAX);
    CODE_LOCKOUT_BASE
        .saturating_mul(multiplier)
        .min(CODE_LOCKOUT_CAP)
}

fn router(options: ServerOptions) -> Router {
    let state = ServerState {
        snapshot_rx: options.snapshot_rx,
        conversation_rx: options.conversation_rx,
        action_tx: options.action_tx,
        bundle_tx: options.bundle_tx,
        receipt_tx: options.receipt_tx,
        preflight_tx: options.preflight_tx,
        move_preparation_tx: options.move_preparation_tx,
        client_state_tx: options.client_state_tx,
        dictation_tx: options.dictation_tx,
        background_task_stop_tx: options.background_task_stop_tx,
        dictation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DICTATIONS)),
        dictation_probe_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DICTATIONS)),
        shutdown: options.shutdown,
        viewer_code: options.viewer_code.into(),
        login_token: options.login_token.into(),
        cookie_key: options.cookie_key.into(),
        session_ttl: options.session_ttl,
        secure_cookie: options.secure_cookie,
        code_guard: Arc::new(Mutex::new(CodeGuard::default())),
        api_token: options.api_token.into(),
        subagent: options.subagent,
    };
    let protected = Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/conversations/{session_id}", get(conversation))
        .route(
            "/api/conversations/{session_id}/read",
            post(mark_conversation_read),
        )
        .route("/api/events", get(events))
        .route("/api/bundles", post(create_bundle))
        .route("/api/preflight/new", post(preflight_new))
        .route("/api/preflight/resume", post(preflight_resume))
        .route("/api/moves/prepare", post(prepare_move))
        .route("/api/sessions/{session_id}/client-state", get(client_state))
        .route(
            "/api/sessions/{session_id}/dictation",
            get(dictation_availability).post(upload_dictation),
        )
        .route(
            "/api/sessions/{session_id}/background-tasks/stop",
            post(stop_background_task),
        )
        .route(
            "/api/sessions/{session_id}/attachments",
            post(upload_attachment).layer(DefaultBodyLimit::max(MAX_ATTACHMENT_UPLOAD_BYTES)),
        )
        .route(
            "/api/sessions/{session_id}/draft",
            put(save_draft).layer(DefaultBodyLimit::max(MAX_DRAFT_BYTES)),
        )
        .route("/api/sessions/{session_id}/history", get(prompt_history))
        .route(
            "/api/workspaces/{workspace_id}/read",
            post(mark_workspace_read),
        )
        .route(
            "/api/actions",
            post(action).layer(DefaultBodyLimit::max(MAX_PROMPT_BODY_BYTES)),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));
    Router::new()
        .route("/", get(viewer))
        .route("/login", get(viewer))
        .route("/viewer.css", get(viewer_css))
        .route("/viewer.js", get(viewer_js))
        .route("/voice-worklet.js", get(voice_worklet_js))
        .route("/voice-worker.js", get(voice_worker_js))
        .route("/markdown.js", get(markdown_js))
        .route("/tool-output.js", get(tool_output_js))
        .route("/manifest.webmanifest", get(manifest))
        .route("/service-worker.js", get(service_worker))
        .route("/icon.svg", get(icon))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/maskable-512.png", get(maskable_512))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/fonts/jetbrains-mono.woff2", get(mono_font))
        .route("/auth/session", post(create_session).delete(clear_session))
        .route("/auth/login", get(create_session_from_query))
        .merge(protected)
        .nest("/api/v1", api::router(state.clone()))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

async fn require_session(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Result<Response<Body>, ApiError> {
    let cookie = request
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME));
    if cookie.is_some_and(|value| session_cookie_valid(&state.cookie_key, value, now_unix())) {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::unauthorized())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginQuery {
    token: String,
}

async fn create_session_from_query(
    State(state): State<ServerState>,
    Query(query): Query<LoginQuery>,
) -> Result<Response<Body>, ApiError> {
    if !constant_time_eq(state.login_token.as_bytes(), query.token.trim().as_bytes()) {
        return Err(ApiError::unauthorized());
    }
    let mut response = issue_session_cookie(&state, StatusCode::SEE_OTHER)?;
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_static("/"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn create_session(
    State(state): State<ServerState>,
    Json(request): Json<LoginRequest>,
) -> Result<Response<Body>, ApiError> {
    if code_locked(&state) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many incorrect codes; wait and try again",
        ));
    }
    if !constant_time_eq(state.viewer_code.as_bytes(), request.code.trim().as_bytes()) {
        record_code_failure(&state);
        return Err(ApiError::unauthorized());
    }
    reset_code_failures(&state);
    issue_session_cookie(&state, StatusCode::NO_CONTENT)
}

fn issue_session_cookie(
    state: &ServerState,
    status: StatusCode,
) -> Result<Response<Body>, ApiError> {
    let ephemeral = state.session_ttl.is_zero();
    let validity = if ephemeral {
        EPHEMERAL_SESSION_TTL
    } else {
        state.session_ttl
    };
    let value = signed_cookie_value(
        &state.cookie_key,
        &generate_viewer_id().map_err(|_| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cookie creation failed")
        })?,
        now_unix().saturating_add(validity.as_secs()),
    );
    let cookie = session_cookie_header(
        &value,
        (!ephemeral).then_some(validity.as_secs()),
        state.secure_cookie,
    )?;
    let mut response = status.into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    Ok(response)
}

async fn clear_session(State(state): State<ServerState>) -> Response<Body> {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, clear_cookie_header(state.secure_cookie));
    response
}

async fn snapshot(State(state): State<ServerState>) -> Response<Body> {
    let mut projection = state.snapshot_rx.borrow().clone();
    // A quiet session can keep the same projection for hours. Clock anchors
    // describe response time, not the last time that projection changed.
    projection.server_time_ms = mj_core::clock::epoch_millis();
    let mut response = Json(projection).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Optimize and install one browser image off the async request task. The
/// request body is deliberately raw bytes: base64 would inflate the upload,
/// and the response contains only the small immutable reference the prompt
/// needs.
async fn upload_attachment(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Json<ViewerPromptImage>, ApiError> {
    validate_public_id(&session_id)?;
    let prompt_images_supported = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?.prompt_images_supported
    };
    if !prompt_images_supported {
        return Err(ApiError::bad_request(
            "this session does not support image prompts",
        ));
    }
    if body.is_empty() {
        return Err(ApiError::bad_request("image upload must not be empty"));
    }

    let result = tokio::task::spawn_blocking(move || {
        let optimized = optimize_image(&body).map_err(|_| {
            ApiError::bad_request("unsupported image format or image could not be decoded")
        })?;
        if optimized.bytes.is_empty() || optimized.bytes.len() > MAX_IMAGE_BYTES {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the image optimizer returned an invalid image size",
            ));
        }
        let reference = AttachmentRef::new(
            &optimized.bytes,
            optimized.mime_type.clone(),
            optimized.width,
            optimized.height,
        )
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not create image attachment",
            )
        })?;
        let store = AttachmentStore::controller(&session_id).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not open the image attachment store",
            )
        })?;
        store.install(&reference, &optimized.bytes).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store the image attachment",
            )
        })?;
        Ok(ViewerPromptImage {
            data_base64: String::new(),
            mime_type: reference.mime_type.clone(),
            width: reference.width,
            height: reference.height,
            attachment: Some(reference),
        })
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the server could not process the image upload",
        )
    })??;

    Ok(Json(result))
}

/// Hand one validated action to the controller and answer as soon as the
/// controller accepts it. Waiting for completion would hold the request open
/// for the whole of a provision, resume or close, which mobile networks end
/// long before the work does — reporting failure for an action that is in fact
/// still running.
async fn action(
    State(state): State<ServerState>,
    Json(action): Json<ControllerAction>,
) -> Result<StatusCode, ApiError> {
    validate_action(&action, &state.snapshot_rx.borrow())?;
    let action = decode_prompt_images_off_task(action).await?;
    let (reply, outcome) = tokio::sync::oneshot::channel();
    state
        .action_tx
        .send(ControllerRequest { action, reply })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    let outcome = outcome
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    match outcome.rejection() {
        Some(rejection) => Err(rejection),
        None => Ok(StatusCode::ACCEPTED),
    }
}

const MAX_BUNDLE_SOURCE_CHARS: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBundleRequest {
    source: String,
}

#[derive(Debug, Serialize)]
struct CreateBundleResponse {
    bundle_id: String,
}

/// Create a quick bundle through the controller's dedicated persistence path.
/// The control loop publishes the resulting config before resolving `reply`,
/// so a successful response can immediately use the returned bundle id in the
/// next new-session request.
async fn create_bundle(
    State(state): State<ServerState>,
    Json(request): Json<CreateBundleRequest>,
) -> Result<Json<CreateBundleResponse>, ApiError> {
    Ok(Json(CreateBundleResponse {
        bundle_id: create_quick_bundle(&state, request.source).await?,
    }))
}

/// Create or reuse the quick bundle for one repository source.
///
/// Both the viewer's `/api/bundles` route and the documented API's session
/// creation need this, and a caller that supplies a project directory instead
/// of a bundle id must get exactly the bundle the viewer would have made.
async fn create_quick_bundle(state: &ServerState, source: String) -> Result<String, ApiError> {
    if source.trim().is_empty() {
        return Err(ApiError::bad_request("repository source cannot be empty"));
    }
    if source.chars().count() > MAX_BUNDLE_SOURCE_CHARS {
        return Err(ApiError::bad_request(
            "repository source must contain 1024 characters or fewer",
        ));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .bundle_tx
        .send(BundleRequest { source, reply })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|failure| match failure {
            BundleFailure::InvalidSource => ApiError::bad_request(
                "use a GitHub owner/repository or an existing Git checkout on the controller host",
            ),
            BundleFailure::Controller => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the controller could not create the bundle",
            ),
        })
}

#[derive(Debug, Deserialize)]
struct ConversationQuery {
    after_seq: Option<u64>,
    presentation_key: Option<String>,
}

async fn conversation(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<BrowserTranscript>, ApiError> {
    validate_public_id(&session_id)?;
    let transitioning = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?.transitioning
    };
    if transitioning {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conversation unavailable while the session is transitioning",
        ));
    }
    let conversations = state.conversation_rx.borrow();
    let transcript = conversations
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("conversation unavailable"))?;
    let mut response = transcript.clone();
    // Presentation grouping can remove or reorder rows without moving the
    // relay cursor. A client carrying a key from the previous Rich topology
    // must replace its append-only DOM when that topology changed.
    let presentation_mismatch = query
        .presentation_key
        .as_deref()
        .is_some_and(|key| key != transcript.presentation_key);
    if let Some(after) = query.after_seq {
        response.reset = presentation_mismatch || after < response.window_start_seq;
        if !response.reset {
            response.entries.retain(|entry| entry.updated_seq > after);
        }
    } else if presentation_mismatch {
        response.reset = true;
    }
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    through: u64,
}

async fn mark_conversation_read(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ReadRequest>,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&session_id)?;
    let transitioning = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?.transitioning
    };
    if transitioning {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conversation unavailable while the session is transitioning",
        ));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    let client_id = viewer_client_id(&state, &headers).ok_or_else(ApiError::unauthorized)?;
    state
        .receipt_tx
        .send(ReadReceiptRequest {
            client_id,
            session_id,
            through: request.through,
            reply,
        })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|_| ApiError::new(StatusCode::CONFLICT, "read receipt failed"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopBackgroundTaskRequest {
    background_task_id: String,
}

/// Ask the live session actor to stop one task the current projection still
/// shows. The snapshot check is intentionally repeated at admission time:
/// a task may have completed, or lost its provider stop capability, between
/// the browser rendering its button and the POST arriving.
async fn stop_background_task(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<StopBackgroundTaskRequest>,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&session_id)?;
    // Task ids are opaque provider ids (the worker currently uses values such
    // as `terminal:<id>`), so they do not use the config id alphabet. The
    // request is still bounded and must name a task in the current snapshot.
    if request.background_task_id.is_empty() || request.background_task_id.len() > 256 {
        return Err(ApiError::bad_request("invalid background task id"));
    }
    {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        let task = session
            .background_tasks
            .iter()
            .find(|task| task.id == request.background_task_id)
            .ok_or_else(|| {
                ApiError::new(StatusCode::CONFLICT, "background task is no longer running")
            })?;
        if !task.can_stop {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "background task cannot be stopped",
            ));
        }
    }

    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .background_task_stop_tx
        .send(BackgroundTaskStopRequest {
            session_id,
            background_task_id: request.background_task_id,
            reply,
        })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    match result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
    {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(BackgroundTaskStopFailure::SessionUnavailable) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the live session is unavailable",
        )),
        Err(BackgroundTaskStopFailure::Provider | BackgroundTaskStopFailure::Internal) => {
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the provider could not stop this background task",
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightNewRequest {
    #[serde(default)]
    remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    #[serde(default)]
    workspace_id: String,
    profile_id: String,
    bundle_id: String,
    target_id: String,
    #[serde(default)]
    project_directory: Option<PathBuf>,
}

/// Answer whether a new session would launch cleanly, and what to warn about.
///
/// The same validation the action itself runs happens here, so a phone learns
/// about an impossible combination while it can still change it rather than
/// after it has committed.
async fn preflight_new(
    State(state): State<ServerState>,
    Json(request): Json<PreflightNewRequest>,
) -> Result<Json<PreflightNew>, ApiError> {
    let project_validation = request.project_directory.is_some();
    let action = ControllerAction::New {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: request.workspace_id,
        profile_id: request.profile_id,
        bundle_id: request.bundle_id.clone(),
        target_id: request.target_id.clone(),
        title: None,
        project_directory: request.project_directory.clone(),
        dirty_ack: Vec::new(),
    };
    validate_action(&action, &state.snapshot_rx.borrow())?;
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::New(NewPreflightRequest {
            bundle_id: request.bundle_id,
            target_id: request.target_id,
            project_directory: request.project_directory,
            remote_repairs: request.remote_repairs,
            reply,
        }))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|failure| match failure {
            PreflightFailure::Validation if project_validation => ApiError::bad_request(
                "project validation failed; check that the directory exists, is accessible, and contains a Git repository with a valid HEAD",
            ),
            PreflightFailure::InvalidRepository(_) => ApiError::bad_request(
                "could not resolve the network repository; check its remote URL, authentication, connectivity, and default branch. Repositories without network remotes require a raw local session",
            ),
            PreflightFailure::Validation => ApiError::bad_request(
                "isolated session repositories need a network Git remote; use a raw local target for a local-only checkout",
            ),
            PreflightFailure::Controller(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not check this project",
            ),
        })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightResumeRequest {
    session_id: String,
    target_id: String,
}

/// Answer what resuming this session on this target would do to its
/// repository content, before the person commits to it.
///
/// A resume that changes nothing answers `Ready` without touching the disk,
/// so the browser can ask about every destination it offers.
async fn preflight_resume(
    State(state): State<ServerState>,
    Json(request): Json<PreflightResumeRequest>,
) -> Result<Json<PreflightResume>, ApiError> {
    if !state
        .snapshot_rx
        .borrow()
        .sessions
        .iter()
        .any(|session| session.id == request.session_id)
    {
        return Err(ApiError::not_found("unknown session"));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest::Resume(ResumePreflightRequest {
            session_id: request.session_id,
            target_id: request.target_id,
            reply,
        }))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|failure| match failure {
            PreflightFailure::Validation | PreflightFailure::InvalidRepository(_) => {
                ApiError::bad_request("this session cannot resume on that target")
            }
            PreflightFailure::Controller(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not check this checkout",
            ),
        })
}

/// Prepare a move without changing the source session. The returned
/// preparation is an expiring, fingerprinted capability: the confirmation
/// action must send it back verbatim, and the daemon rechecks it immediately
/// before interrupting work.
async fn prepare_move(
    State(state): State<ServerState>,
    Json(selection): Json<MoveSelection>,
) -> Result<Json<MovePreparation>, ApiError> {
    validate_move_selection(&selection, &state.snapshot_rx.borrow())?;
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .move_preparation_tx
        .send(MovePreparationRequest { selection, reply })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    let preparation = result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|error| {
            tracing::debug!(error = %error, "move preparation was rejected");
            ApiError::new(
                StatusCode::CONFLICT,
                "move preparation was rejected; refresh and try again",
            )
        })?;
    Ok(Json(preparation))
}

/// Ask the state channel one thing and wait for its answer.
async fn ask_client_state<T>(
    state: &ServerState,
    build: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> ClientStateRequest,
) -> Result<T, ApiError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    state
        .client_state_tx
        .send(build(reply))
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    answer
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not reach stored viewer state",
            )
        })
}

/// This viewer's draft and read frontier for one session.
async fn client_state(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ViewerClientState>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let Some(client_id) = viewer_client_id(&state, &headers) else {
        return Ok(Json(ViewerClientState::default()));
    };
    ask_client_state(&state, |reply| ClientStateRequest::Read {
        client_id,
        session_id,
        reply,
    })
    .await
    .map(Json)
}

#[derive(Debug, Serialize)]
struct DictationAvailability {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct DictationTranscript {
    text: String,
}

/// Report whether one of the session's Codex profiles has usable subscription
/// credentials. The controller selects profile paths from its current session
/// state, so this endpoint never accepts a browser-supplied credential path.
async fn dictation_availability(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<DictationAvailability>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let _permit = state
        .dictation_probe_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many dictation requests"))?;
    let result = dispatch_dictation(&state, session_id, DictationOperation::Availability).await?;
    match result {
        DictationResponse::Availability { available, reason } => {
            Ok(Json(DictationAvailability { available, reason }))
        }
        DictationResponse::Transcript { .. } => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller returned an invalid dictation response",
        )),
    }
}

/// Receive one bounded WAV upload and send it to the supervised controller
/// request loop. The semaphore is acquired before `Request::into_body`, so a
/// third concurrent upload is rejected without polling its body at all.
async fn upload_dictation(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<DictationTranscript>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let _permit = state
        .dictation_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many dictation requests"))?;

    if request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_AUDIO_BYTES as u64)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio upload is too large",
        ));
    }
    let body = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiError::controller_unavailable()),
        result = tokio::time::timeout(
            crate::dictation::DICTATION_TIMEOUT,
            to_bytes(request.into_body(), MAX_AUDIO_BYTES),
        ) => match result {
            Ok(result) => result.map_err(|_| {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "audio upload is too large")
            })?,
            Err(_) => return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "dictation upload timed out",
            )),
        },
    };
    // A bounded WAV may still contain millions of small metadata chunks.
    // Keep that scan off the HTTP event loop as well as the provider work.
    let audio = body.clone();
    tokio::task::spawn_blocking(move || validate_wav(&audio))
        .await
        .map_err(|error| {
            tracing::warn!(%error, "dictation audio validation task failed");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "audio validation failed")
        })?
        .map_err(dictation_api_error)?;
    let result =
        dispatch_dictation(&state, session_id, DictationOperation::Transcribe(body)).await?;
    match result {
        DictationResponse::Transcript { text } => Ok(Json(DictationTranscript { text })),
        DictationResponse::Availability { .. } => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the controller returned an invalid dictation response",
        )),
    }
}

/// Cancels a request as soon as Axum drops its handler future, which happens
/// when a browser disconnects while a provider request is still running.
struct DictationCancellationGuard(CancellationToken);

impl Drop for DictationCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn dispatch_dictation(
    state: &ServerState,
    session_id: String,
    operation: DictationOperation,
) -> Result<DictationResponse, ApiError> {
    let cancel = CancellationToken::new();
    let _guard = DictationCancellationGuard(cancel.clone());
    let (reply, answer) = tokio::sync::oneshot::channel();
    let request = DictationRequest {
        session_id,
        operation,
        cancel: cancel.clone(),
        reply,
    };
    tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiError::controller_unavailable()),
        result = state.dictation_tx.send(request) => {
            result.map_err(|_| ApiError::controller_unavailable())?;
        }
    }
    let answer = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiError::controller_unavailable()),
        result = answer => result.map_err(|_| ApiError::controller_unavailable())?,
    };
    answer.map_err(dictation_api_error)
}

fn dictation_api_error(error: DictationError) -> ApiError {
    match error {
        DictationError::SessionNotFound => ApiError::not_found("unknown session"),
        DictationError::CredentialsUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "dictation is unavailable because no Codex subscription is signed in",
        ),
        DictationError::InvalidAudio(message) => ApiError::bad_request(message),
        DictationError::Cancelled => {
            ApiError::new(StatusCode::REQUEST_TIMEOUT, "dictation cancelled")
        }
        DictationError::TimedOut => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "dictation transcription timed out",
        ),
        DictationError::CredentialProbe => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "dictation credentials could not be checked",
        ),
        DictationError::Provider(error) => {
            tracing::warn!(%error, "Codex dictation transcription failed");
            ApiError::new(StatusCode::BAD_GATEWAY, "dictation transcription failed")
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftRequest {
    draft: String,
}

async fn save_draft(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<DraftRequest>,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    if request.draft.len() > MAX_DRAFT_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "draft must be 65536 bytes or fewer",
        ));
    }
    let Some(client_id) = viewer_client_id(&state, &headers) else {
        // Nothing to key it to. The phone keeps its draft in the composer, and
        // silently accepting would promise a persistence that is not there.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this viewer has no stored identity; unlock again to keep drafts",
        ));
    };
    ask_client_state(&state, |reply| ClientStateRequest::SaveDraft {
        client_id,
        session_id,
        draft: request.draft,
        reply,
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Mark every session in a workspace read, in one request.
///
/// Opening a workspace should not cost one request per session.
async fn mark_workspace_read(
    State(state): State<ServerState>,
    Path(workspace_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    validate_public_id(&workspace_id)?;
    let Some(client_id) = viewer_client_id(&state, &headers) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    ask_client_state(&state, |reply| ClientStateRequest::MarkWorkspaceRead {
        client_id,
        workspace_id,
        reply,
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    scope: Option<String>,
}

/// Search this session's or this project's earlier prompts.
async fn prompt_history(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<ViewerPromptHistory>, ApiError> {
    validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    if query.q.chars().count() > MAX_TITLE_CHARS {
        return Err(ApiError::bad_request("search text is too long"));
    }
    let scope = query.scope.unwrap_or_else(|| "project".to_owned());
    if !matches!(scope.as_str(), "session" | "project" | "all") {
        return Err(ApiError::bad_request(
            "scope must be session, project or all",
        ));
    }
    ask_client_state(&state, |reply| ClientStateRequest::History {
        session_id,
        query: query.q,
        scope,
        reply,
    })
    .await
    .map(Json)
}

async fn events(State(state): State<ServerState>) -> impl IntoResponse {
    let mut snapshots = state.snapshot_rx.clone();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(8);
    tokio::spawn(async move {
        let initial = snapshots.borrow().revision;
        if tx
            .send(Ok(Event::default()
                .event("revision")
                .data(initial.to_string())))
            .await
            .is_err()
        {
            return;
        }
        while snapshots.changed().await.is_ok() {
            let revision = snapshots.borrow_and_update().revision;
            if tx
                .send(Ok(Event::default()
                    .event("revision")
                    .data(revision.to_string())))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Check attached images without decoding megabytes of base64 on the task that
/// serves the request. Everything else about an action is cheap enough to
/// check inline; a full multi-image prompt is not.
async fn decode_prompt_images_off_task(
    action: ControllerAction,
) -> Result<ControllerAction, ApiError> {
    let ControllerAction::Prompt { images, .. } = &action else {
        return Ok(action);
    };
    if images.is_empty() {
        return Ok(action);
    }
    tokio::task::spawn_blocking(move || {
        let mut action = action;
        let ControllerAction::Prompt {
            session_id, images, ..
        } = &action
        else {
            unreachable!("only prompt actions carry images")
        };
        validate_prompt_images(images)?;
        let store = AttachmentStore::controller(session_id).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not open the image attachment store",
            )
        })?;
        let ControllerAction::Prompt { images, .. } = &mut action else {
            unreachable!("only prompt actions carry images")
        };
        for image in images {
            if let Some(reference) = image.attachment.clone() {
                // Reading through this session's store both verifies the
                // digest and prevents a reference from another session being
                // smuggled into a prompt.
                store
                    .read(&reference)
                    .map_err(|_| ApiError::bad_request("the image attachment is unavailable"))?;
                image.data_base64.clear();
                image.mime_type = reference.mime_type;
                image.width = reference.width;
                image.height = reference.height;
            } else {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data_base64)
                    .map_err(|_| ApiError::bad_request("image data must be valid base64"))?;
                let optimized = optimize_image(&bytes).map_err(|_| {
                    ApiError::bad_request("unsupported image format or image could not be decoded")
                })?;
                let reference = AttachmentRef::new(
                    &optimized.bytes,
                    optimized.mime_type.clone(),
                    optimized.width,
                    optimized.height,
                )
                .map_err(|_| {
                    ApiError::bad_request("the inline image could not become an attachment")
                })?;
                store.install(&reference, &optimized.bytes).map_err(|_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "could not store the image attachment",
                    )
                })?;
                image.data_base64.clear();
                image.attachment = Some(reference);
                image.mime_type = optimized.mime_type;
                image.width = optimized.width;
                image.height = optimized.height;
            }
        }
        Ok(action)
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the server could not check the attached images",
        )
    })?
}

fn validate_prompt_images(images: &[ViewerPromptImage]) -> Result<(), ApiError> {
    if images.len() > MAX_PROMPT_IMAGES {
        return Err(ApiError::bad_request(
            "a prompt may contain at most 10 images",
        ));
    }
    for image in images {
        if !image.mime_type.starts_with("image/") {
            return Err(ApiError::bad_request(
                "image mime type must start with image/",
            ));
        }
        if image.width == 0 || image.height == 0 {
            return Err(ApiError::bad_request(
                "image dimensions must be greater than zero",
            ));
        }
        if let Some(reference) = &image.attachment {
            if !image.data_base64.is_empty() {
                return Err(ApiError::bad_request(
                    "an image cannot contain both inline data and an attachment",
                ));
            }
            if reference.mime_type != image.mime_type
                || reference.width != image.width
                || reference.height != image.height
            {
                return Err(ApiError::bad_request(
                    "image attachment metadata does not match the prompt",
                ));
            }
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .map_err(|_| ApiError::bad_request("image data must be valid base64"))?;
        if bytes.is_empty() {
            return Err(ApiError::bad_request("image data must not be empty"));
        }
    }
    Ok(())
}

const MAX_MOVE_QUEUE_ITEMS: usize = 256;
const MAX_MOVE_MOUNTS: usize = 32;

fn validate_move_selection(
    selection: &MoveSelection,
    snapshot: &ViewerSnapshot,
) -> Result<(), ApiError> {
    validate_public_id(&selection.session_id)?;
    if selection.profile_id.is_none() && selection.target_template_id.is_none() {
        return Err(ApiError::bad_request(
            "move must select a profile, a target, or both",
        ));
    }
    if selection.clear_resource_allocation && selection.resource_allocation.is_some() {
        return Err(ApiError::bad_request(
            "clear resource sizing cannot be combined with an explicit allocation",
        ));
    }
    if let Some(profile_id) = selection.profile_id.as_deref() {
        validate_public_id(profile_id)?;
        require_profile(snapshot, profile_id)?;
    }
    if let Some(target_id) = selection.target_template_id.as_deref() {
        validate_public_id(target_id)?;
        require_target(snapshot, target_id)?;
        let session = require_session_record(snapshot, &selection.session_id)?;
        if session
            .incompatible_resume_targets
            .iter()
            .any(|id| id == target_id)
        {
            return Err(ApiError::bad_request(
                "this session cannot resume on that target",
            ));
        }
    } else {
        require_session_record(snapshot, &selection.session_id)?;
    }
    if let Some(mounts) = &selection.additional_mounts {
        validate_move_mounts(mounts)?;
    }
    let session = require_session_record(snapshot, &selection.session_id)?;
    let retryable_move = session
        .move_recovery
        .as_ref()
        .is_some_and(|recovery| recovery.checkpoint_retained);
    if !session.capabilities.move_session && !retryable_move {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this session cannot be moved now",
        ));
    }
    Ok(())
}

fn validate_move_mounts(mounts: &[AdditionalMount]) -> Result<(), ApiError> {
    if mounts.len() > MAX_MOVE_MOUNTS {
        return Err(ApiError::bad_request("a move may carry at most 32 mounts"));
    }
    for mount in mounts {
        for path in [&mount.source, &mount.destination] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|component| component == Component::ParentDir)
            {
                return Err(ApiError::bad_request(
                    "move mount paths must be absolute and must not contain '..'",
                ));
            }
        }
    }
    Ok(())
}

fn validate_resume_settings(
    additional_mounts: Option<&Vec<AdditionalMount>>,
    resource_allocation: Option<&SessionResourceAllocation>,
) -> Result<(), ApiError> {
    if let Some(mounts) = additional_mounts {
        validate_move_mounts(mounts)?;
    }
    if let Some(allocation) = resource_allocation {
        allocation
            .validate()
            .map_err(|_| ApiError::bad_request("resource allocation is invalid"))?;
    }
    Ok(())
}

fn validate_move_request(
    request: &MoveSessionRequest,
    snapshot: &ViewerSnapshot,
) -> Result<(), ApiError> {
    let preparation = &request.preparation;
    validate_move_selection(&preparation.selection, snapshot)?;
    let session = require_session_record(snapshot, &preparation.selection.session_id)?;
    if preparation.operation_id.trim().is_empty() || preparation.fingerprint.trim().is_empty() {
        return Err(ApiError::bad_request(
            "move confirmation is missing its preparation identity",
        ));
    }
    if preparation.queued_commands.len() > MAX_MOVE_QUEUE_ITEMS {
        return Err(ApiError::bad_request(
            "move queue is too large; prepare again",
        ));
    }
    let active_now = preparation.active
        || session.chat_phase == ViewerChatPhase::Running
        || !session.active_user_shells.is_empty();
    if active_now && !request.acknowledge_interruption {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "confirm that the active turn may be interrupted",
        ));
    }
    if !preparation.queued_commands.is_empty() && request.queue.is_none() {
        return Err(ApiError::bad_request(
            "choose whether queued work is discarded or started after the move",
        ));
    }
    Ok(())
}

/// What every surface accepts as prompt text.
///
/// Session creation carries a first prompt before any session record exists,
/// so this is separate from [`validate_action`]'s prompt arm rather than being
/// restated there: both must refuse the same text.
fn validate_prompt_text(text: &str, has_images: bool) -> Result<(), ApiError> {
    if text.starts_with('!') {
        return Err(ApiError::bad_request(
            "leading ! is reserved for shell commands",
        ));
    }
    if text.chars().count() > MAX_PROMPT_CHARS {
        return Err(ApiError::bad_request(
            "prompt must contain 1-65536 characters",
        ));
    }
    if text.trim().is_empty() && !has_images {
        return Err(ApiError::bad_request(
            "prompt must contain text or an image",
        ));
    }
    Ok(())
}

fn validate_action(action: &ControllerAction, snapshot: &ViewerSnapshot) -> Result<(), ApiError> {
    match action {
        ControllerAction::New {
            workspace_id,
            profile_id,
            bundle_id,
            target_id,
            title,
            project_directory,
            dirty_ack,
            create_managed_worktree,
            mjolnir_subagents: _,
        } => {
            if !workspace_id.is_empty() {
                validate_public_id(workspace_id)?;
            }
            validate_public_id(profile_id)?;
            validate_public_id(bundle_id)?;
            validate_public_id(target_id)?;
            if let Some(title) = title {
                validate_title(title)?;
            }
            // An acknowledgement names repositories the preflight reported.
            // Unbounded or malformed entries would travel to the controller
            // and be compared against a real set, so they are refused here.
            if dirty_ack.len() > MAX_DIRTY_ACKNOWLEDGEMENTS
                || dirty_ack
                    .iter()
                    .any(|repository| repository.trim().is_empty() || repository.len() > 256)
            {
                return Err(ApiError::bad_request(
                    "dirty acknowledgement must name 0-32 repositories",
                ));
            }
            require_profile(snapshot, profile_id)?;
            require_bundle(snapshot, bundle_id)?;
            let target = require_target(snapshot, target_id)?;
            if *create_managed_worktree == Some(true) && !target.requires_project_directory {
                return Err(ApiError::bad_request(
                    "managed worktree creation requires a bare Git project",
                ));
            }
            if target.requires_project_directory != project_directory.is_some() {
                return Err(ApiError::bad_request(
                    "project_directory is required exactly for bare targets",
                ));
            }
            if let Some(directory) = project_directory
                && (mj_core::path_input::validate_absolute_input(directory).is_err()
                    || directory
                        .components()
                        .any(|component| component == Component::ParentDir))
            {
                return Err(ApiError::bad_request(
                    "project_directory must be an absolute safe path",
                ));
            }
        }
        ControllerAction::Resume {
            session_id,
            workspace_id,
            profile_id,
            target_id,
            additional_mounts,
            resource_allocation,
            ..
        } => {
            validate_public_id(session_id)?;
            validate_public_id(workspace_id)?;
            validate_public_id(profile_id)?;
            validate_public_id(target_id)?;
            let session = require_session_record(snapshot, session_id)?;
            require_workspace(snapshot, workspace_id)?;
            require_profile(snapshot, profile_id)?;
            require_target(snapshot, target_id)?;
            if session
                .incompatible_resume_targets
                .iter()
                .any(|incompatible| incompatible == target_id)
            {
                return Err(ApiError::bad_request(
                    "this session cannot resume on that target",
                ));
            }
            validate_resume_settings(additional_mounts.as_ref(), resource_allocation.as_ref())?;
        }
        ControllerAction::Move { request } => validate_move_request(request, snapshot)?,
        ControllerAction::Open { session_id }
        | ControllerAction::Close { session_id }
        | ControllerAction::ForceClose { session_id }
        | ControllerAction::Cancel { session_id }
        | ControllerAction::StartReview { session_id } => {
            validate_public_id(session_id)?;
            require_session_record(snapshot, session_id)?;
        }
        ControllerAction::ResolveReview {
            session_id,
            resolution,
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            let Some(resolution) = resolution_from_name(resolution) else {
                return Err(ApiError::bad_request(
                    "a review is resolved by forward, dismiss, or cancel",
                ));
            };
            let Some(review) = session.turn_review.as_ref() else {
                return Err(ApiError::bad_request("no review is open for that session"));
            };
            // Cancel is always available; the rest wait for the verdict the
            // daemon published, which is the same gate the daemon enforces
            // when it actually resolves.
            let allowed = resolution == mj_core::review::driver::Resolution::Cancelled
                || review.verdict.as_ref().is_some_and(|verdict| {
                    resolution_name(&resolution)
                        .is_some_and(|name| verdict.allowed.iter().any(|allowed| allowed == name))
                });
            if !allowed {
                return Err(ApiError::bad_request(
                    "that review cannot be resolved that way yet",
                ));
            }
        }
        ControllerAction::Rename { session_id, title } => {
            validate_public_id(session_id)?;
            validate_title(title)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.rename {
                return Err(ApiError::bad_request("this session cannot be renamed"));
            }
        }
        ControllerAction::CancelTurn { session_id } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.cancel_turn {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "this session has no turn to cancel",
                ));
            }
        }
        ControllerAction::SetConfig {
            session_id,
            key,
            value,
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.set_config {
                return Err(ApiError::bad_request(
                    "this session cannot change configuration now",
                ));
            }
            // The harness decides what it accepts. Forwarding a key it never
            // advertised, or a value outside the ones it offered, asks it to
            // refuse something the viewer should not have offered.
            let option = session
                .config_options
                .iter()
                .find(|option| option.key == *key)
                .ok_or_else(|| ApiError::bad_request("this agent does not offer that setting"))?;
            if !option.choices.iter().any(|choice| choice.value == *value) {
                return Err(ApiError::bad_request(
                    "this agent does not offer that value for that setting",
                ));
            }
        }
        ControllerAction::SetPlanMode { session_id, .. } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session.capabilities.set_plan_mode {
                return Err(ApiError::bad_request(
                    "this session cannot change plan mode now",
                ));
            }
        }
        ControllerAction::RefreshQuota { profile_id } => {
            validate_public_id(profile_id)?;
            require_profile(snapshot, profile_id)?;
        }
        ControllerAction::RefreshCapacity { target_id } => {
            validate_public_id(target_id)?;
            require_target(snapshot, target_id)?;
        }
        ControllerAction::Prompt {
            session_id,
            text,
            images,
        } => {
            validate_public_id(session_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if images.len() > MAX_PROMPT_IMAGES {
                return Err(ApiError::bad_request(
                    "a prompt may contain at most 10 images",
                ));
            }
            validate_prompt_text(text, !images.is_empty())?;
            if !images.is_empty() && !session.prompt_images_supported {
                return Err(ApiError::bad_request(
                    "this session does not support image prompts",
                ));
            }
            // Review is synchronous: the turn under review stays where the
            // review found it. The daemon's own submit path is what makes this
            // true; refusing here as well is what turns it into an immediate
            // answer rather than a rejected prompt.
            if session.turn_review.is_some() {
                return Err(ApiError::bad_request(
                    crate::review_host::PROMPT_HELD_MESSAGE,
                ));
            }
        }
        ControllerAction::RunShell {
            session_id,
            command,
        } => {
            validate_public_id(session_id)?;
            require_session_record(snapshot, session_id)?;
            if command.trim().is_empty() || command.chars().count() > MAX_PROMPT_CHARS {
                return Err(ApiError::bad_request(
                    "shell command must contain 1-65536 characters",
                ));
            }
        }
        ControllerAction::CancelShell {
            session_id,
            shell_command_id,
        } => {
            validate_public_id(session_id)?;
            validate_public_id(shell_command_id)?;
            let session = require_session_record(snapshot, session_id)?;
            if !session
                .active_user_shells
                .iter()
                .any(|shell| shell.id == *shell_command_id)
            {
                return Err(ApiError::bad_request("unknown active shell command"));
            }
        }
        ControllerAction::RemoveQueuedPrompt {
            session_id,
            queue_id,
        } => {
            validate_public_id(session_id)?;
            validate_public_id(queue_id)?;
            require_session_record(snapshot, session_id)?;
        }
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        } => {
            validate_public_id(session_id)?;
            validate_public_id(elicitation_id)?;
            let session = require_session_record(snapshot, session_id)?;
            let request = session
                .pending_elicitations
                .iter()
                .find(|request| request.id == *elicitation_id)
                .ok_or_else(|| ApiError::not_found("unknown elicitation"))?;
            if serde_json::to_vec(response).map_or(usize::MAX, |encoded| encoded.len())
                > MAX_ELICITATION_BYTES
            {
                return Err(ApiError::bad_request("elicitation answer is too large"));
            }
            // The answer has to satisfy the question the agent actually asked.
            // A phone can post one for a request the session has already
            // replaced, and forwarding that would answer a live question with
            // content the agent never offered.
            if request.validate_response(response).is_err() {
                return Err(ApiError::bad_request(
                    "the answer does not match this elicitation request",
                ));
            }
        }
    }
    Ok(())
}

fn validate_public_id(id: &str) -> Result<(), ApiError> {
    validate_id("request", id).map_err(|_| ApiError::bad_request("invalid id"))
}

fn validate_title(title: &str) -> Result<(), ApiError> {
    if title.trim().is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        Err(ApiError::bad_request("title must contain 1-120 characters"))
    } else {
        Ok(())
    }
}

fn require_session_record<'a>(
    snapshot: &'a ViewerSnapshot,
    id: &str,
) -> Result<&'a ViewerSession, ApiError> {
    snapshot
        .sessions
        .iter()
        .find(|session| session.id == id)
        .ok_or_else(|| ApiError::not_found("unknown session"))
}

fn require_workspace(snapshot: &ViewerSnapshot, id: &str) -> Result<(), ApiError> {
    snapshot
        .workspaces
        .iter()
        .any(|workspace| workspace.id == id)
        .then_some(())
        .ok_or_else(|| ApiError::bad_request("unknown workspace"))
}

fn require_profile<'a>(
    snapshot: &'a ViewerSnapshot,
    id: &str,
) -> Result<&'a ViewerProfile, ApiError> {
    snapshot
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .ok_or_else(|| ApiError::bad_request("unknown profile"))
}

fn require_target<'a>(
    snapshot: &'a ViewerSnapshot,
    id: &str,
) -> Result<&'a ViewerTarget, ApiError> {
    snapshot
        .targets
        .iter()
        .find(|target| target.id == id)
        .ok_or_else(|| ApiError::bad_request("unknown target"))
}

fn require_bundle(snapshot: &ViewerSnapshot, id: &str) -> Result<(), ApiError> {
    snapshot
        .bundles
        .iter()
        .any(|bundle| bundle.id == id)
        .then_some(())
        .ok_or_else(|| ApiError::bad_request("unknown bundle"))
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl ApiError {
    const fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }

    const fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    const fn bad_request(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    const fn not_found(message: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    const fn controller_unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "controller unavailable")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

fn code_locked(state: &ServerState) -> bool {
    state
        .code_guard
        .lock()
        .expect("viewer code guard poisoned")
        .locked_at(Instant::now())
}

fn record_code_failure(state: &ServerState) {
    state
        .code_guard
        .lock()
        .expect("viewer code guard poisoned")
        .record_failure_at(Instant::now());
}

fn reset_code_failures(state: &ServerState) {
    *state.code_guard.lock().expect("viewer code guard poisoned") = CodeGuard::default();
}

fn generate_viewer_code() -> AnyResult<String> {
    // Rejection sampling avoids modulo bias in the deliberately small code
    // space. Online attempts are separately rate-limited.
    const RANGE: u32 = 1_000_000;
    const LIMIT: u32 = u32::MAX - (u32::MAX % RANGE);
    loop {
        let mut bytes = [0_u8; 4];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("generate Mjolnir viewer code: {error}"))?;
        let value = u32::from_le_bytes(bytes);
        if value < LIMIT {
            return Ok(format!("{:06}", value % RANGE));
        }
    }
}

fn generate_login_token() -> AnyResult<String> {
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir viewer login token: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token))
}

fn generate_cookie_key() -> AnyResult<[u8; COOKIE_KEY_BYTES]> {
    let mut key = [0_u8; COOKIE_KEY_BYTES];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir cookie key: {error}"))?;
    Ok(key)
}

/// A random name for one viewer, minted at unlock.
///
/// The cookie used to sign only an expiry, which meant two phones unlocking in
/// the same second received byte-identical cookies and one phone's cookie
/// changed on every login. Nothing keyed to it could mean anything: a draft
/// would have leaked between phones and vanished on re-login. This is the
/// identity everything per-viewer hangs from.
fn generate_viewer_id() -> AnyResult<String> {
    let mut id = [0_u8; 16];
    getrandom::fill(&mut id)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir viewer id: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id))
}

fn signed_cookie_value(key: &[u8], viewer: &str, expiry: u64) -> String {
    // The signed text separates its parts with a character the parts cannot
    // contain, so no two different pairs can produce the same signed text.
    let canonical = format!("{viewer}|{expiry}");
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(canonical.as_bytes());
    let signature =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{viewer}.{expiry}.{signature}")
}

fn session_cookie_valid(key: &[u8], value: &str, now: u64) -> bool {
    cookie_viewer(key, value, now).is_some()
}

/// Mint a signed viewer-session cookie value without the HTTP login flow.
///
/// The desktop shell pre-authorizes its WebView with this: it runs as the same
/// user as the daemon and reads the same persisted signing key, so possession
/// of the key is the credential. The cookie carries the ephemeral TTL — a
/// desktop window re-mints on every launch, so it never needs a long life.
pub fn mint_desktop_session_cookie(key: &[u8]) -> AnyResult<String> {
    let viewer = generate_viewer_id()?;
    Ok(signed_cookie_value(
        key,
        &viewer,
        now_unix().saturating_add(EPHEMERAL_SESSION_TTL.as_secs()),
    ))
}

/// The viewer a cookie names, or `None` when the cookie is not valid.
fn cookie_viewer(key: &[u8], value: &str, now: u64) -> Option<String> {
    let [viewer, expiry, _] = value.split('.').collect::<Vec<_>>()[..] else {
        return None;
    };
    let expiry = expiry.parse::<u64>().ok()?;
    if now >= expiry {
        return None;
    }
    let expected = signed_cookie_value(key, viewer, expiry);
    constant_time_eq(expected.as_bytes(), value.as_bytes()).then(|| viewer.to_owned())
}

fn session_cookie_header(
    value: &str,
    max_age: Option<u64>,
    secure: bool,
) -> Result<HeaderValue, ApiError> {
    let mut header = format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Strict");
    if secure {
        header.push_str("; Secure");
    }
    if let Some(max_age) = max_age {
        header.push_str(&format!("; Max-Age={max_age}"));
    }
    HeaderValue::from_str(&header)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cookie creation failed"))
}

fn clear_cookie_header(secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0"
    ))
    .expect("static cookie header is valid")
}

/// The stored-state key for the viewer making this request.
fn viewer_client_id(state: &ServerState, headers: &HeaderMap) -> Option<String> {
    let cookie = headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME))?;
    cookie_viewer(&state.cookie_key, cookie, now_unix()).map(|viewer| format!("phone:{viewer}"))
}

fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(cookie_name, _)| *cookie_name == name)
        .map(|(_, value)| value)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(u64::MAX)
}

/// Every asset the browser application is built from. They are real files
/// under `src/web/` and `src/icons/` rather than string literals, so the
/// JavaScript can be read, formatted and tested as JavaScript, and so the
/// content-security policy below can forbid inline script outright.
const VIEWER_HTML: &str = include_str!("web/viewer.html");
const VIEWER_CSS: &str = include_str!("web/viewer.css");
const VIEWER_JS: &str = include_str!("web/viewer.js");
const MARKDOWN_JS: &str = include_str!("web/markdown.js");
const TOOL_OUTPUT_JS: &str = include_str!("web/tool-output.js");
const VOICE_WORKLET_JS: &str = include_str!("web/voice-worklet.js");
const VOICE_WORKER_JS: &str = include_str!("web/voice-worker.js");
/// A fake DOM for running the shipped renderers under Node. It is deliberately
/// not served: it exists so `cargo test` can exercise `markdown.js` without a
/// browser.
#[cfg(test)]
const TEST_DOM_JS: &str = include_str!("web/test-dom.js");
const SERVICE_WORKER: &str = include_str!("web/service-worker.js");
const MANIFEST: &str = include_str!("web/manifest.webmanifest");
const ICON_SVG: &str = include_str!("../src/icons/icon.svg");
const ICON_192: &[u8] = include_bytes!("../src/icons/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../src/icons/icon-512.png");
const MASKABLE_512: &[u8] = include_bytes!("../src/icons/maskable-512.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../src/icons/apple-touch-icon.png");
const MONO_FONT: &[u8] = include_bytes!("../src/fonts/jetbrains-mono.woff2");

/// What the browser is permitted to load and execute.
///
/// `default-src 'none'` refuses everything not named below, so a future asset
/// has to be allowed deliberately. Script and style come only from this
/// origin, which is why none of either may be inline. `img-src` allows `blob:`
/// for browser-local attachment previews and keeps `data:` for legacy image
/// content rendered in a transcript.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
script-src 'self'; \
style-src 'self'; \
img-src 'self' data: blob:; \
font-src 'self'; \
connect-src 'self'; \
manifest-src 'self'; \
base-uri 'none'; \
form-action 'none'; \
frame-ancestors 'none'";

async fn viewer() -> Response<Body> {
    static_response("text/html; charset=utf-8", VIEWER_HTML, true)
}

async fn viewer_css() -> Response<Body> {
    static_response("text/css; charset=utf-8", VIEWER_CSS, false)
}

async fn viewer_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", VIEWER_JS, false)
}

async fn markdown_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", MARKDOWN_JS, false)
}

async fn voice_worklet_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", VOICE_WORKLET_JS, false)
}

async fn voice_worker_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", VOICE_WORKER_JS, false)
}

async fn tool_output_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", TOOL_OUTPUT_JS, false)
}

async fn manifest() -> Response<Body> {
    static_response("application/manifest+json", MANIFEST, false)
}

/// The worker itself is never cached: a stale worker is what keeps a phone on
/// a superseded application, and it is the one asset that can never be fixed
/// by a later upgrade.
async fn service_worker() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", SERVICE_WORKER, true)
}

async fn icon() -> Response<Body> {
    static_response("image/svg+xml", ICON_SVG, false)
}

async fn icon_192() -> Response<Body> {
    binary_response("image/png", ICON_192)
}

async fn icon_512() -> Response<Body> {
    binary_response("image/png", ICON_512)
}

async fn maskable_512() -> Response<Body> {
    binary_response("image/png", MASKABLE_512)
}

async fn apple_touch_icon() -> Response<Body> {
    binary_response("image/png", APPLE_TOUCH_ICON)
}

async fn mono_font() -> Response<Body> {
    binary_response("font/woff2", MONO_FONT)
}

fn static_response(
    content_type: &'static str,
    body: &'static str,
    no_store: bool,
) -> Response<Body> {
    finish_static(Response::new(Body::from(body)), content_type, no_store)
}

fn binary_response(content_type: &'static str, body: &'static [u8]) -> Response<Body> {
    finish_static(Response::new(Body::from(body)), content_type, false)
}

/// Cacheable assets still revalidate. `no-cache` means "ask first", not "do
/// not store", so an upgraded viewer is picked up on the next load while an
/// unchanged one costs one conditional request.
fn finish_static(
    mut response: Response<Body>,
    content_type: &'static str,
    no_store: bool,
) -> Response<Body> {
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if no_store { "no-store" } else { "no-cache" }),
    );
    response
}

/// Headers every response carries, applied once as a layer so no route can
/// forget them.
///
/// The layer also owns `no-store` for live state and authentication, rather
/// than leaving it to each handler. A rejected request never reaches its
/// handler, so a handler-set header is missing from exactly the responses that
/// are least worth storing.
async fn security_headers(request: Request, next: Next) -> Response<Body> {
    let live = {
        let path = request.uri().path();
        path.starts_with("/api/") || path.starts_with("/auth/")
    };
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY_HEADER,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if live {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

pub fn viewer_config_options(
    config_options: &[agent_client_protocol::schema::v1::SessionConfigOption],
    facts: &mj_core::acp::AcpSessionFacts,
) -> Vec<ViewerConfigOption> {
    ["model", "effort"]
        .into_iter()
        .filter_map(|key| {
            let choices = mj_core::acp::session_config_choices(config_options, key);
            if choices.is_empty() {
                return None;
            }
            Some(ViewerConfigOption {
                key: key.to_owned(),
                label: key.to_owned(),
                current: match key {
                    "model" => facts.current_model(),
                    "effort" => facts.current_effort(),
                    _ => None,
                }
                .map(str::to_owned),
                choices: choices
                    .into_iter()
                    .map(|choice| ViewerConfigChoice {
                        value: choice.value,
                        name: choice.name,
                        description: choice.description,
                    })
                    .collect(),
            })
        })
        .collect()
}

pub fn session_config_view(
    harness: mj_core::config::HarnessKind,
    state: &mj_core::relay::RelayOperationalState,
) -> Vec<ViewerConfigOption> {
    // Selectors are ACP categories; the config map supplies legacy current values.
    let facts = mj_core::acp::AcpSessionFacts::from_operational(
        harness,
        &state.config,
        &state.config_options,
        state.modes.as_ref(),
    );
    viewer_config_options(&state.config_options, &facts)
}

#[cfg(test)]
mod tests;
