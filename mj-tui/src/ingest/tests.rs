use std::collections::BTreeMap;
use std::sync::Arc;

use mj_core::state::{
    MaterializedExecutionState, MaterializedSession, SessionState, State, TranscriptBody,
    TranscriptItem,
};

use mj_chat::chat::Notices;
use mj_core::targets::ProvisionStage;

use super::*;
use crate::test_support::*;

use crate::{DashboardState, SessionOperationKind};

#[test]
fn delayed_projection_and_summary_cannot_revive_acknowledged_content() {
    let mut dashboard = dashboard_with_session(running_session());
    let materialized = materialized_session_for(
        "session-1",
        vec![agent_message(2, "done"), work_interruption(3)],
    );
    let prepared = PreparedMaterializedSessionDetail::from_materialized(
        materialized.clone(),
        0,
        Default::default(),
    );
    let summary = PreparedMaterializedSessionSummary::from_materialized(
        MaterializedSessionSummary {
            session_id: "session-1".into(),
            applied_event_ordinal: 3,
            last_activity_at_ms: None,
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            last_agent_message: Some("done".into()),
            last_user_message: None,
            last_agent_message_follows_last_user: true,
            agent_message_latest_content_ordinals: vec![2],
            interruption_event_ordinals: vec![3],
        },
        0,
    );
    dashboard
        .state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 3;
    assert!(dashboard.apply_prepared_materialized_session_summary(summary));
    assert!(!dashboard.session_details["session-1"].has_unread());
    assert!(dashboard.apply_prepared_materialized_session(prepared));
    assert!(!dashboard.session_details["session-1"].has_unread());
    let newer = materialized_session_for(
        "session-1",
        vec![
            agent_message(2, "done"),
            work_interruption(3),
            agent_message(4, "new reply"),
        ],
    );
    dashboard.apply_materialized_session(&newer);
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        1
    );
}

#[test]
fn idle_restart_history_does_not_need_attention_or_notify() {
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.config.notify.mode = mj_core::config::NotifyMode::Terminal;
    let mut materialized =
        materialized_session_for("session-1", vec![session_restart(2), session_restart(3)]);
    materialized.execution = MaterializedExecutionState::Idle;
    dashboard.apply_materialized_session(&materialized);
    assert_eq!(
        dashboard.attention_level("session-1"),
        crate::AttentionLevel::Idle
    );
    assert!(dashboard.notification_events(0).is_empty());
    assert!(dashboard.notification_events(60_000).is_empty());
    assert!(!dashboard.session_details["session-1"].has_unread());
}

#[test]
fn unchanged_config_after_a_save_preserves_a_new_palette_and_its_query() {
    use crossterm::event::KeyCode;
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    let saved_config = dashboard.config.clone();
    open_palette(&mut dashboard);
    dashboard.handle_paste("container");
    dashboard.set_config(saved_config);
    assert!(matches!(dashboard.mode, crate::Mode::Palette(_)));
    // The query still selects the same command after the background reply.
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(dashboard.mode, crate::Mode::EditContainer(_)));
}

#[test]
fn changed_config_preserves_a_new_palette_and_its_query() {
    use crossterm::event::KeyCode;
    let mut dashboard = dashboard_with_session(running_session());
    let mut saved_config = dashboard.config.clone();
    saved_config.advanced.show_stopped_sessions = !saved_config.advanced.show_stopped_sessions;
    open_palette(&mut dashboard);
    dashboard.handle_paste("rename");
    dashboard.set_config(saved_config.clone());
    assert_eq!(dashboard.config, saved_config);
    assert!(matches!(dashboard.mode, crate::Mode::Palette(_)));
    dashboard.handle_key(key(KeyCode::Enter));
    assert!(matches!(dashboard.mode, crate::Mode::Rename(_)));
}

