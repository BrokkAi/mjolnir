//! Shared turn-review requests, status and recovery data.

use super::lanes::UserMessage;
use super::verdict::ReviewVerdict;
use crate::relay::RepoDelta;
use crate::subagent::ReviewOverride;

/// What the driver needs the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewRequest {
    /// Ask the worker to capture pushable commits.
    CaptureDelta,
    /// Register a fresh reviewer child with this initial prompt.
    StartReviewer {
        role: String,
        command_id: String,
        prompt: String,
    },
    /// Close the reviewer child.
    CloseReviewer { role: String },
    /// The review is over; close its pane.
    Close,
}

/// Which stage of a review one role is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleState {
    Pending,
    Running,
    Clean,
    Findings,
    Failed,
}

impl RoleState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Clean => "done",
            Self::Findings => "findings",
            Self::Failed => "failed",
        }
    }
}

/// One reviewing agent's row in the review pane. It crosses the daemon's
/// snapshot to every surface, so it serializes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleStatus {
    pub role: String,
    pub label: String,
    pub state: RoleState,
    /// The host projects its authoritative registration onto this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
}

/// How a review ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Delivery completed. Keep the terminal tag older clients understand.
    #[serde(rename = "dismissed")]
    Completed,
    /// The user stopped the review; its commits remain unreviewed.
    Cancelled,
}

/// Where the review has got to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TurnReviewPhase {
    /// Asking the worker for committed work.
    CapturingDelta,
    /// Staging and starting the first reviewing agent.
    LaunchingReviewer,
    /// One or more reviewing agents are working.
    Running {
        roles: Vec<RoleStatus>,
    },
    /// The verdict is being delivered to the agent.
    Verdict(ReviewVerdict),
    Resolved(Resolution),
}

/// The legacy sole-reviewer role, retained for old review records.
pub const REVIEWER_ROLE: &str = "reviewer";

/// How every command a turn review sends a reviewing role begins, so a
/// reviewer's running prompt says which kind of review it belongs to.
pub const COMMAND_ID_PREFIX: &str = "turn-review-";

/// Everything about the reviewed turn that is known before the capture lands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct TurnReviewSeed {
    /// The latest real user prompt; earlier requirements remain in the history.
    pub task: String,
    /// All real user messages in chronological order, excluding harness notes.
    pub user_messages: Vec<UserMessage>,
    /// Findings the author declined to fix, for the reviewer prompt. An older
    /// snapshot has none.
    #[serde(default)]
    pub declined: Vec<ReviewOverride>,
}

/// One frozen reviewer slot. Its verdict is the only terminal-state fact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewerOutcomeSlot {
    pub role: String,
    pub command_id: String,
    pub outcome: Option<ReviewVerdict>,
}

/// Durable pure-driver state. Child registration and live identity remain host-owned.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ReviewDriverSnapshot {
    pub review_id: String,
    pub seed: TurnReviewSeed,
    pub phase: TurnReviewPhase,
    pub last_verdict: Option<ReviewVerdict>,
    pub deltas: Vec<RepoDelta>,
    pub roles: Vec<ReviewerOutcomeSlot>,
    pub sequence: u64,
    /// Exact requests emitted by the current stage, before any side effect.
    #[serde(default, deserialize_with = "deserialize_replay_requests")]
    pub replay_requests: Vec<ReviewRequest>,
}

fn deserialize_replay_requests<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<ReviewRequest>, D::Error> {
    let values = <Vec<serde_json::Value> as serde::Deserialize>::deserialize(d)?;
    values
        .into_iter()
        .filter(|v| {
            let kind = v.as_str().or_else(|| {
                v.as_object()
                    .and_then(|m| m.keys().next().map(String::as_str))
            });
            matches!(
                kind,
                Some("CaptureDelta" | "StartReviewer" | "CloseReviewer" | "Close")
            )
        })
        .map(|v| {
            if v.get("CaptureDelta").is_some() {
                Ok(ReviewRequest::CaptureDelta)
            } else {
                serde_json::from_value(v).map_err(serde::de::Error::custom)
            }
        })
        .collect()
}

// Keep old snapshot readers working while the runtime carries no turn state.
impl serde::Serialize for TurnReviewSeed {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("TurnReviewSeed", 5)?;
        value.serialize_field("task", &self.task)?;
        value.serialize_field("user_messages", &self.user_messages)?;
        value.serialize_field("declined", &self.declined)?;
        value.serialize_field(
            "baselines",
            &std::collections::BTreeMap::<String, String>::new(),
        )?;
        value.serialize_field("through_ordinal", &0_u64)?;
        value.end()
    }
}
impl serde::Serialize for ReviewDriverSnapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("ReviewDriverSnapshot", 9)?;
        value.serialize_field("review_id", &self.review_id)?;
        value.serialize_field("seed", &self.seed)?;
        value.serialize_field("phase", &self.phase)?;
        value.serialize_field("last_verdict", &self.last_verdict)?;
        value.serialize_field("deltas", &self.deltas)?;
        value.serialize_field("roles", &self.roles)?;
        value.serialize_field("provenance", "independent_reviewers")?;
        value.serialize_field("sequence", &self.sequence)?;
        let requests: Vec<_> = self
            .replay_requests
            .iter()
            .map(|r| match r {
                ReviewRequest::CaptureDelta => serde_json::json!({"CaptureDelta":{"baselines":{}}}),
                other => serde_json::to_value(other).expect("review requests contain JSON values"),
            })
            .collect();
        value.serialize_field("replay_requests", &requests)?;
        value.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_push_snapshots_skip_retired_replay_and_keep_live_requests_strict() {
        let mut stored = serde_json::json!({
            "review_id":"push-1", "seed":{"task":"review commits","user_messages":[],"baselines":{},"through_ordinal":12},
            "phase":{"phase":"capturing_delta"}, "last_verdict":null,
            "deltas":[], "roles":[], "provenance":"single_reviewer", "sequence":0,
            "replay_requests":[{"CaptureDelta":{"baselines":{}}},{"AdvanceBaseline":{"trees":{},"reviewed_through_ordinal":12}},"ClearPriorReview",{"PromptPrimary":{"command_id":"old","prompt":"old"}},"Close"]
        });
        let decoded: ReviewDriverSnapshot = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(
            decoded.replay_requests,
            vec![ReviewRequest::CaptureDelta, ReviewRequest::Close]
        );
        let saved = serde_json::to_value(decoded).unwrap();
        assert_eq!(saved["seed"]["baselines"], serde_json::json!({}));
        assert_eq!(saved["seed"]["through_ordinal"], 0);
        assert_eq!(saved["provenance"], "independent_reviewers");
        assert_eq!(
            saved["replay_requests"][0],
            serde_json::json!({"CaptureDelta":{"baselines":{}}})
        );
        stored["replay_requests"] = serde_json::json!([{"StartReviewer":{}}]);
        assert!(serde_json::from_value::<ReviewDriverSnapshot>(stored).is_err());
    }
}
