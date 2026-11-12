//! Evidence-only GLiDE boundary, independent of the older classifiers' budgets.
use anyhow::{Result, ensure};
use mj_core::{
    activity::{
        ActivityFacts,
        verdict::{TurnEvidence, TurnPhase},
    },
    assessment::ContextHistory,
};
use serde_json::Value;
pub(crate) fn project(
    evidence: &TurnEvidence,
    history: ContextHistory,
    facts: &ActivityFacts,
    native_agent_count: usize,
) -> Result<Value> {
    ensure!(
        evidence.phase == TurnPhase::Replied,
        "GLiDE requires a completed turn"
    );
    let mut base = evidence.clone();
    base.transcript_summary.clear();
    base.authorization = Some(history.clone());
    let mut body = mj_core::jev::request_body(&base, None);
    let latest = history.messages.iter().rposition(|m| m.role == "user");
    body["latest_user_message"] = latest
        .map(|i| history.messages[i].text.clone())
        .unwrap_or_default()
        .into();
    body["current_assistant_replies"] = serde_json::json!(
        history
            .messages
            .iter()
            .skip(latest.map_or(0, |i| i + 1))
            .filter(|m| m.role == "assistant")
            .map(|m| &m.text)
            .collect::<Vec<_>>()
    );
    body["runtime_observations"] = serde_json::json!([
        format!(
            "Current phase {:?}, silence {} seconds.",
            base.phase, base.silent_for_s
        ),
        format!(
            "Currently observed tools: {}; background commands: {}; queued prompts: {}.",
            serde_json::to_string(&base.tools_in_flight)?,
            facts.background_commands,
            facts.queued_commands
        ),
        format!(
            "Final tool outcomes: {}; background inventory: {}.",
            serde_json::to_string(&base.final_tool_calls)?,
            serde_json::to_string(&base.background)?
        ),
        format!(
            "Currently running native agents: {native_agent_count}. Goal active: {}; running: {}; pending resume: {}.",
            facts.goal_active, facts.goal_running, facts.goal_pending_resume
        )
    ]);
    mj_core::redact::redact_json_strings(&mut body);
    Ok(body)
}

/// Remove the oldest authorization message first, then progressively shorten
/// the longest remaining message while preserving both ends.
pub(crate) fn shrink(history: &mut ContextHistory) -> bool {
    let latest_user = history.messages.iter().rposition(|m| m.role == "user");
    if latest_user.is_some_and(|i| i > 0) {
        let removed = history.messages.remove(0);
        if removed.role == "user" {
            history.authorization_complete = false;
        } else {
            history.assistant_history_omitted = true;
        }
        return true;
    }
    let Some((index, message)) = history
        .messages
        .iter()
        .enumerate()
        .max_by_key(|(_, m)| m.text.len())
    else {
        return false;
    };
    if message.text.is_empty() {
        return false;
    }
    let keep = message.text.len() / 2;
    let head = keep / 2;
    let tail = keep - head;
    let text = &message.text;
    let head = text.floor_char_boundary(head);
    let tail_start = text.ceil_char_boundary(text.len() - tail);
    history.messages[index].text = format!("{}{}", &text[..head], &text[tail_start..]);
    if history.messages[index].role == "user" {
        history.authorization_complete = false;
    } else {
        history.assistant_history_omitted = true;
    }
    true
}

pub(crate) use mj_core::activity::eot_glide::types::Probabilities;

#[cfg(test)]
pub(crate) fn test_samples() -> Value {
    let mut samples: Value = serde_json::from_str(include_str!(
        "../../../../mj-core/src/activity/eot_glide_fresh_sample.json"
    ))
    .unwrap();
    // These recorded vectors predate attention. Exercise the new axis without
    // altering the frozen action probabilities or their recorded provenance.
    for sample in samples.as_array_mut().unwrap() {
        sample["answers"]["attention"] = serde_json::json!({"probabilities": {
            "needs_input": 0.0, "important": 1.0, "redundant_request": 0.0,
            "none": 0.0, "unclear": 0.0
        }});
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn glide_distributions_validate_real_provider_vectors_and_reject_partial_scores() {
        let samples: Value = crate::acp::verdict_client::glide_turn::test_samples();
        for sample in samples.as_array().unwrap() {
            Probabilities::parse(sample).unwrap();
            let mut partial = sample.clone();
            partial["answers"]["input"]["probabilities"]
                .as_object_mut()
                .unwrap()
                .remove("required");
            assert!(Probabilities::parse(&partial).is_err());
            let mut invalid = sample.clone();
            invalid["answers"]["turn_state"]["probabilities"]["finished"] = 2.0.into();
            assert!(Probabilities::parse(&invalid).is_err());
            let mut older_proxy = sample.clone();
            older_proxy["answers"]
                .as_object_mut()
                .unwrap()
                .remove("attention");
            // Hard-won: 4dbf5e62d: a proxy without the attention question failed every verdict.
            let parsed = Probabilities::parse(&older_proxy).unwrap();
            assert!(parsed.attention.is_none());
            let mut partial_attention = sample.clone();
            partial_attention["answers"]["attention"]["probabilities"]
                .as_object_mut()
                .unwrap()
                .remove("important");
            assert!(Probabilities::parse(&partial_attention).is_err());
        }
    }
}