#[test]
fn resume_is_projected_into_active_while_background_work_runs() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);

    assert_eq!(
        dashboard.state.sessions["session-1"].state,
        SessionState::Stopped
    );
    assert_eq!(dashboard.ordered_sessions().len(), 1);
    assert_eq!(
        dashboard.session_operations["session-1"].kind,
        SessionOperationKind::Resuming
    );
}

#[test]
fn a_completed_remote_launch_restores_the_ready_row_without_another_record_change() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Launching, None);
    let mut ready = stopped_session();
    ready.state = SessionState::Running;
    let mut state = dashboard.state.clone();
    state.sessions.insert(ready.id.clone(), ready);
    dashboard.set_state(state);
    assert!(dashboard.transition_kind("session-1").is_some());
    dashboard.finish_session_operation("session-1");
    assert_eq!(dashboard.transition_kind("session-1"), None);
    assert_eq!(dashboard.ordered_sessions()[0].state, SessionState::Running);
    dashboard.select_active_session("session-1");
    assert!(matches!(
        dashboard.open_selected_session(),
        crate::DashboardAction::Open { .. }
    ));
}

#[test]
fn stopped_cleanup_removes_history_after_its_owner_finishes() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Suspending, None);
    assert_eq!(dashboard.ordered_sessions().len(), 1);
    assert_eq!(
        dashboard.state.sessions["session-1"].state,
        SessionState::Stopped
    );
    dashboard.finish_session_operation("session-1");
    assert_eq!(dashboard.ordered_sessions().len(), 0);
}

#[test]
fn notice_replacement_does_not_overwrite_a_newer_notice() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_notice("Refreshing profile quotas…");
    assert!(dashboard.replace_notice_if("Refreshing profile quotas…", "Profile quotas refreshed."));
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Profile quotas refreshed.")
    );

    dashboard.set_notice("A later operation failed");
    assert!(
        !dashboard.replace_notice_if("Refreshing profile quotas…", "Profile quotas refreshed.")
    );
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("A later operation failed")
    );
}

#[test]
fn runtime_review_projection_restores_and_removes_session_activity() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let review = mj_client::review::RuntimeReviewView {
        session_id: "session-1".into(),
        tier: mj_core::review::lanes::ReviewTier::Quick,
        phase: mj_core::review::driver::TurnReviewPhase::LaunchingReviewer,
        roles: Vec::new(),
        status: "starting the reviewer…".into(),
        verdict: None,
    };

    dashboard.set_session_reviews([review]);
    assert!(dashboard.session_review("session-1").is_some());

    // Runtime snapshots are complete projections: an empty replacement
    // closes the badge rather than leaving the previous review stuck.
    dashboard.set_session_reviews(Vec::new());
    assert!(dashboard.session_review("session-1").is_none());
}

/// The dashboard and every other view (chat, background workers) share
/// one notifications bar: a clone installed with `share_notices` sees
/// what the dashboard sets, and the dashboard sees what the clone sets.
#[test]
fn a_shared_notice_is_visible_through_every_clone_of_the_handle() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    let shared = Notices::default();
    dashboard.share_notices(shared.clone());

    dashboard.set_notice("Background import finished");
    assert_eq!(
        shared.current().as_deref(),
        Some("Background import finished")
    );

    shared.clear();
    assert_eq!(dashboard.notice(), None);

    shared.set("Quota refresh finished");
    assert_eq!(
        dashboard.notice().as_deref(),
        Some("Quota refresh finished")
    );
}

#[test]
fn unread_count_uses_logical_agent_positions_after_the_detach_cursor() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    apply_materialized_transcript(
        &mut dashboard,
        vec![
            agent_message(1, "first message"),
            thought(3, "thinking"),
            agent_message(4, "second message"),
        ],
    );

    let detail = dashboard.session_details.get("session-1").unwrap();
    assert_eq!(detail.unread_agent_messages, 2);

    let mut state = dashboard.state.clone();
    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 1;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        1
    );

    let mut state = dashboard.state.clone();
    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 4;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        0
    );
}

