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
use axum::body::Body;
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
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use hel::hel_config::{HelConfig, TargetTemplate, validate_id};
use hel::hel_elicitation::{ElicitationRequest, ElicitationResponse, MAX_ELICITATION_BYTES};
use hel::hel_state::{HelState, SessionState};

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
const COOKIE_KEY_BYTES: usize = 32;
const COOKIE_KEY_FILE: &str = "phone-cookie-key";

/// Where the phone cookie signing key lives: beside Hel's other private
/// controller state, never in the shared config directory.
/// The conversation shape the phone reads. The chat layer projects its
/// entries into this; the browser API owns the wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserTranscript {
    pub latest_seq: u64,
    pub window_start_seq: u64,
    pub reset: bool,
    pub entries: Vec<BrowserTranscriptEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserTranscriptEntry {
    pub id: u64,
    pub updated_seq: u64,
    pub role: &'static str,
    pub label: String,
    pub recorded_at_ms: Option<i64>,
    pub lines: Vec<String>,
    /// The glyph the terminal draws for this role, so both surfaces read alike
    /// without the browser keeping a second copy of the mapping. Taken from
    /// the same `entry_visual` the terminal renders from.
    pub glyph: &'static str,
    /// The semantic colour name, not a colour. The stylesheet decides what
    /// `agent` or `failed` looks like; this says which one applies.
    pub tone: &'static str,
    /// A tool call's state, for a tool entry. `None` for every other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<&'static str>,
    /// The changed files a tool reported, as data rather than as extra lines
    /// appended to `lines`. The terminal formats these for a terminal; a
    /// browser re-parsing that formatting is how the phone came to render
    /// every diffstat as one unsplit path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffstats: Vec<BrowserDiffStat>,
}

/// One file a tool changed, and by how much.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserDiffStat {
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
}

/// How long stored viewer state outlives its last use.
///
/// It matches the session cookie's own lifetime: state keyed to an identity
/// that can no longer authenticate has nothing left to belong to.
pub const fn default_session_ttl() -> Duration {
    DEFAULT_SESSION_TTL
}

