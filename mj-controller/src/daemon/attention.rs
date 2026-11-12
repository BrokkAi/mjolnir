//! The sole person-facing classification, applied at the common runtime publication.
use mj_client::runtime_feed::RuntimeProjection;
use mj_core::attention::{AssessmentAttention, SessionAttention};
use mj_core::review::{driver::TurnReviewPhase, verdict::ReviewVerdict};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
pub(super) fn project(runtime: &RuntimeProjection) -> BTreeMap<String, SessionAttention> {
    project_ids(
        runtime,
        runtime
            .records
            .keys()
            .chain(runtime.native_agents.keys())
            .cloned()
            .collect(),
    )
}

/// Only publication calls this rule. Raw owner views are compared before attention
/// is overlaid, so the overlay cannot make an unchanged input appear dirty.
pub(super) fn update(previous: &RuntimeProjection, next: &mut RuntimeProjection) {
    let global = previous.metadata.config != next.metadata.config
        || previous.metadata.reviews != next.metadata.reviews
        || previous.metadata.lifecycles != next.metadata.lifecycles
        || previous.subagents != next.subagents;
    let mut ids: BTreeSet<String> = if global {
        previous
            .records
            .keys()
            .chain(next.records.keys())
            .chain(previous.native_agents.keys())
            .chain(next.native_agents.keys())
            .cloned()
            .collect()
    } else {
        previous
            .records
            .changes(&next.records)
            .map(|(id, _)| id.clone())
            .chain(
                previous
                    .sessions
                    .changes(&next.sessions)
                    .map(|(id, _)| id.clone()),
            )
            .chain(
                previous
                    .transcripts
                    .changes(&next.transcripts)
                    .map(|(id, _)| id.clone()),
            )
            .chain(
                previous
                    .native_agents
                    .changes(&next.native_agents)
                    .map(|(id, _)| id.clone()),
            )
            .collect()
    };
    if ids.is_empty() {
        next.metadata.session_attention = previous.metadata.session_attention.clone();
        return;
    }
    for id in ids.clone() {
        for runtime in [previous, &*next] {
            let mut current = runtime
                .native_agents
                .get(&id)
                .map_or(id.as_str(), |native| native.agent.owner_session_id.as_str());
            let mut seen = BTreeSet::new();
            while seen.insert(current) {
                ids.insert(current.to_owned());
                let Some(parent) = runtime.subagents.get(current) else {
                    break;
                };
                current = &parent.parent_session_id;
            }
        }
    }
    let states = project_ids(next, ids.clone());
    next.metadata.session_attention = previous.metadata.session_attention.clone();
    for id in ids {
        if let Some(state) = states.get(&id) {
            if next.metadata.session_attention.get(&id) != Some(state) {
                next.metadata.session_attention.insert(id, *state);
            }
        } else {
            next.metadata.session_attention.remove(&id);
        }
    }
}

