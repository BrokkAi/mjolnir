//! The sole person-facing classification, applied at the common runtime publication.
use mj_client::runtime_feed::RuntimeProjection;
use mj_core::attention::{AssessmentAttention, SessionAttention};
use mj_core::review::{driver::TurnReviewPhase, verdict::ReviewVerdict};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn project(runtime: &RuntimeProjection) -> BTreeMap<String, SessionAttention> {
    use SessionAttention::*;
    let mut questions = BTreeSet::new();
    // Native children never receive semantic assessments. Only an actual
    // harness form reaches their owner and its managed ancestors.
    for native in runtime.native_agents.values().filter(|native| {
        native.pending_input
            && native.agent.state == mj_core::native_agent::NativeAgentState::Running
    }) {
        let mut current = native.agent.owner_session_id.as_str();
        let mut visited = BTreeSet::new();
        while visited.insert(current.to_owned())
            && runtime
                .records
                .get(current)
                .is_some_and(|record| record.state.is_active())
        {
            questions.insert(current.to_owned());
            let Some(parent) = runtime.subagents.get(current) else {
                break;
            };
            current = &parent.parent_session_id;
        }
    }
    for (id, record) in &runtime.records {
        if record.state.is_active()
            && (runtime
                .transcripts
                .get(id)
                .is_some_and(|tail| !tail.header.pending_elicitations.is_empty())
                || runtime
                    .metadata
                    .reviews
                    .iter()
                    .find(|review| review.session_id == *id)
                    .is_some_and(|review| !review.questions.is_empty()))
        {
            let mut current = id.as_str();
            let mut visited = BTreeSet::new();
            while visited.insert(current.to_owned()) {
                questions.insert(current.to_owned());
                let Some(parent) = runtime.subagents.get(current) else {
                    break;
                };
                current = &parent.parent_session_id;
                if !runtime
                    .records
                    .get(current)
                    .is_some_and(|record| record.state.is_active())
                {
                    break;
                }
            }
        }
    }
    let mut attention: BTreeMap<_, _> = runtime
        .records
        .iter()
        .map(|(id, record)| {
            let view = runtime.sessions.get(id);
            let live = view.and_then(|view| view.operational.as_deref());
            let review = runtime
                .metadata
                .reviews
                .iter()
                .find(|review| review.session_id == *id);
            let transitioning = record.state.transition_kind().is_some()
                || runtime
                    .metadata
                    .lifecycles
                    .iter()
                    .any(|operation| operation.session_id == *id);
            let state = if record.last_error.is_some()
                || matches!(
                    record.state,
                    mj_core::state::SessionState::Lost
                        | mj_core::state::SessionState::Error
                        | mj_core::state::SessionState::DestroyedWithDataLoss
                )
                || view.is_some_and(|view| {
                    matches!(
                        view.error,
                        Some(
                            mj_client::session::ViewError::TargetMissing(_)
                                | mj_client::session::ViewError::ProjectionIntegrity(_)
                        )
                    )
                }) {
                Failed
            } else if transitioning || record.state == mj_core::state::SessionState::Checkpointing {
                Working
            } else if !record.state.is_active() {
                Inactive
            } else if record.state == mj_core::state::SessionState::Parked {
                if questions.contains(id) {
                    NeedsInput
                } else {
                    Idle
                }
            } else if record.state == mj_core::state::SessionState::Disconnected
                || !view.is_some_and(|view| view.connected)
                || view.is_some_and(|view| {
                    matches!(
                        view.error,
                        Some(mj_client::session::ViewError::Unreachable(_))
                    )
                })
            {
                Unreachable
            } else if review.is_some_and(|review| {
                matches!(
                    review.phase,
                    TurnReviewPhase::Verdict(ReviewVerdict::Failed { .. })
                )
            }) {
                Failed
            } else if questions.contains(id) {
                NeedsInput
            } else if let Some(review) =
                review.filter(|review| !matches!(review.phase, TurnReviewPhase::Resolved(_)))
            {
                match review.phase {
                    TurnReviewPhase::Verdict(ReviewVerdict::Findings { .. }) => NeedsInput,
                    TurnReviewPhase::Verdict(ReviewVerdict::Clean) => Idle,
                    _ => Working,
                }
            } else if let Some(live) = live {
                let activity = live.activity_state();
                if activity.is_idle() {
                    match live
                        .assessment
                        .as_deref()
                        .filter(|assessment| assessment.current())
                    {
                        Some(assessment) if assessment.inferred_question().is_some() => NeedsInput,
                        Some(assessment)
                            if assessment.attention == Some(AssessmentAttention::Important) =>
                        {
                            Important
                        }
                        _ => Idle,
                    }
                } else {
                    Working
                }
            } else {
                Unknown
            };
            (id.clone(), state)
        })
        .collect();
    for (id, native) in &runtime.native_agents {
        use mj_core::native_agent::NativeAgentState;
        let state = match native.agent.state {
            NativeAgentState::Failed => Failed,
            NativeAgentState::Disconnected => Unreachable,
            NativeAgentState::Completed | NativeAgentState::Cancelled => Idle,
            NativeAgentState::Running if native.pending_input => NeedsInput,
            NativeAgentState::Running => Working,
        };
        attention.insert(id.clone(), state);
    }
    attention
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MaterializedSession, ProjectionWindow, SessionState};

    #[test]
    fn native_forms_promote_the_owner_and_finished_children_are_idle() {
        use mj_core::native_agent::*;
        let mut runtime = runtime();
        let agent = NativeAgent {
            owner_session_id: "parent".into(),
            session_id: "native".into(),
            parent_session_id: None,
            name: "Inspect".into(),
            task: "Inspect files".into(),
            capabilities: Default::default(),
            state: NativeAgentState::Running,
            availability: Default::default(),
            availability_reason: None,
            stable_id: None,
        };
        let id = agent.view_id();
        runtime.native_agents.insert(
            id.clone(),
            NativeAgentSummary {
                generation_ordinal: 1,
                agent,
                projection_ordinal: 0,
                projection_digest: String::new(),
                pending_input: true,
            },
        );
        let attention = project(&runtime);
        assert_eq!(attention[&id], SessionAttention::NeedsInput);
        assert_eq!(attention["parent"], SessionAttention::NeedsInput);
        let native = runtime.native_agents.get_mut(&id).unwrap();
        native.agent.state = NativeAgentState::Completed;
        native.pending_input = false;
        assert_eq!(project(&runtime)[&id], SessionAttention::Idle);
        assert_ne!(project(&runtime)["parent"], SessionAttention::NeedsInput);
    }

    fn runtime() -> RuntimeProjection {
        let mut runtime = RuntimeProjection::default();
        for id in ["parent", "child"] {
            runtime.records.insert(
                id.into(),
                super::super::tests::runtime_test_session(id, "workspace", SessionState::Running),
            );
            runtime.sessions.insert(
                id.into(),
                mj_client::daemon::RuntimeSessionView::from_managed(
                    id.into(),
                    mj_client::session::ManagedSessionView {
                        connected: true,
                        ..Default::default()
                    },
                ),
            );
        }
        runtime
    }

    #[test]
    fn attention_failure_is_distinct_from_connection_loss_and_reviews_override_questions() {
        let mut runtime = runtime();
        let view = runtime.sessions.get_mut("parent").unwrap();
        view.connected = false;
        view.error = Some(mj_client::session::ViewError::Unreachable("offline".into()));
        assert_eq!(project(&runtime)["parent"], SessionAttention::Unreachable);
        runtime.records.get_mut("parent").unwrap().state = SessionState::Error;
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        runtime.records.get_mut("parent").unwrap().state = SessionState::Running;
        runtime.sessions.get_mut("parent").unwrap().connected = true;
        runtime.sessions.get_mut("parent").unwrap().error = None;
        runtime
            .metadata
            .reviews
            .push(mj_client::review::RuntimeReviewView {
                child_session_id: None,
                session_id: "parent".into(),
                phase: TurnReviewPhase::Verdict(ReviewVerdict::Clean),
                roles: vec![],
                status: String::new(),
                verdict: None,
                questions: vec![],
            });
        assert_eq!(project(&runtime)["parent"], SessionAttention::Idle);
        runtime.metadata.reviews[0].phase = TurnReviewPhase::CapturingDelta;
        assert_eq!(project(&runtime)["parent"], SessionAttention::Working);
    }

    #[test]
    fn attention_descendant_question_propagates_clears_and_terminates_on_cycles() {
        let mut runtime = runtime();
        for (child, parent) in [("child", "parent"), ("parent", "child")] {
            runtime.subagents.insert(
                child.into(),
                mj_core::subagent::SubagentMetadata {
                    kind: Default::default(),
                    child_session_id: child.into(),
                    parent_session_id: parent.into(),
                    task_name: String::new(),
                    profile_id: String::new(),
                    model: None,
                    effort: None,
                    working_directory: Default::default(),
                    request_key: String::new(),
                    created_at: String::new(),
                    noticed_turn: None,
                    reported_finish: None,
                    handback_tool: false,
                },
            );
        }
        let mut materialized = MaterializedSession::empty("child");
        materialized
            .pending_elicitations
            .push(mj_core::elicitation::ElicitationRequest {
                id: "question".into(),
                message: "Which?".into(),
                title: None,
                description: None,
                fields: vec![],
                tool_permission: None,
            });
        let window = ProjectionWindow::of(&materialized);
        runtime.transcripts.insert(
            "child".into(),
            mj_client::runtime_feed::SessionTail::of(&materialized, &window, 10),
        );
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        runtime
            .metadata
            .reviews
            .push(mj_client::review::RuntimeReviewView {
                child_session_id: None,
                session_id: "parent".into(),
                phase: TurnReviewPhase::Verdict(ReviewVerdict::Failed {
                    reason: "review failed".into(),
                }),
                roles: vec![],
                status: String::new(),
                verdict: None,
                questions: vec![],
            });
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        runtime.metadata.reviews[0].phase = TurnReviewPhase::Verdict(ReviewVerdict::Findings {
            synthesis: "findings".into(),
            evidence: Default::default(),
        });
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        runtime.metadata.reviews.clear();
        materialized.pending_elicitations.clear();
        runtime.transcripts.insert(
            "child".into(),
            mj_client::runtime_feed::SessionTail::of(&materialized, &window, 10),
        );
        assert_eq!(project(&runtime)["parent"], SessionAttention::Unknown);
    }
}
