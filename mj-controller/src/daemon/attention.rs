//! The sole person-facing classification, applied at the common runtime publication.
use mj_client::runtime_feed::{CardForm, NotificationCard, RuntimeProjection};
use mj_core::attention::{
    AssessmentAttention, EntryMessage, NotificationDismissal, NotificationKind,
    NotificationStackEntry, SessionAttention,
};
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
        next.metadata.form_holders = previous.metadata.form_holders.clone();
        next.metadata.notification_cards = Some(
            previous
                .metadata
                .notification_cards
                .clone()
                .unwrap_or_default(),
        );
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
    let AttentionProjection {
        states,
        targets,
        holders,
        cards,
        held_questions,
    } = project_states(next, ids.clone());
    next.metadata.form_holders = previous.metadata.form_holders.clone();
    next.metadata.session_attention = previous.metadata.session_attention.clone();
    next.metadata.notification_dismissals = previous.metadata.notification_dismissals.clone();
    next.metadata.unread_frontiers = previous.metadata.unread_frontiers.clone();
    let mut published_cards = previous
        .metadata
        .notification_cards
        .clone()
        .unwrap_or_default();
    for id in ids {
        match cards.get(&id) {
            Some(card) if published_cards.get(&id) != Some(card) => {
                published_cards.insert(id.clone(), card.clone());
            }
            Some(_) => {}
            None => {
                published_cards.remove(&id);
            }
        }
        if let Some(holder) = holders.get(&id) {
            if next.metadata.form_holders.get(&id) != Some(holder) {
                next.metadata
                    .form_holders
                    .insert(id.clone(), holder.clone());
            }
        } else {
            next.metadata.form_holders.remove(&id);
        }
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
            let held = held_questions.contains(&id);
            let was_held = matches!(
                previous.metadata.session_attention.get(&id),
                Some(SessionAttention::Working | SessionAttention::Important)
            ) && semantic_question(previous, &id).is_some()
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
                && was_held
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
    next.metadata.notification_cards = Some(published_cards);
}

/// Read receipts are durable before the worker reclaims any entries. The
/// publication filters them immediately; losing a prune only delays cleanup.
pub(super) async fn prune_read_notifications(
    state: std::sync::Arc<super::RuntimeState>,
    cancellation: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    use mj_core::snapshot_map::SnapshotMap;
    let mut revisions = state.revisions.subscribe();
    let mut previous_records = SnapshotMap::default();
    let mut previous_views = SnapshotMap::default();
    let mut attempted = BTreeMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        revisions.borrow_and_update();
        let (records, views) = {
            let owner = state.owner();
            (
                owner.controller().state.sessions.clone(),
                owner.sessions.clone(),
            )
        };
        let changed: BTreeSet<_> = previous_records
            .changes(&records)
            .map(|(id, _)| id)
            .chain(previous_views.changes(&views).map(|(id, _)| id))
            .collect();
        for id in changed {
            let candidate = records
                .get(id)
                .zip(views.get(id))
                .and_then(|(record, view)| {
                    let live = view.operational.as_deref()?;
                    let through = record.viewed_through_event_ordinal;
                    let has_read_entry = live.notification_stack.entries.iter().any(|entry| {
                        entry.kind == NotificationKind::Important
                            && entry.completed_ordinal <= through
                    });
                    (view.connected && record.state.is_active() && has_read_entry)
                        .then(|| crate::session_manager::notifications_read(live, through))
                        .flatten()
                });
            let Some((command_id, command, through)) = candidate else {
                attempted.remove(id);
                continue;
            };
            if attempted.get(id) == Some(&through) {
                continue;
            }
            attempted.insert(id.clone(), through);
            let id = id.clone();
            let manager = state.session_manager.clone();
            tasks.spawn(async move {
                let result = async {
                    manager.session(&id).await?.submit(command_id, command).await
                }.await;
                if let Err(error) = result {
                    tracing::warn!(session_id = %id, %error, "could not prune read notification entries");
                }
            });
        }
        previous_records = records;
        previous_views = views;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    tasks.shutdown().await;
                    return Ok(());
                }
                changed = revisions.changed() => {
                    changed?;
                    break;
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    joined?;
                }
            }
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
    // Holding is safe only when the worker retains the request: a pre-64
    // worker's latest-turn question is replaced by the agent-only turn that
    // ends the wait, so holding it would lose the question.
    if runtime.subagents.contains_key(id)
        || !runtime
            .records
            .get(id)
            .is_some_and(|record| record.harness_kind.has_reliable_background_inventory())
        || !runtime
            .sessions
            .get(id)
            .and_then(|view| view.operational.as_deref())
            .is_some_and(|live| live.supports_open_request())
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
                // The shared "still owns work" answer: a closed child (or one
                // that is only expecting a continuation) must not hold forever.
                .is_some_and(|child| {
                    child.activity_state().has_work_in_flight() || !child.queued_prompts.is_empty()
                })
    }) {
        work.push("managed child");
    }
    if live.native_agent_count > 0 {
        work.push("native agent");
    }
    // Background commands do not hold: coordinators keep Claude Monitors (and
    // agents keep servers) running for hours, so a hold bounded by their end
    // could hide a real question indefinitely. Children and sub-agents end.
    if mj_core::activity::turn_imminent(&live.facts(), mj_core::clock::epoch_millis()) {
        work.push("imminent turn");
    }
    work
}

