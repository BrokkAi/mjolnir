//! Bind the remote bearer credential to a proven HTTP/1.1 connection.
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Either, Empty, Limited};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{AUTHORIZATION, CONNECTION, HOST, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{convert::Infallible, net::SocketAddr, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::http::{HttpProof, MAX_PROOF_RESPONSE_BYTES, proof_nonce, verify_http_api_proof};

type ForwardBody = Either<Empty<Bytes>, Incoming>;

pub(super) struct SshHttpProxy {
    address: SocketAddr,
    token: String,
    // The lease owns the port even if the accept task exits early.
    _listener: Arc<TcpListener>,
    cancel: CancellationToken,
    accept: JoinHandle<()>,
}

struct UpstreamTarget {
    address: SocketAddr,
    host: HeaderValue,
    remote_token: String,
    remote_bearer: HeaderValue,
    proxy_bearer: HeaderValue,
}

struct ProvenConnection {
    sender: SendRequest<ForwardBody>,
    _driver: AbortOnDropHandle<()>,
}

impl SshHttpProxy {
    pub(super) async fn start(address: SocketAddr, remote_token: String) -> Result<Self> {
        let listener = Arc::new(
            TcpListener::bind("127.0.0.1:0")
                .await
                .context("bind the SSH HTTP proxy")?,
        );
        let proxy_address = listener.local_addr()?;
        let token = proof_nonce()?;
        let target = Arc::new(UpstreamTarget {
            address,
            host: HeaderValue::from_str(&address.to_string())?,
            remote_bearer: bearer(&remote_token)?,
            remote_token,
            proxy_bearer: bearer(&token)?,
        });
        let cancel = CancellationToken::new();
        let accept =
            mj_core::runtime::spawn(accept_connections(listener.clone(), target, cancel.clone()));
        Ok(Self {
            address: proxy_address,
            token,
            _listener: listener,
            cancel,
            accept,
        })
    }

    pub(super) fn endpoint(&self) -> (String, String) {
        (format!("http://{}", self.address), self.token.clone())
    }

    pub(super) async fn stop(mut self) {
        self.cancel.cancel();
        if let Err(error) = (&mut self.accept).await {
            tracing::warn!(%error, "SSH HTTP proxy shutdown failed");
        }
    }
}

impl Drop for SshHttpProxy {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn bearer(token: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    Ok(value)
}

async fn accept_connections(
    listener: Arc<TcpListener>,
    target: Arc<UpstreamTarget>,
    cancel: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "SSH HTTP proxy connection task failed");
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((socket, _)) => {
                        connections.spawn(serve_client(socket, target.clone()));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "SSH HTTP proxy accept failed");
                        break;
                    }
                }
            }
        }
    }
    connections.shutdown().await;
}

async fn serve_client(socket: TcpStream, target: Arc<UpstreamTarget>) {
    let upstream = Mutex::new(None);
    let service = service_fn(|request| async {
        let response = match forward(request, &target, &upstream).await {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, "SSH HTTP proxy upstream failed");
                empty_response(StatusCode::BAD_GATEWAY)
            }
        };
        Ok::<_, Infallible>(response)
    });
    if let Err(error) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(socket), service)
        .await
    {
        tracing::debug!(%error, "SSH HTTP proxy client disconnected");
    }
}

async fn forward(
    mut request: Request<Incoming>,
    target: &UpstreamTarget,
    upstream: &Mutex<Option<ProvenConnection>>,
) -> Result<Response<ForwardBody>> {
    let mut authorization = request.headers().get_all(AUTHORIZATION).iter();
    let authenticated = match authorization.next() {
        None => false,
        Some(value)
            if authorization.next().is_none()
                && bool::from(value.as_bytes().ct_eq(target.proxy_bearer.as_bytes())) =>
        {
            true
        }
        Some(_) => return Ok(empty_response(StatusCode::UNAUTHORIZED)),
    };
    let mut upstream = upstream.lock().await;
    if let Some(connection) = upstream.as_mut()
        && connection.sender.ready().await.is_err()
    {
        *upstream = None;
    }
    let connection = match upstream.as_mut() {
        Some(connection) => connection,
        None => upstream.insert(connect_proven(target).await?),
    };
    strip_connection_headers(request.headers_mut());
    request.headers_mut().insert(HOST, target.host.clone());
    request.headers_mut().remove(AUTHORIZATION);
    if authenticated {
        request
            .headers_mut()
            .insert(AUTHORIZATION, target.remote_bearer.clone());
    }
    let mut response = match connection
        .sender
        .send_request(request.map(Either::Right))
        .await
    {
        Ok(response) => response,
        Err(error) => {
            // The daemon may have received the request. Reconnect on the next
            // request without replaying this one.
            *upstream = None;
            return Err(error).context("send request on the proven SSH HTTP connection");
        }
    };
    strip_connection_headers(response.headers_mut());
    Ok(response.map(Either::Right))
}

