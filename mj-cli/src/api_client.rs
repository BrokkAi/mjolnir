//! A typed client for the daemon's documented `/api/v1` routes.
//!
//! The CLI subcommands are thin: they resolve the viewer URL and the bearer
//! token, serialize the request structs `mj_controller::server::api`
//! exports, and print what comes back. Keeping the wire shapes in one crate
//! means the CLI and the server cannot disagree about them.

pub(crate) mod events;
mod pinned_tls;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mj_controller::server::api::{
    API_VERSION, API_VERSION_HEADER, ApiSession, CreateWorkspaceRequest, CreateWorkspaceResponse,
    ExportRequest, PromptRequest, PromptResponse, PushedBranch, ResumeSessionRequest,
    ResumeSessionResponse, SessionListResponse, StartSessionRequest, StartSessionResponse,
    SuspendSessionResponse, TranscriptResponse, WaitRequest, WaitResponse, WorkspaceListResponse,
};
use mj_controller::server::api_token_path;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon;

/// How long an ordinary request may take. Every route but `wait` and the
/// exports answers from memory or from SQLite, so this only has to outlast a
/// busy daemon.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an export may take. The daemon runs git on the target, which for a
/// large checkout over SSH is slow; its own ceiling is five minutes.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(600);
/// How much longer than the wait itself the HTTP request may take, so a wait
/// that reaches its deadline answers `timeout` rather than failing the client.
const WAIT_SLACK: Duration = Duration::from_secs(30);

/// How a request the daemon refused with 429 (all of its concurrent action
/// slots are taken) is retried. A 429 is answered before the daemon admits
/// the action, so sending it again cannot repeat any work.
#[derive(Debug, Clone, Copy)]
struct BusyRetry {
    first_delay: Duration,
    max_delay: Duration,
    /// How long to keep retrying before reporting the refusal.
    limit: Duration,
}

impl BusyRetry {
    /// Session creation holds a slot until provisioning ends, which over SSH
    /// takes minutes, so the client waits about as long as one provision.
    const DEFAULT: Self = Self {
        first_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(15),
        limit: Duration::from_secs(600),
    };
}

/// A client for one daemon's API.
pub(crate) struct ApiClient {
    base_url: String,
    token: String,
    busy_retry: BusyRetry,
    http: reqwest::Client,
}

impl ApiClient {
    /// Resolve the daemon's viewer URL and the bearer token, starting the
    /// daemon if it is not running.
    pub(crate) async fn connect() -> Result<Self> {
        let mut client = daemon::connect_or_start().await?;
        let viewer_url = daemon::wait_for_web_viewer(&mut client).await?;
        // The viewer may serve a self-signed certificate; the daemon publishes
        // its SHA-256 beside the URL so this client trusts exactly that one.
        let certificate_sha256 = match client.web_access().await? {
            mj_client::web::WebViewerAccess::Ready {
                certificate_sha256, ..
            } => certificate_sha256,
            _ => None,
        };
        let http = http_client(certificate_sha256.as_deref())?;
        probe_api(&http, &viewer_url).await?;
        let token_path = api_token_path();
        let token = std::fs::read_to_string(&token_path)
            .with_context(|| {
                format!(
                    "read the API token {}; this daemon supports the API, but its token is missing or unreadable; check permissions or run `mj daemon restart`",
                    token_path.display()
                )
            })?
            .trim()
            .to_owned();
        if token.is_empty() {
            bail!(
                "the API token file {} is empty; delete it and run `mj daemon restart` to mint a new one",
                token_path.display()
            );
        }
        Ok(Self::with_http(viewer_url, token, http))
    }

    #[cfg(test)]
    pub(crate) fn new(base_url: String, token: String) -> Result<Self> {
        Ok(Self::with_http(base_url, token, http_client(None)?))
    }

    fn with_http(base_url: String, token: String, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            busy_retry: BusyRetry::DEFAULT,
            http,
        }
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn token_path() -> PathBuf {
        api_token_path()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base_url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        self.dispatch(request).await?.map_err(ApiError::into_error)
    }