#[test]
fn full_materialized_projection_carries_pending_questions_into_session_detail() {
    let mut session = materialized_session_for("session-1", Vec::new());
    let request = ElicitationRequest::from_acp_params(
        "request-1",
        serde_json::json!({
            "mode": "form",
            "sessionId": "session-1",
            "message": "Choose a path",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }
        }),
    )
    .expect("valid test question");
    session.pending_elicitations = vec![request.clone()];
    let mut dashboard = dashboard_with_session(running_session());

    dashboard.apply_materialized_session(&session);

    assert_eq!(
        dashboard.session_details["session-1"].pending_elicitations,
        vec![request]
    );
    assert_eq!(
        dashboard.attention_level("session-1"),
        crate::AttentionLevel::Waiting
    );

    let mut answered = session;
    answered.applied_event_ordinal += 1;
    answered.pending_elicitations.clear();
    dashboard.apply_materialized_session(&answered);
    assert!(
        dashboard.session_details["session-1"]
            .pending_elicitations
            .is_empty()
    );
    assert_ne!(
        dashboard.attention_level("session-1"),
        crate::AttentionLevel::Waiting
    );
}

#[test]
fn pending_question_ordinal_stays_paired_when_a_newer_summary_arrives() {
    let mut session = materialized_session_for("session-1", Vec::new());
    session.applied_event_ordinal = 5;
    session.pending_elicitations = vec![
        ElicitationRequest::from_acp_params(
            "request-1",
            serde_json::json!({
                "mode": "form",
                "sessionId": "session-1",
                "message": "Choose a path",
                "requestedSchema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}}
                }
            }),
        )
        .expect("valid test question"),
    ];
    let mut dashboard = dashboard_with_session(running_session());
    dashboard.apply_materialized_session(&session);
    assert_eq!(
        dashboard
            .pending_elicitations("session-1")
            .map(|(ordinal, _)| ordinal),
        Some(5)
    );

    let summary = MaterializedSessionSummary {
        session_id: "session-1".into(),
        applied_event_ordinal: 6,
        last_activity_at_ms: None,
        execution: MaterializedExecutionState::Idle,
        session_title: None,
        last_agent_message: None,
        last_user_message: None,
        last_agent_message_follows_last_user: false,
        agent_message_latest_content_ordinals: Vec::new(),
        interruption_event_ordinals: Vec::new(),
    };
    dashboard.apply_prepared_materialized_session_summary(
        PreparedMaterializedSessionSummary::from_materialized(summary, 0),
    );
    assert_eq!(
        dashboard
            .pending_elicitations("session-1")
            .map(|(ordinal, _)| ordinal),
        Some(5)
    );
}

#[test]
fn interruption_marker_is_unread_until_the_existing_cursor_passes_it() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    let mut materialized = materialized_session_for("session-1", vec![work_interruption(3)]);
    materialized.execution = MaterializedExecutionState::Idle;
    dashboard.apply_materialized_session(&materialized);

    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.unread_agent_messages, 0);
    assert_eq!(detail.unread_interruptions, 1);
    assert!(detail.has_unread());
    assert!(detail.current_turn_started_at.is_none());

    let mut state = dashboard.state.clone();
    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 3;
    dashboard.set_state(state);
    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.unread_interruptions, 0);
    assert!(!detail.has_unread());
}

#[test]
fn adjacent_interruption_markers_keep_each_ordinal_for_unread_tracking() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    let mut materialized = materialized_session_for(
        "session-1",
        vec![work_interruption(3), work_interruption(4)],
    );
    materialized.applied_event_ordinal = 4;
    dashboard.apply_materialized_session(&materialized);

    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.interruption_event_ordinals, [3, 4]);
    assert_eq!(detail.unread_interruptions, 2);

    let mut state = dashboard.state.clone();
    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 3;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.session_details["session-1"].unread_interruptions, 1,
        "reading through the older marker leaves the newer ordinal unread"
    );
}

