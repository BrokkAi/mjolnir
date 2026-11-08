//! Controller-side Jev client for GitHub item interest and ownership.

use anyhow::{Context, Result};
use mj_core::github_item::{GithubItemEvidence, GithubItemVerdict, MAX_BODY_BYTES};
use serde_json::json;

const DIRECT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const HOSTED_ENDPOINT: &str =
    "https://mj-jev-proxy.eng-admin-a63.workers.dev/v2/github-item-verdict";

pub async fn classify(evidence: &GithubItemEvidence) -> Result<GithubItemVerdict> {
    let evidence = evidence.bounded_for_request()?;
    let key = tokio::task::spawn_blocking(mj_core::activity::verdict::api_key)
        .await
        .context("resolve Jev key")?;
    let (endpoint, source, direct_questions) = if key.is_some() {
        (
            DIRECT_ENDPOINT,
            "direct",
            Some(mj_core::github_item::questions()),
        )
    } else {
        (HOSTED_ENDPOINT, "hosted", None)
    };
    let request_body =
        serde_json::to_vec(&mj_core::jev::request_body(&evidence, direct_questions))?;

    let attempt = match mj_core::jev::DecisionLog::open(mj_core::jev::controller_log_dir()) {
        Ok(log) => Some(log.start(
            &format!("github:{}#{}", evidence.item.repo, evidence.item.number),
            "github-item-verdict",
            "Is this GitHub item relevant to this session, based on its compact digest?",
            &format!(
                "{} #{}; compact session digest",
                evidence.item.repo, evidence.item.number
            ),
        )),
        Err(error) => {
            tracing::warn!(%error, "Jev diagnostic log unavailable for GitHub item classification");
            None
        }
    };
    if let Some(attempt) = &attempt {
        let request = serde_json::from_slice::<serde_json::Value>(&request_body)?;
        attempt.update(
            None,
            json!({
                "request": request,
                "source": source,
                "contract": "github-item-verdict-v2",
                "questions": mj_core::github_item::questions(),
                "model": "jev-latest"
            }),
        );
    }

    let response = crate::jev_transport::post_bounded_json(
        endpoint,
        key.as_deref(),
        request_body,
        MAX_BODY_BYTES,
        "request GitHub item verdict",
    )
    .await;
    let result = response.and_then(|body| GithubItemVerdict::parse(&body));
    if let Some(attempt) = attempt {
        match &result {
            Ok(verdict) => {
                let answer = format!("Interested: {}.", verdict.interested);
                attempt.update(Some(&answer), json!({"answer": answer}));
                attempt.finish(
                    "classified",
                    "The GitHub item was classified for this session.",
                );
            }
            Err(error) => {
                attempt.update(
                    Some("Jev did not return a usable GitHub item verdict."),
                    json!({"error": format!("{error:#}")}),
                );
                attempt.finish(
                    "failed",
                    "GitHub item classification failed; no interest or watch decision was applied.",
                );
            }
        }
    }
    result
}