    /// Send a request whose subject may simply not exist: a 404 is `None`
    /// rather than an error, and every other refusal is reported as
    /// [`ApiClient::send`] reports it.
    async fn send_optional(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Option<reqwest::Response>> {
        match self.dispatch(request).await? {
            Ok(response) => Ok(Some(response)),
            Err(failure) if failure.status == reqwest::StatusCode::NOT_FOUND => Ok(None),
            Err(failure) => Err(failure.into_error()),
        }
    }

    /// One request, separating "the API refused it" from "the API could not be
    /// reached or does not speak this contract". Only the refusal is something
    /// a caller may interpret.
    ///
    /// A 429 means the daemon is already running as many actions as it allows
    /// and did not admit this one. The request is sent again with growing
    /// delays for up to [`BusyRetry::limit`], and each wait is reported on
    /// stderr so the person sees what the command is waiting for.
    async fn dispatch(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<std::result::Result<reqwest::Response, ApiError>> {
        let started = std::time::Instant::now();
        let mut delay = self.busy_retry.first_delay;
        let mut request = request;
        loop {
            let retry = request.try_clone();
            match self.dispatch_once(request).await? {
                Err(failure) if failure.status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    let Some(retry) = retry else {
                        return Ok(Err(failure));
                    };
                    if started.elapsed() + delay > self.busy_retry.limit {
                        return Ok(Err(failure));
                    }
                    eprintln!(
                        "The daemon is busy: {}. Trying again in {}s.",
                        failure.message.trim_end_matches("; retry shortly"),
                        delay.as_secs().max(1),
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(self.busy_retry.max_delay);
                    request = retry;
                }
                outcome => return Ok(outcome),
            }
        }
    }

    async fn dispatch_once(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<std::result::Result<reqwest::Response, ApiError>> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .context("reach the Mjolnir API")?;
        check_version(&response)?;
        let status = response.status();
        if status.is_success() {
            return Ok(Ok(response));
        }
        let body = response.bytes().await.unwrap_or_default();
        let message = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|body| {
                body.get("error")
                    .and_then(|error| error.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&body).trim().to_owned());
        Ok(Err(ApiError { status, message }))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .send(self.http.get(self.url(path)).timeout(REQUEST_TIMEOUT))
            .await?;
        decode(response).await
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T> {
        let response = self
            .send(self.http.post(self.url(path)).timeout(timeout).json(body))
            .await?;
        decode(response).await
    }

    pub(crate) async fn workspaces(&self) -> Result<WorkspaceListResponse> {
        self.get_json("/workspaces").await
    }

    /// Create the named workspace, or get the one that already carries the
    /// name. The route is idempotent, so a script may call it every run.
    pub(crate) async fn create_workspace(&self, name: String) -> Result<CreateWorkspaceResponse> {
        self.post_json(
            "/workspaces",
            &CreateWorkspaceRequest { name },
            REQUEST_TIMEOUT,
        )
        .await
    }

    pub(crate) async fn sessions_in_workspace(
        &self,
        workspace_id: Option<String>,
    ) -> Result<SessionListResponse> {
        let response = self
            .send(
                self.http
                    .get(self.url("/sessions"))
                    .query(&mj_controller::server::api::SessionListQuery { workspace_id })
                    .timeout(REQUEST_TIMEOUT),
            )
            .await?;
        decode(response).await
    }

    pub(crate) async fn models(
        &self,
        profile: &str,
        model: Option<String>,
    ) -> Result<mj_core::worker_launch::ProfileConfig> {
        let mut request = self
            .http
            .get(self.url(&format!("/profiles/{profile}/config")))
            .timeout(Duration::from_secs(330));
        if let Some(model) = model {
            request = request.query(&[("model", model)]);
        }
        decode(self.send(request).await?).await
    }

    pub(crate) async fn set_config(
        &self,
        session: &str,
        request: &mj_controller::server::api::SetConfigRequest,
    ) -> Result<ApiSession> {
        decode(
            self.send(
                self.http
                    .patch(self.url(&format!("/sessions/{session}/config")))
                    .json(request)
                    .timeout(REQUEST_TIMEOUT),
            )
            .await?,
        )
        .await
    }

    /// One session, or `None` when the daemon knows no session with that id.
    /// An id that names nothing here may still name a SessionWiki row.
    pub(crate) async fn session_if_known(&self, session_id: &str) -> Result<Option<ApiSession>> {
        let request = self
            .http
            .get(self.url(&format!("/sessions/{session_id}")))
            .timeout(REQUEST_TIMEOUT);
        match self.send_optional(request).await? {
            Some(response) => decode(response).await.map(Some),
            None => Ok(None),
        }
    }

    /// What the SessionWiki index knows about one row, or `None` when the
    /// index holds no session with that id.
    pub(crate) async fn wiki_session(
        &self,
        wiki_id: &str,
    ) -> Result<Option<mj_client::daemon::WikiSessionInfo>> {
        let request = self
            .http
            .get(self.url(&format!("/wiki/sessions/{wiki_id}")))
            .timeout(REQUEST_TIMEOUT);
        match self.send_optional(request).await? {
            Some(response) => decode(response).await.map(Some),
            None => Ok(None),
        }
    }

    /// Start a new session carrying a hand-off compacted from an indexed one.
    pub(crate) async fn wiki_restore(
        &self,
        wiki_id: &str,
        request: &mj_controller::server::api::WikiRestoreBody,
    ) -> Result<StartSessionResponse> {
        self.post_json(
            &format!("/wiki/sessions/{wiki_id}/restore"),
            request,
            Duration::from_secs(660),
        )
        .await
    }

    pub(crate) async fn start(
        &self,
        request: &StartSessionRequest,
    ) -> Result<StartSessionResponse> {
        self.post_json("/sessions", request, Duration::from_secs(660))
            .await
    }

    pub(crate) async fn prompt(&self, session_id: &str, text: String) -> Result<PromptResponse> {
        self.post_json(
            &format!("/sessions/{session_id}/prompt"),
            &PromptRequest { text },
            REQUEST_TIMEOUT,
        )
        .await
    }

    /// Block until a turn ends. The HTTP timeout outlasts the wait itself, so
    /// a deadline is reported by the server as a `timeout` outcome rather than
    /// by the client as a dropped request.
    pub(crate) async fn wait(
        &self,
        session_id: &str,
        request: &WaitRequest,
    ) -> Result<WaitResponse> {
        let timeout = Duration::from_secs(request.timeout_secs.unwrap_or(600)) + WAIT_SLACK;
        self.post_json(&format!("/sessions/{session_id}/wait"), request, timeout)
            .await
    }

    pub(crate) async fn transcript(
        &self,
        session_id: &str,
        after_seq: Option<u64>,
        limit: Option<usize>,
        role: Option<mj_core::transcript::TranscriptRole>,
    ) -> Result<TranscriptResponse> {
        let mut query = Vec::new();
        if let Some(role) = role {
            query.push(format!("role={}", role.as_str()));
        }
        if let Some(after_seq) = after_seq {
            query.push(format!("after_seq={after_seq}"));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let path = match query.is_empty() {
            true => format!("/sessions/{session_id}/transcript"),
            false => format!("/sessions/{session_id}/transcript?{}", query.join("&")),
        };
        self.get_json(&path).await
    }

    pub(crate) async fn events(
        &self,
        filter: &mj_controller::database::ApiEventFilter,
        after_seq: Option<u64>,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .get(self.url("/events"))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .query(filter);
        if let Some(after_seq) = after_seq {
            request = request.query(&[("after_seq", after_seq)]);
        }
        let response = self.send(request).await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.starts_with("text/event-stream") {
            bail!("API events endpoint did not return an event stream");
        }
        Ok(response)
    }

    pub(crate) async fn usage(
        &self,
        session_id: &str,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<mj_controller::database::UsagePage> {
        self.get_json(&format!(
            "/sessions/{session_id}/usage?after_seq={}&limit={}",
            after_seq.unwrap_or(0),
            limit.unwrap_or(200)
        ))
        .await
    }

    pub(crate) async fn diff(
        &self,
        session_id: &str,
        base: Option<&str>,
        json: bool,
    ) -> Result<String> {
        let mut request = self
            .http
            .get(self.url(&format!("/sessions/{session_id}/diff")));
        if let Some(base) = base {
            request = request.query(&[("base", base)]);
        }
        if json {
            request = request.query(&[("json", "true")]);
        }
        let response = self.send(request.timeout(EXPORT_TIMEOUT)).await?;
        response.text().await.context("read the session diff")
    }

    pub(crate) async fn put_file(
        &self,
        session_id: &str,
        path: &str,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> Result<mj_controller::server::api::WriteFileResponse> {
        self.send(
            self.http
                .put(self.url(&format!("/sessions/{session_id}/files")))
                .query(&[
                    ("path", path),
                    ("overwrite", if overwrite { "true" } else { "false" }),
                ])
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(bytes)
                .timeout(EXPORT_TIMEOUT),
        )
        .await?
        .json()
        .await
        .context("read file injection result")
    }

    pub(crate) async fn elicitations(
        &self,
        session_id: &str,
    ) -> Result<Vec<mj_core::elicitation::ElicitationRequest>> {
        self.get_json(&format!("/sessions/{session_id}/elicitations"))
            .await
    }

    pub(crate) async fn respond_elicitation(
        &self,
        session_id: &str,
        elicitation_id: &str,
        response: &mj_core::elicitation::ElicitationResponse,
    ) -> Result<()> {
        self.send(
            self.http
                .post(self.url(&format!(
                    "/sessions/{session_id}/elicitations/{elicitation_id}"
                )))
                .json(response)
                .timeout(REQUEST_TIMEOUT),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn read_file(&self, session_id: &str, path: &str) -> Result<Vec<u8>> {
        let response = self
            .send(
                self.http
                    .get(self.url(&format!("/sessions/{session_id}/files")))
                    .query(&[("path", path)])
                    .timeout(EXPORT_TIMEOUT),
            )
            .await?;
        Ok(response
            .bytes()
            .await
            .context("read the session file")?
            .to_vec())
    }

    /// Export the session's work. A branch push answers JSON; a patch and a
    /// bundle answer bytes, so the caller gets both and uses the one its kind
    /// produced.
    pub(crate) async fn export(
        &self,
        session_id: &str,
        request: &ExportRequest,
    ) -> Result<ExportResult> {
        let response = self
            .send(
                self.http
                    .post(self.url(&format!("/sessions/{session_id}/export")))
                    .timeout(EXPORT_TIMEOUT)
                    .json(request),
            )
            .await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await.context("read the export")?.to_vec();
        if content_type.starts_with("application/json") {
            let pushed =
                serde_json::from_slice::<PushedBranch>(&bytes).context("read the pushed branch")?;
            return Ok(ExportResult::Branch(pushed));
        }
        Ok(ExportResult::Bytes(bytes))
    }

    /// Ask the daemon to suspend a session. The answer says how many
    /// sub-agents the suspend stops, and warns when some of them have not
    /// handed back. An older daemon answers with no body, which reads as a
    /// suspend that stops none.
    pub(crate) async fn suspend(
        &self,
        session_id: &str,
        acknowledge_unpublished_work: bool,
    ) -> Result<SuspendSessionResponse> {
        let response = self
            .send(
                self.http
                    .post(self.url(&format!("/sessions/{session_id}/suspend")))
                    .json(&serde_json::json!({
                        // An older daemon refuses to suspend a parent with
                        // active sub-agents without this; a current one
                        // ignores it.
                        "acknowledge_active_subagents": true,
                        "acknowledge_unpublished_work": acknowledge_unpublished_work,
                    }))
                    .timeout(REQUEST_TIMEOUT),
            )
            .await?;
        let body = response.bytes().await.context("read the suspend answer")?;
        if body.is_empty() {
            return Ok(SuspendSessionResponse {
                session_id: session_id.to_owned(),
                ..SuspendSessionResponse::default()
            });
        }
        serde_json::from_slice(&body).context("read the suspend answer")
    }

    pub(crate) async fn destroy(&self, session_id: &str, delete_branch: bool) -> Result<()> {
        self.send(
            self.http
                .post(self.url(&format!("/sessions/{session_id}/destroy")))
                .json(&serde_json::json!({"delete_branch": delete_branch}))
                .timeout(REQUEST_TIMEOUT),
        )
        .await
        .map(|_| ())
    }

    /// Ask the daemon to resume a stopped session. It answers as soon as the
    /// resume is admitted, so this does not wait for the session to come up.
    pub(crate) async fn resume(
        &self,
        session_id: &str,
        request: &ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse> {
        self.post_json(
            &format!("/sessions/{session_id}/resume"),
            request,
            REQUEST_TIMEOUT,
        )
        .await
    }

    pub(crate) async fn interrupt_turn(&self, session_id: &str) -> Result<()> {
        self.send(
            self.http
                .post(self.url(&format!("/sessions/{session_id}/interrupt-turn")))
                .timeout(REQUEST_TIMEOUT),
        )
        .await
        .map(|_| ())
    }
}

/// The HTTP client for the daemon API. With a pin it trusts only the
/// certificate the daemon published; without one (plain HTTP, or a publicly
/// trusted Tailscale certificate) it verifies as any HTTPS client does.
fn http_client(certificate_sha256: Option<&str>) -> Result<reqwest::Client> {
    let mut builder =
        reqwest::Client::builder().user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")));
    if let Some(pin) = certificate_sha256 {
        builder = builder.tls_backend_preconfigured(pinned_tls::pinned_client_config(pin)?);
    }
    builder.build().context("build the API HTTP client")
}

/// An unauthenticated versioned 401 proves the route exists before a token is read.
async fn probe_api(http: &reqwest::Client, base_url: &str) -> Result<()> {
    let response = http
        .get(format!(
            "{}/api/v1/sessions",
            base_url.trim_end_matches('/')
        ))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("reach the daemon API; check `mj daemon status`")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND
        && !response.headers().contains_key(API_VERSION_HEADER)
    {
        bail!("this daemon predates the Mjolnir API; run `mj daemon restart`");
    }
    check_version(&response)?;
    if response.status().is_success() || response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(());
    }
    bail!(
        "the daemon API is unavailable ({}); check `mj daemon status`",
        response.status()
    )
}

/// A request the API refused, kept apart from a transport failure so a caller
/// can tell "there is no such thing" from "the call did not work".
struct ApiError {
    status: reqwest::StatusCode,
    message: String,
}

impl ApiError {
    fn into_error(self) -> anyhow::Error {
        match self.message.is_empty() {
            true => anyhow!("the Mjolnir API answered {}", self.status),
            false => anyhow!("the Mjolnir API answered {}: {}", self.status, self.message),
        }
    }
}

/// What an export answered with.
pub(crate) enum ExportResult {
    Branch(PushedBranch),
    Bytes(Vec<u8>),
}

/// Refuse a response that does not name this contract, or names another major
/// version of it. A client that parsed such a body would be guessing.
fn check_version(response: &reqwest::Response) -> Result<()> {
    let version = response
        .headers()
        .get(API_VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            anyhow!(
                "the server at this URL did not answer with {API_VERSION_HEADER}; it is not a Mjolnir API"
            )
        })?;
    let major = version.split('.').next().unwrap_or_default();
    if major != API_VERSION {
        bail!(
            "this daemon speaks Mjolnir API version {version}; this `mj` speaks {API_VERSION}. Upgrade whichever is older."
        );
    }
    Ok(())
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let bytes = response.bytes().await.context("read the API response")?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "read the API response body: {}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(200)
                .collect::<String>()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path as AxumPath, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};

    type Seen = Arc<Mutex<Vec<String>>>;

    fn sample_session(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "workspace_id": "workspace-1",
            "title": "a session",
            "harness_kind": "claude",
            "profile_id": "profile-1",
            "target_id": "target-1",
            "bundle_id": "bundle-1",
            "state": "Running",
            "lifecycle": "live",
            "chat_phase": "idle",
            "is_idle": true,
            "has_error": false,
            "created_at": "2026-09-11T00:00:00Z",
            "updated_at": "2026-09-11T00:00:01Z",
        })
    }

    /// A canned server standing in for the daemon: it records what the client
    /// sent and stamps whichever contract version the test is about.
    async fn serve(version_header: Option<&'static str>) -> (String, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/api/v1/sessions",
                get(|State(seen): State<Seen>, headers: HeaderMap| async move {
                    record_authorization(&seen, &headers);
                    Json(serde_json::json!({ "sessions": [sample_session("session-1")] }))
                }),
            )
            .route(
                "/api/v1/sessions/{session_id}",
                get(
                    |State(seen): State<Seen>,
                     AxumPath(session_id): AxumPath<String>,
                     headers: HeaderMap| async move {
                        record_authorization(&seen, &headers);
                        Json(sample_session(&session_id))
                    },
                ),
            )
            .route(
                "/api/v1/sessions/{session_id}/files",
                get(
                    |State(seen): State<Seen>,
                     Query(query): Query<std::collections::BTreeMap<String, String>>,
                     headers: HeaderMap| async move {
                        record_authorization(&seen, &headers);
                        seen.lock().unwrap().push(format!("path={}", query["path"]));
                        b"file bytes".to_vec()
                    },
                ),
            )
            .route(
                "/api/v1/sessions/{session_id}/diff",
                get(|State(seen): State<Seen>, Query(query): Query<std::collections::BTreeMap<String, String>>, headers: HeaderMap| async move {
                    record_authorization(&seen, &headers);
                    if query.get("json").is_some_and(|value| value == "true") {
                        seen.lock().unwrap().push(format!("base={}", query["base"]));
                        return (StatusCode::OK, Json(serde_json::json!({
                            "diff": "+task work\n", "base": "a".repeat(40), "head": "c".repeat(40),
                            "head_descends_from_base": false
                        })));
                    }
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({ "error": "no session base recorded" })),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    let mut response = next.run(request).await;
                    if let Some(version) = version_header {
                        response
                            .headers_mut()
                            .insert(API_VERSION_HEADER, version.parse().unwrap());
                    }
                    response
                },
            ))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), seen)
    }