async fn connect_proven(target: &UpstreamTarget) -> Result<ProvenConnection> {
    let socket = TcpStream::connect(target.address).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(socket)).await?;
    let driver = AbortOnDropHandle::new(mj_core::runtime::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "SSH HTTP upstream connection closed");
        }
    }));

    let nonce = proof_nonce()?;
    let request = Request::get(format!("/api/v1/prove?nonce={nonce}"))
        .header(HOST, "127.0.0.1")
        .body(Either::Left(Empty::new()))?;
    let response = sender.send_request(request).await?;
    if response.status() != StatusCode::OK {
        bail!(
            "SSH HTTP forward refused the identity challenge ({})",
            response.status()
        );
    }
    let body = Limited::new(response.into_body(), MAX_PROOF_RESPONSE_BYTES)
        .collect()
        .await
        .map_err(|error| anyhow::anyhow!("read SSH HTTP identity proof: {error}"))?
        .to_bytes();
    let proof: HttpProof =
        serde_json::from_slice(&body).context("decode SSH HTTP identity proof")?;
    ensure!(
        verify_http_api_proof(&target.remote_token, &nonce, &proof.mac),
        "SSH HTTP forward submitted an invalid identity proof"
    );
    sender.ready().await?;
    Ok(ProvenConnection {
        sender,
        _driver: driver,
    })
}

fn empty_response(status: StatusCode) -> Response<ForwardBody> {
    let mut response = Response::new(Either::Left(Empty::new()));
    *response.status_mut() = status;
    response
}