#[test]
fn materialized_message_update_does_not_duplicate_unread_count() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    let mut initial = materialized_session_for("session-1", vec![agent_message(1, "first ")]);
    initial
        .queued_prompts
        .push(mj_core::state::MaterializedQueuedPrompt {
            accepted_ordinal: None,
            command_id: "queued-1".into(),
            kind: mj_core::state::QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({ "type": "text", "text": "next task" })],
            queued_at_ms: 0,
        });
    dashboard.apply_materialized_session(&initial);

    let mut state = dashboard.state.clone();
    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 1;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        0
    );

    let mut updated = agent_message(1, "first continuation");
    Arc::make_mut(&mut updated).latest_content_event_ordinal = Some(2);
    Arc::make_mut(&mut updated).last_changed_at_ms = 2_000;
    let mut projection = materialized_session_for("session-1", vec![updated]);
    projection.applied_event_ordinal = 2;
    dashboard.apply_materialized_session(&projection);

    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.unread_agent_messages, 1);
    assert_eq!(
        detail.last_agent_message.as_deref(),
        Some("first continuation")
    );
    assert!(detail.queued_prompts.is_empty());

    let mut state = dashboard.state.clone();
    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 2;
    dashboard.set_state(state);
    assert_eq!(
        dashboard.session_details["session-1"].unread_agent_messages,
        0
    );
}

#[test]
fn prepared_materialized_session_drops_stale_ordinals() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    let mut latest = materialized_session_for("session-1", vec![agent_message(2, "latest")]);
    latest.applied_event_ordinal = 2;
    let mut stale = materialized_session_for("session-1", vec![agent_message(1, "stale")]);
    stale.applied_event_ordinal = 1;

    assert!(dashboard.apply_prepared_materialized_session(
        PreparedMaterializedSessionDetail::from_materialized(
            latest,
            0,
            MaterializedProjectionCache::default(),
        ),
    ));
    assert!(!dashboard.apply_prepared_materialized_session(
        PreparedMaterializedSessionDetail::from_materialized(
            stale,
            0,
            MaterializedProjectionCache::default(),
        ),
    ));

    assert_eq!(
        dashboard.session_details["session-1"]
            .last_agent_message
            .as_deref(),
        Some("latest")
    );
}

#[test]
fn stored_summary_restores_dashboard_messages_without_marking_transcript_ready() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    session.viewed_through_event_ordinal = 3;
    let mut dashboard = dashboard_with_session(session);
    let summary = MaterializedSessionSummary {
        session_id: "session-1".into(),
        applied_event_ordinal: 7,
        last_activity_at_ms: Some(8_000),
        execution: MaterializedExecutionState::Running {
            started_at_ms: 4_000,
        },
        session_title: Some("Persisted title".into()),
        last_agent_message: Some("Persisted answer".into()),
        last_user_message: Some("Persisted question".into()),
        last_agent_message_follows_last_user: true,
        agent_message_latest_content_ordinals: vec![2, 5, 7],
        interruption_event_ordinals: vec![6],
    };

    assert!(dashboard.apply_prepared_materialized_session_summary(
        PreparedMaterializedSessionSummary::from_materialized(summary, 3),
    ));

    let detail = &dashboard.session_details["session-1"];
    assert_eq!(
        detail.last_user_message.as_deref(),
        Some("Persisted question")
    );
    assert_eq!(
        detail.last_agent_message.as_deref(),
        Some("Persisted answer")
    );
    assert_eq!(detail.unread_agent_messages, 2);
    assert_eq!(detail.unread_interruptions, 1);
    assert!(detail.last_agent_message_follows_last_user);
    assert_eq!(detail.current_turn_started_at, Some(4));
    assert_eq!(detail.transcript_hydration, TranscriptHydration::Loading);
    assert!(detail.transcript.is_none());
    assert_eq!(
        dashboard.state.sessions["session-1"]
            .acp_session_title
            .as_deref(),
        Some("Persisted title")
    );
}

