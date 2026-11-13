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
        next.metadata.unread_frontiers = previous.metadata.unread_frontiers.clone();
        next.metadata.notification_dismissals = previous.metadata.notification_dismissals.clone();
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
    let (states, targets) = project_states(next, ids.clone());
    next.metadata.session_attention = previous.metadata.session_attention.clone();
    next.metadata.notification_dismissals = previous.metadata.notification_dismissals.clone();
    next.metadata.unread_frontiers = previous.metadata.unread_frontiers.clone();
    for id in ids {
        if let Some(frontier) = unread_frontier(next, &id) {
            if next.metadata.unread_frontiers.get(&id) != Some(&frontier) {
                next.metadata.unread_frontiers.insert(id.clone(), frontier);
            }
        } else {
            next.metadata.unread_frontiers.remove(&id);
        }
        if let Some(request) = next
            .sessions
            .get(&id)
            .and_then(|view| view.operational.as_deref())
            .and_then(|live| live.open_request.as_ref())
        {
            let work = request_hold(next, &id);
            let held = states.get(&id) == Some(&SessionAttention::Working)
                && semantic_question(next, &id).is_some()
                && !work.is_empty();
            let was_held = previous.metadata.session_attention.get(&id)
                == Some(&SessionAttention::Working)
                && previous
                    .sessions
                    .get(&id)
                    .and_then(|view| view.operational.as_deref())
                    .and_then(|live| live.open_request.as_ref())
                    .map(|request| &request.question)
                    == Some(&request.question)
                && !request_hold(previous, &id).is_empty();
            if held && !was_held {
                tracing::info!(session_id = %id, asked_at_ordinal = request.question.completed_ordinal, ?work, "holding open request");
            } else if states.get(&id) == Some(&SessionAttention::NeedsInput)
                && previous.metadata.session_attention.get(&id) == Some(&SessionAttention::Working)
                && previous
                    .sessions
                    .get(&id)
                    .and_then(|view| view.operational.as_deref())
                    .and_then(|live| live.open_request.as_ref())
                    .map(|request| &request.question)
                    == Some(&request.question)
            {
                tracing::info!(session_id = %id, asked_at_ordinal = request.question.completed_ordinal, work = ?waiting_on_own_work(previous, &id), "releasing open request");
            }
        }
        let target = targets.get(&id).cloned();
        if let Some(target) = target {
            if next.metadata.notification_dismissals.get(&id) != Some(&target) {
                next.metadata
                    .notification_dismissals
                    .insert(id.clone(), target);
            }
        } else {
            next.metadata.notification_dismissals.remove(&id);
        }
        if let Some(state) = states.get(&id) {
            if next.metadata.session_attention.get(&id) != Some(state) {
                next.metadata.session_attention.insert(id, *state);
            }
        } else {
            next.metadata.session_attention.remove(&id);
        }
    }
}

fn unread_frontier(runtime: &RuntimeProjection, id: &str) -> Option<u64> {
    let frontier = runtime
        .transcripts
        .get(id)?
        .window
        .latest_unread_event_ordinal?;
    (frontier > runtime.records.get(id)?.viewed_through_event_ordinal).then_some(frontier)
}

/// The request owner is the worker. Pre-64 workers retain the latest-turn rule;
/// remove that compatibility branch when relay protocols below 64 are retired.
fn semantic_question(
    runtime: &RuntimeProjection,
    id: &str,
) -> Option<mj_core::assessment::InferredQuestion> {
    let live = runtime.sessions.get(id)?.operational.as_deref()?;
    let tail = runtime.transcripts.get(id)?;
    if !live.activity_state().is_idle() {
        return None;
    }
    if live
        .relay_protocol_version
        .is_some_and(|version| version >= mj_core::relay::RELAY_OPEN_REQUEST_PROTOCOL)
    {
        let request = live.open_request.as_ref()?;
        if tail.header.active_turn.is_some()
            || !tail.header.queued_prompts.is_empty()
            || tail
                .latest_user_position
                .is_some_and(|position| position > request.question.completed_ordinal)
        {
            return None;
        }
        Some(request.question.clone())
    } else {
        let assessment = live.assessment.as_deref();
        let outcome = tail
            .header
            .unanswered_input_outcome(assessment, tail.latest_user_position)?;
        Some(mj_core::assessment::InferredQuestion {
            turn_id: outcome.command_id.clone(),
            completed_ordinal: outcome.completed_ordinal,
        })
    }
}