fn project_ids(
    runtime: &RuntimeProjection,
    ids: BTreeSet<String>,
) -> BTreeMap<String, SessionAttention> {
    use SessionAttention::*;
    let mut reviews = BTreeMap::new();
    for review in &runtime.metadata.reviews {
        reviews.entry(review.session_id.as_str()).or_insert(review);
    }
    let lifecycles: BTreeSet<_> = runtime
        .metadata
        .lifecycles
        .iter()
        .map(|operation| operation.session_id.as_str())
        .collect();
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
    for id in runtime
        .transcripts
        .iter()
        .filter(|(_, tail)| !tail.header.pending_elicitations.is_empty())
        .map(|(id, _)| id)
        .chain(
            reviews
                .values()
                .filter(|review| !review.questions.is_empty())
                .map(|review| &review.session_id),
        )
    {
        if runtime
            .records
            .get(id)
            .is_some_and(|record| record.state.is_active())
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
    let mut attention: BTreeMap<_, _> = ids
        .iter()
        .filter_map(|id| runtime.records.get(id).map(|record| (id, record)))
        .map(|(id, record)| {
            let view = runtime.sessions.get(id);
            let live = view.and_then(|view| view.operational.as_deref());
            let review = reviews.get(id.as_str()).copied();
            let transitioning =
                record.state.transition_kind().is_some() || lifecycles.contains(id.as_str());
            let state = if record
                .configuration_issue(&runtime.metadata.config)
                .is_some()
                || record.last_error.is_some()
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
                    let tail = runtime.transcripts.get(id);
                    let assessment = live.assessment.as_deref();
                    if tail.is_some_and(|tail| {
                        tail.header
                            .unanswered_input_outcome(assessment, tail.latest_user_position)
                            .is_some()
                    }) {
                        return (id.clone(), NeedsInput);
                    }
                    match live.assessment.as_deref().filter(|assessment| {
                        tail.and_then(|tail| tail.header.last_turn_outcome.as_ref())
                            .is_some_and(|outcome| assessment.applies_to(outcome))
                    }) {
                        Some(assessment)
                            if assessment.attention == Some(AssessmentAttention::Important)
                                && record.viewed_through_event_ordinal
                                    < assessment.completed_ordinal =>
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
    for (id, native) in ids
        .iter()
        .filter_map(|id| runtime.native_agents.get(id).map(|native| (id, native)))
    {
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

    #[test]
    fn incremental_publications_match_full_projection_through_mixed_changes() {
        let mut published = RuntimeProjection::default();
        let mut next = runtime();
        next.records
            .insert("unrelated".into(), next.records["parent"].clone());
        next.subagents.insert(
            "child".into(),
            mj_core::subagent::SubagentMetadata {
                kind: Default::default(),
                child_session_id: "child".into(),
                parent_session_id: "parent".into(),
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
        for step in 0..144 {
            match step % 12 {
                0 => next.records.get_mut("child").unwrap().state = SessionState::Error,
                1 => next.records.get_mut("child").unwrap().state = SessionState::Running,
                2 => {
                    let mut materialized = MaterializedSession::empty("child");
                    materialized.pending_elicitations.push(
                        mj_core::elicitation::ElicitationRequest {
                            id: "question".into(),
                            message: "Which?".into(),
                            title: None,
                            description: None,
                            fields: vec![],
                            tool_permission: None,
                        },
                    );
                    next.transcripts.insert(
                        "child".into(),
                        mj_client::runtime_feed::SessionTail::of(
                            &materialized,
                            &ProjectionWindow::of(&materialized),
                            10,
                        ),
                    );
                }
                3 => {
                    next.transcripts.remove("child");
                }
                4 => {
                    let mut config = (*next.metadata.config).clone();
                    config.profiles.remove("codex");
                    next.metadata.config = config.into();
                }
                5 => next.metadata.config = super::super::tests::runtime_test_config().into(),
                6 => {
                    next.sessions.get_mut("child").unwrap().connected = false;
                }
                7 => {
                    next.sessions.get_mut("child").unwrap().connected = true;
                }
                8 => {
                    next.records.remove("unrelated");
                }
                9 => {
                    next.records
                        .insert("unrelated".into(), next.records["parent"].clone());
                }
                10 => next
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
                    }),
                _ => next.metadata.reviews.clear(),
            }
            update(&published, &mut next);
            let expected = project(&next);
            assert_eq!(
                next.metadata
                    .session_attention
                    .iter()
                    .collect::<BTreeMap<_, _>>(),
                expected.iter().collect::<BTreeMap<_, _>>(),
                "step {step}"
            );
            if step % 12 == 2 || step % 12 == 3 {
                let changed: BTreeSet<_> = published
                    .metadata
                    .session_attention
                    .changes(&next.metadata.session_attention)
                    .map(|(id, _)| id.as_str())
                    .collect();
                assert_eq!(changed, BTreeSet::from(["child", "parent"]));
                assert!(std::ptr::eq(
                    published
                        .metadata
                        .session_attention
                        .get("unrelated")
                        .unwrap(),
                    next.metadata.session_attention.get("unrelated").unwrap()
                ));
            }
            published = next.clone();
        }
    }

    fn runtime() -> RuntimeProjection {
        let mut runtime = RuntimeProjection::default();
        runtime.metadata.config = super::super::tests::runtime_test_config().into();
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

    // Hard-won: e569a20c5: unassessed harness questions disappeared and answered questions lingered.
    #[test]
    fn attention_uses_the_unanswered_latest_outcome_even_when_the_answer_is_trimmed() {
        use mj_core::assessment::{Action, Status, TurnAssessment};
        use mj_core::state::{
            MaterializedTurnOutcome, TranscriptBody, TranscriptItem, TurnOutcomeKind,
        };
        let mut runtime = runtime();
        let mut relay = mj_core::relay::RelaySnapshot::new("parent".into());
        relay.execution = mj_core::relay::RelayExecutionState::Idle;
        let mut materialized = MaterializedSession::empty("parent");
        materialized.last_turn_outcome = Some(MaterializedTurnOutcome {
            command_id: "turn".into(),
            accepted_ordinal: None,
            turn_start_position: None,
            completed_ordinal: 10,
            completed_at_ms: 10,
            outcome: TurnOutcomeKind::Completed {
                stop_reason: "AwaitingInput".into(),
            },
            answered_through: None,
            diagnostic: None,
            usage: None,
            work_facts: None,
        });
        let publish = |runtime: &mut RuntimeProjection,
                       materialized: &MaterializedSession,
                       relay: &mj_core::relay::RelaySnapshot| {
            runtime.sessions.get_mut("parent").unwrap().operational =
                Some(relay.operational_state().into());
            let window = ProjectionWindow::of(materialized);
            if let Some(tail) = runtime.transcripts.get_mut("parent") {
                tail.publish(materialized, &window, 0);
            } else {
                runtime.transcripts.insert(
                    "parent".into(),
                    mj_client::runtime_feed::SessionTail::of(materialized, &window, 0),
                );
            }
        };
        publish(&mut runtime, &materialized, &relay);
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        let mut assessment = TurnAssessment::pending(
            "turn".into(),
            10,
            10,
            mj_core::activity::verdict::CompletionEvidence {
                stop_reason: "EndTurn".into(),
                diagnostic: None,
            },
        );
        assessment.status = Status::Assessed;
        assessment.action = Some(Action::AwaitInput);
        materialized.last_turn_outcome.as_mut().unwrap().outcome = TurnOutcomeKind::Completed {
            stop_reason: "EndTurn".into(),
        };
        relay.assessment = Some(std::sync::Arc::new(assessment.clone()));
        publish(&mut runtime, &materialized, &relay);
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        materialized
            .transcript
            .push(std::sync::Arc::new(TranscriptItem {
                stable_id: "answer".into(),
                position: 11,
                latest_content_event_ordinal: None,
                created_at_ms: 11,
                last_changed_at_ms: 11,
                body: TranscriptBody::User { content: vec![] },
            }));
        publish(&mut runtime, &materialized, &relay);
        assert!(runtime.transcripts["parent"].items.is_empty());
        assert_eq!(project(&runtime)["parent"], SessionAttention::Idle);
        materialized.transcript.clear();
        assessment.completed_ordinal = 9;
        relay.assessment = Some(std::sync::Arc::new(assessment.clone()));
        publish(&mut runtime, &materialized, &relay);
        assert_eq!(project(&runtime)["parent"], SessionAttention::Idle);
        assessment.attention = Some(AssessmentAttention::Important);
        relay.assessment = Some(std::sync::Arc::new(assessment.clone()));
        publish(&mut runtime, &materialized, &relay);
        assert_eq!(project(&runtime)["parent"], SessionAttention::Idle);
        assessment.completed_ordinal = 10;
        assessment.action = Some(Action::Finished);
        relay.assessment = Some(std::sync::Arc::new(assessment));
        publish(&mut runtime, &materialized, &relay);
        assert_eq!(project(&runtime)["parent"], SessionAttention::Important);
        runtime
            .records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 9;
        assert_eq!(project(&runtime)["parent"], SessionAttention::Important);
        runtime
            .records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 10;
        assert_eq!(project(&runtime)["parent"], SessionAttention::Idle);
        let mut assessment = (*relay.assessment.as_ref().unwrap().as_ref()).clone();
        assessment.action = Some(Action::AwaitInput);
        relay.assessment = Some(std::sync::Arc::new(assessment));
        publish(&mut runtime, &materialized, &relay);
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
    }

    // Hard-won: 912798901: configuration repair errors lost their failure state.
    #[test]
    fn attention_failure_is_distinct_from_connection_loss_and_reviews_override_questions() {
        let mut runtime = runtime();
        let normal = project(&runtime)["parent"];
        let mut config = (*runtime.metadata.config).clone();
        let profile = config.profiles.remove("codex").unwrap();
        runtime.metadata.config = config.clone().into();
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        config.profiles.insert("codex".into(), profile);
        runtime.metadata.config = config.into();
        assert_eq!(project(&runtime)["parent"], normal);
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
