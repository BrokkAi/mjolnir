//! Typed access to the daemon's direct HTTP API.
use crate::daemon::{
    DaemonClient, DaemonMetadata, DaemonRefusal, DaemonTransportLost, connect_existing,
    connector::DaemonConnector, ssh::SshLease,
};
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Method, Request, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

mod projects;
mod sessions;
mod terminal;
pub use terminal::{ContainerSettingsRequest, ContainerSettingsResponse};
mod workspaces;

pub use workspaces::{
    DiscardDraftsRequest, ReadReceiptRequest, RecoveredDraft, TerminalDraftRequest,
};

pub use sessions::{
    SessionWorkingContext, StopBackgroundTaskRequest, WikiRestoreBody, WikiRestoreResponse,
};

pub(crate) const MAX_PROOF_RESPONSE_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
pub(crate) struct HttpProof {
    pub(crate) mac: String,
}

pub fn is_valid_proof_nonce(nonce: &str) -> bool {
    (32..=128).contains(&nonce.len()) && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn proof_nonce() -> Result<String> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate HTTP proof nonce: {error}"))?;
    Ok(mj_core::hex::lower_hex(random))
}

fn encode_segment(segment: &str) -> Result<String> {
    // The URL parser resolves dot segments even when percent-encoded.
    ensure!(
        !matches!(segment, "" | "." | ".."),
        "invalid API path segment"
    );
    let encoded: String = url::form_urlencoded::byte_serialize(segment.as_bytes()).collect();
    Ok(encoded.replace('+', "%20"))
}

fn proof_mac(token: &str, nonce: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(b"mjolnir-http-api-identity-v1\0");
    mac.update(nonce.as_bytes());
    mac
}

pub fn http_api_proof_hex(token: &str, nonce: &str) -> String {
    mj_core::hex::lower_hex(proof_mac(token, nonce).finalize().into_bytes())
}

pub fn verify_http_api_proof(token: &str, nonce: &str, mac_hex: &str) -> bool {
    if mac_hex.len() != 64 || !mac_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&mac_hex[index * 2..index * 2 + 2], 16)
            .expect("checked ASCII hexadecimal");
    }
    proof_mac(token, nonce).verify_slice(&bytes).is_ok()
}

pub struct HttpDaemonClient {
    endpoint: RwLock<EndpointState>,
    connector: Option<Arc<DaemonConnector>>,
    allow_reconnect: bool,
    http: reqwest::Client,
    #[cfg(test)]
    test_reconnect: Option<TestReconnect>,
}

#[cfg(test)]
type TestReconnect = Box<
    dyn Fn() -> crate::session::BoxFuture<'static, Result<Option<EndpointState>>> + Send + Sync,
>;

#[derive(Clone)]
struct EndpointState {
    base_url: String,
    token: String,
    instance_id: Option<String>,
    shares_client_filesystem: bool,
    previous_metadata: Option<DaemonMetadata>,
    ssh_lease: Option<Arc<SshLease>>,
    generation: u64,
}

impl EndpointState {
    fn new(base_url: String, token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            instance_id: None,
            shares_client_filesystem: false,
            previous_metadata: None,
            ssh_lease: None,
            generation: 0,
        }
    }

    async fn resolve(client: &mut DaemonClient) -> Result<Option<Self>> {
        let identity = client.identity().await?;
        let Some((base_url, token)) = client.direct_api_endpoint(&identity)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            instance_id: Some(identity.instance_id.clone()),
            shares_client_filesystem: client.shares_client_filesystem(&identity),
            previous_metadata: Some(client.metadata().clone()),
            ssh_lease: client.ssh_lease(),
            ..Self::new(base_url, token)
        }))
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn authorize(&self, request: &mut Request) -> Result<()> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.token))?;
        authorization.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, authorization);
        Ok(())
    }
}

const MISSING_ENDPOINT: &str = "The daemon did not publish a loopback HTTP API endpoint. If it predates the loopback HTTP API, update and restart the daemon.";

#[derive(Debug)]
pub struct HttpApiError {
    pub status: StatusCode,
    pub code: Option<String>,
    pub message: String,
    handoff_header: bool,
}