#[test]
fn late_startup_summary_does_not_erase_live_tool_preview_at_the_same_frontier() {
    let mut dashboard = dashboard_with_session(running_session());
    let snapshot = materialized_session_for("session-1", vec![thought(2, "Checking files")]);
    dashboard.apply_materialized_session(&snapshot);
    let summary = MaterializedSessionSummary {
        session_id: "session-1".into(),
        applied_event_ordinal: snapshot.applied_event_ordinal,
        last_activity_at_ms: snapshot.last_activity_at_ms,
        execution: snapshot.execution,
        session_title: None,
        last_agent_message: None,
        last_user_message: None,
        last_agent_message_follows_last_user: false,
        agent_message_latest_content_ordinals: vec![],
        interruption_event_ordinals: vec![],
    };
    assert!(!dashboard.apply_prepared_materialized_session_summary(
        PreparedMaterializedSessionSummary::from_materialized(summary, 0),
    ));
    assert_eq!(
        dashboard.session_details["session-1"]
            .latest_agent_activity_after_last_user
            .as_deref(),
        Some("Checking files")
    );
}

#[test]
fn stored_summary_elides_hidden_context_from_the_name_and_user_preview() {
    let mut session = stopped_session();
    session.acp_session_title = None;
    let mut dashboard = dashboard_with_session(session);
    let summary = MaterializedSessionSummary {
        session_id: "session-1".into(),
        applied_event_ordinal: 2,
        last_activity_at_ms: Some(2_000),
        execution: MaterializedExecutionState::Idle,
        session_title: Some("<hel-project-memory>private and truncated".into()),
        last_agent_message: None,
        last_user_message: Some(
            concat!(
                "<hel-project-memory>private</hel-project-memory>\n",
                "Visible question"
            )
            .into(),
        ),
        last_agent_message_follows_last_user: false,
        agent_message_latest_content_ordinals: vec![],
        interruption_event_ordinals: vec![],
    };

    assert!(dashboard.apply_prepared_materialized_session_summary(
        PreparedMaterializedSessionSummary::from_materialized(summary, 0),
    ));

    assert_eq!(
        dashboard.session_details["session-1"]
            .last_user_message
            .as_deref(),
        Some("Visible question")
    );
    assert_eq!(
        dashboard.state.sessions["session-1"].acp_session_title,
        None
    );
}

/// Rewrites one agent message the way the projection does: the item is
/// copied, so every other handle in the transcript survives.
fn set_agent_text(item: &mut Arc<TranscriptItem>, text: &str, content_ordinal: u64) {
    let item = Arc::make_mut(item);
    item.body = TranscriptBody::Agent {
        chunks: vec![serde_json::json!({
            "content": {"type": "text", "text": text}
        })],
        streaming: false,
    };
    item.latest_content_event_ordinal = Some(content_ordinal);
    item.last_changed_at_ms = i64::try_from(content_ordinal).unwrap() * 1_000;
}

