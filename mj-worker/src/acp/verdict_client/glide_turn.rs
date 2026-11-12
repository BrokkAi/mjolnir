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
// Hosted route allowance, measured evaluation bodies reached approximately 79 KB.
const REQUEST_BYTES: usize = 80 * 1024;

pub(crate) fn project(
    evidence: &TurnEvidence,
    mut history: ContextHistory,
    facts: &ActivityFacts,
    native_agent_count: usize,
) -> Result<Value> {
    ensure!(
        evidence.phase == TurnPhase::Replied,
        "GLiDE requires a completed turn"
    );
    let mut base = evidence.clone();
    base.transcript_summary.clear();
    loop {
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
        if serde_json::to_vec(&body)?.len() <= REQUEST_BYTES {
            return Ok(body);
        }
        // Drop oldest history first, explicitly recording lost authorization.
        // Keep the latest user and all its replies until older context is gone.
        if latest.is_some_and(|i| i > 0) {
            let removed = history.messages.remove(0);
            if removed.role == "user" {
                history.authorization_complete = false;
            } else {
                history.assistant_history_omitted = true;
            }
        } else if let Some(oldest) = history
            .messages
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= latest.map_or(0, |i| i + 1) && m.role == "assistant")
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
            .split_last()
            .and_then(|(_, older)| older.first().copied())
        {
            history.messages.remove(oldest);
            history.assistant_history_omitted = true;
        } else {
            anyhow::bail!("current GLiDE user message and final reply exceed 80 KiB");
        }
    }
}

pub(crate) use mj_core::activity::eot_glide::types::Probabilities;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn glide_distributions_validate_real_provider_vectors_and_reject_partial_scores() {
        let samples: Value = serde_json::from_str(include_str!(
            "../../../../mj-core/src/activity/eot_glide_fresh_sample.json"
        ))
        .unwrap();
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
        }
    }
}
