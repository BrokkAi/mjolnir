//! Shared interpretation of the ACP goal and execution extensions.
use agent_client_protocol::schema::v1::SessionUpdate;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROJECTION_KEY: &str = "mj_goal_state";
pub const RECOVERY_ID: &str = "session-goal-recovery";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    pub objective: String,
    pub status: String,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub control_method: Option<String>,
    #[serde(flatten)]
    pub details: std::collections::BTreeMap<String, Value>,
}

impl GoalSnapshot {
    pub fn active(&self) -> bool {
        self.status == "active"
    }
    pub fn same_goal(&self, other: &Self) -> bool {
        self.objective == other.objective && self.created_at == other.created_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalExecution {
    pub version: u32,
    #[serde(default)]
    pub revision: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalDecision {
    pub goal: GoalSnapshot,
    pub resume: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalState {
    #[serde(default)]
    pub known: bool,
    #[serde(default)]
    pub snapshot: Option<GoalSnapshot>,
    #[serde(default)]
    pub execution: Option<GoalExecution>,
    /// The explicit resume request whose question has already been answered.
    #[serde(default)]
    pub answered_resume: Option<String>,
    #[serde(default)]
    pub pending_resume: Option<String>,
    #[serde(default)]
    pub decision: Option<GoalDecision>,
}

impl GoalState {
    pub fn from_configuration(
        configuration: &std::collections::BTreeMap<String, Value>,
    ) -> Result<Self> {
        configuration
            .get(PROJECTION_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .context("decode projected goal state")
            .map(Option::unwrap_or_default)
    }

    pub fn active(&self) -> bool {
        self.snapshot.as_ref().is_some_and(GoalSnapshot::active)
    }
    pub fn running(&self) -> bool {
        self.execution
            .as_ref()
            .is_some_and(|e| e.status == "running")
    }
    pub fn synchronized(&self) -> bool {
        self.known
            && self.execution.is_some()
            && self.snapshot.as_ref().is_none_or(|g| {
                matches!(
                    g.status.as_str(),
                    "active" | "paused" | "blocked" | "complete" | "limited"
                )
            })
    }
    pub fn restart(&mut self) {
        self.known = false;
        self.execution = None;
    }

    /// Missing fields are unchanged; explicit null clears a goal.
    pub fn apply(&mut self, update: &SessionUpdate) -> Result<bool> {
        let SessionUpdate::SessionInfoUpdate(info) = update else {
            return Ok(false);
        };
        let Some(meta) = &info.meta else {
            return Ok(false);
        };
        let mut changed = false;
        if let Some(goal) = meta.get("goal") {
            let was_active = self.active();
            self.snapshot =
                serde_json::from_value(goal.clone()).context("decode ACP goal state")?;
            // A later pause or replacement supersedes an unacknowledged resume.
            if let Some(decision) = &self.decision {
                let current = self.snapshot.as_ref();
                let still_current = current.is_some_and(|goal| {
                    goal.same_goal(&decision.goal)
                        && matches!(goal.status.as_str(), "active" | "paused")
                });
                let later_pause = decision.resume
                    && current.is_some_and(|goal| {
                        goal.status == "paused"
                            && (was_active
                                || goal
                                    .details
                                    .get("updatedAt")
                                    .and_then(Value::as_i64)
                                    .zip(
                                        decision
                                            .goal
                                            .details
                                            .get("updatedAt")
                                            .and_then(Value::as_i64),
                                    )
                                    .is_some_and(|(new, old)| new > old))
                    });
                if !still_current || later_pause {
                    self.decision = None;
                }
            }
            self.known = true;
            changed = true;
        }
        if let Some(execution) = meta.get("execution") {
            let execution: GoalExecution =
                serde_json::from_value(execution.clone()).context("decode ACP execution state")?;
            ensure!(
                execution.version == 1 && matches!(execution.status.as_str(), "running" | "idle"),
                "unsupported ACP execution state"
            );
            if self
                .execution
                .as_ref()
                .and_then(|e| e.revision)
                .zip(execution.revision)
                .is_some_and(|(old, new)| new < old)
            {
                return Ok(changed);
            }
            // Ignore an old completion that arrived after a newer native turn.
            if execution.status == "running"
                || !self.running()
                || self
                    .execution
                    .as_ref()
                    .and_then(|e| e.turn_id.as_ref())
                    .is_none()
                || execution.turn_id.is_none()
                || self.execution.as_ref().and_then(|e| e.turn_id.as_ref())
                    == execution.turn_id.as_ref()
            {
                self.execution = Some(execution);
            }
            changed = true;
        }
        if let Some(request) = meta.get("mjGoalResumePending").and_then(Value::as_str) {
            self.pending_resume = Some(request.to_owned());
            changed = true;
        }
        if let Some(request) = meta.get("mjGoalResumeAnswered").and_then(Value::as_str) {
            self.answered_resume = Some(request.to_owned());
            self.pending_resume = None;
            changed = true;
        }
        if let Some(decision) = meta.get("mjGoalDecision") {
            self.decision =
                serde_json::from_value(decision.clone()).context("decode goal decision")?;
            changed = true;
        }
        Ok(changed)
    }
}

/// Persist a recovery decision before sending its native control action.
#[derive(Clone)]
pub struct GoalJournal(pub std::sync::Arc<dyn Fn(SessionUpdate) -> Result<()> + Send + Sync>);
impl std::fmt::Debug for GoalJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GoalJournal")
    }
}

#[derive(Debug, Clone, Default)]
pub struct GoalRecoveryContext {
    pub state: GoalState,
    pub request: Option<String>,
    pub journal: Option<GoalJournal>,
}
impl GoalRecoveryContext {
    pub fn asking(&self) -> bool {
        self.state.pending_resume.is_some()
            || self
                .request
                .as_ref()
                .is_some_and(|r| self.state.answered_resume.as_ref() != Some(r))
    }
    pub fn request_id(&self) -> Option<&str> {
        self.state
            .pending_resume
            .as_deref()
            .or(self.request.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn update(meta: Value) -> SessionUpdate {
        serde_json::from_value(
            serde_json::json!({"sessionUpdate":"session_info_update","_meta":meta}),
        )
        .unwrap()
    }
    #[test]
    fn goal_metadata_survives_prompt_boundaries_and_distinguishes_missing_from_clear() {
        let mut state = GoalState::default();
        state.apply(&update(serde_json::json!({"goal":{"objective":"finish","status":"active","createdAt":12,"tokensUsed":42,"tokenBudget":100}}))).unwrap();
        assert!(state.active());
        state
            .apply(&update(
                serde_json::json!({"execution":{"version":1,"status":"running","turnId":"next"}}),
            ))
            .unwrap();
        state
            .apply(&update(
                serde_json::json!({"execution":{"version":1,"status":"idle","turnId":"old"}}),
            ))
            .unwrap();
        assert!(state.running());
        assert!(state.synchronized());
        state.restart();
        assert!(state.active());
        assert!(!state.synchronized());
        assert_eq!(state.snapshot.as_ref().unwrap().details["tokensUsed"], 42);
        state
            .apply(&update(serde_json::json!({"goal":null})))
            .unwrap();
        assert!(state.snapshot.is_none());
    }
    #[test]
    fn a_later_pause_supersedes_a_saved_resume_decision() {
        let goal: GoalSnapshot = serde_json::from_value(serde_json::json!({"objective":"finish","status":"paused","createdAt":1,"updatedAt":10})).unwrap();
        let mut state = GoalState {
            snapshot: Some(goal.clone()),
            decision: Some(GoalDecision { goal, resume: true }),
            ..Default::default()
        };
        state.restart();
        state.apply(&update(serde_json::json!({"goal":{"objective":"finish","status":"paused","createdAt":1,"updatedAt":10}}))).unwrap();
        assert!(
            state.decision.is_some(),
            "the original pre-resume pause retains the accepted decision"
        );
        state.apply(&update(serde_json::json!({"goal":{"objective":"finish","status":"paused","createdAt":1,"updatedAt":11}}))).unwrap();
        assert!(
            state.decision.is_none(),
            "a newer pause must not be undone during recovery"
        );
    }

    #[test]
    fn explicit_resume_decision_survives_a_restart() {
        let mut context = GoalRecoveryContext {
            request: Some("launch-1".into()),
            ..Default::default()
        };
        assert!(context.asking());
        context
            .state
            .apply(&update(
                serde_json::json!({"mjGoalResumePending":"launch-1"}),
            ))
            .unwrap();
        context.request = None;
        context.state.restart();
        assert!(context.asking());
        context
            .state
            .apply(&update(
                serde_json::json!({"mjGoalResumeAnswered":"launch-1"}),
            ))
            .unwrap();
        context.state.restart();
        assert!(!context.asking());
    }
}