/// The projection reuses per-item results across updates, so every shape
/// of transcript change must land where a full rescan would.
#[test]
fn incremental_projection_matches_a_full_rescan_through_transcript_changes() {
    let viewed_through_event_ordinal = 1;
    // One transcript, changed the way the projection changes it: items are
    // appended, and an item that changes is replaced by a copy while the
    // rest keep their handles.
    let mut transcript: Vec<Arc<TranscriptItem>> = Vec::new();
    let mut updates = vec![transcript.clone()];
    transcript.push(agent_message(1, "first"));
    transcript.push(thought(2, "thinking"));
    updates.push(transcript.clone());
    transcript.push(agent_message(3, "answer"));
    updates.push(transcript.clone());
    // More content streams into the tail message.
    set_agent_text(&mut transcript[2], "answer, at length", 4);
    updates.push(transcript.clone());
    // The tail message loses its text, so the previous answer no longer
    // holds and the earlier items have to decide it.
    set_agent_text(&mut transcript[2], "   ", 5);
    updates.push(transcript.clone());
    // An item inside the unchanged prefix changes.
    set_agent_text(&mut transcript[0], "first, corrected", 6);
    updates.push(transcript.clone());
    transcript.push(work_interruption(7));
    updates.push(transcript.clone());
    // A restore rebuilds every item, sharing no handles.
    transcript = vec![agent_message(1, "restored"), agent_message(2, "and again")];
    updates.push(transcript.clone());
    // A checkpoint restore leaves a shorter transcript.
    transcript.truncate(1);
    updates.push(transcript);

    let mut cache = MaterializedProjectionCache::default();
    for (index, transcript) in updates.into_iter().enumerate() {
        let session = materialized_session_for("session-1", transcript);
        let incremental = PreparedMaterializedSessionDetail::from_materialized(
            session.clone(),
            viewed_through_event_ordinal,
            cache,
        );
        let rescanned = PreparedMaterializedSessionDetail::from_materialized(
            session,
            viewed_through_event_ordinal,
            MaterializedProjectionCache::default(),
        );
        assert_eq!(
            incremental.last_agent_message, rescanned.last_agent_message,
            "last agent message after update {index}"
        );
        assert_eq!(
            incremental.agent_message_latest_content_ordinals,
            rescanned.agent_message_latest_content_ordinals,
            "agent ordinals after update {index}"
        );
        assert_eq!(
            incremental.unread_agent_messages, rescanned.unread_agent_messages,
            "unread count after update {index}"
        );
        assert_eq!(
            incremental.interruption_event_ordinals, rescanned.interruption_event_ordinals,
            "interruption ordinals after update {index}"
        );
        assert_eq!(
            incremental.unread_interruptions, rescanned.unread_interruptions,
            "unread interruption count after update {index}"
        );
        cache = incremental.projection;
    }
}

#[test]
fn projection_cache_keeps_terminal_diffstats_across_unrelated_updates() {
    let tool = Arc::new(TranscriptItem {
        stable_id: "tool:edit".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 2,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "edit",
                "title": "Edit src/lib.rs",
                "status": "completed",
                "content": [{
                    "type": "diff",
                    "path": "/workspace/src/lib.rs",
                    "oldText": "alpha\n",
                    "newText": "alpha\nbeta\n"
                }]
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        },
    });
    let first = PreparedMaterializedSessionDetail::from_materialized(
        materialized_session_for("session-1", vec![tool.clone()]),
        0,
        MaterializedProjectionCache::default(),
    );
    assert_eq!(first.projection.tool_diffstats.len(), 1);

    let second = PreparedMaterializedSessionDetail::from_materialized(
        materialized_session_for(
            "session-1",
            vec![tool, agent_message(2, "unrelated update")],
        ),
        0,
        first.projection,
    );
    assert_eq!(second.projection.tool_diffstats.len(), 1);
    assert_eq!(
        second.transcript.browser_transcript(None).entries[0].lines,
        ["Edit", "/workspace/src/lib.rs  +1 −0"]
    );
}

/// Unchanged items keep their handles, so a projection that follows one
/// only reads the items that changed.
#[test]
fn projection_rereads_only_the_changed_tail() {
    let head = vec![agent_message(1, "first"), thought(2, "thinking")];
    let mut transcript = head.clone();
    transcript.push(agent_message(3, "answer"));
    let first = PreparedMaterializedSessionDetail::from_materialized(
        materialized_session_for("session-1", transcript.clone()),
        0,
        MaterializedProjectionCache::default(),
    );

    transcript.push(agent_message(4, "and more"));
    assert_eq!(
        first.projection.unchanged_prefix(&transcript),
        3,
        "appending leaves the earlier items untouched"
    );

    let mut streamed = transcript.clone();
    Arc::make_mut(&mut streamed[3]).last_changed_at_ms = 9_000;
    assert_eq!(
        first.projection.unchanged_prefix(&streamed),
        3,
        "a copy-on-write update only breaks the item it touches"
    );

    let restored = vec![agent_message(1, "first"), thought(2, "thinking")];
    assert_eq!(
        first.projection.unchanged_prefix(&restored),
        0,
        "rebuilt items share nothing, so everything is read again"
    );
}

