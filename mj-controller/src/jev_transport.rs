//! Shared bounded HTTP transport for controller-side Jev classifiers.

use anyhow::{Context, Result, ensure};
use serde_json::Value;

pub(crate) async fn post_bounded_json(
    endpoint: &str,
    key: Option<&str>,
    request_body: Vec<u8>,
    response_limit: usize,
    request_context: &'static str,
) -> Result<Value> {
    ensure!(
        request_body.len() <= response_limit,
        "oversized Jev request"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(request_body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let mut response = request
        .send()
        .await
        .context(request_context)?
        .error_for_status()?;
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= response_limit as u64),
        "oversized Jev response"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= response_limit,
            "oversized Jev response"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&body)?)
}