pub fn cookie_key_path() -> PathBuf {
    hel::hel_config::data_dir().join(COOKIE_KEY_FILE)
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
    hel::hel_config::atomic_write(path, &key)
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
pub struct ServerOptions {
    pub bind: SocketAddr,
    pub snapshot_rx: watch::Receiver<ViewerSnapshot>,
    pub conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    pub action_tx: mpsc::Sender<ControllerRequest>,
    pub receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    pub preflight_tx: mpsc::Sender<PreflightRequest>,
    pub client_state_tx: mpsc::Sender<ClientStateRequest>,
    pub shutdown: CancellationToken,
    pub session_ttl: Duration,
    /// Keep this enabled for direct HTTPS or an HTTPS reverse proxy. It may be
    /// disabled only for an explicitly trusted HTTP development endpoint.
    pub secure_cookie: bool,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
    viewer_code: String,
    login_token: String,
    cookie_key: Vec<u8>,
}

impl ServerOptions {
    pub fn new(
        bind: SocketAddr,
        snapshot_rx: watch::Receiver<ViewerSnapshot>,
        conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
        action_tx: mpsc::Sender<ControllerRequest>,
        receipt_tx: mpsc::Sender<ReadReceiptRequest>,
        preflight_tx: mpsc::Sender<PreflightRequest>,
        client_state_tx: mpsc::Sender<ClientStateRequest>,
    ) -> AnyResult<Self> {
        Ok(Self {
            bind,
            snapshot_rx,
            conversation_rx,
            action_tx,
            receipt_tx,
            preflight_tx,
            client_state_tx,
            shutdown: CancellationToken::new(),
            session_ttl: DEFAULT_SESSION_TTL,
            secure_cookie: true,
            tls_config: None,
            viewer_code: generate_viewer_code()?,
            login_token: generate_login_token()?,
            cookie_key: generate_cookie_key()?.to_vec(),
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

    #[cfg(test)]
    fn with_test_credentials(mut self, code: &str, key: &[u8]) -> Self {
        self.viewer_code = code.to_string();
        self.login_token = "test-login-token".into();
        self.cookie_key = key.to_vec();
        self.secure_cookie = false;
        self
    }
}

/// Run the phone server until its shutdown token is cancelled.
///
/// This binds only the requested listener. It does not daemonize, provision a
/// target, or keep sessions alive: controller availability is required, just
/// like MJ's explicit remote-viewer model.
pub async fn run_server(options: ServerOptions) -> AnyResult<()> {
    let mut options = options;
    let bind = options.bind;
    let shutdown = options.shutdown.clone();
    let viewer_code = options.viewer_code.clone();
    let tls_config = options.tls_config.take();
    let app = router(options);
    println!("Mjolnir viewer code: {viewer_code}");
    if let Some(tls_config) = tls_config {
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(2)));
        });
        axum_server::bind_rustls(bind, tls_config)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .context("run Mjolnir HTTPS phone server")
    } else {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind Mjolnir phone server to {bind}"))?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .context("run Mjolnir HTTP phone server")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSnapshot {
    pub revision: u64,
    pub generated_at: String,
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
    /// One entry per host or fleet that can be probed. Empty until the phone
    /// server's capacity poller has published a reading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity: Vec<ViewerTargetCapacity>,
}

impl ViewerSnapshot {
    /// Build the public projection. In particular, this never copies profile
    /// homes/environment, SSH hosts/keys, container environment, AWS details,
    /// concrete resource locators, native session IDs, or raw error strings.
    pub fn from_config_state(config: &HelConfig, state: &HelState, revision: u64) -> Self {
        let sessions = state
            .sessions
            .values()
            .map(|session| {
                let incompatible = config
                    .targets
                    .keys()
                    .filter(|target_id| {
                        crate::hel_controller::resume_compatibility(session, config, target_id)
                            .is_err()
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let lifecycle = ViewerLifecycleCategory::of(session.state);
                ViewerSession {
                    id: session.id.clone(),
                    workspace_id: session.workspace_id.clone(),
                    title: session.display_title().to_owned(),
                    harness_kind: session.harness_kind.id().into(),
                    profile_id: session.last_profile.clone(),
                    bundle_id: session.bundle_id.clone(),
                    target_id: session.target_template_id.clone(),
                    state: session_state_name(session.state).into(),
                    created_at: session.created_at.clone(),
                    updated_at: session.updated_at.clone(),
                    has_error: session.last_error.is_some(),
                    preview: Vec::new(),
                    queued_prompts: Vec::new(),
                    active_user_shells: Vec::new(),
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
                    project_label: session.project_name(config),
                    project_key: project_key(&session.project_source(config).key),
                    lifecycle,
                    latest_event_ordinal: 0,
                    activity: String::new(),
                    operation: None,
                    chat_phase: ViewerChatPhase::default(),
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
                        set_config: false,
                        set_plan_mode: false,
                    },
                }
            })
            .collect();
        let profiles = config
            .profiles
            .iter()
            .map(|(id, profile)| ViewerProfile {
                id: id.clone(),
                harness_kind: profile.kind.id().into(),
                quota: None,
            })
            .collect();
        let targets = config
            .targets
            .iter()
            .map(|(id, target)| ViewerTarget {
                id: id.clone(),
                kind: target_kind_name(target).into(),
                requires_project_directory: matches!(
                    target,
                    TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
                ),
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
            capacity: Vec::new(),
        }
    }
}

/// A stable, opaque grouping key for a project.
///
/// The controller's own project identity is a filesystem path or a Git remote,
/// and this projection publishes neither. A digest groups exactly as well and
/// says nothing: two sessions in the same project share a key, and a key on
/// its own reveals no path.
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
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    pub title: String,
    pub harness_kind: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_id: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub has_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<ViewerQueuedPrompt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_user_shells: Vec<ViewerUserShell>,
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
    /// The project this session works in, as a name a person recognises. This
    /// is the leaf of a path or a repository name, never the path itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_label: String,
    /// A stable key for grouping sessions by project. The controller's own
    /// identity for a project is a path or a remote, so what travels is a
    /// digest of it: enough to group by, and nothing to read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    pub lifecycle: ViewerLifecycleCategory,
    /// How far the controller's projection of this session has advanced. A
    /// phone compares it against its own read frontier to know what is unread,
    /// without fetching a transcript to find out.
    #[serde(default)]
    pub latest_event_ordinal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<ViewerOperation>,
    #[serde(default)]
    pub chat_phase: ViewerChatPhase,
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
    /// `pending`, `running`, `clean`, `findings`, or `failed`.
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
    pub fn from_runtime(review: &crate::hel_review_host::RuntimeReviewView) -> Self {
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
                    crate::hel_review_host::VerdictKind::Clean => "clean",
                    crate::hel_review_host::VerdictKind::Findings => "findings",
                    crate::hel_review_host::VerdictKind::Failed => "failed",
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
pub fn resolution_name(resolution: &hel::hel_review::driver::Resolution) -> Option<&'static str> {
    match resolution {
        hel::hel_review::driver::Resolution::Forwarded => Some("forward"),
        hel::hel_review::driver::Resolution::Dismissed => Some("dismiss"),
        hel::hel_review::driver::Resolution::Cancelled => Some("cancel"),
        // Not resolutions a surface asks for: the review reaches these itself.
        hel::hel_review::driver::Resolution::NothingToReview
        | hel::hel_review::driver::Resolution::CoverageStarted => None,
    }
}

/// The resolution a phone's button asked for.
#[must_use]
pub fn resolution_from_name(name: &str) -> Option<hel::hel_review::driver::Resolution> {
    match name {
        "forward" => Some(hel::hel_review::driver::Resolution::Forwarded),
        "dismiss" => Some(hel::hel_review::driver::Resolution::Dismissed),
        "cancel" => Some(hel::hel_review::driver::Resolution::Cancelled),
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
    Stop,
    Checkpoint,
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
/// Destructive force-cleanup and secret/config editing are intentionally not
/// representable here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControllerAction {
    New {
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
        profile_id: String,
        target_id: String,
        queue: ResumeQueueDisposition,
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

/// One image a phone attached to a prompt, still base64-encoded as the browser
/// read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerPromptImage {
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeQueueDisposition {
    Start,
    Discard,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Admitted and now running; watch the snapshot for what happens next.
    Accepted,
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
    /// The reply an outcome owes the phone, or `None` when it was accepted.
    const fn rejection(self) -> Option<ApiError> {
        match self {
            Self::Accepted => None,
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
/// cleanly, and what it should be warned about first.
///
/// This is not a `ControllerAction`: it starts nothing, it takes no session
/// slot, and it must answer before the person has decided anything. It also
/// needs the controller, because whether a repository has uncommitted changes
/// is a fact about the disk rather than about the projection.
#[derive(Debug)]
pub struct PreflightRequest {
    pub bundle_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<PreflightNew, String>>,
}

/// What a preflight found.
///
/// The repositories are leaf names. The controller knows them by absolute
/// path, and a phone is told just enough to recognise the repository it is
/// about to launch over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightNew {
    pub dirty_repositories: Vec<String>,
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

#[derive(Clone)]
struct ServerState {
    snapshot_rx: watch::Receiver<ViewerSnapshot>,
    conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
    action_tx: mpsc::Sender<ControllerRequest>,
    receipt_tx: mpsc::Sender<ReadReceiptRequest>,
    preflight_tx: mpsc::Sender<PreflightRequest>,
    client_state_tx: mpsc::Sender<ClientStateRequest>,
    viewer_code: Arc<str>,
    login_token: Arc<str>,
    cookie_key: Arc<[u8]>,
    session_ttl: Duration,
    secure_cookie: bool,
    code_guard: Arc<Mutex<CodeGuard>>,
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
        receipt_tx: options.receipt_tx,
        preflight_tx: options.preflight_tx,
        client_state_tx: options.client_state_tx,
        viewer_code: options.viewer_code.into(),
        login_token: options.login_token.into(),
        cookie_key: options.cookie_key.into(),
        session_ttl: options.session_ttl,
        secure_cookie: options.secure_cookie,
        code_guard: Arc::new(Mutex::new(CodeGuard::default())),
    };
    let protected = Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/conversations/{session_id}", get(conversation))
        .route(
            "/api/conversations/{session_id}/read",
            post(mark_conversation_read),
        )
        .route("/api/events", get(events))
        .route("/api/preflight/new", post(preflight_new))
        .route("/api/sessions/{session_id}/client-state", get(client_state))
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
    let mut response = Json(state.snapshot_rx.borrow().clone()).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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

#[derive(Debug, Deserialize)]
struct ConversationQuery {
    after_seq: Option<u64>,
}

async fn conversation(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<BrowserTranscript>, ApiError> {
    validate_public_id(&session_id)?;
    let conversations = state.conversation_rx.borrow();
    let transcript = conversations
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("conversation unavailable"))?;
    let mut response = transcript.clone();
    if let Some(after) = query.after_seq {
        response.reset = after < response.window_start_seq;
        if !response.reset {
            response.entries.retain(|entry| entry.updated_seq > after);
        }
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
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
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
struct PreflightNewRequest {
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
    let action = ControllerAction::New {
        workspace_id: request.workspace_id,
        profile_id: request.profile_id,
        bundle_id: request.bundle_id.clone(),
        target_id: request.target_id,
        title: None,
        project_directory: request.project_directory.clone(),
        dirty_ack: Vec::new(),
    };
    validate_action(&action, &state.snapshot_rx.borrow())?;
    // A bare target opens a directory the person named; there is no bundle to
    // have uncommitted changes in.
    if request.project_directory.is_some() {
        return Ok(Json(PreflightNew {
            dirty_repositories: Vec::new(),
        }));
    }
    let (reply, result) = tokio::sync::oneshot::channel();
    state
        .preflight_tx
        .send(PreflightRequest {
            bundle_id: request.bundle_id,
            reply,
        })
        .await
        .map_err(|_| ApiError::controller_unavailable())?;
    result
        .await
        .map_err(|_| ApiError::controller_unavailable())?
        .map(Json)
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the controller could not check this project",
            )
        })
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
    // A viewer with a legacy cookie has no identity and so has nothing stored.
    // Answering with an empty state is the truth, and is what lets an older
    // phone keep working through a deployment.
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
        let ControllerAction::Prompt { images, .. } = &action else {
            unreachable!("only prompt actions carry images")
        };
        validate_prompt_images(images)?;
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
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .map_err(|_| ApiError::bad_request("image data must be valid base64"))?;
        if bytes.is_empty() {
            return Err(ApiError::bad_request("image data must not be empty"));
        }
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
            if target.requires_project_directory != project_directory.is_some() {
                return Err(ApiError::bad_request(
                    "project_directory is required exactly for bare targets",
                ));
            }
            if let Some(directory) = project_directory
                && (!directory.is_absolute()
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
            profile_id,
            target_id,
            ..
        } => {
            validate_public_id(session_id)?;
            validate_public_id(profile_id)?;
            validate_public_id(target_id)?;
            let session = require_session_record(snapshot, session_id)?;
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
        }
        ControllerAction::Open { session_id }
        | ControllerAction::Close { session_id }
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
            let allowed = resolution == hel::hel_review::driver::Resolution::Cancelled
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
            if text.trim().is_empty() && images.is_empty() {
                return Err(ApiError::bad_request(
                    "prompt must contain text or an image",
                ));
            }
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
                    crate::hel_review_host::PROMPT_HELD_MESSAGE,
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

/// The cookie value a viewer with no identity used to receive.
///
/// Still accepted, so a phone holding one is not signed out by a deployment.
/// It carries no viewer, so it stores nothing and is replaced by a three-part
/// cookie at its next unlock.
fn legacy_signed_cookie_value(key: &[u8], expiry: u64) -> String {
    let canonical = expiry.to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(canonical.as_bytes());
    let signature =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{canonical}.{signature}")
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
///
/// A legacy two-part cookie validates and names no viewer, which is the
/// difference between "signed out" and "signed in with nothing stored".
fn cookie_viewer(key: &[u8], value: &str, now: u64) -> Option<Option<String>> {
    let parts = value.split('.').collect::<Vec<_>>();
    let (viewer, expiry, expected) = match parts.as_slice() {
        [viewer, expiry, _] => {
            let expiry_value = expiry.parse::<u64>().ok()?;
            (
                Some((*viewer).to_owned()),
                expiry_value,
                signed_cookie_value(key, viewer, expiry_value),
            )
        }
        [expiry, _] => {
            let expiry_value = expiry.parse::<u64>().ok()?;
            (
                None,
                expiry_value,
                legacy_signed_cookie_value(key, expiry_value),
            )
        }
        _ => return None,
    };
    if now >= expiry {
        return None;
    }
    constant_time_eq(expected.as_bytes(), value.as_bytes()).then_some(viewer)
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
///
/// A viewer with a legacy cookie has no identity, so it has no stored state:
/// it reads and writes nothing rather than sharing a bucket with every other
/// phone that unlocked in the same second, which is what the old whole-cookie
/// key amounted to.
fn viewer_client_id(state: &ServerState, headers: &HeaderMap) -> Option<String> {
    let cookie = headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME))?;
    cookie_viewer(&state.cookie_key, cookie, now_unix())
        .flatten()
        .map(|viewer| format!("phone:{viewer}"))
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

const fn session_state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Provisioning => "provisioning",
        SessionState::Running => "running",
        SessionState::Disconnected => "disconnected",
        SessionState::Checkpointing => "checkpointing",
        SessionState::Closing => "closing",
        SessionState::Destroying => "destroying",
        SessionState::Stopped => "stopped",
        SessionState::Lost => "lost",
        SessionState::Error => "error",
        SessionState::DestroyedWithDataLoss => "destroyed-with-data-loss",
    }
}

const fn target_kind_name(target: &TargetTemplate) -> &'static str {
    match target {
        TargetTemplate::LocalBare => "local-bare",
        TargetTemplate::LocalPodman { .. } => "local-podman",
        TargetTemplate::LocalDocker { .. } => "local-docker",
        TargetTemplate::AppleContainer { .. } => "apple-container",
        TargetTemplate::AwsEc2 { .. } => "aws-ec2",
        TargetTemplate::SshBare { .. } => "ssh-bare",
        TargetTemplate::SshPodman { .. } => "ssh-podman",
    }
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
/// origin, which is why none of either may be inline. `img-src` needs `data:`
/// because attached images render from data URLs the browser itself just
/// built from a file the person picked.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
script-src 'self'; \
style-src 'self'; \
img-src 'self' data:; \
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use hel::hel_config::{
        CONFIG_VERSION, ContainerTemplate, HarnessKind, HarnessProfile, ProjectBundle,
        ProjectRepository,
    };
    use hel::hel_state::{STATE_VERSION, SessionRecord};

    #[test]
    fn minted_desktop_cookie_validates_and_names_a_viewer() {
        let key = vec![7u8; COOKIE_KEY_BYTES];
        let value = mint_desktop_session_cookie(&key).unwrap();
        let viewer = cookie_viewer(&key, &value, now_unix());
        assert!(
            matches!(viewer, Some(Some(_))),
            "minted cookie must validate and carry a viewer id: {value:?}"
        );
        assert!(!session_cookie_valid(
            &[8u8; COOKIE_KEY_BYTES],
            &value,
            now_unix()
        ));
    }

    fn sample_config_state() -> (HelConfig, HelState) {
        let config = HelConfig {
            version: CONFIG_VERSION,
            newer_config_version: None,
            phone: Default::default(),
            review: Default::default(),
            profiles: BTreeMap::from([(
                "codex-1".into(),
                HarnessProfile {
                    context_window_bytes: None,
                    kind: HarnessKind::Codex,
                    home: "/highly/secret/codex".into(),
                    executable: None,
                    environment: BTreeMap::from([("GH_TOKEN".into(), "secret-token".into())]),
                },
            )]),
            bundles: BTreeMap::from([(
                "hel".into(),
                ProjectBundle {
                    primary_repo: "hel".into(),
                    repositories: vec![ProjectRepository {
                        id: "hel".into(),
                        github: Some("owner/hel".into()),
                        local: Some("/private/source/hel".into()),
                        destination: "hel".into(),
                        git_ref: None,
                    }],
                },
            )]),
            targets: BTreeMap::from([
                (
                    "podman".into(),
                    TargetTemplate::LocalPodman {
                        container: ContainerTemplate {
                            image: "secret.registry/image".into(),
                            pull_policy: Default::default(),
                            platform: None,
                            cpus: None,
                            memory: None,
                            environment: BTreeMap::from([("TOKEN".into(), "secret-target".into())]),
                        },
                    },
                ),
                ("raw".into(), TargetTemplate::LocalBare),
            ]),
        };
        let state = HelState {
            version: STATE_VERSION,
            sessions: BTreeMap::from([(
                "session-1".into(),
                SessionRecord {
                    workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                    archived: false,
                    container_cpus: None,
                    container_memory: None,
                    id: "session-1".into(),
                    title: "Build Hel".into(),
                    harness_kind: HarnessKind::Codex,
                    last_profile: "codex-1".into(),
                    bundle_id: "hel".into(),
                    project_directory: None,
                    managed_worktree: None,
                    target_template_id: "podman".into(),
                    resource_allocation: None,
                    additional_mounts: vec![],
                    state: SessionState::Running,
                    target: None,
                    native_session_id: Some("native-secret-id".into()),
                    acp_session_title: Some("Build Hel".into()),
                    session_title_override: None,
                    created_at: "now".into(),
                    updated_at: "now".into(),
                    viewed_through_event_ordinal: 0,
                    draft_input: String::new(),
                    last_error: Some("secret-token at /highly/secret/codex".into()),
                    last_checkpoint_error: None,
                    checkpoint: None,
                },
            )]),
            mount_history: BTreeMap::new(),
            container_sizes: BTreeMap::new(),
        };
        (config, state)
    }

    type TestServer = (
        Router,
        mpsc::Receiver<ControllerRequest>,
        mpsc::Receiver<ReadReceiptRequest>,
        mpsc::Receiver<PreflightRequest>,
        mpsc::Receiver<ClientStateRequest>,
    );

    fn app() -> TestServer {
        app_with_conversations(BTreeMap::new())
    }

    fn app_with_conversations(conversations: BTreeMap<String, BrowserTranscript>) -> TestServer {
        app_with(conversations, |_| {})
    }

    fn app_with_snapshot(adjust: impl FnOnce(&mut ViewerSnapshot)) -> TestServer {
        app_with(BTreeMap::new(), adjust)
    }

    fn app_with(
        conversations: BTreeMap<String, BrowserTranscript>,
        adjust: impl FnOnce(&mut ViewerSnapshot),
    ) -> TestServer {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        adjust(&mut snapshot);
        let (_snapshot_tx, snapshot_rx) = watch::channel(snapshot);
        let (_conversation_tx, conversation_rx) = watch::channel(conversations);
        let (action_tx, action_rx) = mpsc::channel(8);
        let (receipt_tx, receipt_rx) = mpsc::channel(8);
        let (preflight_tx, preflight_rx) = mpsc::channel(8);
        let (client_state_tx, client_state_rx) = mpsc::channel(8);
        let options = test_options(
            snapshot_rx,
            conversation_rx,
            action_tx,
            receipt_tx,
            preflight_tx,
            client_state_tx,
        )
        .with_test_credentials("123456", b"01234567890123456789012345678901");
        (
            router(options),
            action_rx,
            receipt_rx,
            preflight_rx,
            client_state_rx,
        )
    }

    fn test_options(
        snapshot_rx: watch::Receiver<ViewerSnapshot>,
        conversation_rx: watch::Receiver<BTreeMap<String, BrowserTranscript>>,
        action_tx: mpsc::Sender<ControllerRequest>,
        receipt_tx: mpsc::Sender<ReadReceiptRequest>,
        preflight_tx: mpsc::Sender<PreflightRequest>,
        client_state_tx: mpsc::Sender<ClientStateRequest>,
    ) -> ServerOptions {
        ServerOptions::new(
            "127.0.0.1:0".parse().unwrap(),
            snapshot_rx,
            conversation_rx,
            action_tx,
            receipt_tx,
            preflight_tx,
            client_state_tx,
        )
        .unwrap()
    }

    fn detached_options() -> ServerOptions {
        let (config, state) = sample_config_state();
        let (_snapshot_tx, snapshot_rx) =
            watch::channel(ViewerSnapshot::from_config_state(&config, &state, 1));
        let (_conversation_tx, conversation_rx) = watch::channel(BTreeMap::new());
        let (action_tx, _action_rx) = mpsc::channel(1);
        let (receipt_tx, _receipt_rx) = mpsc::channel(1);
        let (preflight_tx, _preflight_rx) = mpsc::channel(1);
        let (client_state_tx, _client_state_rx) = mpsc::channel(1);
        test_options(
            snapshot_rx,
            conversation_rx,
            action_tx,
            receipt_tx,
            preflight_tx,
            client_state_tx,
        )
    }

    /// A valid session cookie for the test server's key.
    ///
    /// Most checks are about what an authenticated request does rather than
    /// about how it authenticated, and going through the login route for each
    /// one buys nothing.
    fn cookie() -> String {
        format!(
            "{COOKIE_NAME}={}",
            signed_cookie_value(
                b"01234567890123456789012345678901",
                "test-viewer",
                now_unix().saturating_add(3600)
            )
        )
    }

    async fn login_cookie(app: &Router) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::post("/auth/session")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"code":"123456"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn api_requires_a_valid_signed_cookie() {
        let (app, _, _, _, _) = app();
        let unauthorized = app
            .clone()
            .oneshot(Request::get("/api/snapshot").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let cookie = login_cookie(&app).await;
        let authorized = app
            .oneshot(
                Request::get("/api/snapshot")
                    .header(COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn qr_login_exchanges_the_secret_for_a_cookie_and_redirects_cleanly() {
        let (app, _, _, _, _) = app();
        let rejected = app
            .clone()
            .oneshot(
                Request::get("/auth/login?token=wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

        let accepted = app
            .oneshot(
                Request::get("/auth/login?token=test-login-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
        assert_eq!(accepted.headers().get(LOCATION).unwrap(), "/");
        assert_eq!(accepted.headers().get(CACHE_CONTROL).unwrap(), "no-store");
        assert!(accepted.headers().contains_key(SET_COOKIE));
    }

    #[test]
    fn signed_cookie_rejects_expiry_and_tampering() {
        let key = b"01234567890123456789012345678901";
        let cookie = signed_cookie_value(key, "test-viewer", 200);
        assert!(session_cookie_valid(key, &cookie, 100));
        assert!(!session_cookie_valid(key, &cookie, 200));
        assert!(!session_cookie_valid(key, &format!("{cookie}x"), 100));
        assert!(!session_cookie_valid(b"another-key", &cookie, 100));
    }

    #[test]
    fn generated_code_and_cookie_attributes_are_phone_safe() {
        let code = generate_viewer_code().unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        let header = session_cookie_header("signed", Some(60), true)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Strict"));
        assert!(header.contains("Secure"));
        assert!(header.contains("Max-Age=60"));
    }

    #[test]
    fn public_snapshot_omits_homes_environment_locators_and_raw_errors() {
        let (config, state) = sample_config_state();
        let json =
            serde_json::to_string(&ViewerSnapshot::from_config_state(&config, &state, 9)).unwrap();
        assert!(!json.contains("/highly/secret"));
        assert!(!json.contains("secret-token"));
        assert!(!json.contains("secret-target"));
        assert!(!json.contains("secret.registry"));
        assert!(!json.contains("native-secret-id"));
        assert!(json.contains("\"has_error\":true"));
    }

    #[test]
    fn public_snapshot_exposes_only_review_status_configuration() {
        let (mut config, state) = sample_config_state();
        config.review = hel::hel_config::ReviewConfig {
            enabled: true,
            tier: hel::hel_review::lanes::ReviewTier::Extended,
            profile: Some("reviewer-1".into()),
            model: Some("private-review-model".into()),
            effort: Some("private-review-effort".into()),
        };

        let value =
            serde_json::to_value(ViewerSnapshot::from_config_state(&config, &state, 9)).unwrap();

        assert_eq!(
            value.get("review_config"),
            Some(&serde_json::json!({
                "enabled": true,
                "tier": "extended",
                "profile": "reviewer-1",
            }))
        );
        let json = value.to_string();
        assert!(!json.contains("private-review-model"));
        assert!(!json.contains("private-review-effort"));
    }

    fn sample_elicitation() -> ElicitationRequest {
        ElicitationRequest::from_acp_params(
            "elicitation-1",
            serde_json::json!({
                "sessionId": "session-1",
                "mode": "form",
                "message": "Which CI architecture should the workflow use?",
                "requestedSchema": {
                    "type": "object",
                    "required": ["question_0"],
                    "properties": {
                        "question_0": {
                            "type": "string",
                            "title": "CI architecture",
                            "oneOf": [
                                {"const": "reusable", "title": "Reusable workflow"},
                                {"const": "matrix", "title": "Matrix job"}
                            ]
                        },
                        "question_0_custom": {
                            "type": "string",
                            "title": "Other",
                            "_meta": {"_askUserQuestionCustomAnswer": {
                                "questionId": "question_0",
                                "isCustomAnswer": true
                            }}
                        }
                    }
                }
            }),
        )
        .expect("sample elicitation parses")
    }

    fn accept(pairs: &[(&str, &str)]) -> ElicitationResponse {
        ElicitationResponse::Accept {
            content: pairs
                .iter()
                .map(|(id, value)| {
                    (
                        (*id).to_owned(),
                        hel::hel_elicitation::ElicitationValue::String((*value).to_owned()),
                    )
                })
                .collect(),
        }
    }

    fn pending_elicitation_snapshot(snapshot: &mut ViewerSnapshot) {
        snapshot.sessions[0].pending_elicitations = vec![sample_elicitation()];
    }

    #[tokio::test]
    async fn elicitation_answer_is_typed_and_forwarded() {
        let (app, mut actions, _, _, _) = app_with_snapshot(pending_elicitation_snapshot);
        let cookie = login_cookie(&app).await;
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"respond-elicitation","session_id":"session-1","elicitation_id":"elicitation-1","response":{"action":"accept","content":{"question_0":"reusable"}}}"#,
                    ))
                    .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();
        assert_eq!(
            action.action,
            ControllerAction::RespondElicitation {
                session_id: "session-1".into(),
                elicitation_id: "elicitation-1".into(),
                response: accept(&[("question_0", "reusable")]),
            }
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn elicitation_answer_for_an_unknown_request_is_refused_without_reaching_the_controller()
    {
        let (app, mut actions, _, _, _) = app_with_snapshot(pending_elicitation_snapshot);
        let cookie = login_cookie(&app).await;
        let response = app
            .oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"respond-elicitation","session_id":"session-1","elicitation_id":"elicitation-9","response":{"action":"cancel"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(actions.try_recv().is_err());
    }

    #[test]
    fn elicitation_answers_are_checked_against_the_request_the_agent_asked() {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        pending_elicitation_snapshot(&mut snapshot);
        let respond = |response: ElicitationResponse| ControllerAction::RespondElicitation {
            session_id: "session-1".into(),
            elicitation_id: "elicitation-1".into(),
            response,
        };

        assert!(validate_action(&respond(accept(&[("question_0", "matrix")])), &snapshot).is_ok());
        // Declining and cancelling never carry content, so they are always
        // answerable.
        assert!(validate_action(&respond(ElicitationResponse::Decline), &snapshot).is_ok());
        // An option the agent never offered, a field it never published, and a
        // missing required answer are all refused.
        assert!(validate_action(&respond(accept(&[("question_0", "cron")])), &snapshot).is_err());
        assert!(validate_action(&respond(accept(&[("smuggled", "yes")])), &snapshot).is_err());
        assert!(validate_action(&respond(accept(&[])), &snapshot).is_err());
        // A custom answer stands in for the select it belongs to, exactly as
        // the chat form submits it.
        assert!(
            validate_action(
                &respond(accept(&[("question_0_custom", "a monorepo pipeline")])),
                &snapshot,
            )
            .is_ok()
        );
    }

    #[test]
    fn oversized_elicitation_answers_are_refused() {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        pending_elicitation_snapshot(&mut snapshot);
        let long = "x".repeat(MAX_ELICITATION_BYTES);
        assert!(
            validate_action(
                &ControllerAction::RespondElicitation {
                    session_id: "session-1".into(),
                    elicitation_id: "elicitation-1".into(),
                    response: accept(&[("question_0_custom", long.as_str())]),
                },
                &snapshot,
            )
            .is_err()
        );
    }

    /// One slice of the browser application, named by the two markers that
    /// bracket it in `src/web/viewer.js`.
    ///
    /// Slicing keeps each check to the functions it is about, so an unrelated
    /// change elsewhere in the application cannot make it fail for the wrong
    /// reason. The markers are ordinary source text, so a rename that moves
    /// them fails loudly here rather than silently testing nothing.
    fn viewer_source(from: &str, to: &str) -> &'static str {
        let start = VIEWER_JS
            .find(from)
            .unwrap_or_else(|| panic!("src/web/viewer.js no longer contains {from:?}"));
        let end = VIEWER_JS[start..]
            .find(to)
            .map(|offset| start + offset)
            .unwrap_or_else(|| {
                panic!("src/web/viewer.js no longer contains {to:?} after {from:?}")
            });
        &VIEWER_JS[start..end]
    }

    /// Run one JavaScript check under Node.
    ///
    /// The check and the modules it imports are written to a real directory
    /// rather than passed to `--eval`, so a failure reports a line number a
    /// person can open, and so a check can import the shipped module under
    /// test by its real name instead of against a copy pasted into a string.
    fn run_web_check(name: &str, check: &str) {
        let directory = tempfile::tempdir().expect("temporary directory for a web check");
        for (file, source) in [
            ("test-dom.js", TEST_DOM_JS),
            ("markdown.js", MARKDOWN_JS),
            ("tool-output.js", TOOL_OUTPUT_JS),
        ] {
            std::fs::write(directory.path().join(file), source).expect("write a web module");
        }
        let path = directory.path().join(format!("{name}.mjs"));
        std::fs::write(&path, check).expect("write the web check");
        let output = std::process::Command::new("node")
            .arg(&path)
            .output()
            .expect("Node.js is required to exercise the web viewer");
        assert!(
            output.status.success(),
            "{name} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Run one JavaScript check that supplies its own environment, for the
    /// checks that slice a function out of `viewer.js` and drive it against a
    /// hand-written stub rather than importing a module.
    fn run_viewer_script(name: &str, script: &str) {
        run_web_check(name, script);
    }

    /// The projection publishes what the browser needs to group and filter
    /// without publishing what the redaction contract keeps back. A project
    /// key groups two sessions in one project together and says nothing about
    /// where that project lives.
    #[test]
    fn the_project_key_groups_without_naming_a_path() {
        let (config, mut state) = sample_config_state();
        let first = state.sessions["session-1"].clone();
        let mut second = first.clone();
        second.id = "session-2".into();
        state.sessions.insert(second.id.clone(), second);
        let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);

        let keys = snapshot
            .sessions
            .iter()
            .map(|session| session.project_key.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys.len(), 1, "two sessions in one project did not group");
        let key = keys.into_iter().next().expect("one key");
        assert!(!key.is_empty(), "the project key is empty");
        assert!(
            !key.contains('/') && !key.contains("hel"),
            "the project key leaks its identity: {key}"
        );
        assert_eq!(
            snapshot.sessions[0].project_label, "hel",
            "the project label should be a name a person recognises"
        );
    }

    /// A phone groups and filters by the lifecycle category, so the mapping
    /// from the controller's precise state has to be the controller's own.
    #[test]
    fn lifecycle_categories_decide_what_the_dashboard_shows() {
        use ViewerLifecycleCategory::{Failed, Live, Starting, Stopped, Stopping};

        for (state, expected, on_dashboard) in [
            (SessionState::Provisioning, Starting, true),
            (SessionState::Running, Live, true),
            (SessionState::Disconnected, Live, true),
            (SessionState::Checkpointing, Live, true),
            (SessionState::Closing, Stopping, true),
            (SessionState::Destroying, Stopping, true),
            (SessionState::Stopped, Stopped, false),
            (SessionState::Lost, Failed, false),
            (SessionState::Error, Failed, false),
            (SessionState::DestroyedWithDataLoss, Failed, false),
        ] {
            let category = ViewerLifecycleCategory::of(state);
            assert_eq!(category, expected, "{state:?}");
            assert_eq!(
                category.is_dashboard_visible(),
                on_dashboard,
                "{state:?} belongs on the dashboard? "
            );
        }
    }

    /// Resume compatibility travels as the set the browser can offer, so it
    /// never has to subtract one list from another and never offers a target
    /// the controller would refuse.
    #[test]
    fn compatible_resume_targets_are_the_complement_of_the_incompatible_ones() {
        let (config, state) = sample_config_state();
        let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        let session = &snapshot.sessions[0];
        let all = config.targets.keys().cloned().collect::<Vec<_>>();

        for target in &all {
            assert_ne!(
                session.compatible_resume_targets.contains(target),
                session.incompatible_resume_targets.contains(target),
                "target {target} is in both lists or neither"
            );
        }
        assert_eq!(
            session.compatible_resume_targets.len() + session.incompatible_resume_targets.len(),
            all.len(),
            "the two lists do not cover every target"
        );
    }

    /// The viewer renders a control because a capability says so. An action
    /// whose capability is false is refused at the boundary, so a forged
    /// request gets the same answer a well-behaved viewer would never ask for.
    #[tokio::test]
    async fn actions_are_refused_when_their_capability_is_false() {
        for (body, capability) in [
            (
                r#"{"action":"cancel-turn","session_id":"session-1"}"#,
                "cancel_turn",
            ),
            (
                r#"{"action":"set-plan-mode","session_id":"session-1","active":true}"#,
                "set_plan_mode",
            ),
            (
                r#"{"action":"set-config","session_id":"session-1","key":"model","value":"x"}"#,
                "set_config",
            ),
        ] {
            let (app, mut actions, _, _, _) = app();
            let response = post_action(app, cookie(), body.to_owned()).await;
            assert!(
                response.status().is_client_error(),
                "{capability} was accepted while false: {}",
                response.status()
            );
            assert!(
                actions.try_recv().is_err(),
                "{capability} reached the controller while false"
            );
        }
    }

    /// A setting the harness never advertised is not a setting. Forwarding one
    /// asks the agent to refuse something the viewer should never have offered.
    #[tokio::test]
    async fn a_config_key_the_harness_never_advertised_is_refused() {
        let capable = |snapshot: &mut ViewerSnapshot| {
            snapshot.sessions[0].capabilities.set_config = true;
            snapshot.sessions[0].config_options = vec![ViewerConfigOption {
                key: "model".into(),
                label: "model".into(),
                current: None,
                choices: vec![ViewerConfigChoice {
                    value: "sonnet".into(),
                    name: "Sonnet".into(),
                    description: None,
                }],
            }];
        };

        for (body, why) in [
            (
                r#"{"action":"set-config","session_id":"session-1","key":"effort","value":"high"}"#,
                "an unadvertised key",
            ),
            (
                r#"{"action":"set-config","session_id":"session-1","key":"model","value":"gpt-9"}"#,
                "an unoffered value",
            ),
        ] {
            let (app, mut actions, _, _, _) = app_with_snapshot(capable);
            let response = post_action(app, cookie(), body.to_owned()).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{why} was accepted"
            );
            assert!(actions.try_recv().is_err(), "{why} reached the controller");
        }

        // The value the harness did advertise is forwarded unchanged.
        let (app, mut actions, _, _, _) = app_with_snapshot(capable);
        let response = tokio::spawn(post_action(
            app,
            cookie(),
            r#"{"action":"set-config","session_id":"session-1","key":"model","value":"sonnet"}"#
                .to_owned(),
        ));
        let action = actions
            .recv()
            .await
            .expect("the action reached the controller");
        assert!(
            matches!(
                action.action,
                ControllerAction::SetConfig { ref key, ref value, .. }
                    if key == "model" && value == "sonnet"
            ),
            "the advertised value was not forwarded unchanged"
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
    }

    /// A dirty-worktree acknowledgement names the repositories the person was
    /// shown. A bare yes could be replayed against a set they never saw.
    #[tokio::test]
    async fn a_dirty_acknowledgement_is_bounded_and_names_repositories() {
        let oversized = (0..40)
            .map(|index| format!(r#""repo-{index}""#))
            .collect::<Vec<_>>()
            .join(",");
        for (ack, why) in [
            (oversized.as_str(), "an unbounded acknowledgement"),
            (r#""""#, "an empty repository name"),
        ] {
            let (app, mut actions, _, _, _) = app();
            let body = format!(
                r#"{{"action":"new","workspace_id":"default","profile_id":"codex-1","bundle_id":"hel","target_id":"podman","dirty_ack":[{ack}]}}"#
            );
            let response = post_action(app, cookie(), body).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{why}");
            assert!(actions.try_recv().is_err(), "{why} reached the controller");
        }
    }

    /// A session created without a title still gets one, derived the way the
    /// terminal derives it, so the two surfaces name a session alike.
    #[tokio::test]
    async fn a_new_session_without_a_title_is_accepted() {
        let (app, mut actions, _, _, _) = app();
        let response = tokio::spawn(post_action(
            app,
            cookie(),
            r#"{"action":"new","workspace_id":"default","profile_id":"codex-1","bundle_id":"hel","target_id":"podman"}"#
                .to_owned(),
        ));
        // The handler answers only once the controller does, so the reply has
        // to be sent before the response can be read.
        let action = actions
            .recv()
            .await
            .expect("the action reached the controller");
        assert!(
            matches!(
                action.action,
                ControllerAction::New { title: None, ref workspace_id, .. }
                    if workspace_id == "default"
            ),
            "the workspace or the absent title did not survive the boundary"
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
    }

    /// Two phones must not share stored state, and one phone's state must
    /// survive its own re-login. Neither is true of a cookie that signs only
    /// an expiry, which is what this replaced.
    #[test]
    fn a_cookie_names_one_viewer_and_two_cookies_never_collide() {
        let key = b"01234567890123456789012345678901";
        let expiry = now_unix().saturating_add(3600);
        let first = signed_cookie_value(key, "viewer-a", expiry);
        let second = signed_cookie_value(key, "viewer-b", expiry);
        assert_ne!(
            first, second,
            "two viewers unlocking in the same second share a cookie"
        );
        assert_eq!(
            cookie_viewer(key, &first, now_unix()),
            Some(Some("viewer-a".to_owned()))
        );
        assert_eq!(
            cookie_viewer(key, &second, now_unix()),
            Some(Some("viewer-b".to_owned()))
        );
    }

    /// A phone holding the previous cookie keeps working through a deployment.
    /// It names no viewer, so it stores nothing, which is the difference
    /// between signed out and signed in with nothing kept.
    #[test]
    fn a_legacy_cookie_still_authenticates_and_stores_nothing() {
        let key = b"01234567890123456789012345678901";
        let expiry = now_unix().saturating_add(3600);
        let legacy = legacy_signed_cookie_value(key, expiry);
        assert_eq!(cookie_viewer(key, &legacy, now_unix()), Some(None));
        assert!(session_cookie_valid(key, &legacy, now_unix()));
        assert!(
            !session_cookie_valid(key, &legacy, expiry),
            "an expired legacy cookie still authenticated"
        );
    }

    /// A forged or tampered cookie names nobody.
    #[test]
    fn a_tampered_cookie_is_refused() {
        let key = b"01234567890123456789012345678901";
        let expiry = now_unix().saturating_add(3600);
        let honest = signed_cookie_value(key, "viewer-a", expiry);
        let swapped = honest.replacen("viewer-a", "viewer-b", 1);
        assert_eq!(cookie_viewer(key, &swapped, now_unix()), None);
        assert_eq!(cookie_viewer(key, "nonsense", now_unix()), None);
        assert_eq!(cookie_viewer(key, &format!("{expiry}."), now_unix()), None);
    }

    /// A composer is for a prompt. The bound exists so one viewer cannot fill
    /// the daemon's database with text it never sent.
    #[tokio::test]
    async fn an_oversized_draft_is_refused_with_a_stable_code() {
        let (app, _, _, _, mut stored) = app();
        let draft = "x".repeat(64 * 1024 + 1);
        let response = app
            .oneshot(
                Request::put("/api/sessions/session-1/draft")
                    .header(COOKIE, cookie())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "draft": draft }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(stored.try_recv().is_err(), "an oversized draft was stored");
    }

    /// A viewer with no identity has nothing stored, and is told so rather
    /// than being promised a persistence that is not there.
    #[tokio::test]
    async fn a_legacy_viewer_reads_empty_state_and_cannot_store_a_draft() {
        let key = b"01234567890123456789012345678901";
        let legacy = format!(
            "{COOKIE_NAME}={}",
            legacy_signed_cookie_value(key, now_unix().saturating_add(3600))
        );

        let (reader, _, _, _, mut stored) = app();
        let response = reader
            .oneshot(
                Request::get("/api/sessions/session-1/client-state")
                    .header(COOKIE, legacy.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let state: ViewerClientState = serde_json::from_slice(&body).unwrap();
        assert_eq!(state, ViewerClientState::default());
        assert!(
            stored.try_recv().is_err(),
            "a legacy viewer read stored state"
        );

        let (writer, _, _, _, mut stored) = app();
        let response = writer
            .oneshot(
                Request::put("/api/sessions/session-1/draft")
                    .header(COOKIE, legacy)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"draft":"text"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(stored.try_recv().is_err(), "a legacy viewer stored a draft");
    }

    /// A search that is not a search is refused before it reaches a database.
    #[tokio::test]
    async fn prompt_history_refuses_an_unknown_scope() {
        let (app, _, _, _, mut stored) = app();
        let response = app
            .oneshot(
                Request::get("/api/sessions/session-1/history?q=ship&scope=everything")
                    .header(COOKIE, cookie())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            stored.try_recv().is_err(),
            "the search reached the controller"
        );
    }

    /// A preflight starts nothing. It answers the questions a person needs
    /// before committing, and it refuses an impossible combination there
    /// rather than after the commit.
    #[tokio::test]
    async fn a_preflight_validates_before_it_reaches_the_controller() {
        for (body, why) in [
            (
                r#"{"profile_id":"nope","bundle_id":"hel","target_id":"podman"}"#,
                "an unknown profile",
            ),
            (
                r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"raw"}"#,
                "a bare target with no directory",
            ),
        ] {
            let (app, _, _, mut preflights, _) = app();
            let response = app
                .oneshot(
                    Request::post("/api/preflight/new")
                        .header(COOKIE, cookie())
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{why}");
            assert!(
                preflights.try_recv().is_err(),
                "{why} reached the controller"
            );
        }
    }

    /// A bare target opens a directory the person named, so there is no bundle
    /// whose repositories could be dirty and nothing to ask the controller.
    #[tokio::test]
    async fn a_bare_preflight_answers_without_the_controller() {
        let (app, _, _, mut preflights, _) = app();
        let response = app
            .oneshot(
                Request::post("/api/preflight/new")
                    .header(COOKIE, cookie())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"raw","project_directory":"/work/project"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(preflights.try_recv().is_err(), "the controller was asked");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let answer: PreflightNew = serde_json::from_slice(&body).unwrap();
        assert!(answer.dirty_repositories.is_empty());
    }

    /// A bundle preflight asks the controller, because whether a working tree
    /// has uncommitted changes is a fact about the disk.
    #[tokio::test]
    async fn a_bundle_preflight_reports_the_repositories_by_leaf_name() {
        let (app, _, _, mut preflights, _) = app();
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/preflight/new")
                    .header(COOKIE, cookie())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"profile_id":"codex-1","bundle_id":"hel","target_id":"podman"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let request = preflights.recv().await.expect("the controller was asked");
        assert_eq!(request.bundle_id, "hel");
        request
            .reply
            .send(Ok(PreflightNew {
                dirty_repositories: vec!["hel".into()],
            }))
            .unwrap();
        let response = response.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let answer: PreflightNew = serde_json::from_slice(&body).unwrap();
        assert_eq!(answer.dirty_repositories, vec!["hel".to_owned()]);
        assert!(
            !String::from_utf8_lossy(&body).contains('/'),
            "the preflight published a path: {}",
            String::from_utf8_lossy(&body)
        );
    }

    /// Everything an agent writes goes through the Markdown renderer, so the
    /// renderer is where injection is stopped. These checks run the shipped
    /// module against a fake DOM: structure has to come out as elements, and
    /// markup an agent typed has to come out as text.
    #[test]
    fn the_markdown_renderer_builds_structure_and_refuses_injection() {
        run_web_check(
            "markdown",
            r#"import { installDocument, elements, only, check, checkEqual } from './test-dom.js';
installDocument();
const { renderMarkdown, renderDiffSummary, safeHref } = await import('./markdown.js');

const render = source => {
  const host = document.createElement('section');
  host.append(renderMarkdown(source));
  return host;
};

// Headings
checkEqual(only(render('# Title'), 'h1').textContent, 'Title', 'h1');
checkEqual(only(render('### Deep'), 'h3').textContent, 'Deep', 'h3');

// Nested lists
const nested = render('- one\n  - inner\n- two');
check(elements(nested, 'ul').length === 2, 'nested list produced ' + elements(nested, 'ul').length + ' lists');
check(elements(elements(nested, 'ul')[0], 'li').length >= 2, 'outer list lost items');

// Ordered lists
checkEqual(elements(render('1. a\n2. b'), 'ol').length, 1, 'ordered list');

// Fenced code stays unparsed
const fenced = render('```rust\nlet x = *y*;\n```');
checkEqual(only(fenced, 'code').textContent, 'let x = *y*;', 'fenced code');
check(elements(fenced, 'span').some(s => s.className === 'tok-kw'), 'fenced rust untinted');
checkEqual(elements(fenced, 'em').length, 0, 'fence emphasised its contents');
checkEqual(only(fenced, 'pre').dataset.lang, 'rust', 'fence language');

// Inline code beats emphasis
checkEqual(only(render('`*not em*`'), 'code').textContent, '*not em*', 'inline code');
checkEqual(elements(render('`*not em*`'), 'em').length, 0, 'inline code emphasised');

// Emphasis
checkEqual(only(render('**bold**'), 'strong').textContent, 'bold', 'strong');
checkEqual(only(render('*it*'), 'em').textContent, 'it', 'em');
checkEqual(only(render('~~gone~~'), 'del').textContent, 'gone', 'del');

// Tables
const table = render('| a | b |\n| --- | ---: |\n| 1 | 2 |');
checkEqual(elements(table, 'table').length, 1, 'table');
checkEqual(elements(table, 'th').length, 2, 'table header cells');
checkEqual(elements(table, 'td').length, 2, 'table body cells');
checkEqual(elements(table, 'th')[1].className, 'align-right', 'table alignment class');
checkEqual(only(table, 'div').className, 'scroll-x', 'table scroll wrapper');

// Blockquote and rule
checkEqual(elements(render('> quoted'), 'blockquote').length, 1, 'blockquote');
checkEqual(elements(render('---'), 'hr').length, 1, 'rule');

// XSS: markup is text, never elements
const injected = render('<img src=x onerror=alert(1)>');
checkEqual(elements(injected, 'img').length, 0, 'raw HTML became an element');
check(injected.textContent.includes('<img src=x onerror=alert(1)>'), 'raw HTML lost its text');

// XSS: refused link schemes
for (const target of ['javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'java\tscript:alert(1)', 'data:text/html,<script>', 'vbscript:x']) {
  const out = render(`[click](${target})`);
  checkEqual(elements(out, 'a').length, 0, `link scheme ${JSON.stringify(target)} was allowed`);
  check(out.textContent.includes('click'), `link scheme ${JSON.stringify(target)} lost its label`);
}

// Accepted schemes keep their href and carry safe rel/target
for (const target of ['https://example.com', 'http://example.com/a', 'mailto:someone@example.com']) {
  const anchor = only(render(`[click](${target})`), 'a');
  checkEqual(anchor.getAttribute('href'), target, 'href');
  checkEqual(anchor.getAttribute('rel'), 'noreferrer noopener', 'rel');
  checkEqual(anchor.getAttribute('target'), '_blank', 'target');
}

// safeHref directly
checkEqual(safeHref('javascript:alert(1)'), null, 'safeHref allowed javascript:');
checkEqual(safeHref(' https://x.test '), 'https://x.test', 'safeHref cleaned value');

// Inline markup inside a link label
checkEqual(only(render('[**bold link**](https://x.test)'), 'strong').textContent, 'bold link', 'link label markup');

// An unclosed delimiter is literal, not markup
checkEqual(render('a * b').textContent, 'a * b', 'unclosed emphasis');
checkEqual(elements(render('a * b'), 'em').length, 0, 'unclosed emphasis made an element');

// Diff summaries: the real format from format_diffstat, two spaces and U+2212
const diff = renderDiffSummary(['src/main.rs  +12 −3', 'unparseable line']);
const items = elements(diff, 'li');
checkEqual(items.length, 2, 'diffstat rows');
checkEqual(elements(items[0], 'span')[0].textContent, 'src/main.rs', 'diffstat path');
checkEqual(elements(items[0], 'span')[1].textContent, '+12', 'diffstat additions');
checkEqual(elements(items[0], 'span')[2].textContent, '−3', 'diffstat deletions');
checkEqual(elements(items[1], 'span').length, 1, 'unparseable diffstat produced counts');
checkEqual(elements(items[1], 'span')[0].textContent, 'unparseable line', 'unparseable diffstat lost its text');

console.log('all markdown checks passed');
"#,
        );
    }

    /// Tool output is not prose, and rendering it as prose loses the parts
    /// that matter: which words in a command are the program and which are
    /// paths, where a JSON payload begins, and whether a five-thousand-line
    /// dump has to be paid for before anyone asks to see it.
    #[test]
    fn tool_output_is_tinted_folded_and_never_read_as_markdown() {
        run_web_check(
            "tool-output",
            r#"import { installDocument, elements, only, check, checkEqual, openFold } from './test-dom.js';
installDocument();
const { renderToolOutput, codeBlock, detectLang, appendCommandTokens, isPathLike } = await import(
  './tool-output.js'
);

const classes = root => elements(root, 'span').map(s => s.className);

// A shell command is told apart into program, subcommand, flag and path.
const line = document.createElement('pre');
appendCommandTokens(line, 'cargo test --workspace src/lib.rs');
const seen = classes(line);
check(seen.includes('cmd-program'), 'no program: ' + seen);
check(seen.includes('cmd-subcommand'), 'no subcommand: ' + seen);
check(seen.includes('cmd-flag'), 'no flag: ' + seen);
check(seen.includes('cmd-path'), 'no path: ' + seen);
checkEqual(line.textContent, 'cargo test --workspace src/lib.rs', 'command text changed');

// An operator starts the program count again, so both programs are found.
const piped = document.createElement('pre');
appendCommandTokens(piped, 'git status && cargo build');
checkEqual(classes(piped).filter(c => c === 'cmd-program').length, 2, 'pipeline reset');

// Prose with a slash is not a path; a real path is.
check(!isPathLike('and/or'), '"and/or" read as a path');
check(isPathLike('src/lib/thing.rs'), 'a real path did not');
check(isPathLike('./x'), 'a relative path did not');
check(isPathLike('Cargo.toml'), 'a file with an extension did not');

// JSON is pretty-printed and tinted, keys apart from values.
const json = renderToolOutput('{"name":"hel","count":3,"ok":true}');
const jsonClasses = classes(json);
check(jsonClasses.includes('tok-key'), 'no JSON key: ' + jsonClasses);
check(jsonClasses.includes('tok-str'), 'no JSON string: ' + jsonClasses);
check(jsonClasses.includes('tok-num'), 'no JSON number: ' + jsonClasses);
check(jsonClasses.includes('tok-kw'), 'no JSON keyword: ' + jsonClasses);
check(json.textContent.includes('"name"'), 'JSON lost its content');

// Rust is tinted; an unknown language is not.
const rust = codeBlock('pub fn main() {\n    let x = 1;\n}', 'rust');
check(classes(rust).includes('tok-kw'), 'rust keywords untinted');
checkEqual(only(rust, 'pre').dataset.lang, 'rust', 'rust data-lang');
const plain = codeBlock('nothing in particular here', 'brainfuck');
checkEqual(classes(plain).length, 0, 'unknown language was tinted');

// Sniffing is conservative: a log stays plain, real code does not.
checkEqual(detectLang('12:03 INFO started\n12:04 INFO done\n12:05 INFO stopped'), '', 'a log was sniffed');
checkEqual(
  detectLang('fn a() {}\nfn b() {}\nlet mut x = 1;\nuse std::fmt;\nimpl Foo {}\nlet y = x.unwrap();'),
  'rust',
  'rust was not sniffed',
);
checkEqual(detectLang('--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new'), 'diff', 'diff was not sniffed');

// A long dump is one closed fold that has built nothing yet.
const long = Array.from({ length: 400 }, (_, i) => `line ${i}`).join('\n');
const folded = renderToolOutput(long);
checkEqual(folded.nodeName, 'DETAILS', 'a 400-line dump was not folded');
checkEqual(elements(folded, 'pre').length, 0, 'a closed fold built its content anyway');
check(only(folded, 'summary').textContent.includes('400 lines'), 'fold summary: ' + only(folded, 'summary').textContent);
openFold(folded);
checkEqual(elements(folded, 'pre').length, 1, 'an opened fold built nothing');
check(elements(folded, 'pre')[0].textContent.includes('line 399'), 'the fold lost its content');

// Opening twice builds once.
openFold(folded);
checkEqual(elements(folded, 'pre').length, 1, 'reopening rebuilt the content');

// A short dump is not folded.
checkEqual(renderToolOutput('one\ntwo').nodeName, 'PRE', 'a short dump was folded');

// Tool output is never parsed as Markdown, so an underscore is an underscore.
const literal = renderToolOutput('a _b_ c <img src=x>');
checkEqual(elements(literal, 'em').length, 0, 'tool output was emphasised');
checkEqual(elements(literal, 'img').length, 0, 'tool output produced an element');
check(literal.textContent.includes('<img src=x>'), 'tool output lost its text');

console.log('all tool-output checks passed');
"#,
        );
    }

    /// The renderer's guarantee is structural — this code cannot inject markup
    /// because it never builds markup — and a single stray assignment would
    /// quietly replace it with no guarantee at all. `escapeHtml` uses
    /// `innerHTML` on a detached node to escape text, which is safe but is
    /// also exactly the shape this test exists to stop spreading, so it is
    /// named rather than pattern-matched.
    #[test]
    fn no_web_module_builds_markup_from_a_string() {
        const SINKS: [&str; 5] = [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "new Function",
        ];
        // There is no allowance. Every one of these sinks was removed in
        // Milestone 2, and the point of the test is that none comes back.
        const ALLOWED: [(&str, &str); 0] = [];
        for (name, source) in [
            ("viewer.js", VIEWER_JS),
            ("markdown.js", MARKDOWN_JS),
            ("tool-output.js", TOOL_OUTPUT_JS),
        ] {
            for (number, line) in source.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                for sink in SINKS {
                    if !trimmed.contains(sink) {
                        continue;
                    }
                    assert!(
                        ALLOWED
                            .iter()
                            .any(|(file, allowed)| *file == name && trimmed == *allowed),
                        "{name}:{} builds markup from a string: {trimmed}",
                        number + 1
                    );
                }
            }
        }
    }

    /// The card cache is the fix for answers vanishing under snapshot polls, so
    /// it is exercised as JavaScript: the render source is lifted out of
    /// `src/web/viewer.js` and run against a stub DOM.
    #[test]
    fn embedded_viewer_keeps_elicitation_answers_across_snapshot_polls() {
        let source = viewer_source(
            "const elicitationCards = new Map()",
            "async function submitElicitation",
        );
        let dom = r#"
let replaceCalls = 0;
class Option {
  constructor(label, value) {
    this.label = label;
    this.value = value;
    this.selected = false;
  }
}
function makeEl(tag) {
  return {
    tagName: tag.toUpperCase(),
    children: [],
    options: [],
    selectedOptions: [],
    className: "",
    textContent: "",
    disabled: false,
    required: false,
    value: "",
    appendChild(child) {
      this.children.push(child);
      if (this.tagName === "SELECT") this.options.push(child);
      return child;
    },
    append(...kids) {
      this.children.push(...kids);
    },
    replaceChildren(...kids) {
      replaceCalls += 1;
      this.children = kids;
    },
    addEventListener() {},
    setCustomValidity() {},
    reportValidity() {
      return true;
    },
  };
}
const created = [];
const document = {
  createElement(tag) {
    const el = makeEl(tag);
    created.push(el);
    return el;
  },
};
const elicitations = makeEl("div");
async function submitElicitation() {}
"#;
        let checks = r#"
const request = {
  id: "elicitation-1",
  message: "Which CI architecture?",
  title: "CI",
  fields: [
    {
      id: "question_0",
      title: "CI architecture",
      required: false,
      kind: "single_select",
      options: [{ value: "reusable", title: "Reusable" }, { value: "matrix", title: "Matrix" }],
    },
    { id: "question_0_custom", title: "Other", required: false, kind: "text" },
  ],
};
const session = { id: "session-1", pending_elicitations: [request] };
renderElicitations(session);
const card = elicitations.children[0];
const select = created.find((el) => el.tagName === "SELECT");
const text = created.find((el) => el.tagName === "INPUT");
select.value = "reusable";
text.value = "keep me";
const attachments = replaceCalls;
renderElicitations(session);
if (elicitations.children[0] !== card) {
  throw new Error("a snapshot rebuilt the pending card");
}
if (select.value !== "reusable" || text.value !== "keep me") {
  throw new Error("a snapshot wiped the half-filled answer");
}
if (replaceCalls !== attachments) {
  throw new Error("a snapshot re-attached an unchanged card and dropped focus");
}
sentElicitations.add(elicitationKey("session-1", request.id));
renderElicitations(session);
if (elicitations.children[0] !== card) {
  throw new Error("a sent answer rebuilt the card");
}
if (!select.disabled || !text.disabled) {
  throw new Error("a sent answer left the controls live");
}
if (select.value !== "reusable") {
  throw new Error("a sent answer wiped the reply");
}
renderElicitations({ id: "session-1", pending_elicitations: [] });
if (elicitations.children.length !== 0 || elicitationCards.size !== 0) {
  throw new Error("an answered request stayed rendered");
}
if (sentElicitations.size !== 0) {
  throw new Error("a resolved request kept its sent marker");
}
"#;
        run_viewer_script(
            "elicitation-rendering",
            &format!("{dom}\n{source}\n{checks}"),
        );
    }

    fn sample_image(pixels: usize) -> ViewerPromptImage {
        ViewerPromptImage {
            data_base64: base64::engine::general_purpose::STANDARD.encode(vec![7_u8; pixels]),
            mime_type: "image/png".into(),
            width: 32,
            height: 24,
        }
    }

    fn image_capable(snapshot: &mut ViewerSnapshot) {
        snapshot.sessions[0].prompt_images_supported = true;
    }

    async fn post_action(app: Router, cookie: String, body: String) -> Response<Body> {
        app.oneshot(
            Request::post("/api/actions")
                .header(COOKIE, cookie)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn image_prompt_reaches_the_controller_with_its_images() {
        let (app, mut actions, _, _, _) = app_with_snapshot(image_capable);
        let cookie = login_cookie(&app).await;
        let image = sample_image(8);
        let body = serde_json::to_string(&ControllerAction::Prompt {
            session_id: "session-1".into(),
            text: String::new(),
            images: vec![image.clone(), image.clone()],
        })
        .unwrap();
        let response = tokio::spawn(post_action(app, cookie, body));
        let action = actions.recv().await.unwrap();
        assert_eq!(
            action.action,
            ControllerAction::Prompt {
                session_id: "session-1".into(),
                text: String::new(),
                images: vec![image.clone(), image],
            }
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
    }

    /// Base64 inflates an upload by a third, so two ordinary photographs pass
    /// the general body limit even when each one fits it. The action route
    /// carries prompts, so it is the route that gets the larger bound.
    #[tokio::test]
    async fn multi_image_prompts_are_accepted_over_the_general_body_limit() {
        let (app, mut actions, _, _, _) = app_with_snapshot(image_capable);
        let cookie = login_cookie(&app).await;
        let image = sample_image(MAX_BODY_BYTES / 2);
        let body = serde_json::to_string(&ControllerAction::Prompt {
            session_id: "session-1".into(),
            text: "look at these".into(),
            images: vec![image.clone(), image],
        })
        .unwrap();
        assert!(body.len() > MAX_BODY_BYTES);
        assert!(body.len() < MAX_PROMPT_BODY_BYTES);
        let response = tokio::spawn(post_action(app, cookie, body));
        let action = actions.recv().await.unwrap();
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(response.await.unwrap().status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_body_over_the_prompt_limit_is_still_refused() {
        let (app, _actions, _, _, _) = app_with_snapshot(image_capable);
        let cookie = login_cookie(&app).await;
        let image = sample_image(MAX_PROMPT_BODY_BYTES);
        let body = serde_json::to_string(&ControllerAction::Prompt {
            session_id: "session-1".into(),
            text: String::new(),
            images: vec![image],
        })
        .unwrap();
        assert!(body.len() > MAX_PROMPT_BODY_BYTES);
        let response = post_action(app, cookie, body).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn malformed_image_payloads_never_reach_the_controller() {
        let cases = [
            ("aW1hZ2U=", "text/plain", 32, 24),
            ("aW1hZ2U=", "image/png", 0, 24),
            ("not base64!", "image/png", 32, 24),
            ("", "image/png", 32, 24),
        ];
        for (data, mime, width, height) in cases {
            let (app, mut actions, _, _, _) = app_with_snapshot(image_capable);
            let cookie = login_cookie(&app).await;
            let body = serde_json::to_string(&ControllerAction::Prompt {
                session_id: "session-1".into(),
                text: String::new(),
                images: vec![ViewerPromptImage {
                    data_base64: data.into(),
                    mime_type: mime.into(),
                    width,
                    height,
                }],
            })
            .unwrap();
            let response = post_action(app, cookie, body).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "expected {data:?}/{mime} {width}x{height} to be refused"
            );
            assert!(actions.try_recv().is_err());
        }
    }

    #[test]
    fn image_prompts_need_text_or_an_image_and_an_agent_that_takes_them() {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        let prompt = |text: &str, images: Vec<ViewerPromptImage>| ControllerAction::Prompt {
            session_id: "session-1".into(),
            text: text.into(),
            images,
        };

        // Without the capability the session takes text only.
        assert!(validate_action(&prompt("ship it", Vec::new()), &snapshot).is_ok());
        assert!(validate_action(&prompt("", vec![sample_image(8)]), &snapshot).is_err());

        image_capable(&mut snapshot);
        // An image is a prompt on its own; nothing at all is not.
        assert!(validate_action(&prompt("", vec![sample_image(8)]), &snapshot).is_ok());
        assert!(validate_action(&prompt("   ", Vec::new()), &snapshot).is_err());
        assert!(validate_action(&prompt("", Vec::new()), &snapshot).is_err());
        // A shell command is still a shell command.
        assert!(validate_action(&prompt("!ls", vec![sample_image(8)]), &snapshot).is_err());
    }

    /// The composer holds a DOM, not a string, so the text a prompt sends is
    /// whatever this reader makes of that DOM. Run it as JavaScript.
    #[test]
    fn embedded_viewer_reads_multiline_composer_text_out_of_its_dom() {
        let source = viewer_source("function composerText()", "function setComposerText(");
        let harness = r##"
const Node = { TEXT_NODE: 3 };
function textNode(value) {
  return { nodeType: 3, nodeValue: value, nodeName: "#text", childNodes: [], dataset: {} };
}
function element(name, children = [], dataset = {}) {
  const node = { nodeType: 1, nodeName: name, dataset, childNodes: children };
  children.forEach((child, index) => {
    child.nextSibling = children[index + 1] || null;
  });
  return node;
}
let promptText = null;
function read(children) {
  promptText = element("DIV", children);
  return composerText();
}
"##;
        let checks = r#"
const plain = read([textNode("ship it")]);
if (plain !== "ship it") throw new Error(`plain text became ${JSON.stringify(plain)}`);

const broken = read([textNode("first"), element("BR"), textNode("second")]);
if (broken !== "first\nsecond") throw new Error(`line break became ${JSON.stringify(broken)}`);

// The trailing break a browser leaves behind to keep the caret on a new line
// is scaffolding, not a line the user typed.
const filler = read([
  textNode("first"),
  element("BR"),
  element("BR", [], { composerFiller: "true" }),
]);
if (filler !== "first\n") throw new Error(`filler break became ${JSON.stringify(filler)}`);

const blocks = read([
  textNode("first"),
  element("DIV", [textNode("second")]),
  element("DIV", [textNode("third")]),
]);
if (blocks !== "first\nsecond\nthird") throw new Error(`blocks became ${JSON.stringify(blocks)}`);

const carriage = read([textNode("first\r\nsecond")]);
if (carriage !== "first\nsecond") throw new Error(`CRLF became ${JSON.stringify(carriage)}`);
"#;
        run_viewer_script("composer-reader", &format!("{harness}\n{source}\n{checks}"));
    }

    /// A page that declares no icon makes every browser request
    /// `/favicon.ico`, which this server does not have. The page therefore has
    /// to name an icon, and that icon has to be served.
    #[tokio::test]
    async fn viewer_declares_the_icon_route_instead_of_requesting_a_missing_favicon() {
        let (app, _, _, _, _) = app();
        let page = fetch_text(app.clone(), "/").await;
        assert!(page.contains(r#"rel="icon""#), "the page declares no icon");
        assert!(page.contains("/icon.svg"), "the page names no icon route");
        let icon = app
            .oneshot(Request::get("/icon.svg").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(icon.status(), StatusCode::OK);
        assert_eq!(
            icon.headers().get(CONTENT_TYPE).unwrap(),
            "image/svg+xml",
            "the icon route does not serve an SVG"
        );
    }

    #[tokio::test]
    async fn valid_action_is_typed_and_forwarded() {
        let (app, mut actions, _, _, _) = app();
        let cookie = login_cookie(&app).await;
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"prompt","session_id":"session-1","text":"ship it"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();
        assert_eq!(
            action.action,
            ControllerAction::Prompt {
                session_id: "session-1".into(),
                text: "ship it".into(),
                images: Vec::new(),
            }
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        let response = response.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn shell_action_is_typed_and_forwarded() {
        let (app, mut actions, _, _, _) = app();
        let cookie = login_cookie(&app).await;
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"run-shell","session_id":"session-1","command":"cargo test"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();
        assert_eq!(
            action.action,
            ControllerAction::RunShell {
                session_id: "session-1".into(),
                command: "cargo test".into(),
            }
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[test]
    fn shell_action_validation_reserves_bang_prompts_and_checks_cancellation_ids() {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        assert!(
            validate_action(
                &ControllerAction::Prompt {
                    session_id: "session-1".into(),
                    text: "!cargo test".into(),
                    images: Vec::new(),
                },
                &snapshot,
            )
            .is_err()
        );
        assert!(
            validate_action(
                &ControllerAction::RunShell {
                    session_id: "session-1".into(),
                    command: "cargo test".into(),
                },
                &snapshot,
            )
            .is_ok()
        );
        assert!(
            validate_action(
                &ControllerAction::CancelShell {
                    session_id: "session-1".into(),
                    shell_command_id: "shell-1".into(),
                },
                &snapshot,
            )
            .is_err()
        );

        snapshot.sessions[0]
            .active_user_shells
            .push(ViewerUserShell {
                id: "shell-1".into(),
                command: "cargo test".into(),
                started_at_ms: Some(10),
            });
        assert!(
            validate_action(
                &ControllerAction::CancelShell {
                    session_id: "session-1".into(),
                    shell_command_id: "shell-1".into(),
                },
                &snapshot,
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn bare_new_action_forwards_an_explicit_safe_project_directory() {
        let (app, mut actions, _, _, _) = app();
        let cookie = login_cookie(&app).await;
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"new","profile_id":"codex-1","bundle_id":"hel","target_id":"raw","title":"Raw work","project_directory":"/work/project"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();
        assert_eq!(
            action.action,
            ControllerAction::New {
                workspace_id: String::new(),
                profile_id: "codex-1".into(),
                bundle_id: "hel".into(),
                target_id: "raw".into(),
                title: Some("Raw work".into()),
                project_directory: Some(PathBuf::from("/work/project")),
                dirty_ack: Vec::new(),
            }
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[test]
    fn new_action_requires_project_directory_exactly_for_bare_targets() {
        let (config, state) = sample_config_state();
        let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        let action = |target_id: &str, project_directory: Option<PathBuf>| ControllerAction::New {
            workspace_id: String::new(),
            profile_id: "codex-1".into(),
            bundle_id: "hel".into(),
            target_id: target_id.into(),
            title: Some("New work".into()),
            project_directory,
            dirty_ack: Vec::new(),
        };

        assert!(validate_action(&action("podman", None), &snapshot).is_ok());
        assert_eq!(
            validate_action(&action("podman", Some("/work".into())), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_action(&action("raw", None), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_action(&action("raw", Some("relative".into())), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_action(&action("raw", Some("/work/../secret".into())), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        assert!(validate_action(&action("raw", Some("/work/project".into())), &snapshot).is_ok());
    }

    #[tokio::test]
    async fn cancel_action_is_typed_and_forwarded() {
        let (app, mut actions, _, _, _) = app();
        let cookie = login_cookie(&app).await;
        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"cancel","session_id":"session-1"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();
        assert_eq!(
            action.action,
            ControllerAction::Cancel {
                session_id: "session-1".into(),
            }
        );
        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn action_validation_accepts_cross_harness_resume_and_rejects_unknown() {
        let (mut config, state) = sample_config_state();
        config.profiles.insert(
            "claude-1".into(),
            HarnessProfile {
                context_window_bytes: None,
                kind: HarnessKind::Claude,
                home: "/secret/claude".into(),
                executable: None,
                environment: BTreeMap::new(),
            },
        );
        let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        validate_action(
            &ControllerAction::Resume {
                session_id: "session-1".into(),
                profile_id: "claude-1".into(),
                target_id: "podman".into(),
                queue: ResumeQueueDisposition::Start,
            },
            &snapshot,
        )
        .unwrap();

        let error = validate_action(
            &ControllerAction::Close {
                session_id: "not-managed".into(),
            },
            &snapshot,
        )
        .unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
    }

    /// A review the daemon is running reaches the phone whole: its tier, what
    /// each reviewing agent is doing, and the findings to answer.
    #[test]
    fn a_running_review_projects_to_the_phone() {
        use crate::hel_review_host::{RuntimeReviewView, VerdictKind, VerdictView};
        use hel::hel_review::driver::{Resolution, RoleState, RoleStatus, TurnReviewPhase};

        let review = RuntimeReviewView {
            session_id: "session-1".into(),
            tier: hel::hel_review::lanes::ReviewTier::Extended,
            phase: TurnReviewPhase::Verdict(hel::hel_review::verdict::ReviewVerdict::Findings {
                synthesis: "[P1] src/lib.rs:1 -- unbounded retry".into(),
                evidence: Default::default(),
            }),
            roles: vec![
                RoleStatus {
                    role: "supervisor".into(),
                    label: "Supervisor".into(),
                    state: RoleState::Clean,
                },
                RoleStatus {
                    role: "tests".into(),
                    label: "Tests".into(),
                    state: RoleState::Findings,
                },
            ],
            status: "Enter to act".into(),
            verdict: Some(VerdictView {
                kind: VerdictKind::Findings,
                text: "[P1] src/lib.rs:1 -- unbounded retry".into(),
                allowed: vec![
                    Resolution::Forwarded,
                    Resolution::Dismissed,
                    Resolution::Cancelled,
                ],
            }),
        };

        let projected = ViewerTurnReview::from_runtime(&review);

        assert_eq!(projected.tier, "extended");
        assert_eq!(
            projected
                .roles
                .iter()
                .map(|role| (role.label.as_str(), role.state.as_str()))
                .collect::<Vec<_>>(),
            vec![("Supervisor", "clean"), ("Tests", "findings")]
        );
        let verdict = projected.verdict.expect("a findings verdict travels");
        assert_eq!(verdict.kind, "findings");
        assert!(verdict.text.contains("unbounded retry"));
        assert_eq!(verdict.allowed, vec!["forward", "dismiss", "cancel"]);
    }

    /// A phone can always cancel a review, and can only forward or dismiss one
    /// the daemon says is ready for it. The same gate runs in the daemon; this
    /// one is what makes the refusal immediate.
    #[test]
    fn resolving_a_review_is_gated_on_what_the_daemon_published() {
        let (config, state) = sample_config_state();
        let mut snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);

        let resolve = |resolution: &str| ControllerAction::ResolveReview {
            session_id: "session-1".into(),
            resolution: resolution.into(),
        };

        // No review at all.
        let error = validate_action(&resolve("cancel"), &snapshot).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);

        snapshot.sessions[0].turn_review = Some(ViewerTurnReview {
            tier: "quick".into(),
            status: "the reviewer is reading the change…".into(),
            roles: Vec::new(),
            verdict: None,
        });
        // Running: cancel works, the rest do not.
        validate_action(&resolve("cancel"), &snapshot).unwrap();
        assert_eq!(
            validate_action(&resolve("forward"), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );

        // A failed review can be dismissed but has nothing to forward.
        snapshot.sessions[0].turn_review = Some(ViewerTurnReview {
            tier: "quick".into(),
            status: "the review failed".into(),
            roles: Vec::new(),
            verdict: Some(ViewerReviewVerdict {
                kind: "failed".into(),
                text: "bifrost exited with 1".into(),
                allowed: vec!["dismiss".into(), "cancel".into()],
            }),
        });
        validate_action(&resolve("dismiss"), &snapshot).unwrap();
        assert_eq!(
            validate_action(&resolve("forward"), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );
        // A resolution that is not one of the three is refused by name.
        assert_eq!(
            validate_action(&resolve("approve"), &snapshot)
                .unwrap_err()
                .status,
            StatusCode::BAD_REQUEST
        );

        // Starting a review needs only a session that exists.
        validate_action(
            &ControllerAction::StartReview {
                session_id: "session-1".into(),
            },
            &snapshot,
        )
        .unwrap();
        assert_eq!(
            validate_action(
                &ControllerAction::StartReview {
                    session_id: "not-managed".into(),
                },
                &snapshot,
            )
            .unwrap_err()
            .status,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn resume_action_refuses_a_target_the_session_cannot_use() {
        let (mut config, state) = sample_config_state();
        // A project that only exists on GitHub cannot become a checkout on this
        // machine, so the bare target stays out of reach for its sessions.
        config.bundles.get_mut("hel").unwrap().repositories[0].local = None;
        let snapshot = ViewerSnapshot::from_config_state(&config, &state, 1);
        assert_eq!(
            snapshot.sessions[0].incompatible_resume_targets,
            vec!["raw".to_owned()]
        );

        let error = validate_action(
            &ControllerAction::Resume {
                session_id: "session-1".into(),
                profile_id: "codex-1".into(),
                target_id: "raw".into(),
                queue: ResumeQueueDisposition::Start,
            },
            &snapshot,
        )
        .unwrap_err();

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn snapshot_endpoint_returns_only_public_projection() {
        let (app, _, _, _, _) = app();
        let cookie = login_cookie(&app).await;
        let response = app
            .oneshot(
                Request::get("/api/snapshot")
                    .header(COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("session-1"));
        assert!(!body.contains("secret-token"));
        assert!(!body.contains("native-secret-id"));
        assert!(!body.contains("/private/source/hel"));

        let snapshot: serde_json::Value = serde_json::from_str(&body).unwrap();
        let repository = &snapshot["bundles"][0]["repositories"][0];
        assert_eq!(repository["id"], "hel");
        assert_eq!(repository["github"], "owner/hel");
        assert_eq!(repository["destination"], "hel");
        assert!(repository.get("local").is_none());
    }

    #[tokio::test]
    async fn conversation_endpoint_returns_authenticated_bounded_deltas() {
        let transcript = BrowserTranscript {
            latest_seq: 8,
            window_start_seq: 3,
            reset: false,
            entries: vec![
                BrowserTranscriptEntry {
                    id: 3,
                    updated_seq: 3,
                    role: "user",
                    label: "You".into(),
                    recorded_at_ms: None,
                    lines: vec!["begin".into()],
                    glyph: "\u{276f}",
                    tone: "user",
                    tool_status: None,
                    diffstats: Vec::new(),
                },
                BrowserTranscriptEntry {
                    id: 7,
                    updated_seq: 8,
                    role: "agent",
                    label: "Agent".into(),
                    recorded_at_ms: None,
                    lines: vec!["live".into()],
                    glyph: "\u{25cf}",
                    tone: "agent",
                    tool_status: None,
                    diffstats: Vec::new(),
                },
            ],
        };
        let (app, _, _, _, _) =
            app_with_conversations(BTreeMap::from([("session-1".into(), transcript)]));
        let cookie = login_cookie(&app).await;
        let response = app
            .oneshot(
                Request::get("/api/conversations/session-1?after_seq=3")
                    .header(COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["latest_seq"], 8);
        assert_eq!(body["reset"], false);
        assert_eq!(body["entries"].as_array().unwrap().len(), 1);
        assert_eq!(body["entries"][0]["lines"][0], "live");
    }

    #[tokio::test]
    async fn conversation_read_receipt_never_contends_with_a_running_action() {
        let (app, mut actions, mut receipts, _, _) = app();
        let cookie = login_cookie(&app).await;
        // A prompt for the same session stays in flight for the whole test, so
        // a receipt that still travelled the action pipeline would either
        // queue behind it or be rejected for the occupied session slot.
        let prompt = tokio::spawn(
            app.clone().oneshot(
                Request::post("/api/actions")
                    .header(COOKIE, cookie.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"action":"prompt","session_id":"session-1","text":"ship it"}"#,
                    ))
                    .unwrap(),
            ),
        );
        let action = actions.recv().await.unwrap();

        let response = tokio::spawn(
            app.oneshot(
                Request::post("/api/conversations/session-1/read")
                    .header(COOKIE, cookie)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"through":42}"#))
                    .unwrap(),
            ),
        );
        let receipt = receipts.recv().await.unwrap();
        assert_eq!(receipt.session_id, "session-1");
        assert_eq!(receipt.through, 42);
        receipt.reply.send(Ok(())).unwrap();
        assert_eq!(
            response.await.unwrap().unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert!(
            actions.try_recv().is_err(),
            "a read receipt must not queue a controller action"
        );

        action.reply.send(ActionOutcome::Accepted).unwrap();
        assert_eq!(
            prompt.await.unwrap().unwrap().status(),
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn each_rejected_action_keeps_its_own_status_and_guidance() {
        for (outcome, status, guidance) in [
            (
                ActionOutcome::Busy,
                StatusCode::TOO_MANY_REQUESTS,
                "concurrent action limit",
            ),
            (
                ActionOutcome::SessionBusy,
                StatusCode::CONFLICT,
                "another operation is already running",
            ),
            (
                ActionOutcome::NotCancellable,
                StatusCode::CONFLICT,
                "no cancellable operation",
            ),
            (
                ActionOutcome::Failed,
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start this action",
            ),
        ] {
            let (app, mut actions, _, _, _) = app();
            let cookie = login_cookie(&app).await;
            let response = tokio::spawn(
                app.oneshot(
                    Request::post("/api/actions")
                        .header(COOKIE, cookie)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"action":"close","session_id":"session-1"}"#))
                        .unwrap(),
                ),
            );
            let request = actions.recv().await.unwrap();
            request.reply.send(outcome).unwrap();

            let response = response.await.unwrap().unwrap();
            assert_eq!(response.status(), status, "{outcome:?}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let error = body["error"].as_str().unwrap();
            assert!(error.contains(guidance), "{outcome:?} answered {error:?}");
        }
    }

    #[tokio::test]
    async fn the_viewer_shows_a_session_whose_action_failed_after_it_was_accepted() {
        // An accepted action reports its outcome only through snapshots, so
        // the application has to react to `has_error` for a late failure to be
        // visible at all.
        let (app, _, _, _, _) = app();
        let script = fetch_text(app, "/viewer.js").await;
        assert!(script.contains("has_error"), "viewer ignores has_error");
    }

    /// Every response, not only the page, carries the policy. A header that
    /// depends on which handler answered is a header somebody will forget.
    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        for path in [
            "/",
            "/viewer.js",
            "/viewer.css",
            "/manifest.webmanifest",
            "/api/snapshot",
        ] {
            let (app, _, _, _, _) = app();
            let response = app
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let headers = response.headers();
            let policy = headers
                .get(CONTENT_SECURITY_POLICY_HEADER)
                .unwrap_or_else(|| panic!("{path} carries no content-security policy"))
                .to_str()
                .unwrap();
            assert!(
                policy.starts_with("default-src 'none';"),
                "{path} does not refuse unlisted sources: {policy}"
            );
            assert!(
                policy.contains("script-src 'self'") && !policy.contains("unsafe-inline"),
                "{path} permits inline script: {policy}"
            );
            assert!(
                policy.contains("frame-ancestors 'none'"),
                "{path} can be framed: {policy}"
            );
            assert_eq!(
                headers.get(X_CONTENT_TYPE_OPTIONS).unwrap(),
                "nosniff",
                "{path} permits content sniffing"
            );
            assert_eq!(
                headers.get(REFERRER_POLICY).unwrap(),
                "no-referrer",
                "{path} leaks a referrer"
            );
        }
    }

    /// The policy forbids inline script and style, so the page must contain
    /// neither. A page that did would simply fail to run in a browser, which
    /// no Rust test would otherwise notice.
    #[tokio::test]
    async fn the_page_carries_no_inline_script_or_style() {
        let (app, _, _, _, _) = app();
        let page = fetch_text(app, "/").await;
        assert!(
            !page.contains("<script>") && !page.contains("<style>"),
            "the page inlines script or style, which the policy blocks"
        );
        assert!(
            page.contains(r#"src="/viewer.js""#) && page.contains(r#"href="/viewer.css""#),
            "the page does not load its script and style as separate assets"
        );
    }

    /// A cached API answer is a lie about live session state, and a cached
    /// service worker is what keeps a phone on a superseded application.
    #[tokio::test]
    async fn live_state_and_the_service_worker_are_never_stored() {
        for path in ["/", "/service-worker.js", "/api/snapshot"] {
            let (app, _, _, _, _) = app();
            let response = app
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.headers().get(CACHE_CONTROL).unwrap(),
                "no-store",
                "{path} may be stored"
            );
        }
    }

    /// The worker must leave live state alone entirely rather than caching it
    /// and hoping the cache is fresh.
    #[test]
    fn the_service_worker_declines_to_handle_live_state() {
        assert!(
            SERVICE_WORKER.contains("url.pathname.startsWith('/api/')"),
            "the service worker does not exclude the API"
        );
        assert!(
            SERVICE_WORKER.contains("url.pathname.startsWith('/auth/')"),
            "the service worker does not exclude authentication"
        );
        assert!(
            SERVICE_WORKER.contains("caches.delete"),
            "the service worker never deletes a superseded cache"
        );
    }

    /// The vendored assets have to reach the browser, not merely exist in the
    /// repository: the manifest names them and a phone installs from it.
    #[tokio::test]
    async fn the_installable_assets_are_served() {
        for (path, content_type) in [
            ("/icon-192.png", "image/png"),
            ("/icon-512.png", "image/png"),
            ("/maskable-512.png", "image/png"),
            ("/apple-touch-icon.png", "image/png"),
            ("/fonts/jetbrains-mono.woff2", "font/woff2"),
        ] {
            let (app, _, _, _, _) = app();
            let response = app
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path} is not served");
            assert_eq!(
                response.headers().get(CONTENT_TYPE).unwrap(),
                content_type,
                "{path} is served as the wrong type"
            );
        }
    }

    /// Fetch one unauthenticated asset and return it as text. Serving the
    /// application from several files means a check about the application has
    /// to name the file it is about.
    async fn fetch_text(app: Router, path: &str) -> String {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path} is not served");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(body.to_vec()).expect("assets are UTF-8")
    }

    #[tokio::test]
    async fn repeated_wrong_codes_lock_the_login_endpoint() {
        let (app, _, _, _, _) = app();
        let attempt = |code: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::post("/auth/session")
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(format!(r#"{{"code":"{code}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };
        for _ in 0..MAX_CODE_FAILURES {
            assert_eq!(attempt("000000").await, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(attempt("000000").await, StatusCode::TOO_MANY_REQUESTS);
        // Even the right code waits out the lockout, so guessing cannot be
        // hidden behind a correct-looking attempt.
        assert_eq!(attempt("123456").await, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn viewer_code_lockouts_lengthen_instead_of_resetting_after_every_wait() {
        let serve_one_lockout = |guard: &mut CodeGuard, now: Instant| {
            for _ in 0..MAX_CODE_FAILURES {
                assert!(!guard.locked_at(now));
                guard.record_failure_at(now);
            }
            assert!(guard.locked_at(now));
            guard.locked_until.expect("the guard is locked") - now
        };

        let start = Instant::now();
        let mut guard = CodeGuard::default();
        let first = serve_one_lockout(&mut guard, start);
        assert_eq!(first, CODE_LOCKOUT_BASE);

        // Waiting out a lockout buys another run of attempts, not another
        // equally short lockout: a guard that reset here gave an attacker
        // MAX_CODE_FAILURES guesses every CODE_LOCKOUT_BASE for ever.
        let second_round = start + first;
        let second = serve_one_lockout(&mut guard, second_round);
        assert_eq!(second, CODE_LOCKOUT_BASE * 2);
        let third = serve_one_lockout(&mut guard, second_round + second);
        assert_eq!(third, CODE_LOCKOUT_BASE * 4);
        assert_eq!(code_lockout(u32::MAX), CODE_LOCKOUT_CAP);

        // A correct code clears the history, so one mistyped digit tomorrow
        // still costs only the shortest wait.
        let mut recovered = CodeGuard::default();
        assert_eq!(serve_one_lockout(&mut recovered, start), CODE_LOCKOUT_BASE);
    }

    #[test]
    fn persisted_cookie_key_survives_a_restart_and_stays_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("phone-cookie-key");

        let first = load_or_create_cookie_key(&path).unwrap();
        assert!(first.len() >= COOKIE_KEY_BYTES);
        assert_eq!(std::fs::read(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Two server processes started from the same key file honour each
        // other's cookies; a process that kept its generated key would not.
        let mut restarted = detached_options();
        restarted
            .set_cookie_key(load_or_create_cookie_key(&path).unwrap())
            .unwrap();
        let mut original = detached_options();
        original.set_cookie_key(first.clone()).unwrap();
        let cookie = signed_cookie_value(&original.cookie_key, "test-viewer", 200);
        assert!(session_cookie_valid(&restarted.cookie_key, &cookie, 100));
        assert!(!session_cookie_valid(
            &detached_options().cookie_key,
            &cookie,
            100
        ));

        // Deleting the key file is the explicit sign-everyone-out gesture.
        std::fs::remove_file(&path).unwrap();
        let rotated = load_or_create_cookie_key(&path).unwrap();
        assert_ne!(rotated, first);
        assert!(!session_cookie_valid(&rotated, &cookie, 100));
    }

    #[test]
    fn corrupt_cookie_key_is_regenerated_instead_of_blocking_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("phone-cookie-key");
        std::fs::write(&path, b"short").unwrap();

        let key = load_or_create_cookie_key(&path).unwrap();

        assert!(key.len() >= COOKIE_KEY_BYTES);
        assert_eq!(std::fs::read(&path).unwrap(), key);
        assert_eq!(load_or_create_cookie_key(&path).unwrap(), key);
    }
}
