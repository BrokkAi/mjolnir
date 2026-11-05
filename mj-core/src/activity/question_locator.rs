//! Locate an open request in the authorization history after an input verdict.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::assessment::ContextHistory;

const MAX_REPLIES: usize = 32;
const STATE_BYTES: usize = 60 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reply {
    pub reply: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct State {
    pub latest_user_message: String,
    pub replies: Vec<Reply>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub state: State,
    /// Authorization message IDs in reply order. Live context can carry raw ACP IDs.
    pub message_ids: Vec<String>,
}

/// Resolve an authorization assistant ID to a materialized transcript ID.
///
/// Both the live context and transcript projection use the same relay event
/// ordinal when ACP supplies no message ID. The projection can nevertheless
/// coalesce that chunk into an earlier item, so the numeric fallback is not a
/// reliable target without consulting the materialized transcript.
pub fn transcript_stable_id(id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    if let Some(suffix) = id.strip_prefix("agent:") {
        if suffix.is_empty() || suffix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return Some(id.to_owned());
    }
    Some(format!("agent:{id}"))
}

fn clip(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let head = budget * 2 / 3;
    let tail = budget / 3;
    format!(
        "{} [...] {}",
        &text[..text.floor_char_boundary(head)],
        &text[text.ceil_char_boundary(text.len() - tail)..]
    )
}

pub fn build(history: &ContextHistory) -> Request {
    let last_user = history.messages.iter().rposition(|m| m.role == "user");
    let user = last_user.map_or("", |i| history.messages[i].text.as_str());
    let latest_user_message = &user[user.ceil_char_boundary(user.len().saturating_sub(4096))..];
    let candidates: Vec<_> = history.messages[last_user.map_or(0, |i| i + 1)..]
        .iter()
        .filter(|m| m.role == "assistant")
        .rev()
        .take(MAX_REPLIES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let message_ids = candidates.iter().map(|m| m.id.clone()).collect();
    let mut budget = 3000;
    loop {
        let state = State {
            latest_user_message: latest_user_message.to_owned(),
            replies: candidates
                .iter()
                .enumerate()
                .map(|(reply, m)| Reply {
                    reply,
                    text: clip(&m.text, budget),
                })
                .collect(),
        };
        if serde_json::to_vec(&state)
            .expect("locator state serializes")
            .len()
            <= STATE_BYTES
        {
            return Request { state, message_ids };
        }
        budget = budget * 4 / 5;
    }
}

static WORDING: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("question_locator.json")).expect("bundled locator wording")
});

pub fn questions(count: usize) -> Value {
    let mut criteria = serde_json::Map::new();
    for i in 0..count {
        criteria.insert(
            format!("r{i}"),
            json!({"what":format!("Reply {i} (replies[{i}]) contains the open request.")}),
        );
    }
    criteria.insert("none".into(), json!({"what":WORDING["none"]}));
    json!({"request_reply":{"type":"choice","instructions":WORDING["instructions"],"criteria":criteria}})
}