    fn record_authorization(seen: &Seen, headers: &HeaderMap) {
        if let Some(authorization) = headers.get(axum::http::header::AUTHORIZATION) {
            seen.lock()
                .unwrap()
                .push(authorization.to_str().unwrap_or_default().to_owned());
        }
    }

    #[tokio::test]
    async fn diff_metadata_requests_encode_the_revision_and_preserve_resolved_commits() {
        let (url, seen) = serve(Some("1")).await;
        let client = ApiClient::new(url, "secret-token".into()).unwrap();
        let body = client
            .diff("session-1", Some("HEAD@{1}"), true)
            .await
            .unwrap();
        let diff: mj_checkpoint::archive::SessionDiff = serde_json::from_str(&body).unwrap();
        assert_eq!(diff.diff, "+task work\n");
        assert_eq!(diff.base, "a".repeat(40));
        assert_eq!(diff.head, "c".repeat(40));
        assert_eq!(diff.head_descends_from_base, Some(false));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["Bearer secret-token", "base=HEAD@{1}"]
        );

        // A worker from before the field answers without it, and the absence
        // reads as "unknown" rather than failing the whole decode.
        let older: mj_checkpoint::archive::SessionDiff =
            serde_json::from_str(r#"{"diff":"+task work\n","base":"aaaa","head":"cccc"}"#).unwrap();
        assert_eq!(older.head_descends_from_base, None);
    }

