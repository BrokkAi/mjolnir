//! A typed client for the daemon's documented `/api/v1` routes.
//!
//! The CLI subcommands are thin: they resolve the viewer URL and the bearer
//! token, serialize the request structs `mj_controller::hel_server::api`
//! exports, and print what comes back. Keeping the wire shapes in one crate
//! means the CLI and the server cannot disagree about them.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mj_controller::hel_server::api::{
    API_VERSION, API_VERSION_HEADER, ApiSession, ExportRequest, PromptRequest, PromptResponse,
    PushedBranch, SessionListResponse, StartSessionRequest, StartSessionResponse,
    TranscriptResponse, WaitRequest, WaitResponse,
};
use mj_controller::hel_server::api_token_path;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon::{self, WebViewerStatus};

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

/// A client for one daemon's API.
pub(crate) struct ApiClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl ApiClient {
    /// Resolve the daemon's viewer URL and the bearer token, starting the
    /// daemon if it is not running.
    pub(crate) async fn connect() -> Result<Self> {
        let mut client = daemon::connect_or_start().await?;
        let status = client.status().await?;
        let viewer_url = match status.phone_status {
            WebViewerStatus::Ready { viewer_url, .. } => viewer_url,
            WebViewerStatus::Disabled => bail!(
                "the web viewer is disabled, so the API is not served; enable [phone] in config.toml and run `mj daemon restart`"
            ),
            WebViewerStatus::Starting => bail!(
                "the web viewer is still starting; retry in a moment or check `mj daemon status`"
            ),
            WebViewerStatus::Stopped => bail!("the web viewer is stopped; run `mj daemon restart`"),
            WebViewerStatus::Error { message } => {
                bail!("the web viewer failed to start: {message}; run `mj daemon restart`")
            }
        };
        probe_api(&viewer_url).await?;
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
        Self::new(viewer_url, token)
    }

    pub(crate) fn new(base_url: String, token: String) -> Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            http: reqwest::Client::builder()
                .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("build the API HTTP client")?,
        })
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
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .context("reach the Mjolnir API")?;
        check_version(&response)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
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
        match message.is_empty() {
            true => Err(anyhow!("the Mjolnir API answered {status}")),
            false => Err(anyhow!("the Mjolnir API answered {status}: {message}")),
        }
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

    pub(crate) async fn sessions_in_workspace(
        &self,
        workspace_id: Option<String>,
    ) -> Result<SessionListResponse> {
        let response = self
            .send(
                self.http
                    .get(self.url("/sessions"))
                    .query(&mj_controller::hel_server::api::SessionListQuery { workspace_id })
                    .timeout(REQUEST_TIMEOUT),
            )
            .await?;
        decode(response).await
    }

    pub(crate) async fn models(
        &self,
        profile: &str,
        model: Option<String>,
    ) -> Result<hel::hel_worker_launch::ProfileConfig> {
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
        request: &mj_controller::hel_server::api::SetConfigRequest,
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

    pub(crate) async fn session(&self, session_id: &str) -> Result<ApiSession> {
        self.get_json(&format!("/sessions/{session_id}")).await
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
        role: Option<hel::hel_transcript::TranscriptRole>,
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

    pub(crate) async fn usage(
        &self,
        session_id: &str,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<hel::hel_database::UsagePage> {
        self.get_json(&format!(
            "/sessions/{session_id}/usage?after_seq={}&limit={}",
            after_seq.unwrap_or(0),
            limit.unwrap_or(200)
        ))
        .await
    }

    pub(crate) async fn diff(&self, session_id: &str) -> Result<String> {
        let response = self
            .send(
                self.http
                    .get(self.url(&format!("/sessions/{session_id}/diff")))
                    .timeout(EXPORT_TIMEOUT),
            )
            .await?;
        response.text().await.context("read the session diff")
    }

    pub(crate) async fn put_file(
        &self,
        session_id: &str,
        path: &str,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> Result<mj_controller::hel_server::api::WriteFileResponse> {
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
    ) -> Result<Vec<hel::hel_elicitation::ElicitationRequest>> {
        self.get_json(&format!("/sessions/{session_id}/elicitations"))
            .await
    }

    pub(crate) async fn respond_elicitation(
        &self,
        session_id: &str,
        elicitation_id: &str,
        response: &hel::hel_elicitation::ElicitationResponse,
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

    pub(crate) async fn close(&self, session_id: &str) -> Result<()> {
        self.send(
            self.http
                .post(self.url(&format!("/sessions/{session_id}/close")))
                .timeout(REQUEST_TIMEOUT),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn cancel_turn(&self, session_id: &str) -> Result<()> {
        self.send(
            self.http
                .post(self.url(&format!("/sessions/{session_id}/cancel-turn")))
                .timeout(REQUEST_TIMEOUT),
        )
        .await
        .map(|_| ())
    }
}

/// An unauthenticated versioned 401 proves the route exists before a token is read.
async fn probe_api(base_url: &str) -> Result<()> {
    let response = reqwest::Client::new()
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
                get(|| async {
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
    async fn every_call_carries_the_bearer_token_and_reads_the_typed_response() {
        let (url, seen) = serve(Some("1")).await;
        let client = ApiClient::new(url, "secret-token".to_owned()).unwrap();

        let sessions = client.sessions_in_workspace(None).await.unwrap();
        assert_eq!(sessions.sessions[0].id, "session-1");
        let session = client.session("session-2").await.unwrap();
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
        let error = client.diff("session-1").await.unwrap_err();
        assert!(
            format!("{error:#}").contains("no session base recorded"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn api_probe_succeeds_without_reading_a_token() {
        let (url, seen) = serve(Some("1")).await;
        probe_api(&url).await.unwrap();
        assert!(
            seen.lock().unwrap().is_empty(),
            "the support probe must not need authentication"
        );
        let (url, _) = serve(Some("2")).await;
        assert!(
            probe_api(&url)
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
            probe_api(&url)
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
}