#[test]
fn later_non_agent_items_do_not_replace_the_last_agent_response() {
    let mut session = stopped_session();
    session.state = SessionState::Running;
    let mut dashboard = dashboard_with_session(session);
    apply_materialized_transcript(
        &mut dashboard,
        vec![
            agent_message(
                1,
                "The container lacked uv, so validation used Python 3 directly.",
            ),
            thought(2, "Checking the result"),
        ],
    );

    assert_eq!(
        dashboard.session_details["session-1"]
            .last_agent_message
            .as_deref(),
        Some("The container lacked uv, so validation used Python 3 directly.")
    );
}

#[test]
fn transcript_without_user_entries_keeps_agent_activity_for_the_sidebar() {
    let mut dashboard = dashboard_with_session(running_session());
    apply_materialized_transcript(&mut dashboard, vec![thought(2, "Checking the workspace")]);
    let detail = &dashboard.session_details["session-1"];
    assert!(detail.last_user_message.is_none());
    assert_eq!(
        detail.latest_agent_activity_after_last_user.as_deref(),
        Some("Checking the workspace")
    );
    apply_materialized_transcript(&mut dashboard, vec![agent_message(3, "The checks passed")]);
    let detail = &dashboard.session_details["session-1"];
    assert!(detail.last_user_message.is_none());
    assert!(detail.last_agent_message_follows_last_user);
    assert_eq!(
        detail.last_agent_message.as_deref(),
        Some("The checks passed")
    );
}

#[test]
fn latest_user_message_tracks_whether_the_agent_has_replied() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut transcript = numbered_conversation(1);
    transcript.push(transcript_item(
        3,
        TranscriptBody::User {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "follow-up question"
            })],
        },
    ));
    transcript.push(thought(4, "checking the workspace"));
    apply_materialized_transcript(&mut dashboard, transcript.clone());

    let detail = &dashboard.session_details["session-1"];
    assert_eq!(detail.last_agent_message.as_deref(), Some("answer 0"));
    assert_eq!(
        detail.last_user_message.as_deref(),
        Some("follow-up question")
    );
    assert!(!detail.last_agent_message_follows_last_user);
    assert_eq!(
        detail.latest_agent_activity_after_last_user.as_deref(),
        Some("checking the workspace")
    );

    transcript.push(transcript_item(
        5,
        TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "test",
                "title": "Inspect src/lib.rs",
                "status": "in_progress"
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
            presentation: None,
        },
    ));
    apply_materialized_transcript(&mut dashboard, transcript.clone());
    assert_eq!(
        dashboard.session_details["session-1"]
            .latest_agent_activity_after_last_user
            .as_deref(),
        Some("Inspect src/lib.rs")
    );

    transcript.push(agent_message(6, "follow-up answer"));
    apply_materialized_transcript(&mut dashboard, transcript);
    assert!(dashboard.session_details["session-1"].last_agent_message_follows_last_user);
}

#[test]
fn materialized_idle_state_clears_a_stale_turn_clock() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut running = MaterializedSession::empty("session-1");
    running.execution = MaterializedExecutionState::Running {
        started_at_ms: 1_000_000,
    };
    dashboard.apply_materialized_session(&running);
    let idle = MaterializedSession::empty("session-1");
    dashboard.apply_materialized_session(&idle);

    assert_eq!(
        dashboard.session_details["session-1"].current_turn_started_at,
        None
    );
}