    #[tokio::test]
    async fn every_call_carries_the_bearer_token_and_reads_the_typed_response() {
        let (url, seen) = serve(Some("1")).await;
        let client = ApiClient::new(url, "secret-token".to_owned()).unwrap();

        let sessions = client.sessions_in_workspace(None).await.unwrap();
        assert_eq!(sessions.sessions[0].id, "session-1");
        let session = client
            .session_if_known("session-2")
            .await
            .unwrap()
            .expect("the route answers with the session");
        assert_eq!(session.id, "session-2");
        assert_eq!(
            client
                .read_file("session-1", "docs/README.md")
                .await
                .unwrap(),
            b"file bytes"
        );

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.iter()
                .filter(|entry| *entry == "Bearer secret-token")
                .count(),
            3,
            "every call authenticates: {seen:?}"
        );
        assert!(
            seen.contains(&"path=docs/README.md".to_owned()),
            "the file path reaches the server as a query: {seen:?}"
        );

        // A refusal reaches the caller as the reason the API gave it.
        let error = client.diff("session-1", None, false).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("no session base recorded"),
            "unexpected error: {error:#}"
        );
    }

    /// A 429 is sent before the daemon admits anything, so the client sends
    /// the request again until it is admitted, and gives up with the
    /// daemon's own reason once its time runs out.
    #[tokio::test]
    async fn a_busy_daemon_is_retried_until_it_admits_the_request() {
        let refusals = Arc::new(Mutex::new(2_usize));
        let app = Router::new()
            .route(
                "/api/v1/sessions/{session_id}/prompt",
                axum::routing::post(|State(refusals): State<Arc<Mutex<usize>>>| async move {
                    let mut left = refusals.lock().unwrap();
                    if *left > 0 {
                        *left -= 1;
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(serde_json::json!({ "error": "at its concurrent action limit" })),
                        );
                    }
                    (
                        StatusCode::ACCEPTED,
                        Json(serde_json::json!({ "turn_id": 7 })),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(
                |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    let mut response = next.run(request).await;
                    response
                        .headers_mut()
                        .insert(API_VERSION_HEADER, "1".parse().unwrap());
                    response
                },
            ))
            .with_state(refusals.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut client = ApiClient::new(url, "secret-token".into()).unwrap();
        client.busy_retry = BusyRetry {
            first_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            limit: Duration::from_secs(5),
        };

        let response = client.prompt("session-1", "hello".into()).await.unwrap();
        assert_eq!(response.turn_id, 7);
        assert_eq!(*refusals.lock().unwrap(), 0);

        *refusals.lock().unwrap() = usize::MAX;
        client.busy_retry.limit = Duration::from_millis(50);
        let error = client
            .prompt("session-1", "hello".into())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("concurrent action limit"),
            "unexpected error: {error:#}"
        );
    }

    /// A suspend answers with how many sub-agents it stops and a warning for
    /// the ones still at work. An older daemon answers with no body, which
    /// reads as no warning; it still gets the acknowledgement it requires.
    #[tokio::test]
    async fn a_suspend_reads_the_sub_agent_warning_and_an_empty_answer_as_none() {
        let answers = Arc::new(Mutex::new(vec![
            String::new(),
            serde_json::json!({
                "session_id": "session-1",
                "stopped_subagents": 2,
                "subagents_not_handed_back": 1,
                "warning": "1 sub-agent has not handed back; suspending stops it",
            })
            .to_string(),
        ]));
        let bodies: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/api/v1/sessions/{session_id}/suspend",
                axum::routing::post(
                    |State((answers, bodies)): State<(Arc<Mutex<Vec<String>>>, Seen)>,
                     body: String| async move {
                        bodies.lock().unwrap().push(body);
                        (StatusCode::ACCEPTED, answers.lock().unwrap().pop().unwrap())
                    },
                ),
            )
            .layer(axum::middleware::from_fn(
                |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    let mut response = next.run(request).await;
                    response
                        .headers_mut()
                        .insert(API_VERSION_HEADER, "1".parse().unwrap());
                    response
                },
            ))
            .with_state((answers, bodies.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = ApiClient::new(url, "secret-token".into()).unwrap();

        let warned = client.suspend("session-1", false).await.unwrap();
        assert_eq!(warned.stopped_subagents, 2);
        assert_eq!(warned.subagents_not_handed_back, 1);
        assert_eq!(
            warned.warning.as_deref(),
            Some("1 sub-agent has not handed back; suspending stops it")
        );
        let older = client.suspend("session-1", true).await.unwrap();
        assert_eq!(older.session_id, "session-1");
        assert_eq!(older.warning, None);
        for body in bodies.lock().unwrap().iter() {
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["acknowledge_active_subagents"], true, "{body}");
        }
    }

    #[tokio::test]
    async fn api_probe_succeeds_without_reading_a_token() {
        let (url, seen) = serve(Some("1")).await;
        probe_api(&http_client(None).unwrap(), &url).await.unwrap();
        assert!(
            seen.lock().unwrap().is_empty(),
            "the support probe must not need authentication"
        );
        let (url, _) = serve(Some("2")).await;
        assert!(
            probe_api(&http_client(None).unwrap(), &url)
                .await
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new()).await.unwrap();
        });
        assert!(
            probe_api(&http_client(None).unwrap(), &url)
                .await
                .unwrap_err()
                .to_string()
                .contains("mj daemon restart")
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_response_without_this_contract_version_is_refused() {
        let (url, _seen) = serve(None).await;
        let error = ApiClient::new(url, "secret-token".to_owned())
            .unwrap()
            .sessions_in_workspace(None)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(API_VERSION_HEADER),
            "unexpected error: {error:#}"
        );

        let (url, _seen) = serve(Some("2")).await;
        let error = ApiClient::new(url, "secret-token".to_owned())
            .unwrap()
            .sessions_in_workspace(None)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("version 2"),
            "unexpected error: {error:#}"
        );
    }

    /// Serve the API's unauthenticated probe over HTTPS with a generated
    /// self-signed certificate; returns the URL and the certificate's pin.
    async fn serve_https(certificate_authority: bool) -> (String, String) {
        mj_controller::server::install_rustls_crypto_provider();
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
        if certificate_authority {
            // What `openssl req -x509` makes by default, as the lab does.
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        }
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let pin =
            mj_controller::server::api::served_certificate_sha256(certificate.pem().as_bytes())
                .unwrap();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            certificate.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
        )
        .await
        .unwrap();
        let app = Router::new().route(
            "/api/v1/sessions",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    [(API_VERSION_HEADER, API_VERSION)],
                )
            }),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .serve(app.into_make_service())
                .await
                .unwrap();
        });
        (url, pin)
    }

    #[tokio::test]
    async fn the_cli_reaches_a_viewer_serving_a_self_signed_ca_certificate_by_its_pin() {
        let (url, pin) = serve_https(true).await;
        let error = probe_api(&http_client(None).unwrap(), &url)
            .await
            .unwrap_err();
        // The platform verifier words the rejection per OS (webpki reports
        // `CaUsedAsEndEntity`, macOS reports an untrusted certificate).
        assert!(
            format!("{error:#}").contains("invalid peer certificate"),
            "unexpected error: {error:#}"
        );
        probe_api(&http_client(Some(&pin)).unwrap(), &url)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_cli_reaches_a_viewer_serving_a_self_signed_leaf_certificate_by_its_pin() {
        let (url, pin) = serve_https(false).await;
        probe_api(&http_client(Some(&pin)).unwrap(), &url)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_cli_refuses_a_certificate_other_than_the_pinned_one() {
        let (url, _pin) = serve_https(true).await;
        let other = "0".repeat(64);
        assert!(
            probe_api(&http_client(Some(&other)).unwrap(), &url)
                .await
                .is_err()
        );
    }
}
