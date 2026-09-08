//! Muse subscription usage queried from the native mint endpoint.
//!
//! Muse keeps the access token in each profile's `auth.json`. The endpoint is
//! intentionally queried directly here so each profile stays isolated and no
//! minted API key is persisted or passed through the rest of the controller.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use hel::hel_credentials::MAX_CREDENTIAL_BYTES;

const MINT_BASE_URL_ENV: &str = "TBH_MINT_BASE_URL";
const DEFAULT_MINT_BASE_URL: &str = "https://api.meta.ai";
const MINT_PATH: &str = "/muse-code/key";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuseUsageWindow {
    pub label: String,
    pub remaining_percent: u8,
    pub resets_at: Option<i64>,
}

/// Query the Muse subscription usage for one profile.
pub async fn query(
    home: &Path,
    environment: &HashMap<String, String>,
) -> Result<Vec<MuseUsageWindow>> {
    let base_url = mint_base_url(environment);
    query_with_timeout(home, &base_url, REQUEST_TIMEOUT).await
}

fn mint_base_url(environment: &HashMap<String, String>) -> String {
    if let Some(value) = environment.get(MINT_BASE_URL_ENV) {
        return value.clone();
    }
    std::env::var(MINT_BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_MINT_BASE_URL.to_owned())
}

async fn query_with_timeout(
    home: &Path,
    base_url: &str,
    timeout: Duration,
) -> Result<Vec<MuseUsageWindow>> {
    let token = read_access_token(home).await?;
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build Muse usage client")?;
    let url = format!("{}{}", base_url.trim_end_matches('/'), MINT_PATH);
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&json!({ "onboard": false }))
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Muse usage request timed out")
            } else {
                // Do not include reqwest's error: it can expose request
                // details when a malformed configured endpoint is involved.
                anyhow::anyhow!("Muse usage request failed")
            }
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        bail!("Muse login expired")
    }
    if !status.is_success() {
        bail!("Muse usage request returned HTTP {status}")
    }
    let body = read_bounded_body(response).await?;
    let payload: Value = serde_json::from_slice(&body).context("decode Muse usage response")?;
    parse(&payload)
}

async fn read_access_token(home: &Path) -> Result<String> {
    let path = home.join("auth.json");
    let file = tokio::fs::File::open(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("Muse login required: auth.json is missing; sign in to Muse")
        } else {
            anyhow::anyhow!("Muse credentials are unavailable")
        }
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CREDENTIAL_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| anyhow::anyhow!("Muse credentials are unavailable"))?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        bail!("Muse credentials exceed the {MAX_CREDENTIAL_BYTES} byte limit")
    }
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Muse credentials are invalid; sign in to Muse"))?;
    let token = payload
        .pointer("/providers/meta/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Muse access token is missing; sign in to Muse"))?;
    if token.chars().any(char::is_whitespace) {
        bail!("Muse access token is invalid; sign in to Muse")
    }
    Ok(token.to_owned())
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Muse usage request timed out")
            } else {
                anyhow::anyhow!("read Muse usage response")
            }
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_CREDENTIAL_BYTES {
            bail!("Muse usage response exceeds the {MAX_CREDENTIAL_BYTES} byte limit")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Parse the native endpoint's subscription usage object.
///
/// A window is shown only when the endpoint supplied the fields needed to
/// describe it. In particular, absent usage never becomes a fabricated 100%
/// allowance. Invalid percentages are rejected so corrupt data cannot look
/// like a healthy quota.
fn parse(payload: &Value) -> Result<Vec<MuseUsageWindow>> {
    let Some(subs_usage_value) = payload.get("subs_usage") else {
        bail!("Muse subscription usage is unavailable")
    };
    let Some(subs_usage) = subs_usage_value.as_object() else {
        if subs_usage_value.is_null() {
            bail!("Muse subscription usage is unavailable")
        }
        bail!("Muse subscription usage is malformed")
    };
    let mut windows = Vec::new();
    if let Some(weekly) = optional_window(subs_usage, "weekly")? {
        let remaining_percent = parse_remaining_percent(weekly.get("used_percent"))?;
        windows.push(MuseUsageWindow {
            label: "Week".to_owned(),
            remaining_percent,
            resets_at: weekly.get("resets_at").and_then(Value::as_i64),
        });
    }
    if let Some(window) = optional_window(subs_usage, "window")? {
        let remaining_percent = parse_remaining_percent(window.get("used_percent"))?;
        let duration = parse_duration(window.get("window_duration_mins"))?;
        windows.push(MuseUsageWindow {
            label: window_label(duration),
            remaining_percent,
            resets_at: window.get("resets_at").and_then(Value::as_i64),
        });
    }
    if windows.is_empty() {
        bail!("Muse subscription usage is unavailable")
    }
    Ok(windows)
}

fn optional_window<'a>(
    subs_usage: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a serde_json::Map<String, Value>>> {
    match subs_usage.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(window)) => Ok(Some(window)),
        Some(_) => bail!("Muse {key} usage window is malformed"),
    }
}