#[test]
fn materialized_running_state_starts_clock_without_transcript_events() {
    let mut dashboard = dashboard_with_session(stopped_session());
    let mut running = MaterializedSession::empty("session-1");
    running.execution = MaterializedExecutionState::Running {
        started_at_ms: 1_000_000,
    };
    dashboard.apply_materialized_session(&running);

    assert_eq!(
        dashboard.session_details["session-1"].current_turn_started_at,
        Some(1_000)
    );
}

#[test]
fn daemon_operation_snapshot_preserves_remote_clocks_and_stages() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_session_operation_at(
        "session-1".into(),
        SessionOperationKind::Resuming,
        None,
        123,
    );
    dashboard.replace_session_operation_stages(
        "session-1",
        [
            (ProvisionStage::Cloning, 456),
            (ProvisionStage::Syncing, 789),
        ],
    );

    let operation = &dashboard.session_operations["session-1"];
    assert_eq!(operation.started_at_epoch_seconds, 123);
    assert_eq!(operation.active_stages[&ProvisionStage::Cloning], 456);
    assert_eq!(operation.active_stages[&ProvisionStage::Syncing], 789);
}

#[test]
fn set_resume_destination_updates_the_in_flight_operation() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Resuming, None);
    dashboard.set_resume_destination("session-1", "grok-1".into(), "localhost".into());

    assert_eq!(
        dashboard.session_operations["session-1"].resume_destination,
        Some(("grok-1".to_string(), "localhost".to_string()))
    );
}

#[test]
fn set_resume_destination_for_an_unknown_session_is_ignored() {
    let mut dashboard = DashboardState::new(config(), State::default(), BTreeMap::new());
    dashboard.set_resume_destination("missing", "grok-1".into(), "localhost".into());
    assert!(dashboard.session_operations.is_empty());
}

#[test]
fn transition_kind_prefers_operations_and_import_does_not_hide_chat() {
    use mj_core::state::SessionTransitionKind;

    let mut session = stopped_session();
    session.state = SessionState::Provisioning;
    let mut dashboard = dashboard_with_session(session);

    assert_eq!(
        dashboard.transition_kind("session-1"),
        Some(SessionTransitionKind::Starting)
    );
    dashboard.begin_session_operation("session-1".into(), SessionOperationKind::Importing, None);
    assert_eq!(dashboard.transition_kind("session-1"), None);
}

#[test]
fn failed_closing_record_is_recovery_status_not_an_active_transition() {
    let mut session = stopped_session();
    session.state = SessionState::Closing;
    session.last_error = Some("checkpoint copy failed".into());
    let dashboard = dashboard_with_session(session);

    assert_eq!(dashboard.transition_kind("session-1"), None);
    assert_eq!(
        dashboard.transition_failure_kind("session-1"),
        Some(mj_core::state::SessionTransitionKind::Suspending)
    );
}

#[test]
fn inferred_input_request_clears_when_a_later_prompt_starts() {
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.last_turn_outcome = Some(mj_core::state::MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: "prompt".into(),
        accepted_ordinal: Some(1),
        turn_start_position: Some(2),
        completed_ordinal: 3,
        completed_at_ms: 1000,
        outcome: mj_core::state::TurnOutcomeKind::Completed {
            stop_reason: mj_core::acp::AWAITING_INPUT_STOP_REASON.into(),
        },
    });
    let prepare = |session| {
        PreparedMaterializedSessionDetail::from_materialized(session, 0, Default::default())
    };
    assert!(prepare(materialized.clone()).awaiting_input);
    materialized.active_turn = Some(mj_core::state::MaterializedTurn {
        command_id: "next".into(),
        accepted_ordinal: Some(4),
        turn_start_position: 5,
        started_at_ms: 2000,
        steered_into: None,
    });
    assert!(!prepare(materialized).awaiting_input);
}