/// Each hop handles framing and connection lifetime independently.
fn strip_connection_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpApiError, HttpDaemonClient, http_api_proof_hex, is_valid_proof_nonce};
    use http_body_util::Full;
    use tokio::sync::mpsc;

    const REAL_TOKEN: &str = "fixture-remote-api-token";
    const PROTECTED: &str = "/api/v1/protected";

    #[derive(Debug)]
    struct Observation {
        path: String,
        authorization: Option<HeaderValue>,
        contains_real_token: bool,
    }

    async fn test_upstream(
        listener: TcpListener,
        stop: CancellationToken,
        observed: mpsc::UnboundedSender<Observation>,
        authentic: bool,
    ) {
        let mut connections = JoinSet::new();
        loop {
            let socket = tokio::select! {
                () = stop.cancelled() => break,
                accepted = listener.accept() => accepted.unwrap().0,
            };
            let observed = observed.clone();
            connections.spawn(async move {
                let proved = std::sync::atomic::AtomicBool::new(false);
                let service = service_fn(|request: Request<Incoming>| async {
                    use std::sync::atomic::Ordering::Relaxed;

                    let path = request.uri().path();
                    let headers = request.headers();
                    let authorization = headers.get(AUTHORIZATION).cloned();
                    observed
                        .send(Observation {
                            path: path.into(),
                            authorization: authorization.clone(),
                            contains_real_token: request.uri().to_string().contains(REAL_TOKEN)
                                || headers.values().any(|value| {
                                    String::from_utf8_lossy(value.as_bytes()).contains(REAL_TOKEN)
                                }),
                        })
                        .unwrap();
                    let mut response = if path == "/api/v1/prove" {
                        assert!(authorization.is_none());
                        assert_eq!(headers[HOST], "127.0.0.1");
                        let nonce = request
                            .uri()
                            .query()
                            .unwrap()
                            .strip_prefix("nonce=")
                            .unwrap();
                        assert!(is_valid_proof_nonce(nonce));
                        proved.store(authentic, Relaxed);
                        let key = if authentic { REAL_TOKEN } else { "wrong-key" };
                        let body = serde_json::json!({ "mac": http_api_proof_hex(key, nonce) });
                        Response::new(Full::new(Bytes::from(body.to_string())))
                    } else if authentic {
                        assert_eq!(path, PROTECTED);
                        assert!(
                            proved.load(Relaxed),
                            "request used a different upstream socket from the proof"
                        );
                        if authorization == Some(bearer(REAL_TOKEN).unwrap()) {
                            let payload = request.into_body().collect().await.unwrap().to_bytes();
                            let payload = String::from_utf8(payload.to_vec()).unwrap();
                            Response::new(Full::new(Bytes::from(
                                serde_json::to_vec(&payload).unwrap(),
                            )))
                        } else {
                            assert!(authorization.is_none());
                            let mut response = Response::new(Full::new(Bytes::new()));
                            *response.status_mut() = StatusCode::UNAUTHORIZED;
                            response
                        }
                    } else {
                        panic!("imposter received a request other than the identity challenge");
                    };
                    response.headers_mut().insert(
                        hyper::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    Ok::<_, Infallible>(response)
                });
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await
            });
        }
        drop(listener);
        connections.shutdown().await;
    }

    #[tokio::test]
    async fn ssh_http_proxy_streams_and_rejects_rebound_imposter_before_sending_bearer() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let stop = CancellationToken::new();
            let (observed, mut requests) = mpsc::unbounded_channel();
            let real = mj_core::runtime::spawn(test_upstream(
                listener,
                stop.clone(),
                observed.clone(),
                true,
            ));
            let proxy = SshHttpProxy::start(address, REAL_TOKEN.into())
                .await
                .unwrap();
            let (url, local_token) = proxy.endpoint();
            assert_ne!(local_token, REAL_TOKEN);
            let typed = HttpDaemonClient::new(&url, &local_token).unwrap();
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let protected_url = format!("{url}{PROTECTED}");

            let wrong = http
                .get(&protected_url)
                .bearer_auth("bad-token")
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
            let mut duplicates = HeaderMap::new();
            duplicates.append(AUTHORIZATION, bearer(&local_token).unwrap());
            duplicates.append(AUTHORIZATION, bearer(&local_token).unwrap());
            let wrong = http
                .get(&protected_url)
                .headers(duplicates)
                .send()
                .await
                .unwrap();
            assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
            assert!(
                requests.try_recv().is_err(),
                "invalid local auth reached upstream"
            );

            let anonymous = http.get(&protected_url).send().await.unwrap();
            assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
            let proof = requests.recv().await.unwrap();
            assert_eq!(proof.path, "/api/v1/prove");
            assert!(!proof.contains_real_token);
            let anonymous = requests.recv().await.unwrap();
            assert_eq!(anonymous.path, PROTECTED);
            assert!(anonymous.authorization.is_none());
            assert!(!anonymous.contains_real_token);

            let payload = "streamed payload ".repeat(8 * 1024);
            for _ in 0..2 {
                let echoed: String = typed
                    .post_bytes(
                        PROTECTED,
                        payload.as_bytes().to_vec(),
                        "application/octet-stream",
                        None,
                    )
                    .await
                    .unwrap();
                assert_eq!(echoed, payload);
            }
            let proof = requests.recv().await.unwrap();
            assert_eq!(proof.path, "/api/v1/prove");
            assert!(!proof.contains_real_token);
            for _ in 0..2 {
                let authenticated = requests.recv().await.unwrap();
                assert_eq!(authenticated.path, PROTECTED);
                assert_eq!(
                    authenticated.authorization,
                    Some(bearer(REAL_TOKEN).unwrap())
                );
            }
            assert!(
                requests.try_recv().is_err(),
                "the proven connection was not reused"
            );

            stop.cancel();
            real.await.unwrap();
            let imposter_listener = TcpListener::bind(address).await.unwrap();
            let stop = CancellationToken::new();
            let imposter = mj_core::runtime::spawn(test_upstream(
                imposter_listener,
                stop.clone(),
                observed,
                false,
            ));
            for client in [typed, HttpDaemonClient::new(&url, &local_token).unwrap()] {
                let error = client.get_json::<String>(PROTECTED).await.unwrap_err();
                assert_eq!(
                    error.downcast_ref::<HttpApiError>().unwrap().status(),
                    StatusCode::BAD_GATEWAY
                );
            }
            stop.cancel();
            imposter.await.unwrap();
            let mut count = 0;
            while let Ok(request) = requests.try_recv() {
                assert_eq!(request.path, "/api/v1/prove");
                assert!(request.authorization.is_none());
                assert!(!request.contains_real_token);
                count += 1;
            }
            assert!(
                count > 0,
                "the impersonation fixture never received a proof challenge"
            );
            proxy.stop().await;
        })
        .await
        .unwrap();
    }
}
