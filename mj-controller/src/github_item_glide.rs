//! Hosted GLiDE boundary for aliases600 evidence.

use anyhow::{Result, ensure};
use mj_core::github_item::{GithubItemEvidence, MAX_BODY_BYTES};

const ENDPOINT: &str = "https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/github-item-glide";

/// Always use the hosted classifier, irrespective of local Jev credentials.
pub async fn classify_probability(evidence: &GithubItemEvidence) -> Result<f64> {
    classify_at(evidence, ENDPOINT).await
}

async fn classify_at(evidence: &GithubItemEvidence, endpoint: &str) -> Result<f64> {
    let evidence = evidence.bounded_for_request()?;
    let response = crate::jev_transport::post_bounded_json(
        endpoint,
        None,
        serde_json::to_vec(&evidence)?,
        MAX_BODY_BYTES,
        "request GitHub item GLiDE probability",
    )
    .await?;
    let probability = response["probability"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing GitHub item GLiDE probability"))?;
    ensure!(
        probability.is_finite() && (0.0..=1.0).contains(&probability),
        "invalid GitHub item GLiDE probability"
    );
    Ok(probability)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn github_glide_http_contract() {
        let mut fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../services/jev-proxy/tests/fixtures/github-item-aliases600.json"
        ))
        .unwrap();
        fixture["evidence"]["session"]
            .as_object_mut()
            .unwrap()
            .remove("history");
        let evidence: GithubItemEvidence =
            serde_json::from_value(fixture["evidence"].clone()).unwrap();
        for body in [
            r#"{"probability":0.1123}"#,
            r#"{}"#,
            r#"{"probability":"0.5"}"#,
            r#"{"probability":-0.1}"#,
            r#"{"probability":1.1}"#,
            r#"{"probability":NaN}"#,
            r#"{"probability":1e999}"#,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "http://{}/v1/github-item-glide",
                listener.local_addr().unwrap()
            );
            let expected = serde_json::to_vec(&evidence).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = String::from_utf8_lossy(&request[..header_end]);
                assert!(header.starts_with("POST /v1/github-item-glide HTTP/1.1"));
                assert!(!header.to_lowercase().contains("authorization:"));
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap();
                while request.len() < header_end + length {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                assert_eq!(&request[header_end..], expected);
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let result = classify_at(&evidence, &endpoint).await;
            server.await.unwrap();
            if body.contains("0.1123") {
                assert_eq!(result.unwrap(), 0.1123);
            } else {
                assert!(result.is_err(), "{body}");
            }
        }
    }
}