fn request_hold(runtime: &RuntimeProjection, id: &str) -> Vec<&'static str> {
    if runtime.subagents.contains_key(id)
        || !runtime
            .records
            .get(id)
            .is_some_and(|record| record.harness_kind.has_reliable_background_inventory())
    {
        return vec![];
    }
    waiting_on_own_work(runtime, id)
}

fn waiting_on_own_work(runtime: &RuntimeProjection, id: &str) -> Vec<&'static str> {
    let Some(live) = runtime
        .sessions
        .get(id)
        .and_then(|view| view.operational.as_deref())
    else {
        return vec![];
    };
    let mut work = vec![];
    if runtime.subagents.iter().any(|(child, metadata)| {
        metadata.parent_session_id == id
            && runtime
                .sessions
                .get(child)
                .and_then(|view| view.operational.as_deref())
                .is_some_and(|child| {
                    child.active_prompt.is_some()
                        || child.harness_turn.is_some()
                        || !child.queued_prompts.is_empty()
                })
    }) {
        work.push("managed child");
    }
    if live.native_agent_count > 0 {
        work.push("native agent");
    }
    if !live.background_commands.is_empty() && live.background_needed == Some(true) {
        work.push("background command");
    }
    if mj_core::activity::turn_imminent(&live.facts(), mj_core::clock::epoch_millis()) {
        work.push("imminent turn");
    }
    work
}

fn dismissal(
    runtime: &RuntimeProjection,
    id: &str,
    attention: SessionAttention,
) -> Option<mj_core::attention::NotificationDismissal> {
    use mj_core::attention::NotificationDismissal;
    if attention == SessionAttention::Unread {
        return unread_frontier(runtime, id).map(NotificationDismissal::ReadThrough);
    }
    let live = runtime.sessions.get(id)?.operational.as_deref()?;
    match attention {
        SessionAttention::Important => Some(NotificationDismissal::ReadThrough(
            live.assessment.as_deref()?.completed_ordinal,
        )),
        SessionAttention::NeedsInput
            if live.relay_protocol_version.is_some_and(|version| {
                version >= mj_core::relay::RELAY_DISMISS_QUESTION_PROTOCOL
            }) =>
        {
            let tail = runtime.transcripts.get(id)?;
            if !tail.header.pending_elicitations.is_empty()
                || runtime.metadata.reviews.iter().any(|review| {
                    review.session_id == id && !matches!(review.phase, TurnReviewPhase::Resolved(_))
                })
            {
                return None;
            }
            let question = semantic_question(runtime, id)?;
            // Pre-64 workers can dismiss only the current inferred assessment.
            if live
                .relay_protocol_version
                .is_some_and(|version| version < mj_core::relay::RELAY_OPEN_REQUEST_PROTOCOL)
                && live
                    .assessment
                    .as_ref()
                    .and_then(|assessment| assessment.inferred_question())
                    .as_ref()
                    != Some(&question)
            {
                return None;
            }
            Some(NotificationDismissal::Question(question))
        }
        _ => None,
    }
}

#[cfg(test)]
fn project_ids(
    runtime: &RuntimeProjection,
    ids: BTreeSet<String>,
) -> BTreeMap<String, SessionAttention> {
    project_states(runtime, ids).0
}