fn parse_duration(value: Option<&Value>) -> Result<i64> {
    let value = value
        .filter(|value| !value.is_null())
        .context("Muse usage window duration is missing")?;
    let Some(duration) = value.as_i64() else {
        bail!("Muse usage returned an invalid window duration")
    };
    if duration <= 0 {
        bail!("Muse usage returned an invalid window duration")
    }
    Ok(duration)
}

fn parse_remaining_percent(value: Option<&Value>) -> Result<u8> {
    let value = value
        .filter(|value| !value.is_null())
        .context("Muse usage percentage is missing")?;
    let Some(used_percent) = value.as_f64() else {
        bail!("Muse usage returned an invalid percentage")
    };
    if !used_percent.is_finite() || used_percent < 0.0 {
        bail!("Muse usage returned an invalid percentage")
    }
    Ok((100.0 - used_percent.min(100.0)).round() as u8)
}

fn window_label(minutes: i64) -> String {
    match minutes {
        300 => "5H".to_owned(),
        value if value % 1_440 == 0 => format!("{}d", value / 1_440),
        value if value % 60 == 0 => format!("{}H", value / 60),
        value => format!("{value}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode};
    use axum::routing::post;
    use axum::{Router, response::Response};
    use std::sync::{Arc, Mutex};
    use tokio::time::sleep;

    #[derive(Clone, Default)]
    struct ServerState {
        requests: Arc<Mutex<Vec<(Method, String, String)>>>,
        response: Arc<Mutex<(StatusCode, String)>>,
        delay: Option<Duration>,
    }

    async fn usage_handler(
        State(state): State<ServerState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response<String> {
        if let Some(delay) = state.delay {
            sleep(delay).await;
        }
        let authorization = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        state.requests.lock().unwrap().push((
            Method::POST,
            authorization,
            String::from_utf8(body.to_vec()).unwrap(),
        ));
        let (status, body) = state.response.lock().unwrap().clone();
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    async fn spawn_server(state: ServerState) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/muse-code/key", post(usage_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    fn write_credentials(home: &Path, token: &str) {
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&json!({
                "providers": { "meta": { "access_token": token } }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn parses_live_shape_and_arbitrary_window_durations() {
        let payload = json!({
            "subs_usage": {
                "weekly": {"used_percent": 1, "resets_at": 1_789_344_000_i64},
                "window": {"used_percent": 3, "window_duration_mins": 300, "resets_at": 1_788_890_595_i64},
                "tier": "premium"
            },
            "is_subs_active": true
        });
        assert_eq!(
            parse(&payload).unwrap(),
            vec![
                MuseUsageWindow {
                    label: "Week".into(),
                    remaining_percent: 99,
                    resets_at: Some(1_789_344_000),
                },
                MuseUsageWindow {
                    label: "5H".into(),
                    remaining_percent: 97,
                    resets_at: Some(1_788_890_595),
                },
            ]
        );
        assert_eq!(window_label(120), "2H");
        assert_eq!(window_label(61), "61m");
        assert_eq!(window_label(2_880), "2d");
    }

    #[test]
    fn preserves_optional_windows_and_resets_but_rejects_incomplete_usage() {
        let payload = json!({
            "subs_usage": {
                "weekly": {"used_percent": 25},
                "window": null
            }
        });
        let windows = parse(&payload).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Week");
        assert_eq!(windows[0].remaining_percent, 75);
        assert_eq!(windows[0].resets_at, None);
        assert_eq!(
            parse(&json!({})).unwrap_err().to_string(),
            "Muse subscription usage is unavailable"
        );
        for window in [
            json!({}),
            json!({"used_percent": 1}),
            json!({"window_duration_mins": 300}),
        ] {
            assert!(
                parse(&json!({"subs_usage": {
                    "weekly": {"used_percent": 25}, "window": window
                }}))
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_negative_and_nonfinite_percentages_and_clamps_overage() {
        assert!(
            parse(&json!({
                "subs_usage": {"weekly": {"used_percent": -1}}
            }))
            .is_err()
        );
        assert!(
            parse(&json!({
                "subs_usage": {"weekly": {"used_percent": "nan"}}
            }))
            .is_err()
        );
        assert!(
            parse(&json!({
                "subs_usage": {"weekly": []}
            }))
            .is_err()
        );
        assert!(
            parse(&json!({
                "subs_usage": {"window": {"used_percent": 1, "window_duration_mins": -1}}
            }))
            .is_err()
        );
        let windows = parse(&json!({
            "subs_usage": {"weekly": {"used_percent": 101}}
        }))
        .unwrap();
        assert_eq!(windows[0].remaining_percent, 0);
        assert_eq!(parse_remaining_percent(Some(&json!(100))).unwrap(), 0);
        assert_eq!(parse_remaining_percent(Some(&json!(3.5))).unwrap(), 97);
    }

    #[tokio::test]
    async fn rejects_missing_invalid_and_oversized_credentials() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            read_access_token(home.path())
                .await
                .unwrap_err()
                .to_string()
                .contains("login required")
        );
        for bytes in [
            b"not-json".to_vec(),
            b"{}".to_vec(),
            vec![b' '; MAX_CREDENTIAL_BYTES + 1],
        ] {
            std::fs::write(home.path().join("auth.json"), bytes).unwrap();
            assert!(read_access_token(home.path()).await.is_err());
        }
    }

    #[tokio::test]
    async fn posts_profile_token_and_native_body_without_persisting_response() {
        let state = ServerState {
            response: Arc::new(Mutex::new((
                StatusCode::OK,
                json!({
                    "subs_usage": {
                        "weekly": {"used_percent": 1, "resets_at": 1_789_344_000_i64}
                    }
                })
                .to_string(),
            ))),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(state.clone()).await;
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_credentials(first.path(), "first-token");
        write_credentials(second.path(), "second-token");
        let first_environment = HashMap::from([(MINT_BASE_URL_ENV.to_owned(), base_url.clone())]);
        let second_environment = HashMap::from([(MINT_BASE_URL_ENV.to_owned(), base_url)]);

        query(first.path(), &first_environment).await.unwrap();
        query(second.path(), &second_environment).await.unwrap();
        assert!(
            std::fs::read_to_string(first.path().join("auth.json"))
                .unwrap()
                .contains("first-token")
        );
        assert!(
            std::fs::read_to_string(second.path().join("auth.json"))
                .unwrap()
                .contains("second-token")
        );
        {
            let requests = state.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].0, Method::POST);
            assert_eq!(requests[0].1, "Bearer first-token");
            assert_eq!(requests[1].1, "Bearer second-token");
            assert_eq!(requests[0].2, r#"{"onboard":false}"#);
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn returns_auth_status_and_timeout_errors_without_response_details() {
        let home = tempfile::tempdir().unwrap();
        write_credentials(home.path(), "secret-token");
        let state = ServerState {
            response: Arc::new(Mutex::new((
                StatusCode::UNAUTHORIZED,
                "secret-payload".into(),
            ))),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(state).await;
        let error = query_with_timeout(home.path(), &base_url, Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Muse login expired");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());

        let delayed = ServerState {
            response: Arc::new(Mutex::new((StatusCode::OK, "{}".into()))),
            delay: Some(Duration::from_secs(1)),
            ..ServerState::default()
        };
        let (base_url, server) = spawn_server(delayed).await;
        let error = query_with_timeout(home.path(), &base_url, Duration::from_millis(20))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Muse usage request timed out");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