fn current_condition_failure(runtime: &RuntimeProjection, id: &str) -> bool {
    runtime.records.get(id).is_some_and(|record| {
        record
            .configuration_issue(&runtime.metadata.config)
            .is_some()
    }) || runtime.sessions.get(id).is_some_and(|view| {
        matches!(
            view.error,
            Some(
                mj_client::session::ViewError::TargetMissing(_)
                    | mj_client::session::ViewError::ProjectionIntegrity(_)
            )
        )
    })
}

fn failed_review(review: Option<&mj_client::review::RuntimeReviewView>) -> bool {
    review.is_some_and(|review| {
        matches!(
            review.phase,
            TurnReviewPhase::Verdict(ReviewVerdict::Failed { .. })
        )
    })
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
    if attention == SessionAttention::Failed {
        // Configuration, view and review failures are current conditions, not
        // recorded lifecycle failures. Their owners resolve them.
        let record = runtime.records.get(id)?;
        if current_condition_failure(runtime, id)
            || failed_review(
                runtime
                    .metadata
                    .reviews
                    .iter()
                    .find(|review| review.session_id == id),
            )
        {
            return None;
        }
        return mj_core::attention::FailureIdentity::of(record).map(NotificationDismissal::Failure);
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
    project_states(runtime, ids).states
}

struct AttentionProjection {
    states: BTreeMap<String, SessionAttention>,
    targets: BTreeMap<String, mj_core::attention::NotificationDismissal>,
    holders: BTreeMap<String, String>,
    cards: BTreeMap<String, NotificationCard>,
    held_questions: BTreeSet<String>,
}

fn project_states(runtime: &RuntimeProjection, ids: BTreeSet<String>) -> AttentionProjection {
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
    let mut native_forms = BTreeMap::new();
    // Native children never receive semantic assessments. Only an actual
    // harness form reaches their owner and its managed ancestors.
    for (source, native) in runtime.native_agents.iter().filter(|(_, native)| {
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
            // A session's own request takes precedence over any descendant.
            native_forms
                .entry(current)
                .and_modify(
                    |selected: &mut (&String, &mj_core::native_agent::NativeAgentSummary)| {
                        if current == native.agent.owner_session_id
                            && selected.1.agent.owner_session_id != current
                        {
                            *selected = (source, native);
                        }
                    },
                )
                .or_insert((source, native));
            let Some(parent) = runtime.subagents.get(current) else {
                break;
            };
            current = &parent.parent_session_id;
        }
    }
    let mut holders = BTreeMap::new();
    for (id, person_owned) in runtime
        .transcripts
        .iter()
        .filter(|(_, tail)| !tail.header.pending_elicitations.is_empty())
        .map(|(id, tail)| {
            (
                id,
                tail.header
                    .pending_elicitations
                    .iter()
                    .any(|request| !request.goes_to_parent()),
            )
        })
        .chain(reviews.values().flat_map(|review| {
            review
                .questions
                .iter()
                // A reviewer is a managed child: its ordinary questions belong
                // to the reviewed session's agent, like any child's.
                .map(|question| (&question.role, !question.request.goes_to_parent()))
        }))
    {
        if !runtime
            .records
            .get(id)
            .is_some_and(|record| record.state.is_active())
        {
            continue;
        }
        questions.insert(id.clone());
        if person_owned || !runtime.subagents.contains_key(id) {
            // A form answered here takes precedence over any descendant.
            holders.insert(id.clone(), id.clone());
        }
        if !person_owned {
            continue;
        }
        let mut current = id.as_str();
        let mut visited = BTreeSet::from([current]);
        while let Some(parent) = runtime.subagents.get(current) {
            current = &parent.parent_session_id;
            if !visited.insert(current)
                || !runtime
                    .records
                    .get(current)
                    .is_some_and(|record| record.state.is_active())
            {
                break;
            }
            questions.insert(current.to_owned());
            holders
                .entry(current.to_owned())
                .or_insert_with(|| id.clone());
        }
    }
    let mut cards = BTreeMap::new();
    let mut held_questions = BTreeSet::new();
    let mut attention: BTreeMap<_, _> = ids
        .iter()
        .filter_map(|id| runtime.records.get(id).map(|record| (id, record)))
        .map(|(id, record)| {
            let view = runtime.sessions.get(id);
            let live = view.and_then(|view| view.operational.as_deref());
            let review = reviews.get(id.as_str()).copied();
            let transitioning =
                record.state.transition_kind().is_some() || lifecycles.contains(id.as_str());
            let failure = mj_core::attention::FailureIdentity::of(record);
            let dismissed = failure
                .as_ref()
                .is_some_and(|failure| record.dismissed_failure.as_ref() == Some(failure));
            // Archiving removes the session from the dashboard; nothing about
            // it needs a person any more.
            let mut raised_question = None;
            let state = if record.archived {
                Inactive
            } else if current_condition_failure(runtime, id) || (failure.is_some() && !dismissed) {
                Failed
            } else if dismissed
                && (!record.state.is_active()
                    || record.state == mj_core::state::SessionState::Error)
            {
                Inactive
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
            } else if failed_review(review) {
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
                if let Some(question) = semantic_question(runtime, id) {
                    if request_hold(runtime, id).is_empty() {
                        raised_question = Some(question);
                        NeedsInput
                    } else {
                        held_questions.insert(id.clone());
                        Working
                    }
                } else if live.activity_state().is_idle() {
                    let tail = runtime.transcripts.get(id);
                    if !live.supports_notification_stack()
                        && live.assessment.as_deref().is_some_and(|assessment| {
                            tail.and_then(|tail| tail.header.last_turn_outcome.as_ref())
                                .is_some_and(|outcome| assessment.applies_to(outcome))
                                && assessment.attention == Some(AssessmentAttention::Important)
                                && record.viewed_through_event_ordinal
                                    < assessment.completed_ordinal
                        })
                    {
                        Important
                    } else {
                        Idle
                    }
                } else {
                    Working
                }
            } else {
                Unknown
            };
            let holder = holders.get(id).filter(|_| state == NeedsInput);
            let card = if matches!(state, NeedsInput | Important | Idle | Working)
                && !transitioning
                && record.state != mj_core::state::SessionState::Checkpointing
            {
                notification_card(
                    runtime,
                    id,
                    state,
                    raised_question.as_ref(),
                    holder,
                    native_forms.get(id.as_str()).copied(),
                    record,
                )
            } else {
                None
            };
            let state = match card.as_ref().map(|card| card.badge) {
                Some(NotificationKind::NeedsInput) => NeedsInput,
                Some(NotificationKind::Important) => Important,
                None => state,
            };
            if let Some(card) = card
                && !record.notifications_ignored
                && !runtime.subagents.contains_key(id)
            {
                cards.insert(id.clone(), card);
            }
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
        .filter_map(|(id, state)| {
            let target = if matches!(state, NeedsInput | Important) {
                cards
                    .get(id)
                    .and_then(|card| card.dismissal.clone())
                    .or_else(|| {
                        let supports_stack = runtime
                            .sessions
                            .get(id)
                            .and_then(|view| view.operational.as_deref())
                            .is_some_and(|live| live.supports_notification_stack());
                        (!supports_stack && !questions.contains(id))
                            .then(|| dismissal(runtime, id, *state))
                            .flatten()
                    })
            } else if !questions.contains(id) {
                dismissal(runtime, id, *state)
            } else {
                None
            };
            target.map(|target| (id.clone(), target))
        })
        .collect();
    holders.retain(|id, _| ids.contains(id) && attention.get(id) == Some(&NeedsInput));
    AttentionProjection {
        states: attention,
        targets,
        holders,
        cards,
        held_questions,
    }
}

/// Older workers did not publish their completed reply id. The current
/// turn's last text item can name its native id when it is in the projection.
fn legacy_completed_reply(runtime: &RuntimeProjection, id: &str, completed: u64) -> EntryMessage {
    let Some(tail) = runtime.transcripts.get(id) else {
        return EntryMessage::None;
    };
    let Some(turn) = tail
        .header
        .last_turn_outcome
        .as_ref()
        .filter(|turn| turn.completed_ordinal == completed)
    else {
        return EntryMessage::None;
    };
    let Some(start) = turn.turn_start_position.or(turn.accepted_ordinal) else {
        return EntryMessage::None;
    };
    let last = tail
        .items
        .values()
        .filter_map(|item| {
            let latest = item
                .latest_content_event_ordinal
                .filter(|ordinal| *ordinal > start && *ordinal <= completed)?;
            let mj_core::state::TranscriptBody::Agent { chunks, .. } = &item.body else {
                return None;
            };
            let text_chunk = chunks
                .iter()
                .rev()
                .find(|chunk| chunk["content"]["type"].as_str() == Some("text"))?;
            Some((latest, text_chunk["messageId"].as_str()))
        })
        .max_by_key(|(latest, _)| *latest);
    last.and_then(|(_, message_id)| message_id)
        .and_then(mj_core::activity::question_locator::transcript_stable_id)
        .map_or(EntryMessage::None, EntryMessage::Found)
}

fn notification_card(
    runtime: &RuntimeProjection,
    id: &str,
    attention: SessionAttention,
    raised_question: Option<&mj_core::assessment::InferredQuestion>,
    holder: Option<&String>,
    native: Option<(&String, &mj_core::native_agent::NativeAgentSummary)>,
    record: &mj_core::state::SessionRecord,
) -> Option<NotificationCard> {
    let live = runtime
        .sessions
        .get(id)
        .and_then(|view| view.operational.as_deref())
        // A parked parent can still own a child's form.
        .filter(|_| record.state != mj_core::state::SessionState::Parked);
    let (entries, dismissal) =
        if let Some(live) = live.filter(|live| live.supports_notification_stack()) {
            let stack = &live.notification_stack;
            let entries: Vec<_> = stack
                .entries
                .iter()
                .filter(|entry| match entry.kind {
                    NotificationKind::Important => {
                        entry.completed_ordinal > record.viewed_through_event_ordinal
                    }
                    NotificationKind::NeedsInput => raised_question.is_some(),
                })
                .cloned()
                .collect();
            let dismissal = if entries
                .iter()
                .any(|entry| entry.kind == NotificationKind::NeedsInput)
            {
                Some(NotificationDismissal::Stack {
                    revision: stack.revision,
                })
            } else {
                entries
                    .last()
                    .map(|entry| NotificationDismissal::ReadThrough(entry.completed_ordinal))
            };
            (entries, dismissal)
        } else {
            let entry = if let Some(question) = raised_question {
                let message = if let Some(target) = live.and_then(|live| live.question_message()) {
                    EntryMessage::Found(target.to_owned())
                } else if live.is_some_and(|live| live.supports_open_request()) {
                    EntryMessage::None
                } else {
                    legacy_completed_reply(runtime, id, question.completed_ordinal)
                };
                Some(NotificationStackEntry {
                    completed_ordinal: question.completed_ordinal,
                    kind: NotificationKind::NeedsInput,
                    message,
                })
            } else if attention == SessionAttention::Important {
                live.and_then(|live| live.assessment.as_ref())
                    .map(|assessment| NotificationStackEntry {
                        completed_ordinal: assessment.completed_ordinal,
                        kind: NotificationKind::Important,
                        message: legacy_completed_reply(runtime, id, assessment.completed_ordinal),
                    })
            } else {
                None
            };
            let dismissal = entry
                .as_ref()
                .and_then(|_| dismissal(runtime, id, attention));
            (entry.into_iter().collect(), dismissal)
        };
    let badge = if attention == SessionAttention::NeedsInput {
        NotificationKind::NeedsInput
    } else if entries.is_empty() {
        return None;
    } else {
        NotificationKind::Important
    };
    let direct_form = holder.and_then(|holder| {
        runtime
            .transcripts
            .get(holder)
            .and_then(|tail| {
                tail.header
                    .pending_elicitations
                    .iter()
                    .rev()
                    .find(|request| {
                        !runtime.subagents.contains_key(holder) || !request.goes_to_parent()
                    })
                    .map(|request| CardForm {
                        request_id: request.id.clone(),
                        message: request.message.clone(),
                    })
            })
            .or_else(|| {
                runtime
                    .metadata
                    .reviews
                    .iter()
                    .flat_map(|review| review.questions.iter())
                    .rev()
                    .find(|question| question.role == *holder && !question.request.goes_to_parent())
                    .map(|question| {
                        let request = question.session_request();
                        CardForm {
                            request_id: request.id,
                            message: request.message,
                        }
                    })
            })
    });
    let native_is_own = native.is_some_and(|(_, native)| native.agent.owner_session_id == id);
    let (holder, form) = if let Some(form) = direct_form
        && (holder.is_some_and(|holder| holder == id) || !native_is_own)
    {
        (holder.cloned(), Some(form))
    } else if let Some((source, native)) = native.filter(|_| badge == NotificationKind::NeedsInput)
    {
        (
            Some(native.agent.owner_session_id.clone()),
            Some(CardForm {
                request_id: format!("native.{source}.{}", native.generation_ordinal),
                message: native.pending_input_message.clone().unwrap_or_default(),
            }),
        )
    } else {
        (holder.cloned(), None)
    };
    Some(NotificationCard {
        badge,
        entries,
        dismissal,
        holder,
        form,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MaterializedSession, ProjectionWindow, SessionState};

    // Hard-won: 8dd7f6384: clearing a visible Important entry dismissed a held question.
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
        parent.notification_stack = mj_core::attention::NotificationStack {
            revision: 11,
            entries: vec![
                NotificationStackEntry {
                    completed_ordinal: 7,
                    kind: NotificationKind::Important,
                    message: EntryMessage::None,
                },
                NotificationStackEntry {
                    completed_ordinal: 10,
                    kind: NotificationKind::NeedsInput,
                    message: EntryMessage::None,
                },
            ],
        }
        .into();
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
        let card = &next.metadata.notification_cards.as_ref().unwrap()["parent"];
        assert_eq!(card.badge, NotificationKind::Important);
        assert_eq!(card.entries.len(), 1);
        let Some(NotificationDismissal::ReadThrough(through)) = card.dismissal else {
            panic!("Clear must leave the held question intact");
        };
        assert_eq!(through, 7);
        let before_clear = next.clone();
        next.records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = through;
        update(&before_clear, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Working
        );
        assert!(
            !next
                .metadata
                .notification_cards
                .as_ref()
                .unwrap()
                .contains_key("parent")
        );
        // A pre-64 worker cannot retain the request, so it is never held.
        let mut legacy_runtime = next.clone();
        let mut legacy = (*next.sessions["parent"].operational.clone().unwrap()).clone();
        legacy.relay_protocol_version = Some(mj_core::relay::RELAY_OPEN_REQUEST_PROTOCOL - 1);
        legacy_runtime
            .sessions
            .get_mut("parent")
            .unwrap()
            .operational = Some(legacy.into());
        assert!(request_hold(&legacy_runtime, "parent").is_empty());
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
        child.harness_turn = None;
        child
            .background_commands
            .push(mj_core::relay::BackgroundCommand {
                id: "child-build".into(),
                started_at_ms: 1,
                command: "cargo test".into(),
                can_stop: false,
            });
        child.background_needed = Some(true);
        assert!(matches!(
            child.activity_state(),
            mj_core::activity::ActivityState::Background { .. }
        ));
        next.sessions.get_mut("child").unwrap().operational = Some(child.clone().into());
        assert_eq!(project(&next)["parent"], SessionAttention::Working);
        let previous = next.clone();
        child.background_commands.clear();
        next.sessions.get_mut("child").unwrap().operational = Some(child.into());
        update(&previous, &mut next);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::NeedsInput
        );
        assert_eq!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::Stack { revision: 11 },
        );
        assert_eq!(
            next.metadata.notification_cards.as_ref().unwrap()["parent"].entries[0]
                .completed_ordinal,
            10,
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
        // A background command alone never holds, even when judged needed: a
        // monitor or server can outlive any wait and hide the question
        // indefinitely. The worker inferred idle here, as after a needs-input
        // verdict, so only the hold could hide the question.
        next.records.get_mut("parent").unwrap().harness_kind = HarnessKind::Claude;
        next.records.get_mut("parent").unwrap().last_profile = "claude".into();
        let mut idle_with_background =
            (*next.sessions["parent"].operational.clone().unwrap()).clone();
        idle_with_background.activity =
            Some(mj_core::activity::ActivityState::Idle { since_ms: Some(1) });
        next.sessions.get_mut("parent").unwrap().operational = Some(idle_with_background.into());
        assert_eq!(project(&next)["parent"], SessionAttention::NeedsInput);
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
                pending_input_message: Some("Approve the native action?".into()),
            },
        );
        let attention = project(&runtime);
        assert_eq!(attention[&id], SessionAttention::NeedsInput);
        assert_eq!(attention["parent"], SessionAttention::NeedsInput);
        let projection =
            project_states(&runtime, BTreeSet::from(["parent".to_owned(), id.clone()]));
        let card = &projection.cards["parent"];
        assert_eq!(card.holder.as_deref(), Some("parent"));
        let form = card.form.as_ref().unwrap();
        assert_eq!(form.request_id, format!("native.{id}.1"));
        assert_eq!(form.message, "Approve the native action?");
        assert!(!projection.cards.contains_key(&id));
        let native = runtime.native_agents.get_mut(&id).unwrap();
        native.agent.state = NativeAgentState::Completed;
        native.pending_input = false;
        native.pending_input_message = None;
        assert_eq!(project(&runtime)[&id], SessionAttention::Idle);
        assert_ne!(project(&runtime)["parent"], SessionAttention::NeedsInput);
    }

    #[test]
    fn notification_stack_is_latched_during_work_but_failure_keeps_precedence() {
        use mj_core::assessment::{InferredQuestion, OpenRequest};
        use mj_core::attention::NotificationStack;
        use mj_core::relay::{HarnessTurn, RELAY_NOTIFICATION_STACK_PROTOCOL, RelaySnapshot};

        let mut next = runtime();
        let mut worker = RelaySnapshot::new("parent".into()).operational_state();
        worker.relay_protocol_version = Some(RELAY_NOTIFICATION_STACK_PROTOCOL);
        worker.harness_turn = Some(HarnessTurn { started_at_ms: 1 });
        worker.open_request = Some(OpenRequest {
            question: InferredQuestion {
                completed_ordinal: 11,
                turn_id: "question".into(),
            },
            question_message: Some("agent:question".into()),
        });
        worker.notification_stack = NotificationStack {
            revision: 14,
            entries: vec![
                NotificationStackEntry {
                    completed_ordinal: 10,
                    kind: NotificationKind::Important,
                    message: EntryMessage::Found("agent:earlier".into()),
                },
                NotificationStackEntry {
                    completed_ordinal: 11,
                    kind: NotificationKind::NeedsInput,
                    message: EntryMessage::Found("agent:question".into()),
                },
            ],
        }
        .into();
        next.sessions.get_mut("parent").unwrap().operational = Some(worker.clone().into());
        let materialized = MaterializedSession::empty("parent");
        next.transcripts.insert(
            "parent".into(),
            mj_client::runtime_feed::SessionTail::of(
                &materialized,
                &ProjectionWindow::of(&materialized),
                0,
            ),
        );
        let mut previous = RuntimeProjection::default();
        let publish = |previous: &mut RuntimeProjection, next: &mut RuntimeProjection| {
            update(previous, next);
            *previous = next.clone();
            next.metadata
                .notification_cards
                .as_ref()
                .unwrap()
                .get("parent")
                .cloned()
        };
        let card = publish(&mut previous, &mut next).unwrap();
        assert_eq!(card.badge, NotificationKind::Important);
        assert_eq!(card.entries, &worker.notification_stack.entries[..1]);
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Important
        );

        next.records.get_mut("parent").unwrap().state = SessionState::Error;
        assert!(publish(&mut previous, &mut next).is_none());
        assert_eq!(
            next.metadata.session_attention["parent"],
            SessionAttention::Failed
        );
        assert!(matches!(
            next.metadata.notification_dismissals["parent"],
            NotificationDismissal::Failure(_)
        ));

        next.records.get_mut("parent").unwrap().state = SessionState::Running;
        assert_eq!(publish(&mut previous, &mut next), Some(card));
        next.records
            .get_mut("parent")
            .unwrap()
            .notifications_ignored = true;
        assert!(publish(&mut previous, &mut next).is_none());
        next.records
            .get_mut("parent")
            .unwrap()
            .notifications_ignored = false;
        next.records
            .get_mut("parent")
            .unwrap()
            .viewed_through_event_ordinal = 12;
        assert!(publish(&mut previous, &mut next).is_none());

        worker.harness_turn = None;
        next.sessions.get_mut("parent").unwrap().operational = Some(worker.clone().into());
        let card = publish(&mut previous, &mut next).unwrap();
        assert_eq!(card.badge, NotificationKind::NeedsInput);
        assert_eq!(card.entries, &worker.notification_stack.entries[1..]);

        worker.relay_protocol_version = Some(RELAY_NOTIFICATION_STACK_PROTOCOL - 1);
        next.sessions.get_mut("parent").unwrap().operational = Some(worker.into());
        let legacy = publish(&mut previous, &mut next).unwrap();
        assert_eq!(legacy.entries, card.entries);
        assert!(matches!(
            legacy.dismissal,
            Some(NotificationDismissal::Question(_))
        ));
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
                            fields: if step % 24 == 2 {
                                vec![mj_core::elicitation::ElicitationField {
                                    id: "secret".into(),
                                    title: "Secret".into(),
                                    description: None,
                                    required: true,
                                    secret: true,
                                    custom_answer_for: None,
                                    custom_answer_option: None,
                                    kind: mj_core::elicitation::ElicitationFieldKind::Text {
                                        default: None,
                                        min_length: None,
                                        max_length: None,
                                        pattern: None,
                                        format: None,
                                    },
                                }]
                            } else {
                                vec![]
                            },
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
                    .targets
                    .iter()
                    .collect::<BTreeMap<_, _>>()
            );
            assert_eq!(
                next.metadata
                    .form_holders
                    .iter()
                    .collect::<BTreeMap<_, _>>(),
                project_states(&next, next.records.keys().cloned().collect())
                    .holders
                    .iter()
                    .collect::<BTreeMap<_, _>>()
            );
            assert_eq!(
                next.metadata
                    .notification_cards
                    .as_ref()
                    .unwrap()
                    .iter()
                    .collect::<BTreeMap<_, _>>(),
                project_states(&next, next.records.keys().cloned().collect())
                    .cards
                    .iter()
                    .collect::<BTreeMap<_, _>>(),
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
                let expected = if step % 24 == 2 || step % 24 == 3 {
                    BTreeSet::from(["child", "parent"])
                } else {
                    BTreeSet::from(["child"])
                };
                assert_eq!(changed, expected);
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
                .targets
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
                .targets
                .get("parent"),
            Some(&mj_core::attention::NotificationDismissal::ReadThrough(10))
        );
        materialized
            .last_turn_outcome
            .as_mut()
            .unwrap()
            .turn_start_position = Some(5);
        materialized.transcript.push(std::sync::Arc::new(TranscriptItem {
            stable_id: "agent:retained-reply".into(),
            position: 8,
            latest_content_event_ordinal: Some(8),
            created_at_ms: 8,
            last_changed_at_ms: 8,
            body: TranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content":{"type":"text","text":"Important result"},"messageId":"retained-reply"})],
                streaming: false,
            },
        }));
        publish(&mut runtime, &materialized, &relay);
        runtime.transcripts.get_mut("parent").unwrap().publish(
            &materialized,
            &ProjectionWindow::of(&materialized),
            10,
        );
        let projection = project_states(&runtime, BTreeSet::from(["parent".into()]));
        assert_eq!(
            projection.cards["parent"].entries[0].message,
            EntryMessage::Found("agent:retained-reply".into()),
        );
        materialized.transcript.clear();
        publish(&mut runtime, &materialized, &relay);
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
        {
            let record = runtime.records.get_mut("parent").unwrap();
            record.state = SessionState::Stopped;
            record.last_error = Some("resume failed: operation cancelled".into());
        }
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        let target = dismissal(&runtime, "parent", SessionAttention::Failed).unwrap();
        let mj_core::attention::NotificationDismissal::Failure(failure) = target else {
            panic!("expected recorded failure")
        };
        runtime.records.get_mut("parent").unwrap().dismissed_failure = Some(failure);
        for state in [
            SessionState::Error,
            SessionState::Lost,
            SessionState::DestroyedWithDataLoss,
            SessionState::Stopped,
        ] {
            let record = runtime.records.get_mut("parent").unwrap();
            record.state = state;
            record.dismissed_failure = mj_core::attention::FailureIdentity::of(record);
            assert_eq!(project(&runtime)["parent"], SessionAttention::Inactive);
        }
        // Hard-won: ca27a43c8: an archived session's undismissed failure resurfaced as a notification.
        {
            let record = runtime.records.get_mut("parent").unwrap();
            record.dismissed_failure = None;
            record.archived = true;
            assert_eq!(project(&runtime)["parent"], SessionAttention::Inactive);
            runtime.records.get_mut("parent").unwrap().archived = false;
            assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
            let record = runtime.records.get_mut("parent").unwrap();
            record.dismissed_failure = mj_core::attention::FailureIdentity::of(record);
        }

        assert_eq!(project(&runtime)["parent"], SessionAttention::Inactive);
        // Hard-won: 0db309adc: dismissing a lifecycle failure hid later questions.
        runtime.records.get_mut("parent").unwrap().state = SessionState::Running;
        let record = runtime.records.get_mut("parent").unwrap();
        record.dismissed_failure = mj_core::attention::FailureIdentity::of(record);
        use mj_core::native_agent::*;
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
                pending_input_message: None,
            },
        );
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        runtime.native_agents.clear();
        runtime.records.get_mut("parent").unwrap().last_error =
            Some("another resume failure".into());
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        runtime.records.get_mut("parent").unwrap().dismissed_failure = None;
        runtime.records.get_mut("parent").unwrap().last_error = None;
        runtime.records.get_mut("parent").unwrap().state = SessionState::Running;
        let normal = project(&runtime)["parent"];
        let mut config = (*runtime.metadata.config).clone();
        let profile = config.profiles.remove("codex").unwrap();
        runtime.metadata.config = config.clone().into();
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        assert!(dismissal(&runtime, "parent", SessionAttention::Failed).is_none());
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
        runtime.metadata.reviews[0].phase = TurnReviewPhase::Verdict(ReviewVerdict::Failed {
            reason: "new review failed".into(),
        });
        let record = runtime.records.get_mut("parent").unwrap();
        record.last_error = Some("old lifecycle error".into());
        record.dismissed_failure = mj_core::attention::FailureIdentity::of(record);
        assert_eq!(project(&runtime)["parent"], SessionAttention::Failed);
        assert!(dismissal(&runtime, "parent", SessionAttention::Failed).is_none());
    }

    #[test]
    fn attention_descendant_question_propagates_clears_and_terminates_on_cycles() {
        let mut runtime = runtime();
        runtime.records.insert(
            "grandparent".into(),
            super::super::tests::runtime_test_session(
                "grandparent",
                "workspace",
                SessionState::Running,
            ),
        );
        runtime.sessions.insert(
            "grandparent".into(),
            mj_client::daemon::RuntimeSessionView::from_managed(
                "grandparent".into(),
                mj_client::session::ManagedSessionView {
                    connected: true,
                    ..Default::default()
                },
            ),
        );
        for (child, parent) in [
            ("child", "parent"),
            ("parent", "grandparent"),
            ("grandparent", "child"),
        ] {
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
        assert_eq!(project(&runtime)["child"], SessionAttention::NeedsInput);
        assert_ne!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        assert_ne!(
            project(&runtime)["grandparent"],
            SessionAttention::NeedsInput
        );
        assert!(
            project_states(&runtime, runtime.records.keys().cloned().collect())
                .holders
                .is_empty()
        );
        // A plan approval belongs to the person and identifies the actual holder.
        materialized.pending_elicitations[0].id =
            format!("{}approval", mj_core::acp::PLAN_REVIEW_ID_PREFIX);
        runtime.transcripts.insert(
            "child".into(),
            mj_client::runtime_feed::SessionTail::of(&materialized, &window, 11),
        );
        assert_eq!(project(&runtime)["parent"], SessionAttention::NeedsInput);
        assert_eq!(
            project_states(&runtime, runtime.records.keys().cloned().collect())
                .holders
                .get("parent")
                .map(String::as_str),
            Some("child")
        );
        // The parent's self entry takes precedence, while the grandparent
        // still names the child that actually holds the descendant form.
        let mut own = MaterializedSession::empty("parent");
        own.pending_elicitations
            .push(materialized.pending_elicitations[0].clone());
        runtime.transcripts.insert(
            "parent".into(),
            mj_client::runtime_feed::SessionTail::of(&own, &ProjectionWindow::of(&own), 11),
        );
        let holders = project_states(&runtime, runtime.records.keys().cloned().collect()).holders;
        assert_eq!(holders.get("parent").map(String::as_str), Some("parent"));
        assert_eq!(
            holders.get("grandparent").map(String::as_str),
            Some("child")
        );
        // A sub-agent's ordinary question is its parent agent's to answer, so
        // the descendant's approval stays reachable from this row.
        own.pending_elicitations[0] = mj_core::elicitation::ElicitationRequest {
            id: "question".into(),
            message: "Which?".into(),
            title: None,
            description: None,
            fields: vec![],
            tool_permission: None,
        };
        runtime.transcripts.insert(
            "parent".into(),
            mj_client::runtime_feed::SessionTail::of(&own, &ProjectionWindow::of(&own), 11),
        );
        let holders = project_states(&runtime, runtime.records.keys().cloned().collect()).holders;
        assert_eq!(holders.get("parent").map(String::as_str), Some("child"));
        // The same ordinary form is answered here when this is a top-level session.
        let relation = runtime.subagents.remove("parent").unwrap();
        let holders = project_states(&runtime, runtime.records.keys().cloned().collect()).holders;
        assert_eq!(holders.get("parent").map(String::as_str), Some("parent"));
        runtime.subagents.insert("parent".into(), relation);
        runtime.transcripts.remove("parent");
        let previous = runtime.clone();
        materialized.pending_elicitations[0].id = "tool-approval".into();
        materialized.pending_elicitations[0].tool_permission =
            Some(mj_core::elicitation::ToolPermissionForm { skip: true });
        runtime.transcripts.insert(
            "child".into(),
            mj_client::runtime_feed::SessionTail::of(&materialized, &window, 12),
        );
        update(&previous, &mut runtime);
        assert_eq!(runtime.metadata.form_holders["parent"], "child");
        assert_eq!(runtime.metadata.form_holders["grandparent"], "child");
        assert_eq!(runtime.metadata.form_holders["child"], "child");
        assert!(
            !runtime
                .metadata
                .notification_dismissals
                .contains_key("parent")
        );
        // The review projection names the child that owns the form too.
        let approval = materialized.pending_elicitations[0].clone();
        runtime.transcripts.remove("child");
        runtime
            .metadata
            .reviews
            .push(mj_client::review::RuntimeReviewView {
                child_session_id: Some("child".into()),
                session_id: "parent".into(),
                phase: TurnReviewPhase::Running { roles: vec![] },
                roles: vec![],
                status: String::new(),
                verdict: None,
                questions: vec![mj_client::review::ReviewerQuestion {
                    role: "child".into(),
                    request: approval,
                }],
            });
        assert_eq!(
            project_states(&runtime, runtime.records.keys().cloned().collect()).holders["parent"],
            "child"
        );
        assert_eq!(
            project_states(&runtime, runtime.records.keys().cloned().collect()).holders["child"],
            "child"
        );
        // An ordinary reviewer question is the reviewed session's agent's.
        runtime.metadata.reviews[0].questions[0]
            .request
            .tool_permission = None;
        let holders = project_states(&runtime, runtime.records.keys().cloned().collect()).holders;
        assert!(!holders.contains_key("parent"));
        assert!(!holders.contains_key("grandparent"));
        runtime.metadata.reviews.clear();
        runtime.transcripts.insert(
            "child".into(),
            mj_client::runtime_feed::SessionTail::of(&materialized, &window, 12),
        );
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
