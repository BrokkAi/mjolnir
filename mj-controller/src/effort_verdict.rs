//! Controller client for per-child Jev effort judgments.

use anyhow::{Context, Result};
use mj_core::effort_verdict::{EffortEvidence, EffortVerdict, MAX_BODY_BYTES};
use serde_json::{Value, json};

const DIRECT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const HOSTED_ENDPOINT: &str = "https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/effort-verdict";
const DECISION_KIND: &str = "effort-verdict";

/// Resolve `adaptive` to an advertised effort, recording every outcome. This
/// is advisory: any Jev or configuration failure falls back to the high rung.
pub(crate) async fn resolve(
    parent_session_id: &str,
    task_name: &str,
    instructions: &str,
    model: &str,
    offered: &[mj_core::acp::SessionConfigChoice],
) -> Option<String> {
    let attempt = match mj_core::jev::DecisionLog::open(mj_core::jev::controller_log_dir()) {
        Ok(log) => Some(log.start(
            parent_session_id,
            DECISION_KIND,
            "Choose a child reasoning effort from its assignment and model choices.",
            task_name,
        )),
        Err(error) => {
            tracing::warn!(%error, task_name, "Jev diagnostic log unavailable for adaptive sub-agent effort");
            None
        }
    };

    if !has_concrete_effort(offered) {
        finish(
            attempt,
            "unavailable",
            None,
            None,
            false,
            None,
            None,
            None,
            Some(json!({"reason":"the model offers no configurable effort"})),
        );
        return None;
    }

    let evidence = match EffortEvidence::bounded(task_name, instructions, model) {
        Ok(evidence) => evidence,
        Err(error) => {
            tracing::warn!(%error, task_name, "adaptive sub-agent effort request could not be bounded; using high fallback");
            return fallback(
                attempt,
                offered,
                None,
                None,
                None,
                "request could not be bounded",
                format!("{error:#}"),
            );
        }
    };

    if mj_core::jev::disabled_by_environment() {
        tracing::warn!(
            task_name,
            "Jev is disabled by the environment; using high fallback for adaptive sub-agent effort"
        );
        return fallback(
            attempt,
            offered,
            None,
            Some("disabled"),
            None,
            "Jev is disabled by the environment",
            format!("{}=1", mj_core::jev::DISABLED_ENVIRONMENT),
        );
    }

    let enabled = match jev_enabled().await {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::warn!(%error, task_name, "could not read Jev configuration for adaptive sub-agent effort; using high fallback");
            return fallback(
                attempt,
                offered,
                None,
                None,
                None,
                "Jev configuration could not be read",
                format!("{error:#}"),
            );
        }
    };
    if !enabled {
        tracing::warn!(
            task_name,
            "Jev is disabled; using high fallback for adaptive sub-agent effort"
        );
        return fallback(
            attempt,
            offered,
            None,
            Some("disabled"),
            None,
            "Jev is disabled",
            "[jev].enabled is false".to_owned(),
        );
    }

    match classify(&evidence).await {
        Ok((source, request, response, verdict)) => {
            let effort = map_concrete_effort(offered, &verdict.choice);
            match effort {
                Some(effort) => {
                    finish(
                        attempt,
                        "classified",
                        Some(&verdict.choice),
                        Some(&effort),
                        false,
                        Some(source),
                        Some(request),
                        Some(response),
                        None,
                    );
                    Some(effort)
                }
                None => {
                    let reason = "the model offers no configurable effort";
                    tracing::warn!(task_name, %reason, "adaptive sub-agent effort could not be mapped; using high fallback");
                    fallback(
                        attempt,
                        offered,
                        Some(request),
                        Some(source),
                        Some(response),
                        reason,
                        "no advertised effort remained after mapping".to_owned(),
                    )
                }
            }
        }
        Err(failure) => {
            tracing::warn!(%failure.cause, task_name, "adaptive sub-agent effort failed; using high fallback");
            fallback(
                attempt,
                offered,
                failure.request,
                failure.source,
                failure.response,
                "Jev returned no usable effort verdict",
                failure.cause,
            )
        }
    }
}

struct ClassificationFailure {
    source: Option<&'static str>,
    request: Option<Value>,
    response: Option<Value>,
    cause: String,
}

