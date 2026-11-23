//! Typed access to the daemon's direct HTTP API.
use crate::daemon::{DaemonClient, ssh::SshLease};
use anyhow::{Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use std::sync::Arc;

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
    base_url: String,
    token: String,
    instance_id: Option<String>,
    shares_client_filesystem: bool,
    http: reqwest::Client,
    _ssh_lease: Option<Arc<SshLease>>,
}

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

    async fn from_response(response: Response) -> Self {
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
        let body = response.text().await.unwrap_or_default();
        let (message, code) = match serde_json::from_str::<ErrorBody>(&body) {
            Ok(error) => (error.error, error.code),
            Err(_) if body.trim().is_empty() => (status.to_string(), None),
            Err(_) => (body, None),
        };
        Self {
            status,
            code,
            message,
            handoff_header,
        }
    }
}

impl HttpDaemonClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        // The workspace's reqwest build leaves provider selection to its caller.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            instance_id: None,
            shares_client_filesystem: false,
            http,
            _ssh_lease: None,
        })
    }

    pub async fn from_daemon_client(client: &mut DaemonClient) -> Result<Self> {
        let identity = client.identity().await?;
        let (base_url, token) = client.direct_api_endpoint(&identity)?.context(
            "The daemon did not publish a loopback HTTP API endpoint. If it predates the loopback HTTP API, update and restart the daemon.",
        )?;
        let mut http = Self::new(base_url, token)?;
        http.shares_client_filesystem = client.shares_client_filesystem(&identity);
        http.instance_id = Some(identity.instance_id);
        http._ssh_lease = client.ssh_lease();
        Ok(http)
    }

    pub fn instance_id(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }

    pub fn shares_client_filesystem(&self) -> bool {
        self.shares_client_filesystem
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(
                method,
                format!("{}/{}", self.base_url, path.trim_start_matches('/')),
            )
            .bearer_auth(&self.token)
    }

    async fn send(request: RequestBuilder) -> Result<Response> {
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(HttpApiError::from_response(response).await.into());
        }
        Ok(response)
    }

    async fn json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
        Ok(Self::send(request).await?.json().await?)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::json(self.request(Method::GET, path)).await
    }

    pub async fn get_json_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        Self::json(self.request(Method::GET, path).query(query)).await
    }

    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        Self::json(self.request(Method::POST, path).json(body)).await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::json(self.request(Method::POST, path)).await
    }

    pub async fn put_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        Self::json(self.request(Method::PUT, path).json(body)).await
    }

    pub async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::json(self.request(Method::DELETE, path)).await
    }

    pub async fn post_bytes<T: DeserializeOwned>(
        &self,
        path: &str,
        body: impl Into<reqwest::Body>,
        content_type: &str,
        headers: Option<HeaderMap>,
    ) -> Result<T> {
        let request = self.request(Method::POST, path).body(body);
        let request = match headers {
            Some(headers) => request.headers(headers),
            None => request,
        };
        Self::json(request.header(CONTENT_TYPE, content_type)).await
    }

    /// Keep this client alive until the stream is consumed to retain its SSH forward.
    pub async fn get_stream(&self, path: &str) -> Result<Response> {
        Self::send(self.request(Method::GET, path)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut first_line = String::new();
                stream.read_line(&mut first_line).await.unwrap();
                let method = if path == "/error" { "GET" } else { "POST" };
                assert_eq!(first_line.trim_end(), format!("{method} {path} HTTP/1.1"));

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

                let length = headers
                    .get("content-length")
                    .map(|length| length.to_str().unwrap().parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                assert_eq!(body, request_body);
                let response = format!(
                    "HTTP/1.1 {response_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
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
