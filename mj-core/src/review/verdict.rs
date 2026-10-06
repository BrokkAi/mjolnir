//! What a review concluded, and how a reviewing agent's reply is classified.
//!
//! The durable verdict and compatibility evidence shared by review drivers,
//! storage and client projections.

use serde::{Deserialize, Serialize};

/// What one review concluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// Findings the user can forward to the primary agent.
    Findings {
        synthesis: String,
        #[serde(default)]
        evidence: ReviewPassEvidence,
    },
    /// Nothing material was found; the review releases the turn itself.
    Clean,
    /// The review could not reach a verdict. It never advances the baseline,
    /// so the same change is reviewed again next time.
    Failed { reason: String },
}

impl ReviewVerdict {
    /// Whether this verdict resolves the review with nothing to forward.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        matches!(self, Self::Clean)
    }
}

/// Legacy lane evidence retained so persisted pending forwards still decode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewPassEvidence {
    /// Specialist lane outcomes from older review records.
    pub lanes: Vec<ReviewLaneEvidence>,
}

/// A specialist lane outcome from a legacy review record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLaneEvidence {
    pub id: String,
    pub outcome: LaneOutcome,
}

/// A legacy lane's terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LaneOutcome {
    Completed,
    Cancelled,
    Failed { reason: String },
}

impl LaneOutcome {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Completed => "completed".to_string(),
            Self::Cancelled => "cancelled".to_string(),
            Self::Failed { reason } => format!("failed: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reviews persisted while the extended tier still ran an intent analyst
    /// carry its brief in their evidence. Those fields are no longer read, and
    /// the stored record still loads.
    #[test]
    fn evidence_stored_with_an_intent_brief_still_loads() {
        let lanes = serde_json::to_value(vec![ReviewLaneEvidence {
            id: "tests".to_string(),
            outcome: LaneOutcome::Completed,
        }])
        .unwrap();
        let stored = serde_json::json!({
            "intent_brief": "Goal: add a retry",
            "intent_available": true,
            "lanes": lanes,
        });
        let evidence: ReviewPassEvidence = serde_json::from_value(stored).unwrap();
        assert_eq!(evidence.lanes.len(), 1);
        assert_eq!(evidence.lanes[0].id, "tests");
        assert_eq!(evidence.lanes[0].outcome, LaneOutcome::Completed);
    }
}