impl std::fmt::Display for HttpApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "HTTP {}: {}", self.status, self.message)
    }
}

impl std::error::Error for HttpApiError {}

impl HttpApiError {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn is_handoff(&self) -> bool {
        self.status == StatusCode::SERVICE_UNAVAILABLE
            && (self.handoff_header || self.code.as_deref() == Some("daemon_handoff"))
    }

    async fn from_response(response: Response) -> reqwest::Result<Self> {
        #[derive(Deserialize)]
        struct ErrorBody {
            error: String,
            code: Option<String>,
        }

        let status = response.status();
        let headers = response.headers();
        let handoff_header = headers.contains_key("mj-handoff")
            || headers
                .get("x-mj-upgrade")
                .is_some_and(|value| value == "pending");
        let body = response.text().await?;
        let (message, code) = match serde_json::from_str::<ErrorBody>(&body) {
            Ok(error) => (error.error, error.code),
            Err(_) if body.trim().is_empty() => (status.to_string(), None),
            Err(_) => (body, None),
        };
        Ok(Self {
            status,
            code,
            message,
            handoff_header,
        })
    }
}

impl HttpDaemonClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Self::with_endpoint(
            EndpointState::new(base_url.into(), token.into()),
            None,
            false,
        )
    }

    fn with_endpoint(
        endpoint: EndpointState,
        connector: Option<Arc<DaemonConnector>>,
        allow_reconnect: bool,
    ) -> Result<Self> {
        // The workspace's reqwest build leaves provider selection to its caller.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            endpoint: RwLock::new(endpoint),
            connector,
            allow_reconnect,
            http,
            #[cfg(test)]
            test_reconnect: None,
        })
    }

    pub async fn from_daemon_client(client: &mut DaemonClient) -> Result<Self> {
        let endpoint = EndpointState::resolve(client)
            .await?
            .context(MISSING_ENDPOINT)?;
        Self::with_endpoint(endpoint, client.connector().cloned(), true)
    }

    pub async fn instance_id(&self) -> Option<String> {
        self.endpoint.read().await.instance_id.clone()
    }

    pub async fn shares_client_filesystem(&self) -> bool {
        self.endpoint.read().await.shares_client_filesystem
    }

    async fn refresh_daemon_endpoint(&self, stale: &EndpointState) -> Result<EndpointState> {
        if let Some(connector) = &self.connector {
            let previous = stale
                .previous_metadata
                .as_ref()
                .context("missing daemon metadata for HTTP reconnect")?;
            let mut client = connector.refresh(previous).await?;
            return EndpointState::resolve(&mut client)
                .await?
                .context(MISSING_ENDPOINT);
        }
        loop {
            // Accept whichever endpoint is currently published, including the
            // original daemon if an abandoned handoff reopens admission after
            // its drain lapses. While it is still draining, the outer retry
            // loop receives another explicit handoff refusal and backs off.
            if let Some(endpoint) = self.local_endpoint().await? {
                return Ok(endpoint);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn local_endpoint(&self) -> Result<Option<EndpointState>> {
        #[cfg(test)]
        if let Some(reconnect) = &self.test_reconnect {
            return reconnect().await;
        }
        let Ok(mut client) = connect_existing().await else {
            return Ok(None);
        };
        match EndpointState::resolve(&mut client).await {
            Err(error)
                if error.is::<DaemonTransportLost>()
                    || error
                        .downcast_ref::<DaemonRefusal>()
                        .is_some_and(|refusal| {
                            refusal.0.starts_with("daemon is shutting down;")
                        }) =>
            {
                Ok(None)
            }
            result => result,
        }
    }

    async fn refresh_endpoint(&self, stale: &EndpointState) -> Result<EndpointState> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut endpoint = self.endpoint.write().await;
        if endpoint.generation == stale.generation {
            let mut replacement = self.refresh_daemon_endpoint(stale).await?;
            replacement.generation = endpoint.generation + 1;
            *endpoint = replacement;
        }
        Ok(endpoint.clone())
    }

    async fn send_once(&self, request: Request) -> Result<Response> {
        let response = self.http.execute(request).await?;
        if !response.status().is_success() {
            return Err(HttpApiError::from_response(response).await?.into());
        }
        Ok(response)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        configure: impl FnOnce(RequestBuilder) -> RequestBuilder,
        reconnect: bool,
    ) -> Result<(Response, Option<Arc<SshLease>>)> {
        let mut endpoint = self.endpoint.read().await.clone();
        let mut request = configure(self.http.request(method, endpoint.url(path))).build()?;
        endpoint.authorize(&mut request)?;
        if !reconnect || !self.allow_reconnect {
            return Ok((self.send_once(request).await?, endpoint.ssh_lease));
        }
        loop {
            let attempt = request
                .try_clone()
                .context("HTTP request body cannot be replayed")?;
            match self.send_once(attempt).await {
                Err(error)
                    if error
                        .downcast_ref::<HttpApiError>()
                        .is_some_and(HttpApiError::is_handoff) =>
                {
                    // Only explicit non-admission permits replay. Keep the
                    // serialized body and query from the original attempt.
                    endpoint = self.refresh_endpoint(&endpoint).await?;
                    let mut url = url::Url::parse(&endpoint.url(path))?;
                    url.set_query(request.url().query());
                    *request.url_mut() = url;
                    endpoint.authorize(&mut request)?;
                }
                result => return Ok((result?, endpoint.ssh_lease)),
            }
        }
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        configure: impl FnOnce(RequestBuilder) -> RequestBuilder,
        reconnect: bool,
    ) -> Result<T> {
        // Keep the exact forward that delivered the response while reading its
        // body. Other calls may have already refreshed the client endpoint.
        let (response, _lease) = self.send(method, path, configure, reconnect).await?;
        Ok(response.json().await?)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.json(Method::GET, path, |request| request, true).await
    }

    pub async fn get_json_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        self.json(Method::GET, path, |request| request.query(query), true)
            .await
    }

    async fn get_json_query_once<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        self.json(Method::GET, path, |request| request.query(query), false)
            .await
    }

    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        self.json(Method::POST, path, |request| request.json(body), true)
            .await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.json(Method::POST, path, |request| request, true).await
    }

    async fn post_empty_once<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.json(Method::POST, path, |request| request, false)
            .await
    }

    pub async fn post_empty_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        self.json(Method::POST, path, |request| request.query(query), true)
            .await
    }

    pub async fn put_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        self.json(Method::PUT, path, |request| request.json(body), true)
            .await
    }

    pub async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.json(Method::DELETE, path, |request| request, true)
            .await
    }

    pub async fn post_json_no_content(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<()> {
        self.send(Method::POST, path, |request| request.json(body), true)
            .await?;
        Ok(())
    }

    pub async fn post_empty_no_content(&self, path: &str) -> Result<()> {
        self.send(Method::POST, path, |request| request, true)
            .await?;
        Ok(())
    }

    pub async fn patch_json_no_content(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<()> {
        self.send(Method::PATCH, path, |request| request.json(body), true)
            .await?;
        Ok(())
    }

    pub async fn put_json_no_content(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<()> {
        self.send(Method::PUT, path, |request| request.json(body), true)
            .await?;
        Ok(())
    }

    pub async fn delete_no_content(&self, path: &str) -> Result<()> {
        self.send(Method::DELETE, path, |request| request, true)
            .await?;
        Ok(())
    }

    pub async fn post_bytes<T: DeserializeOwned>(
        &self,
        path: &str,
        body: impl Into<Bytes>,
        content_type: &str,
        headers: Option<HeaderMap>,
    ) -> Result<T> {
        let body = body.into();
        self.json(
            Method::POST,
            path,
            |request| {
                let request = request.body(body);
                let request = match headers {
                    Some(headers) => request.headers(headers),
                    None => request,
                };
                request.header(CONTENT_TYPE, content_type)
            },
            true,
        )
        .await
    }

    /// Keep a dedicated client alive until the stream is consumed to retain its SSH forward.
    pub async fn get_stream(&self, path: &str) -> Result<Response> {
        let (mut response, lease) = self
            .send(Method::GET, path, |request| request, false)
            .await?;
        if let Some(lease) = lease {
            response.extensions_mut().insert(lease);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::{TcpListener, TcpStream};

    const HANDOFF: &str = r#"{"error":"restarting","code":"daemon_handoff"}"#;
    const UNAVAILABLE: &str = "503 Service Unavailable";

    async fn read_request(
        listener: &TcpListener,
    ) -> (BufReader<TcpStream>, String, HeaderMap, Vec<u8>) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut first_line = String::new();
        stream.read_line(&mut first_line).await.unwrap();
        let mut headers = HeaderMap::new();
        loop {
            let mut line = String::new();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.trim().parse().unwrap(),
            );
        }
        let length = headers
            .get("content-length")
            .map(|length| length.to_str().unwrap().parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        (stream, first_line.trim_end().to_owned(), headers, body)
    }

    async fn respond(mut stream: BufReader<TcpStream>, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    fn reconnecting_client(
        initial: &TcpListener,
        replacement: Option<&TcpListener>,
    ) -> (HttpDaemonClient, Arc<AtomicUsize>) {
        let mut client = HttpDaemonClient::new(
            format!("http://{}", initial.local_addr().unwrap()),
            "endpoint-secret",
        )
        .unwrap();
        client.allow_reconnect = true;
        client.endpoint.get_mut().instance_id = Some("original-instance".into());
        let reconnects = Arc::new(AtomicUsize::new(0));
        let count = reconnects.clone();
        let replacement = replacement.map(|listener| {
            let mut endpoint = EndpointState::new(
                format!("http://{}/replacement", listener.local_addr().unwrap()),
                "replacement-secret".into(),
            );
            endpoint.instance_id = Some("next-instance".into());
            endpoint.shares_client_filesystem = true;
            endpoint
        });
        client.test_reconnect = Some(Box::new(move || {
            let probe = count.fetch_add(1, Ordering::SeqCst);
            let replacement = replacement.clone();
            Box::pin(async move {
                let replacement = replacement.context("unexpected reconnect")?;
                // Simulate the handoff gap where the old daemon has exited and
                // the replacement has not yet published its endpoint.
                Ok((probe > 0).then_some(replacement))
            })
        }));
        (client, reconnects)
    }

    #[tokio::test]
    async fn attachment_renewal_and_detach_return_without_reconnecting_during_handoff() {
        let initial = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, reconnects) = reconnecting_client(&initial, None);

        let server = async {
            for action in ["attach", "detach"] {
                let (stream, line, _, _) = read_request(&initial).await;
                assert_eq!(
                    line,
                    format!("POST /api/v1/terminal-clients/display-client/{action} HTTP/1.1")
                );
                respond(stream, UNAVAILABLE, HANDOFF).await;
            }
        };
        let requests = async {
            assert!(!client.try_attach("display-client".into()).await.unwrap());
            let error = client.detach("display-client".into()).await.unwrap_err();
            assert!(
                error
                    .downcast_ref::<HttpApiError>()
                    .is_some_and(HttpApiError::is_handoff)
            );
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
        assert_eq!(reconnects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_handoff_refusals_refresh_once_and_replay_exact_requests() {
        let initial = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let replacement = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, reconnects) = reconnecting_client(&initial, Some(&replacement));
        let payload = Bytes::from(vec![0x9b; 128 * 1024]);
        let body = json!({"name":"updated"});
        let server = async {
            let mut attempts = std::collections::BTreeMap::new();
            let mut streams = Vec::new();
            // Hold the refusals until all requests are in flight at the same endpoint.
            for _ in 0..6 {
                let (stream, line, headers, body) = read_request(&initial).await;
                assert_eq!(headers[AUTHORIZATION], "Bearer endpoint-secret");
                assert!(attempts.insert(line, (headers, body)).is_none());
                streams.push(stream);
            }
            for stream in streams {
                respond(stream, UNAVAILABLE, HANDOFF).await;
            }
            for _ in 0..6 {
                let (stream, line, headers, body) = read_request(&replacement).await;
                let initial = line.replacen("/replacement/", "/", 1);
                let (previous_headers, previous_body) = attempts.remove(&initial).unwrap();
                assert_eq!(headers[AUTHORIZATION], "Bearer replacement-secret");
                for name in [CONTENT_TYPE.as_str(), "x-upload-name", "x-command-id"] {
                    assert_eq!(headers.get(name), previous_headers.get(name));
                }
                assert_eq!(body, previous_body);
                respond(stream, "200 OK", r#"{"ok":true}"#).await;
            }
            assert!(attempts.is_empty());
        };
        let requests = async {
            let mut headers = HeaderMap::new();
            headers.insert("x-upload-name", "fixture".parse().unwrap());
            headers.insert("x-command-id", "test-command".parse().unwrap());
            let (posted, (), (), (), uploaded, queried) = tokio::try_join!(
                client.post_json::<Value>("/create", &body),
                client.rename_workspace("workspace/id".into(), "updated".into()),
                client.put_json_no_content("/update", &body),
                client.remove_project("bundle".into()),
                client.post_bytes::<Value>(
                    "/upload",
                    payload,
                    "application/octet-stream",
                    Some(headers)
                ),
                client.post_empty_query::<Value>("/projects", &[("retry", "true")]),
            )
            .unwrap();
            assert_eq!(posted, json!({"ok":true}));
            assert_eq!(uploaded, posted);
            assert_eq!(queried, posted);
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(client.instance_id().await.as_deref(), Some("next-instance"));
        assert!(client.shares_client_filesystem().await);
    }

    #[tokio::test]
    async fn abandoned_handoff_resumes_against_the_same_daemon() {
        let daemon = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = HttpDaemonClient::new(
            format!("http://{}", daemon.local_addr().unwrap()),
            "endpoint-secret",
        )
        .unwrap();
        client.allow_reconnect = true;
        client.endpoint.get_mut().instance_id = Some("original-instance".into());
        let same_endpoint = client.endpoint.get_mut().clone();
        let reconnects = Arc::new(AtomicUsize::new(0));
        let count = reconnects.clone();
        client.test_reconnect = Some(Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            let endpoint = same_endpoint.clone();
            Box::pin(async move { Ok(Some(endpoint)) })
        }));
        let server = async {
            // Drain refuses twice, then lapses and the original daemon accepts.
            for _ in 0..2 {
                let (stream, line, _, _) = read_request(&daemon).await;
                assert_eq!(line, "POST /api/v1/workspaces/sample/touch HTTP/1.1");
                respond(stream, UNAVAILABLE, HANDOFF).await;
            }
            let (stream, line, _, _) = read_request(&daemon).await;
            assert_eq!(line, "POST /api/v1/workspaces/sample/touch HTTP/1.1");
            respond(stream, "204 No Content", "").await;
        };
        let ((), result) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(server, client.touch_workspace("sample".into()))
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(
            client.instance_id().await.as_deref(),
            Some("original-instance")
        );
    }

    #[tokio::test]
    async fn lost_acknowledgement_does_not_replay_a_mutation() {
        let initial = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, reconnects) = reconnecting_client(&initial, None);
        let body = json!({"name":"maybe saved"});
        let server = async {
            let (stream, line, _, received) = read_request(&initial).await;
            assert_eq!(line, "POST /create HTTP/1.1");
            assert_eq!(serde_json::from_slice::<Value>(&received).unwrap(), body);
            drop(stream);
        };
        let (_, error) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(server, client.post_json::<Value>("/create", &body))
        })
        .await
        .unwrap();
        assert!(error.unwrap_err().is::<reqwest::Error>());
        assert_eq!(reconnects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ordinary_service_failures_and_conflicts_do_not_replay_mutations() {
        let initial = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, reconnects) = reconnecting_client(&initial, None);
        let server = async {
            for (status, body) in [
                (UNAVAILABLE, r#"{"error":"unavailable"}"#),
                ("409 Conflict", HANDOFF),
            ] {
                let (stream, line, _, _) = read_request(&initial).await;
                assert_eq!(line, "DELETE /remove HTTP/1.1");
                respond(stream, status, body).await;
            }
        };
        let requests = async {
            for status in [StatusCode::SERVICE_UNAVAILABLE, StatusCode::CONFLICT] {
                let error = client.delete_no_content("/remove").await.unwrap_err();
                let error = error.downcast_ref::<HttpApiError>().unwrap();
                assert_eq!(error.status(), status);
                assert!(!error.is_handoff());
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
        assert_eq!(reconnects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn wiki_episode_and_stream_requests_expose_handoff_without_reconnect() {
        let initial = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, reconnects) = reconnecting_client(&initial, None);
        let server = async {
            for expected in [
                "POST /api/v1/wiki/sync HTTP/1.1",
                "GET /api/v1/wiki/sync/watch?epoch=original&after=17 HTTP/1.1",
                "GET /api/v1/wiki/search?q=words&limit=2 HTTP/1.1",
                "GET /api/v1/events HTTP/1.1",
            ] {
                let (stream, line, _, _) = read_request(&initial).await;
                assert_eq!(line, expected);
                respond(stream, UNAVAILABLE, HANDOFF).await;
            }
        };
        let requests = async {
            for error in [
                client.wiki_sync_request().await.unwrap_err(),
                client.wiki_sync_watch("original", 17).await.unwrap_err(),
                client.wiki_search("words", 2).await.unwrap_err(),
                client.get_stream("/api/v1/events").await.unwrap_err(),
            ] {
                assert!(error.downcast_ref::<HttpApiError>().unwrap().is_handoff());
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
        assert_eq!(reconnects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn authenticates_json_and_binary_requests_and_decodes_api_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = HttpDaemonClient::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "endpoint-secret",
        )
        .unwrap();
        let payload = vec![0x9b; 128 * 1024];
        let server = async {
            for (path, request_body, content_type, response_status, response_body) in [
                (
                    "/json",
                    br#"{"id":42}"#.as_slice(),
                    "application/json",
                    "200 OK",
                    r#"{"id":42}"#,
                ),
                (
                    "/bytes",
                    payload.as_slice(),
                    "application/octet-stream",
                    "200 OK",
                    r#"{"id":43}"#,
                ),
                (
                    "/error",
                    b"".as_slice(),
                    "",
                    "503 Service Unavailable",
                    r#"{"error":"restarting","code":"daemon_handoff"}"#,
                ),
            ] {
                let (stream, first_line, headers, body) = read_request(&listener).await;
                let method = if path == "/error" { "GET" } else { "POST" };
                assert_eq!(first_line, format!("{method} {path} HTTP/1.1"));
                assert_eq!(headers["authorization"], "Bearer endpoint-secret");
                assert_eq!(
                    headers["user-agent"],
                    concat!("mj/", env!("CARGO_PKG_VERSION"))
                );
                if !content_type.is_empty() {
                    assert_eq!(headers[CONTENT_TYPE], content_type);
                }
                if path == "/bytes" {
                    assert_eq!(headers["x-upload-name"], "fixture");
                }

                assert_eq!(body, request_body);
                respond(stream, response_status, response_body).await;
            }
        };

        let requests = async {
            let echoed: Value = client.post_json("/json", &json!({"id":42})).await.unwrap();
            assert_eq!(echoed, json!({"id":42}));
            let mut headers = HeaderMap::new();
            headers.insert("x-upload-name", "fixture".parse().unwrap());
            let uploaded: Value = client
                .post_bytes(
                    "/bytes",
                    payload.clone(),
                    "application/octet-stream",
                    Some(headers),
                )
                .await
                .unwrap();
            assert_eq!(uploaded, json!({"id":43}));
            let error = client.get_json::<Value>("/error").await.unwrap_err();
            let error = error.downcast_ref::<HttpApiError>().unwrap();
            assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(error.code.as_deref(), Some("daemon_handoff"));
            assert_eq!(error.message, "restarting");
            assert!(error.is_handoff());
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
    }
}