async fn classify(
    evidence: &EffortEvidence,
) -> std::result::Result<(&'static str, Value, Value, EffortVerdict), ClassificationFailure> {
    let key = tokio::task::spawn_blocking(mj_core::activity::verdict::api_key)
        .await
        .context("resolve Jev key")
        .map_err(classification_failure)?;
    let (endpoint, source, request_body) = if key.is_some() {
        (
            DIRECT_ENDPOINT,
            "direct",
            evidence.upstream_body().map_err(classification_failure)?,
        )
    } else {
        (
            HOSTED_ENDPOINT,
            "hosted",
            evidence.hosted_body().map_err(classification_failure)?,
        )
    };
    let request = serde_json::from_slice::<Value>(&request_body)
        .context("decode effort verdict request for diagnostics")
        .map_err(|error| ClassificationFailure {
            source: Some(source),
            request: None,
            response: None,
            cause: format!("{error:#}"),
        })?;
    let response = crate::jev_transport::post_bounded_json(
        endpoint,
        key.as_deref(),
        request_body,
        MAX_BODY_BYTES,
        "request adaptive sub-agent effort verdict",
    )
    .await
    .map_err(|error| ClassificationFailure {
        source: Some(source),
        request: Some(request.clone()),
        response: None,
        cause: format!("{error:#}"),
    })?;
    let verdict = EffortVerdict::parse(&response).map_err(|error| ClassificationFailure {
        source: Some(source),
        request: Some(request.clone()),
        response: Some(response.clone()),
        cause: format!("{error:#}"),
    })?;
    Ok((source, request, response, verdict))
}

fn classification_failure(error: impl std::fmt::Display) -> ClassificationFailure {
    ClassificationFailure {
        source: None,
        request: None,
        response: None,
        cause: error.to_string(),
    }
}

async fn jev_enabled() -> Result<bool> {
    tokio::task::spawn_blocking(mj_core::config::Config::load)
        .await
        .context("join Jev configuration load")?
        .map(|config| config.jev.enabled)
        .context("load Jev configuration")
}

fn fallback(
    attempt: Option<mj_core::jev::Attempt>,
    offered: &[mj_core::acp::SessionConfigChoice],
    request: Option<Value>,
    source: Option<&str>,
    response: Option<Value>,
    reason: &str,
    cause: String,
) -> Option<String> {
    let effort = map_concrete_effort(offered, "high");
    finish(
        attempt,
        "fallback",
        Some("high"),
        effort.as_deref(),
        true,
        source,
        request,
        response,
        Some(json!({"reason":reason,"cause":cause})),
    );
    effort
}

fn has_concrete_effort(offered: &[mj_core::acp::SessionConfigChoice]) -> bool {
    offered.iter().any(|choice| {
        !choice.value.eq_ignore_ascii_case("default")
            && choice.value != mj_core::subagent::ADAPTIVE_EFFORT
    })
}

fn map_concrete_effort(
    offered: &[mj_core::acp::SessionConfigChoice],
    rung: &str,
) -> Option<String> {
    mj_core::effort_verdict::map_rung_to_offered(offered, rung)
        .filter(|effort| effort != mj_core::subagent::ADAPTIVE_EFFORT)
}

#[allow(clippy::too_many_arguments)]
fn finish(
    attempt: Option<mj_core::jev::Attempt>,
    status: &str,
    rung: Option<&str>,
    effort: Option<&str>,
    fallback: bool,
    source: Option<&str>,
    request: Option<Value>,
    response: Option<Value>,
    detail: Option<Value>,
) {
    let answer = format!(
        "rung={}; effort={}; fallback={fallback}",
        rung.unwrap_or("none"),
        effort.unwrap_or("default")
    );
    let action = format!("child starts at {}", effort.unwrap_or("default"));
    if let Some(attempt) = attempt {
        let technical = json!({
            "contract":"effort-verdict-v1",
            "source":source,
            "request":request,
            "response":response,
            "questions":mj_core::effort_verdict::questions(),
            "fallback":fallback,
            "rung":rung,
            "effort":effort,
            "detail":detail,
        });
        attempt.update(Some(&answer), technical);
        attempt.finish(status, &action);
    }
}