/// Accept either the direct TypeSafe envelope or the proxy's answer object.
pub fn parse(response: &Value, count: usize) -> Result<Option<usize>> {
    let answer = response
        .get("answers")
        .map_or(response, |v| &v["request_reply"]);
    if let Some(kind) = answer.get("type") {
        ensure!(kind == "choice", "invalid locator answer type");
    }
    let choice = answer["choice"]
        .as_str()
        .context("missing locator choice")?;
    let probabilities = answer["probabilities"]
        .as_object()
        .context("missing locator probabilities")?;
    ensure!(
        probabilities.len() == count + 1 && probabilities.contains_key("none"),
        "incomplete locator probabilities"
    );
    let mut sum = 0.0;
    let mut winner = None;
    for (key, value) in probabilities {
        let valid = key == "none"
            || key
                .strip_prefix('r')
                .and_then(|s| s.parse::<usize>().ok())
                .is_some_and(|i| i < count && format!("r{i}") == *key);
        ensure!(valid, "invalid locator probability key");
        let p = value.as_f64().context("invalid locator probability")?;
        ensure!(
            p.is_finite() && (0.0..=1.0).contains(&p),
            "invalid locator probability"
        );
        sum += p;
        if key == choice {
            winner = Some(p);
        }
    }
    ensure!(
        (sum - 1.0).abs() <= 0.005 * (count + 1) as f64 + 1e-12,
        "invalid locator distribution"
    );
    let winner = winner.context("unknown locator choice")?;
    ensure!(
        probabilities
            .values()
            .all(|p| winner + 1e-12 >= p.as_f64().unwrap_or(2.0)),
        "locator choice is not a probability winner"
    );
    if choice == "none" {
        Ok(None)
    } else {
        Ok(Some(choice[1..].parse()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuation::EvidenceMessage;

    fn message(id: &str, role: &str, text: &str) -> EvidenceMessage {
        EvidenceMessage {
            id: id.into(),
            role: role.into(),
            text: text.into(),
        }
    }

    #[test]
    fn builder_uses_stable_ids_after_last_user_and_utf8_boundaries() {
        let history = ContextHistory {
            messages: vec![
                message("agent:old", "assistant", "old"),
                message("user:last", "user", &"🦀".repeat(1200)),
                message("agent:one", "assistant", &"é".repeat(2000)),
                message("tool:x", "tool", "ignored"),
                message("agent:two", "assistant", "choice"),
            ],
            ..Default::default()
        };
        let request = build(&history);
        assert_eq!(request.message_ids, ["agent:one", "agent:two"]);
        assert!(request.state.latest_user_message.len() <= 4096);
        assert!(request.state.replies[0].text.contains(" [...] "));
        assert_eq!(request.state.replies[1].reply, 1);
    }

    #[test]
    fn live_and_seeded_message_ids_resolve_to_the_same_transcript_item() {
        let mut live = ContextHistory::default();
        live.assistant(Some("msg_011CgihPmwyh6hJGX2Azu5L5"), 42, "Choose?");
        assert_eq!(
            transcript_stable_id(&live.messages[0].id).as_deref(),
            Some("agent:msg_011CgihPmwyh6hJGX2Azu5L5")
        );
        let seeded = message("agent:msg_011CgihPmwyh6hJGX2Azu5L5", "assistant", "Choose?");
        assert_eq!(
            transcript_stable_id(&seeded.id).as_deref(),
            Some("agent:msg_011CgihPmwyh6hJGX2Azu5L5")
        );
        let mut fallback = ContextHistory::default();
        fallback.assistant(None, 42, "Choose?");
        assert_eq!(fallback.messages[0].id, "agent:42");
        assert_eq!(transcript_stable_id(&fallback.messages[0].id), None);
    }

    #[test]
    fn state_shrinks_to_wire_budget_and_retains_newest_replies() {
        let mut history = ContextHistory::default();
        history.messages.push(message("user:u", "user", "task"));
        for i in 0..40 {
            history.messages.push(message(
                &format!("agent:{i}"),
                "assistant",
                &"\\".repeat(6000),
            ));
        }
        let request = build(&history);
        assert_eq!(request.message_ids.len(), 32);
        assert_eq!(request.message_ids[0], "agent:8");
        assert!(serde_json::to_vec(&request.state).unwrap().len() <= STATE_BYTES);
        assert!(request.state.replies[0].text.len() < 3000);
    }

    #[test]
    fn questions_and_probability_winner_are_consistent() {
        for n in [1, 32] {
            assert_eq!(
                questions(n)["request_reply"]["criteria"]
                    .as_object()
                    .unwrap()
                    .len(),
                n + 1
            );
        }
        let answer = json!({"choice":"r0","probabilities":{"r0":0.6,"none":0.4}});
        assert_eq!(parse(&answer, 1).unwrap(), Some(0));
        assert!(
            parse(
                &json!({"choice":"none","probabilities":{"r0":0.6,"none":0.4}}),
                1
            )
            .is_err()
        );
        assert!(
            parse(
                &json!({"choice":"r1","probabilities":{"r0":0.6,"none":0.4}}),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn assessment_field_round_trips_and_old_shape_still_reads() {
        let mut assessment = crate::assessment::TurnAssessment::pending(
            "turn".into(),
            1,
            0,
            crate::activity::verdict::CompletionEvidence {
                stop_reason: "EndTurn".into(),
                diagnostic: None,
            },
        );
        let old = serde_json::to_value(&assessment).unwrap();
        assert!(old.get("question_message").is_none());
        assert_eq!(
            serde_json::from_value::<crate::assessment::TurnAssessment>(old)
                .unwrap()
                .question_message,
            None
        );
        assessment.question_message = Some("agent:reply".into());
        let new = serde_json::to_value(&assessment).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::assessment::TurnAssessment>(new)
                .unwrap()
                .question_message
                .as_deref(),
            Some("agent:reply")
        );
    }
}