fn project_states(
    runtime: &RuntimeProjection,
    ids: BTreeSet<String>,
) -> (
    BTreeMap<String, SessionAttention>,
    BTreeMap<String, mj_core::attention::NotificationDismissal>,
) {
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
                if semantic_question(runtime, id).is_some() {
                    let held = !request_hold(runtime, id).is_empty();
                    return (id.clone(), if held { Working } else { NeedsInput });
                }
                let activity = live.activity_state();
                if activity.is_idle() {
                    let tail = runtime.transcripts.get(id);
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
            let state = if state == Idle && unread_frontier(runtime, id).is_some() {
                Unread
            } else {
                state
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
    let targets = attention
        .iter()
        .filter(|(id, _)| !questions.contains(id.as_str()))
        .filter_map(|(id, state)| dismissal(runtime, id, *state).map(|target| (id.clone(), target)))
        .collect();
    (attention, targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MaterializedSession, ProjectionWindow, SessionState};

    #[test]
    fn open_request_releases_when_only_the_managed_child_changes() {
        use mj_core::assessment::{InferredQuestion, OpenRequest};
        use mj_core::config::HarnessKind;
        use mj_core::relay::{HarnessTurn, RELAY_PROTOCOL_VERSION, RelaySnapshot};
        let mut next = runtime();
        let mut config = (*next.metadata.config).clone();
        for (id, kind) in [("claude", HarnessKind::Claude), ("kimi", HarnessKind::Kimi)] {
            config.profiles.insert(
                id.into(),
                mj_core::config::HarnessProfile::new(kind, std::path::PathBuf::new()),
            );
        }
        next.metadata.config = config.into();
        next.records.get_mut("parent").unwrap().harness_kind = HarnessKind::Claude;
        next.records.get_mut("parent").unwrap().last_profile = "claude".into();
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
        let mut parent = RelaySnapshot::new("parent".into()).operational_state();
        parent.relay_protocol_version = Some(RELAY_PROTOCOL_VERSION);
        parent.open_request = Some(OpenRequest {
            question: InferredQuestion {
                turn_id: "original".into(),
                completed_ordinal: 10,
            },
            question_message: None,
        });
        next.sessions.get_mut("parent").unwrap().operational = Some(parent.into());
        let mut materialized = MaterializedSession::empty("parent");
        materialized
            .transcript
            .push(std::sync::Arc::new(mj_core::state::TranscriptItem {
                stable_id: "user:session-message-turn-background".into(),
                position: 20,
                latest_content_event_ordinal: None,
                created_at_ms: 20,
                last_changed_at_ms: 20,
                body: mj_core::state::TranscriptBody::User {
                    content: vec![
                        serde_json::json!({"type":"text","text":"An agent-only follow-up"}),
                    ],
                },
            }));
        next.transcripts.insert(
            "parent".into(),
            mj_client::runtime_feed::SessionTail::of(
                &materialized,
                &ProjectionWindow::of(&materialized),
                0,
            ),
        );
        let mut child = RelaySnapshot::new("child".into()).operational_state();
        child.harness_turn = Some(HarnessTurn { started_at_ms: 1 });
        next.sessions.get_mut("child").unwrap().operational = Some(child.clone().into());
        update(&RuntimeProjection::default(), &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Working
        );
        next.transcripts
            .get_mut("parent")
            .unwrap()
            .header
            .pending_elicitations
            .push(mj_core::elicitation::ElicitationRequest {
                id: "form".into(),
                message: "A question now".into(),
                title: None,
                description: None,
                fields: vec![],
                tool_permission: None,
            });
        assert_eq!(project(&next)["parent"], SessionAttention::NeedsInput);
        next.transcripts
            .get_mut("parent")
            .unwrap()
            .header
            .pending_elicitations
            .clear();
        next.records.get_mut("parent").unwrap().harness_kind = HarnessKind::Kimi;
        next.records.get_mut("parent").unwrap().last_profile = "kimi".into();
        assert_eq!(project(&next)["parent"], SessionAttention::NeedsInput);
        next.records.get_mut("parent").unwrap().harness_kind = HarnessKind::Claude;
        next.records.get_mut("parent").unwrap().last_profile = "claude".into();
        let previous = next.clone();
        child.harness_turn = None;
        next.sessions.get_mut("child").unwrap().operational = Some(child.into());
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::NeedsInput
        );
        assert!(
            matches!(next.metadata.notification_dismissals.get("parent"), Some(mj_core::attention::NotificationDismissal::Question(question)) if question.completed_ordinal == 10)
        );
        let original = next.sessions["parent"].operational.clone().unwrap();
        let mut with_background = (*original).clone();
        with_background
            .background_commands
            .push(mj_core::relay::BackgroundCommand {
                id: "build".into(),
                started_at_ms: 1,
                command: "build".into(),
                can_stop: true,
            });
        with_background.background_needed = Some(true);
        next.sessions.get_mut("parent").unwrap().operational = Some(with_background.into());
        assert!(matches!(
            next.sessions["parent"]
                .operational
                .as_ref()
                .unwrap()
                .activity_state(),
            mj_core::activity::ActivityState::Background { .. }
        ));
        assert_eq!(project(&next)["parent"], SessionAttention::Working);
        next.records.get_mut("parent").unwrap().harness_kind = HarnessKind::Kimi;
        next.records.get_mut("parent").unwrap().last_profile = "kimi".into();
        assert_eq!(project(&next)["parent"], SessionAttention::Working);
    }

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
            assert_eq!(
                next.metadata
                    .notification_dismissals
                    .iter()
                    .collect::<BTreeMap<_, _>>(),
                project_states(&next, next.records.keys().cloned().collect())
                    .1
                    .iter()
                    .collect::<BTreeMap<_, _>>()
            );
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

    // Hard-won: 92684ad3b: hidden output and interruptions lost their unread frontier.
    // Hard-won: 8c2f98de2: unread output outside the transcript window lost its mark.
    #[test]
    fn unread_tracks_content_and_cursor_only_changes_incrementally_without_overriding_attention() {
        use mj_core::attention::NotificationDismissal;
        use mj_core::transcript::{TranscriptBody, TranscriptItem};
        let mut next = runtime();
        let mut relay = mj_core::relay::RelaySnapshot::new("parent".into());
        relay.execution = mj_core::relay::RelayExecutionState::Idle;
        next.sessions.get_mut("parent").unwrap().operational =
            Some(relay.operational_state().into());
        let mut materialized = MaterializedSession::empty("parent");
        let item = |ordinal| {
            std::sync::Arc::new(TranscriptItem {
                stable_id: "agent".into(),
                position: 1,
                latest_content_event_ordinal: Some(ordinal),
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body: TranscriptBody::Agent {
                    chunks: vec![serde_json::json!({"content": {"type": "text", "text": "Ready"}})],
                    streaming: false,
                },
            })
        };
        materialized.transcript.push(item(10));
        next.transcripts.insert(
            "parent".into(),
            mj_client::runtime_feed::SessionTail::of(
                &materialized,
                &ProjectionWindow::of(&materialized),
                0,
            ),
        );
        let mut previous = RuntimeProjection::default();
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Unread
        );
        assert_eq!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::ReadThrough(10)
        );
        assert!(
            next.transcripts["parent"].items.is_empty(),
            "content watermark survives tail trimming"
        );

        // The actor itself trims history, before the feed applies its own tail limit.
        let mut actor = materialized.clone();
        for position in 2..=1024 {
            actor.transcript.push(std::sync::Arc::new(TranscriptItem {
                stable_id: format!("later:{position}"),
                position,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body: if position % 100 == 0 {
                    TranscriptBody::User {
                        content: vec![serde_json::json!({"type":"text", "text":"Later turn"})],
                    }
                } else {
                    TranscriptBody::System {
                        text: "Later activity".into(),
                    }
                },
            }));
        }
        let mut actor_window = ProjectionWindow::of(&actor);
        actor_window.trim(&mut actor, crate::database::PROJECTION_TAIL_ITEMS);
        assert!(
            !actor
                .transcript
                .iter()
                .any(|item| item.is_nonempty_agent_message())
        );
        previous = next.clone();
        next.transcripts
            .get_mut("parent")
            .unwrap()
            .publish(&actor, &actor_window, 0);
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Unread
        );
        assert_eq!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::ReadThrough(10)
        );
        previous = next.clone();
        next.records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 10;
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Idle
        );
        assert!(!next.metadata.notification_dismissals.contains_key("parent"));
        assert!(std::ptr::eq(
            previous.metadata.session_attention.get("child").unwrap(),
            next.metadata.session_attention.get("child").unwrap()
        ));
        previous = next.clone();
        materialized.transcript[0] = item(11);
        next.transcripts.get_mut("parent").unwrap().publish(
            &materialized,
            &ProjectionWindow::of(&materialized),
            0,
        );
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Unread
        );
        assert_eq!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::ReadThrough(11)
        );
        previous = next.clone();
        let mut working = relay.operational_state();
        working.activity = Some(mj_core::activity::ActivityState::Turn {
            started_at_ms: Some(1),
            last_activity_at_ms: None,
        });
        next.sessions.get_mut("parent").unwrap().operational = Some(working.into());
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Working
        );
        assert!(!next.metadata.notification_dismissals.contains_key("parent"));
        assert_eq!(next.metadata.unread_frontiers["parent"], 11);
        previous = next.clone();
        next.records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 11;
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Working
        );
        assert!(!next.metadata.unread_frontiers.contains_key("parent"));
        previous = next.clone();
        next.sessions.get_mut("parent").unwrap().operational =
            Some(relay.operational_state().into());
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Idle,
            "acknowledged output does not return when work stops"
        );
        previous = next.clone();
        next.sessions.get_mut("parent").unwrap().operational =
            Some(relay.operational_state().into());
        materialized
            .pending_elicitations
            .push(mj_core::elicitation::ElicitationRequest {
                id: "form".into(),
                message: "Choose".into(),
                title: None,
                description: None,
                fields: vec![],
                tool_permission: None,
            });
        next.transcripts.get_mut("parent").unwrap().publish(
            &materialized,
            &ProjectionWindow::of(&materialized),
            0,
        );
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::NeedsInput
        );
        assert!(!next.metadata.notification_dismissals.contains_key("parent"));
        previous = next.clone();
        next.sessions.get_mut("parent").unwrap().connected = false;
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Unreachable
        );
        assert!(!next.metadata.notification_dismissals.contains_key("parent"));
        previous = next.clone();
        next.records.get_mut("parent").unwrap().state = SessionState::Error;
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Failed
        );
        assert!(!next.metadata.notification_dismissals.contains_key("parent"));
        previous = next.clone();
        next.records.get_mut("parent").unwrap().state = SessionState::Running;
        next.sessions.get_mut("parent").unwrap().connected = true;
        materialized.pending_elicitations.clear();
        materialized.last_turn_outcome = Some(mj_core::state::MaterializedTurnOutcome {
            command_id: "interrupted".into(),
            accepted_ordinal: Some(1),
            turn_start_position: Some(1),
            completed_ordinal: 12,
            completed_at_ms: 12,
            outcome: mj_core::state::TurnOutcomeKind::Interrupted {
                message: "stopped".into(),
                reason: None,
            },
            answered_through: None,
            diagnostic: None,
            usage: None,
            work_facts: None,
        });
        next.transcripts.get_mut("parent").unwrap().publish(
            &materialized,
            &ProjectionWindow::of(&materialized),
            0,
        );
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Unread
        );
        assert_eq!(next.metadata.unread_frontiers["parent"], 12);
        assert_eq!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::ReadThrough(12)
        );
        previous = next.clone();
        next.records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 12;
        materialized
            .transcript
            .push(std::sync::Arc::new(TranscriptItem {
                stable_id: format!("{}13", mj_core::transcript::WORK_INTERRUPTED_ITEM_PREFIX),
                position: 13,
                latest_content_event_ordinal: None,
                created_at_ms: 13,
                last_changed_at_ms: 13,
                body: TranscriptBody::System {
                    text: "Work interrupted".into(),
                },
            }));
        next.transcripts.get_mut("parent").unwrap().publish(
            &materialized,
            &ProjectionWindow::of(&materialized),
            0,
        );
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Unread
        );
        assert_eq!(next.metadata.unread_frontiers["parent"], 13);
        assert_eq!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::ReadThrough(13)
        );
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
    // Hard-won: 92684ad3b: dismissed questions reappeared as unread without a turn read receipt.
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
            let mut operational = relay.operational_state();
            operational.relay_protocol_version =
                Some(mj_core::relay::RELAY_DISMISS_QUESTION_PROTOCOL);
            runtime.sessions.get_mut("parent").unwrap().operational = Some(operational.into());
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
        assert!(matches!(
            project_states(&runtime, BTreeSet::from(["parent".into()]))
                .1
                .get("parent"),
            Some(mj_core::attention::NotificationDismissal::Question(_))
        ));
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
        assert_eq!(
            project_states(&runtime, BTreeSet::from(["parent".into()]))
                .1
                .get("parent"),
            Some(&mj_core::attention::NotificationDismissal::ReadThrough(10))
        );
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
        materialized
            .transcript
            .push(std::sync::Arc::new(TranscriptItem {
                stable_id: "question-text".into(),
                position: 2,
                latest_content_event_ordinal: Some(9),
                created_at_ms: 9,
                last_changed_at_ms: 9,
                body: TranscriptBody::Agent {
                    chunks: vec![
                        serde_json::json!({"content": {"type": "text", "text": "Which branch?"}}),
                    ],
                    streaming: false,
                },
            }));
        runtime
            .records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 0;
        publish(&mut runtime, &materialized, &relay);
        let mut previous = RuntimeProjection::default();
        update(&previous, &mut runtime);
        let Some(mj_core::attention::NotificationDismissal::Question(target)) = runtime
            .metadata
            .notification_dismissals
            .get("parent")
            .cloned()
        else {
            panic!("dismissible question");
        };
        assert_eq!(runtime.metadata.unread_frontiers["parent"], 9);
        let mut dismissed = (*relay.assessment.as_ref().unwrap().as_ref()).clone();
        dismissed.status = Status::Superseded;
        dismissed.reason = "question_dismissed".into();
        relay.assessment = Some(std::sync::Arc::new(dismissed));
        previous = runtime.clone();
        runtime
            .records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = target.completed_ordinal;
        publish(&mut runtime, &materialized, &relay);
        update(&previous, &mut runtime);
        assert_eq!(
            runtime.metadata.session_attention["parent"],
            SessionAttention::Idle
        );
        assert!(!runtime.metadata.unread_frontiers.contains_key("parent"));
        assert!(
            !runtime
                .metadata
                .notification_dismissals
                .contains_key("parent")
        );
        previous = runtime.clone();
        let mut later = (*materialized.transcript[0]).clone();
        later.latest_content_event_ordinal = Some(11);
        materialized.transcript[0] = std::sync::Arc::new(later);
        publish(&mut runtime, &materialized, &relay);
        update(&previous, &mut runtime);
        assert_eq!(
            runtime.metadata.session_attention["parent"],
            SessionAttention::Unread
        );
        assert_eq!(runtime.metadata.unread_frontiers["parent"], 11);
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
