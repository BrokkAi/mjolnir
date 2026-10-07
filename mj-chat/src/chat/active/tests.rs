use super::*;

use crate::chat::test_support::{
    agent_message_item, agent_transcript_item, drawn_transcript, fast_mode_option, queued, snapshot,
};
use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelect, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionConfigValueId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use mj_core::elicitation::{ElicitationField, ElicitationFieldKind, ElicitationRequest};
use mj_core::relay::RELAY_EVENT_GENESIS_DIGEST;
use mj_core::relay::{SequencedEvent, WorkerEvent};
use mj_core::transcript::ChatRole;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use std::collections::BTreeMap;

// Hard-won: 61a1cfa: Codex command updates were lost during replay, hiding worker-owned goal controls
#[tokio::test]
async fn live_goal_commands_follow_worker_state_despite_a_damaged_projection() {
    let mut view = managed_view(MaterializedSession::empty("goal-commands"));
    view.snapshot.as_mut().unwrap().operational.goal = serde_json::from_value(serde_json::json!({
        "known": true,
        "capability": {"version": 1, "controlMethod": "_session/goal", "actions": ["pause", "resume", "clear"]},
        "snapshot": {"objective": "finish", "status": "paused"},
        "execution": {"version": 1, "status": "idle"}
    })).unwrap();
    // Older workers can also have lost the base command advertisement.
    assert!(
        view.snapshot
            .as_ref()
            .unwrap()
            .operational
            .available_commands
            .is_empty()
    );
    let fixture = mj_client::session::replacement_session_test_fixture("goal-commands", 12);
    fixture.replacement_view.send_replace(view.clone());
    let session = fixture.control.session("goal-commands").await.unwrap();
    let mut active = ActiveChat::open(
        session,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let chat = &mut active.state;
    chat.set_input("/goal resume".into());
    assert!(matches!(
        chat.submit_input(),
        ChatAction::GoalControl {
            action: mj_core::goal::GoalControlAction::Resume
        }
    ));
    for _ in 0..2 {
        assert!(apply_session_view(chat, Ok(view.clone())));
        chat.set_input("/goal finish the work".into());
        assert!(
            matches!(chat.submit_input(), ChatAction::Prompt(text) if text == "/goal finish the work")
        );
        chat.set_input("/goal resume".into());
        assert!(matches!(
            chat.submit_input(),
            ChatAction::GoalControl {
                action: mj_core::goal::GoalControlAction::Resume
            }
        ));
        chat.set_input("/unknown-command".into());
        assert!(matches!(chat.submit_input(), ChatAction::None));
    }
    // A newly connected adapter owns capabilities too; do not retain old controls.
    view.snapshot.as_mut().unwrap().operational.goal = Default::default();
    apply_session_view(chat, Ok(view));
    chat.set_input("/goal finish the work".into());
    assert!(matches!(chat.submit_input(), ChatAction::None));
}

#[tokio::test]
async fn handle_event_result_reports_which_events_the_chat_consumed() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-event-result", 72);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );

    let moved = chat.handle_event_result(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(!moved.consumed);

    let ignored = chat.handle_event_result(Event::Key(KeyEvent::new(
        KeyCode::F(12),
        KeyModifiers::NONE,
    )));
    assert!(!ignored.consumed);

    let typed = chat.handle_event_result(Event::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    )));
    assert!(typed.consumed);
    assert_eq!(chat.draft(), "x");

    // A cursor move and a cursor move that is already clamped are both the
    // composer's to answer, so neither reaches the dashboard behind it.
    let moved_cursor =
        chat.handle_event_result(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
    assert!(moved_cursor.consumed);

    let clamped =
        chat.handle_event_result(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
    assert!(clamped.consumed);
}

#[test]
fn a_disconnected_view_without_a_snapshot_stops_stale_animation() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.turn_started_at_epoch_seconds = Some(1);
    assert!(chat.needs_animation());
    apply_session_view(
        &mut chat,
        Ok(ManagedSessionView {
            snapshot: None,
            connected: false,
            error: None,
        }),
    );
    assert!(!chat.needs_animation());
}

/// Captures the real conversation renderer for visual review. The caller
/// chooses an artifact path; ordinary test runs never write screenshots.
#[test]
#[ignore = "writes a terminal-cell capture to MJ_CHAT_CAPTURE_PATH"]
fn capture_chat_preview() {
    let path = std::env::var_os("MJ_CHAT_CAPTURE_PATH")
        .expect("set MJ_CHAT_CAPTURE_PATH to the preview JSON path");
    let dimension = |name, fallback| {
        std::env::var_os(name)
            .map(|value| {
                value
                    .to_str()
                    .expect("capture dimensions must be Unicode")
                    .parse::<u16>()
                    .expect("capture dimensions must be unsigned integers")
            })
            .unwrap_or(fallback)
    };
    let columns = dimension("MJ_CHAT_CAPTURE_COLUMNS", 110);
    let rows = dimension("MJ_CHAT_CAPTURE_ROWS", 40);
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_header_summary("local / mjolnir", "Claude · Sonnet", "");
    chat.mark_prompt_submitted("Make the terminal feel beautifully crafted.");
    chat.turn_started_at_epoch_seconds = Some(mj_core::clock::epoch_seconds().saturating_sub(42));
    chat.set_current_step_start(Some(mj_core::clock::epoch_millis().saturating_sub(7_000)));
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        checking_response: false,
        execution: Some(mj_core::relay::RelayExecutionState::Running),
        ..Default::default()
    });
    let tool = |seq, title: &str, summary: &str, status| {
        let mut entry = ChatEntry::tool(seq, title, None, status);
        entry.tool_summary = Some(summary.to_owned());
        entry
    };
    chat.entries = vec![
        ChatEntry::plain(
            1,
            ChatRole::User,
            "Make the terminal feel beautifully crafted. Keep it fast, readable, and calm.",
        ),
        ChatEntry::plain(
            2,
            ChatRole::Agent,
            "I’m bringing the interface together around a midnight palette, clear hierarchy, and the original animated activity indicators.\n\n### A little more room to think\n\n- Focus follows a soft teal border\n- **Your conversation stays readable** while tools work\n- Code and keyboard shortcuts have their own quiet surfaces",
        ),
        tool(
            3,
            "cd dir && python x.py | cat | wc ; print ok",
            "cd && python | cat | wc ; print",
            mj_core::transcript::ToolStatus::Completed,
        ),
        tool(
            4,
            "cargo test -p brokk-mj-chat",
            "cargo test",
            mj_core::transcript::ToolStatus::Completed,
        ),
        ChatEntry::plain(
            5,
            ChatRole::Agent,
            "The shared theme is in place. Here’s the panel style used throughout the app:\n\n```rust\nlet panel = theme::panel(focused)\n    .title(\" Conversation \");\n```\n\nI’m checking the narrow layouts and selection behavior now.",
        ),
        tool(
            6,
            "cargo clippy --all-targets -- -D warnings",
            "cargo clippy",
            mj_core::transcript::ToolStatus::Running,
        ),
    ];
    // Keep the capture representative of the in-place tool expansion:
    // the first completed call opens to its provider title and splits the
    // surrounding completed streak.
    chat.expanded_tool_calls.insert(3);
    let mut terminal = Terminal::new(TestBackend::new(columns, rows)).expect("terminal");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("render preview");
    let buffer = terminal.backend().buffer();
    let color = |color, fallback| match color {
        ratatui::style::Color::Rgb(r, g, b) => [r, g, b],
        _ => fallback,
    };
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| {
                    let cell = &buffer[(x, y)];
                    serde_json::json!({
                        "text": cell.symbol(),
                        "fg": color(cell.fg, [223, 235, 244]),
                        "bg": color(cell.bg, [11, 18, 32]),
                        "bold": cell.modifier.contains(ratatui::style::Modifier::BOLD),
                        "italic": cell.modifier.contains(ratatui::style::Modifier::ITALIC),
                        "underline": cell.modifier.contains(ratatui::style::Modifier::UNDERLINED),
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let capture = serde_json::json!({ "width": buffer.area.width, "height": buffer.area.height, "rows": rows });
    std::fs::write(path, serde_json::to_vec(&capture).expect("encode preview"))
        .expect("write preview");
}

fn prepare_storage_test_chat() -> PreparedChat {
    let fixture = mj_client::session::replacement_session_test_fixture("review-storage", 1);
    ActiveChat::prepare_with_persistence(
        fixture.stopped,
        "bundle",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
        None,
    )
}

#[tokio::test]
async fn client_review_storage_failure_is_visible_without_preventing_chat_open() {
    let mut chat = prepare_storage_test_chat()
        .with_review_state(Err("database read failed".into()))
        .open();
    assert!(
        chat.state
            .notice()
            .unwrap()
            .contains("database read failed")
    );
    chat.state.set_input("still editable".into());
    assert_eq!(chat.draft(), "still editable");
    assert!(chat.state.second_opinion().is_none());
}

#[tokio::test]
async fn replacement_chat_preserves_the_latest_same_session_draft_even_when_cleared() {
    fn prepare(id: &str, draft: &str) -> PreparedChat {
        let fixture = mj_client::session::replacement_session_test_fixture(id, 89);
        ActiveChat::prepare_with_persistence(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            draft.into(),
            Notices::default(),
            None,
        )
    }
    let mut previous = prepare("moving", "saved before preparation").open();
    let pending = prepare("moving", "stale saved draft");
    previous.state.set_input("edited during preparation".into());
    let mut replacement = pending.open_replacing(Some(&previous));
    assert_eq!(replacement.draft(), "edited during preparation");

    replacement.state.clear_input();
    let replacement =
        prepare("moving", "stale draft must not return").open_replacing(Some(&replacement));
    assert!(replacement.draft().is_empty());
    let different = prepare("other", "other session draft").open_replacing(Some(&previous));
    assert_eq!(different.draft(), "other session draft");
}

fn managed_view(session: MaterializedSession) -> ManagedSessionView {
    let session_id = session.session_id.clone();
    let latest_ordinal = session.applied_event_ordinal;
    let latest_digest = session.applied_event_digest.clone();
    ManagedSessionView {
        snapshot: Some(mj_core::state::ManagedSessionSnapshot {
            window: mj_core::state::ProjectionWindow::of(&session),
            materialized: session,
            latest_credential_sync_signal: None,
            worker_build: None,
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
            operational: mj_core::relay::RelayOperationalState {
                assessment: None,
                assessment_context: None,
                turn_completion: None,
                continuation: Default::default(),
                relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
                native_agents: Vec::new(),
                steering: None,
                cancelling_prompt_id: None,
                clear_context: false,
                clear_context_started_at_ms: None,
                native_agent_count: 0,
                expected_continuation: None,
                inferred_idle_since_ms: None,
                task_settled_at_ms: None,
                background_needed: None,
                goal: Default::default(),

                capacity_retry: None,
                retry_assessment_pending: false,
                activity_turn_started_at_ms: None,
                store_id: None,
                idle_since_ms: None,
                session_id,
                execution: mj_core::relay::RelayExecutionState::Idle,
                latest_ordinal,
                latest_digest: latest_digest.clone(),
                acknowledged_through: latest_ordinal,
                acknowledged_digest: latest_digest,
                recovery_floor_ordinal: 0,
                recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
                native_session_id: None,
                native_continuity_lost: false,
                replaced_unused_native_session_id: None,
                checkpoint_only: false,
                acp_ready: None,
                harness_preparation: None,
                agent_capabilities: None,
                agent_info: None,
                runtime: None,
                steering_supported: None,
                config_options: Vec::new(),
                modes: None,
                available_commands: Vec::new(),
                config: BTreeMap::new(),
                active_prompt: None,
                queued_prompts: Vec::new(),
                active_user_shells: Vec::new(),
                active_agent_terminals: Vec::new(),
                checkpoint_barrier: None,
                checkpoint_ready: None,
                last_acp_activity_at_ms: None,
                current_step_started_at_ms: None,
                foreground_tool_started_at_ms: None,
                tools_in_flight: Vec::new(),
                activity: None,
                harness_turn: None,
                last_harness_turn_started_ordinal: None,
                background_commands: Vec::new(),
                background_work_known: None,
            },
        }),
        connected: true,
        error: None,
    }
}
#[test]
fn detaching_leaves_the_unsent_input_where_the_dashboard_saves_it_from() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("half typed thought".into());
    let mut session = MaterializedSession::empty("session-detach");
    session.applied_event_ordinal = 12;
    session.transcript = vec![agent_transcript_item("read through here", 12)];
    apply_session_view(&mut chat, Ok(managed_view(session)));
    chat.queued_prompts.push_back(queued("queued-1", "queued"));

    // Detaching keeps the composer intact: the warm chat goes on holding
    // it, and the surface reads it here to write it to the session row.
    assert_eq!(detach_chat(&mut chat), 12);
    assert_eq!(chat.input, "half typed thought");
    assert_eq!(chat.entries.len(), 1);
    assert!(chat.queued_prompts.is_empty());

    assert_eq!(detach_chat(&mut chat), 12);
    assert_eq!(chat.input, "half typed thought");

    let mut empty = ChatState::new(&snapshot(), &[]);
    detach_chat(&mut empty);
    assert_eq!(empty.input, "");
}
#[test]
fn transcript_wheel_scrolls_the_reduced_viewport_independently() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = (0..40)
        .map(|index| ChatEntry::plain(index, ChatRole::Agent, format!("transcript row {index}")))
        .collect();
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "scroll-question".into(),
            message: "Answer this".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        },
    ));
    chat.task_dialog_open = true;
    drawn_transcript(&mut chat, 80, 24);
    let transcript = chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript registered")
        .rect;
    let before = chat.anchor;

    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: transcript.x + 1,
            row: transcript.y + 1,
            modifiers: KeyModifiers::NONE,
        }),
        ChatAction::None
    );
    assert_ne!(chat.anchor, before);
    assert!(chat.elicitation.is_some());

    let scrollbar_x = chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript registered")
        .rect
        .right();
    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: scrollbar_x,
            row: transcript.y + transcript.height / 2,
            modifiers: KeyModifiers::NONE,
        }),
        ChatAction::None
    );
    assert!(chat.transcript_scrollbar_dragging());
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(crossterm::event::MouseButton::Left),
        column: scrollbar_x,
        row: transcript.y + transcript.height / 2,
        modifiers: KeyModifiers::NONE,
    });
    assert!(!chat.transcript_scrollbar_dragging());
}

#[test]
fn question_pointer_capture_wins_when_dragged_into_transcript() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries = (0..40)
        .map(|index| ChatEntry::plain(index, ChatRole::Agent, "transcript"))
        .collect();
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "captured-question".into(),
            message: "Choose whether to continue".into(),
            title: None,
            description: None,
            fields: vec![ElicitationField {
                id: "continue".into(),
                title: "Continue".into(),
                description: None,
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::Boolean {
                    default: Some(false),
                },
            }],
        },
    ));
    drawn_transcript(&mut chat, 80, 24);
    let form = chat
        .frame_surfaces()
        .surface(SurfaceId::ModalBody)
        .expect("question form registered")
        .rect;
    let press = MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: form.x + 1,
        row: form.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    assert!(chat.component_handles_mouse(press));
    chat.handle_mouse(press);
    let transcript = chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript registered")
        .rect;
    let drag = MouseEvent {
        kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        column: transcript.x + 1,
        row: transcript.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    assert!(chat.component_handles_mouse(drag));
    let before = chat.anchor;
    chat.handle_mouse(drag);
    assert_eq!(chat.anchor, before);
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ..drag
    });
}

#[test]
fn an_off_screen_chat_follows_the_session_view() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_transcript_loading(true);
    let mut session = MaterializedSession::empty("session-warm");
    session.applied_event_ordinal = 5;
    session.applied_event_digest = "a".repeat(64);
    session.transcript = vec![agent_transcript_item("first", 5)];

    let mut first_view = managed_view(session.clone());
    first_view
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .current_step_started_at_ms = Some(12_345);
    assert!(apply_session_view(&mut chat, Ok(first_view)));
    assert_eq!(chat.latest_seq(), 5);
    assert_eq!(chat.entries.len(), 1);
    assert_eq!(chat.current_step_started_at_ms, Some(12_345));
    assert!(!chat.transcript_loading);

    session.applied_event_ordinal = 8;
    session.transcript.push(agent_transcript_item("second", 8));
    assert!(apply_session_view(&mut chat, Ok(managed_view(session))));
    assert_eq!(chat.latest_seq(), 8);
    assert_eq!(chat.entries.len(), 2);
    assert_eq!(chat.current_step_started_at_ms, None);
}

#[test]
fn a_transient_error_before_the_first_snapshot_keeps_the_loading_row() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_transcript_loading(true);
    let view = ManagedSessionView {
        snapshot: None,
        connected: false,
        error: Some(ViewError::Unreachable("database is busy".into())),
    };

    assert!(apply_session_view(&mut chat, Ok(view)));
    assert!(chat.transcript_loading);
    assert_eq!(
        chat.notice().as_deref(),
        Some("connection lost: database is busy")
    );
    assert_eq!(
        super::super::test_support::transcript_text(&mut chat, 80),
        ["Loading…"]
    );
}

#[test]
fn a_stopped_session_manager_retires_its_feed_and_says_so() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_transcript_loading(true);

    let open = apply_session_view(&mut chat, Err(anyhow::anyhow!("session manager stopped")));

    assert!(!open);
    assert!(chat.transcript_loading);
    assert_eq!(
        chat.notice().as_deref(),
        Some("connection lost: session manager stopped")
    );
}

#[tokio::test]
async fn dictation_completion_preserves_edits_and_recovers_after_errors() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-dictation", 72);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        "original".into(),
        Notices::default(),
    );
    chat.state.voice_active = true;
    chat.state.set_input("edited while recording".into());
    chat.apply_voice_update(VoiceUpdate::Finished(Ok("spoken words".into())));
    assert_eq!(chat.draft(), "edited while recording spoken words");
    assert!(!chat.state.voice_active);
    chat.state.voice_active = true;
    chat.apply_voice_update(VoiceUpdate::Finished(Err(anyhow::anyhow!(
        "capture failed"
    ))));
    assert_eq!(chat.draft(), "edited while recording spoken words");
    assert!(!chat.state.voice_active);
    assert!(chat.state.notice().unwrap().contains("capture failed"));
    chat.apply_voice_update(VoiceUpdate::Finished(Ok(String::new())));
    assert_eq!(chat.draft(), "edited while recording spoken words");
}

/// Launch findings A-4 and E-9: with no voice helper or no dictation sign-in,
/// the dictation chord changed nothing on screen and wrote no log line, so the
/// key looked broken. It now says why nothing started.
// Hard-won: 6003829: unavailable dictation chords were silently ignored
#[tokio::test]
async fn the_dictation_chord_says_why_dictation_is_unavailable() {
    let fixture =
        mj_client::session::replacement_session_test_fixture("session-dictation-unavailable", 72);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );

    let notice = chat.toggle_dictation().expect("the chord explains itself");

    assert!(!chat.state.voice_active);
    assert!(notice.starts_with("Dictation is unavailable"), "{notice}");
    assert_eq!(chat.draft(), "");

    // Once the probe has answered, the chord gives its reason.
    chat.apply_voice_update(VoiceUpdate::Availability(
        Vec::new(),
        Ok(Err(io::DICTATION_NEEDS_A_CODEX_PROFILE.to_owned())),
    ));
    chat.state.clear_notice();
    assert_eq!(
        chat.toggle_dictation().as_deref(),
        Some(io::DICTATION_NEEDS_A_CODEX_PROFILE)
    );
    assert!(!chat.state.voice_active);
}

#[tokio::test]
async fn an_open_chat_hands_off_to_a_replacement_actor_without_losing_its_draft() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-replaced", 73);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        "half-written prompt".into(),
        Notices::default(),
    );

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            ActiveChat::pump(Some(&mut chat)).await;
            if chat.session_feed_open() && !chat.session.is_stopped() {
                break;
            }
        }
    })
    .await
    .expect("the replacement actor became the live chat feed");

    assert_eq!(chat.draft(), "half-written prompt");
    assert!(chat.state.notice().is_none());
}

#[tokio::test]
async fn detaching_a_chat_keeps_a_reviewer_draft_waiting_for_its_late_stream() {
    let fixture =
        mj_client::session::replacement_session_test_fixture("session-deferred-review", 74);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let request = ElicitationRequest {
        id: "reviewer-deferred-1".into(),
        message: "Allow the reviewer to continue?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    let mut reviewer = ChatState::new(&snapshot(), &[]);
    assert!(reviewer.show_review_role_elicitation(Some("reviewer-a".into()), request.clone(),));
    let reviewer_draft = reviewer
        .elicitation_draft()
        .expect("reviewer form can be snapshotted");

    // The primary form is visible while the sidecar has not surfaced its
    // form yet. A session switch must retain both local snapshots so the
    // reviewer answer is restored when its stream catches up.
    chat.state.restore_elicitation(request);
    chat.deferred_elicitation_draft = Some(reviewer_draft);
    let drafts = chat.elicitation_drafts();
    assert_eq!(drafts.len(), 2);
    assert!(drafts.iter().any(|draft| !draft.reviewer()));
    assert!(
        drafts
            .iter()
            .any(|draft| draft.reviewer() && draft.reviewer_role() == Some("reviewer-a"))
    );
}

#[tokio::test]
async fn an_external_review_removal_closes_its_visible_question() {
    let fixture =
        mj_client::session::replacement_session_test_fixture("session-review-removed", 76);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let request = ElicitationRequest {
        id: "reviewer-removed-1".into(),
        message: "Allow the reviewer to continue?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    assert!(
        chat.state
            .show_review_role_elicitation(Some("reviewer-a".into()), request)
    );
    assert!(chat.state.reviewer_elicitation_open());

    // A missing runtime review is authoritative even when the journal
    // poll that used to carry the form has no more events.
    chat.apply_review_view(None);
    assert!(!chat.state.reviewer_elicitation_open());
    assert!(chat.elicitation_draft().is_none());
}

/// A Codex session exposes plan mode through its `collaboration_mode`
/// config, which the chat reads only once it knows the harness is Codex.
/// That fact reaches the chat through the header now that the daemon owns
/// the recovery context the open path used to carry, so a Codex session
/// must still list `/plan`.
// Hard-won: 1714125: Codex /plan disappeared when recovery stopped supplying harness context
#[tokio::test]
async fn a_codex_session_lists_plan_from_the_header_harness() {
    use crate::chat::test_support::select_config_option;
    use mj_core::config::HarnessKind;

    let fixture = mj_client::session::replacement_session_test_fixture("session-codex", 75);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity {
            harness_kind: Some(HarnessKind::Codex),
            ..Default::default()
        },
        String::new(),
        Notices::default(),
    );

    chat.state.set_config_options(&[select_config_option(
        "collaboration_mode",
        "default",
        &["default", "plan"],
    )]);

    assert!(
        chat.state.lists_command("plan"),
        "a Codex session lists /plan once the header names the harness"
    );
}

/// A workspace id no configuration uses, so the per-workspace rows the
/// constructor reads are simply absent and it falls back to its defaults —
/// the same tolerance the other open tests rely on.
const CONTEXT_TEST_WORKSPACE: &str = "workspace-for-chat-session-context-tests";

fn context_session_record(id: &str, workspace_id: &str) -> SessionRecord {
    SessionRecord {
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        subagents: None,
        create_managed_worktree: None,
        id: id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        title: "work".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex-1".into(),
        bundle_id: "bundle-1".into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        container_cpus: None,
        container_memory: None,
        state: mj_core::state::SessionState::Running,
        archived: false,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-09T12:00:00Z".into(),
        updated_at: "2026-08-09T12:01:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

fn config_with_profiles(profiles: &[(&str, mj_core::config::HarnessKind)]) -> Config {
    Config {
        profiles: profiles
            .iter()
            .map(|(id, kind)| {
                (
                    (*id).to_owned(),
                    mj_core::config::HarnessProfile {
                        enabled: true,
                        kind: *kind,
                        home: std::path::PathBuf::from("/profiles").join(id),
                        environment: Default::default(),
                        context_window_bytes: None,
                        subagents: Default::default(),
                        guardian_review_model: None,
                    },
                )
            })
            .collect(),
        ..Config::default()
    }
}

fn chat_context(
    session_id: &str,
    profiles: &[(&str, mj_core::config::HarnessKind)],
) -> ChatSessionContext {
    ChatSessionContext {
        config: config_with_profiles(profiles),
        session: context_session_record(session_id, CONTEXT_TEST_WORKSPACE),
        reviewer_stager: mj_client::session::ReviewerStager::unavailable(
            "reviewer staging is unavailable in this chat test",
        ),
    }
}

/// Both review surfaces reflect the shared settings as configuration changes.
#[tokio::test]
async fn review_status_configuration_is_applied_on_open_and_refresh() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-review", 88);
    let mut context = chat_context("session-review", &[]);
    context.config.review = mj_core::config::ReviewConfig {
        enabled: true,
        tier: Some("extended".into()),
        profile: Some("reviewer-a".into()),
        model: None,
        effort: None,
    };
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(context),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );

    assert_eq!(
        chat.state.review_config(),
        mj_core::config::ReviewConfig {
            enabled: true,
            tier: Some("extended".into()),
            profile: Some("reviewer-a".into()),
            model: None,
            effort: None,
        }
    );
    let title = prompt_title_parts(&chat.state);
    assert!(title.iter().any(|part| part == "review"), "{title:?}");
    assert!(
        title
            .iter()
            .all(|part| part != "quick" && part != "extended"),
        "{title:?}"
    );

    let mut reloaded = Config::default();
    reloaded.review.profile = Some("reviewer-b".into());
    chat.refresh_context(&reloaded, None, None);

    assert_eq!(chat.state.review_config(), reloaded.review);
}

/// A failed recovery copy is the one thing the user has to see on opening
/// the session, so it is raised after the connection notice a cold open
/// also sets: a notice is a single slot, and the last write wins.
// Hard-won: dc7ee47: the checkpoint failure notice disappeared behind cold-open connection feedback
#[tokio::test]
async fn a_recorded_checkpoint_error_reaches_the_notice_when_the_chat_opens() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-checkpoint", 81);
    let mut context = chat_context("session-checkpoint", &[]);
    context.session.last_checkpoint_error = Some("the target ran out of disk".into());

    let chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(context),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );

    assert_eq!(
        chat.state.notice().as_deref(),
        Some("Recovery copy failed: the target ran out of disk")
    );
}

/// Late startup results cannot consume a replacement plan decision.
#[tokio::test]
async fn second_opinion_ignores_stale_preparation_and_uses_resolved_settings() {
    use mj_core::review::settings::{ResolvedReviewSettings, ReviewModelSettings};
    let fixture = mj_client::session::replacement_session_test_fixture("session-prepared", 86);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(chat_context("session-prepared", &[])),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let request = ElicitationRequest {
        id: "new-plan".into(),
        message: "Proceed?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    chat.state.open_second_opinion(CapturedProposal {
        request,
        proposal: "the new plan".into(),
    });
    chat.reviewer_preparation_sequence = 2;
    let resolved = ResolvedReviewSettings {
        profile: "only-profile".into(),
        generation: 42,
        main: ReviewModelSettings {
            model: Some("gpt-6-astra".into()),
            effort: Some("medium".into()),
            fast_mode: false,
        },
        automatic: true,
        same_provider: true,
    };
    chat.apply_reviewer_prepared(1, Ok(resolved.clone()));
    assert!(matches!(
        chat.state.second_opinion(),
        Some(SecondOpinion::Setup { .. })
    ));
    assert_ne!(chat.reviewer_generation, 42);
    chat.apply_reviewer_prepared(2, Ok(resolved));
    assert_eq!(chat.reviewer_generation, 42);
    assert!(matches!(
        chat.state.second_opinion(),
        Some(SecondOpinion::Review(_))
    ));
}

/// The chat snapshots the configuration when it opens, so a reload has to
/// be handed to it; otherwise a long-lived conversation goes on offering
/// the profiles that existed when it was opened.
#[tokio::test]
async fn a_refreshed_config_updates_context_for_review() {
    use mj_core::config::HarnessKind;

    let fixture = mj_client::session::replacement_session_test_fixture("session-refresh", 85);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(chat_context(
            "session-refresh",
            &[("codex-1", HarnessKind::Codex)],
        )),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    assert_eq!(chat.context.as_ref().unwrap().config.profiles.len(), 1);

    let reloaded = config_with_profiles(&[
        ("codex-1", HarnessKind::Codex),
        ("claude-1", HarnessKind::Claude),
    ]);
    let moved = context_session_record("session-refresh", "workspace-moved");
    chat.refresh_context(&reloaded, Some(&moved), Some(&moved));

    assert_eq!(
        chat.context
            .as_ref()
            .unwrap()
            .config
            .profiles
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["claude-1".to_owned(), "codex-1".to_owned()]
    );
    assert_eq!(
        chat.context
            .as_ref()
            .map(|context| context.session.workspace_id.as_str()),
        Some("workspace-moved")
    );

    // Another session's record is not this session's, so it is ignored.
    let other = context_session_record("session-other", "workspace-other");
    chat.refresh_context(&reloaded, Some(&other), Some(&other));
    assert_eq!(
        chat.context
            .as_ref()
            .map(|context| context.session.workspace_id.as_str()),
        Some("workspace-moved")
    );

    // A chat opened without a context has nothing to refresh.
    let fixture = mj_client::session::replacement_session_test_fixture("session-bare", 86);
    let mut bare = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    bare.refresh_context(&reloaded, None, None);
    assert!(bare.context.is_none());
}

#[tokio::test]
async fn a_same_session_context_refresh_updates_the_visible_header_without_losing_chat_state() {
    use mj_core::config::HarnessKind;

    let fixture =
        mj_client::session::replacement_session_test_fixture("session-header-refresh", 89);
    let mut initial = chat_context("session-header-refresh", &[("codex-1", HarnessKind::Codex)]);
    initial.session.target_template_id = "localhost".into();
    initial.session.last_profile = "codex-1".into();
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(initial),
        fixture.control,
        SessionHeaderIdentity {
            target: "localhost".into(),
            profile: "codex-1".into(),
            title: "Original session title".into(),
            harness_kind: Some(HarnessKind::Codex),
            subagent_count: 0,
        },
        "keep this draft".into(),
        Notices::default(),
    );
    chat.state.entries.push(ChatEntry::plain(
        1,
        ChatRole::User,
        "history that must remain",
    ));

    let reloaded = config_with_profiles(&[
        ("codex-1", HarnessKind::Codex),
        ("claude-2", HarnessKind::Claude),
    ]);
    let mut moved = context_session_record("session-header-refresh", "workspace-moved");
    moved.target_template_id = "podman".into();
    moved.last_profile = "claude-2".into();
    moved.harness_kind = HarnessKind::Claude;
    moved.acp_session_title = Some("Harness session title".into());
    moved.session_title_override = Some("Renamed session".into());
    chat.refresh_context(&reloaded, Some(&moved), Some(&moved));

    assert_eq!(chat.draft(), "keep this draft");
    assert!(
        chat.state
            .entries
            .iter()
            .any(|entry| entry.text == "history that must remain")
    );

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat.state, false))
        .expect("draw refreshed chat");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        rendered.contains("podman  Idle  claude-2  Renamed session"),
        "the refreshed target/profile must be visible in the conversation header: {rendered:?}"
    );
    assert!(!rendered.contains("localhost  Idle  codex-1"));
    assert!(!rendered.contains("Original session title"));
    assert!(!rendered.contains("Harness session title"));
}

/// Launch finding R3-11: before the harness named a new session, the
/// conversation header showed its 32-hex id while the Sessions row showed the
/// title it was created with ("project via fake"). The header uses the
/// listed title too.
// Hard-won: b05328c: unnamed session headers showed the UUID instead of the listed title
#[tokio::test]
async fn an_unnamed_session_s_header_uses_its_listed_title_not_its_id() {
    use mj_core::config::HarnessKind;

    let session_id = "a225e234d043d75737319553cd926f50";
    let fixture = mj_client::session::replacement_session_test_fixture(session_id, 91);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(chat_context(session_id, &[("codex-1", HarnessKind::Codex)])),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    let reloaded = config_with_profiles(&[("codex-1", HarnessKind::Codex)]);
    let mut unnamed = context_session_record(session_id, "workspace-1");
    unnamed.title = "project via fake".into();
    unnamed.acp_session_title = None;
    unnamed.session_title_override = None;
    chat.refresh_context(&reloaded, Some(&unnamed), Some(&unnamed));

    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat.state, false))
        .expect("draw refreshed chat");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("project via fake"), "{rendered:?}");
    assert!(!rendered.contains("a225e234d043"), "{rendered:?}");
}

#[tokio::test]
async fn an_active_runtime_record_rearms_a_chat_after_its_handoff_timed_out() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-resumed", 74);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        "still drafting".into(),
        Notices::default(),
    );
    let original_instance = chat.instance();
    chat.session_open = false;
    chat.finish_session_reconnect(Err("session session-resumed is not managed".into()));

    chat.set_session_feed_expected(true);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            ActiveChat::pump(Some(&mut chat)).await;
            if chat.session_feed_open() && !chat.session.is_stopped() {
                break;
            }
        }
    })
    .await
    .expect("the active runtime record restarted the session handoff");
    assert!(
        !chat.is_instance(&original_instance),
        "a reattached actor retires the previous view identity"
    );

    assert_eq!(chat.draft(), "still drafting");
    assert!(chat.state.notice().is_none());
}

// Hard-won: 66c7315: an intentional stop triggered a false reconnect wait
#[tokio::test]
async fn a_retiring_session_does_not_reconnect_when_its_feed_closes() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-stop", 12);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    chat.set_session_retiring(true);

    chat.apply_session_view(Err(anyhow::anyhow!(
        "session manager stopped: channel closed"
    )));

    assert!(!chat.session_feed_open());
    assert!(
        !chat.session_reconnect_in_flight,
        "a deliberate stop starts no handoff"
    );
    assert!(
        !chat.state.notice().is_some_and(|notice| {
            notice.contains("Could not reconnect") || notice.contains("connection lost")
        }),
        "a deliberate stop reports neither a lost connection nor a failed handoff, but the notice was {:?}",
        chat.state.notice()
    );

    // A session that becomes runnable again is expected once more, and the
    // handoff comes back with it.
    chat.set_session_feed_expected(true);
    assert!(!chat.session_retiring());
    assert!(chat.session_reconnect_in_flight);
}

// Hard-won: 66c7315: an intentional stop produced a false relay reconnect error
#[tokio::test]
async fn a_retiring_sessions_reconnect_failure_is_not_reported() {
    let fixture = mj_client::session::replacement_session_test_fixture("session-destroy", 13);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    chat.session_open = false;
    chat.set_session_retiring(true);

    chat.finish_session_reconnect(Err("session session-destroy is not managed".into()));

    assert!(
        !chat
            .state
            .notice()
            .is_some_and(|notice| notice.contains("Could not reconnect")),
        "a deliberate stop reports no reconnect failure, but the notice was {:?}",
        chat.state.notice()
    );
    assert!(!chat.session_reconnect_in_flight);
}

// Hard-won: 79c0a8c: resume reused a sealed chat view and rejected the next prompt
#[test]
fn retiring_the_session_feed_keeps_a_closing_phase_in_place() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Closed;

    assert!(!apply_session_view(
        &mut chat,
        Err(anyhow::anyhow!("session manager stopped"))
    ));
    assert_eq!(chat.phase(), WorkerPhase::Closed);
}
/// The composer's own row is where a user typing in it learns the keys, so it
/// carries the same groups the dashboard's row does: the composer's own keys,
/// then the host's prefix chords.
/// A-12: the composer's footer dropped the prefix label with the first chord,
/// so from 100 columns down it read `: palette · ? keys` — plain keys, as far
/// as the reader could tell. The label must ride on the first chord left.
// Hard-won: 48c2a9c: narrow composer footers hid the prefix and made a chord look like a plain key
#[test]
fn the_composer_footer_names_the_prefix_at_every_width() {
    let chat = ChatState::new(&snapshot(), &[]);
    let chords = [
        "c create",
        "g sessions",
        "a read",
        "b panes",
        "q detach",
        "u web",
        ": palette",
        "? keys",
    ];
    for width in [140_u16, 100, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                let footer = crate::chat::ChatFooter {
                    area,
                    chords: &chords,
                    chord_prefix: "ctrl+b then ",
                    functions: &[],
                    banner: None,
                };
                render_chat_footer(frame, footer, &chat, true);
            })
            .expect("draw footer");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let chord_group = text
            .split('│')
            .nth(1)
            .unwrap_or_else(|| panic!("{width}: no chord group in {text:?}"));
        assert!(
            chord_group.trim_start().starts_with("ctrl+b then "),
            "{width}: {text:?}"
        );
        assert!(text.contains(": palette · ? keys"), "{width}: {text:?}");
        assert!(!text.contains("then:"), "{width}: {text:?}");
    }
}

/// `symbols = "ascii"` is for a Linux console or a locale without UTF-8. The
/// composer's own hints were written out with a literal middle dot joining
/// them, so splitting on the glyph set's separator found nothing under the
/// ASCII set and the dot reached the screen anyway. Each branch that builds
/// those hints (idle, queued, and dictating) is checked here.
// Hard-won: 8dc493a: ASCII mode still emitted a Unicode middle-dot separator
#[test]
fn the_ascii_symbol_set_reaches_the_composers_own_footer_hints() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let draw = |chat: &ChatState| {
        // Wide enough that no hint gives way to the row's width limit; a
        // dropped hint would hide the joiner this test exists to check.
        let mut terminal = Terminal::new(TestBackend::new(200, 1)).expect("terminal");
        theme::with_symbols(mj_core::config::SymbolSet::Ascii, || {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_chat_footer(frame, test_footer(area), chat, true);
                })
                .expect("draw ascii footer");
        });
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    let idle = draw(&chat);
    assert!(idle.is_ascii(), "{idle:?}");
    let paste = if cfg!(target_os = "macos") {
        "Cmd-V paste"
    } else {
        "Ctrl-V paste"
    };
    assert!(idle.contains(paste), "{idle:?}");

    chat.queued_prompts.push_back(queued("queued-1", "next"));
    let queued_footer = draw(&chat);
    assert!(queued_footer.is_ascii(), "{queued_footer:?}");
    assert!(
        queued_footer.contains("Ctrl-R history"),
        "{queued_footer:?}"
    );
    chat.queued_prompts.clear();

    // The dictating hint keeps its own "…" (unrelated to the separator this
    // fix addresses), so only the joiner between hints is checked here.
    chat.voice_active = true;
    let dictating = draw(&chat);
    assert!(!dictating.contains('\u{b7}'), "{dictating:?}");
    assert!(dictating.contains("Listening"), "{dictating:?}");
}
/// A phase alone is not evidence of an interruptible turn.
#[tokio::test]
async fn escape_interrupts_a_native_goal_turn_without_an_active_prompt() {
    for harness_started in [false, true] {
        let mut fixture =
            mj_client::session::replacement_session_test_fixture("goal-interrupt", 12);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        let materialized = MaterializedSession::empty("goal-interrupt");
        let goal: mj_core::goal::GoalState = serde_json::from_value(serde_json::json!({
            "known": true,
            "snapshot": {"objective": "finish the work", "status": "active"},
            "execution": {"version": 1, "status": "running", "turnId": "native-turn"}
        }))
        .unwrap();
        let mut view = managed_view(materialized);
        let operational = &mut view.snapshot.as_mut().unwrap().operational;
        operational.goal = goal;
        operational.execution = mj_core::relay::RelayExecutionState::Running;
        if harness_started {
            operational.harness_turn = Some(mj_core::relay::HarnessTurn { started_at_ms: 1 });
        }
        apply_session_view(&mut chat.state, Ok(view));
        assert!(!chat.state.prompt_in_flight());
        assert!(chat.state.session_activity.pursuing_goal);
        assert!(
            prompt_bottom_queue_control(&chat.state)
                .unwrap()
                .to_string()
                .contains("Esc interrupts")
        );

        chat.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), fixture.submitted.recv())
                .await
                .expect("Esc must submit cancellation for the native turn"),
            Some(mj_core::relay::RelayCommand::CancelTurn)
        );

        // An active goal between turns is not itself a running turn.
        chat.state.goal_state.execution.as_mut().unwrap().status = "idle".into();
        chat.state.session_activity.harness_turn_started_at_ms = None;
        assert!(!chat.state.turn_interruptible());
        assert!(prompt_bottom_queue_control(&chat.state).is_none());
    }
}

#[tokio::test]
async fn escape_names_steering_through_submission_and_acceptance() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mj_core::state::{MaterializedQueuedPrompt, QueuedCommandKind};

    use mj_core::relay::{ActiveRelayPrompt, RelayCommand};

    for protocol in [None, Some(16), Some(17)] {
        for (supported, queue_kind, hint, sending, _requested) in [
            (
                Some(true),
                Some(QueuedCommandKind::Prompt),
                "Esc steers next",
                "Steering turn…",
                "Steering requested",
            ),
            (
                Some(false),
                Some(QueuedCommandKind::Prompt),
                "Esc steers next",
                "Steering turn…",
                "Cancellation requested",
            ),
            (
                None,
                Some(QueuedCommandKind::Prompt),
                "Esc steers next",
                "Steering turn…",
                "Queued prompt requested",
            ),
            (
                Some(true),
                Some(QueuedCommandKind::SetConfig {
                    key: "model".into(),
                    value: "next-model".into(),
                }),
                "Esc interrupts",
                "Interrupting turn…",
                "Cancellation requested",
            ),
            (
                Some(true),
                None,
                "Esc interrupts",
                "Interrupting turn…",
                "Cancellation requested",
            ),
        ] {
            let mut fixture =
                mj_client::session::replacement_session_test_fixture("steering-session", 12);
            let mut chat = ActiveChat::open(
                fixture.stopped,
                "bundle-1",
                None,
                fixture.control,
                SessionHeaderIdentity::default(),
                String::new(),
                Notices::default(),
            );
            let mut materialized = MaterializedSession::empty("steering-session");
            let targeted = protocol.is_some_and(|version| version >= 17);
            let hint = if targeted { hint } else { "Esc interrupts" };
            let sending = if targeted {
                sending
            } else {
                "Interrupting turn…"
            };
            let steering = targeted && queue_kind.as_ref().is_some_and(|kind| kind.is_prompt());
            if let Some(kind) = queue_kind {
                materialized.queued_prompts.push(MaterializedQueuedPrompt {
                    accepted_ordinal: None,
                    command_id: "queued-correction".into(),
                    kind,
                    content: vec![serde_json::json!({"type": "text", "text": "change direction"})],
                    queued_at_ms: 0,
                });
            }
            let mut view = managed_view(materialized);
            let operational = &mut view.snapshot.as_mut().unwrap().operational;
            operational.relay_protocol_version = protocol;
            operational.steering_supported = supported;
            operational.active_prompt = Some(ActiveRelayPrompt {
                command_id: "running-prompt".into(),
                created_at_ms: 0,
                started_at_ms: 0,
            });
            apply_session_view(&mut chat.state, Ok(view));
            let screen = drawn_transcript(&mut chat.state, 100, 24).join("\n");
            assert!(screen.contains(hint), "{screen}");

            let previous_feedback = chat.state.notice();
            chat.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            assert_eq!(chat.state.notice(), previous_feedback);
            let pending_screen = drawn_transcript(&mut chat.state, 100, 24).join("\n");
            assert!(pending_screen.contains(sending), "{pending_screen}");

            let result = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let result = chat
                        .remote
                        .recv()
                        .await
                        .expect("remote worker remains open");
                    if matches!(result, ChatRemoteResult::Cancel { .. }) {
                        break result;
                    }
                }
            })
            .await
            .expect("turn control request completes");
            assert_eq!(
                fixture.submitted.recv().await,
                Some(if !targeted {
                    RelayCommand::CancelTurn
                } else if steering {
                    RelayCommand::Steer {
                        active_prompt_id: "running-prompt".into(),
                        queued_prompt_id: "queued-correction".into(),
                    }
                } else {
                    RelayCommand::CancelTurnFor {
                        active_prompt_id: "running-prompt".into(),
                    }
                })
            );

            // A newer view may already have consumed the queue. The reply
            // must still describe the request that was actually submitted.
            chat.state.queued_prompts.clear();
            apply_chat_remote_result(&mut chat.state, result);
            assert_eq!(chat.state.notice(), previous_feedback);
            assert!(
                chat.state.operation_feedback.contains_key("turn-control"),
                "acceptance is not execution"
            );
            chat.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            assert!(
                fixture.submitted.try_recv().is_err(),
                "Escape cannot escalate while waiting for execution state"
            );
        }
    }
}

/// Background commands are exposed through the embedded task control;
/// the composer title stays focused on meaningful state.
#[test]
fn composer_title_names_the_work_the_agent_left_running() {
    let now_seconds = 10_000;
    let started_at_ms = now_seconds as i64 * 1_000 - 2_616_000;
    let mut chat = ChatState::new(&snapshot(), &[]);
    assert!(prompt_title(&chat).is_empty());

    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        checking_response: false,
        quota_recovery: None,
        capacity_retry: None,
        activity_turn_started_at_ms: None,
        prompt_in_flight: false,
        idle_since_ms: None,
        execution: None,
        harness_turn_started_at_ms: None,
        state: None,
        foreground_tool_started_at_ms: None,
        background_commands: vec![mj_core::relay::BackgroundCommand {
            id: "test:active".into(),
            started_at_ms,
            command: "cargo   test".into(),
            can_stop: false,
        }],
        active_user_shells: Vec::new(),
    });
    assert!(prompt_title(&chat).is_empty());
    assert!(!prompt_title(&chat).contains("Background"));

    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        checking_response: false,
        quota_recovery: None,
        capacity_retry: None,
        activity_turn_started_at_ms: None,
        prompt_in_flight: false,
        idle_since_ms: None,
        execution: None,
        harness_turn_started_at_ms: None,
        state: None,
        foreground_tool_started_at_ms: None,
        background_commands: vec![
            mj_core::relay::BackgroundCommand {
                id: "test:active-1".into(),
                started_at_ms,
                command: "cargo test".into(),
                can_stop: false,
            },
            mj_core::relay::BackgroundCommand {
                id: "test:active-2".into(),
                started_at_ms: started_at_ms + 1_000,
                command: "npm run build".into(),
                can_stop: false,
            },
        ],
        active_user_shells: Vec::new(),
    });
    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(screen.contains("Tasks (2)"), "{screen}");

    // The spinner represents a running turn, whatever it left behind.
    chat.phase = WorkerPhase::Running;
    assert!(!prompt_title(&chat).contains("Running"));
}
#[test]
fn effort_separator_between_prompt_chips_is_not_clickable() {
    for palette in theme::UiTheme::ALL {
        theme::with_theme(palette, || {
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.set_config_options(&[
                select_config("model", "fast", &["fast", "slow"]),
                select_config("effort", "high", &["low", "high"]),
            ]);
            let (title, _, chips) = prompt_title_line(&chat, Rect::new(0, 0, 100, 3));
            chat.config_chip_areas = chips;

            let separator = title
                .spans
                .iter()
                .position(|span| span.content == "· ")
                .expect("the effort separator is rendered");
            assert_eq!(title.spans[separator].style, theme::muted());
            assert_eq!(
                title.spans[separator + 1].content,
                format!("{} ▾ ", chat.current_effort().unwrap())
            );
            assert_eq!(title.spans[separator + 1].style, theme::actionable_chip());

            let (_, model) = chat
                .config_chip_areas
                .iter()
                .find(|(key, _)| *key == "model")
                .copied()
                .expect("the model chip is rendered");
            let (_, effort) = chat
                .config_chip_areas
                .iter()
                .find(|(key, _)| *key == "effort")
                .copied()
                .expect("the effort value is rendered");
            assert_eq!(effort.x, model.right() + display_width("· ") as u16);
            assert_eq!(
                chat.prompt_config_chip_at(model.right(), model.y),
                None,
                "the separator dot has no click hitbox"
            );
            assert_eq!(chat.prompt_config_chip_at(model.right() + 1, model.y), None);
            assert_eq!(chat.prompt_config_chip_at(model.x, model.y), Some("model"));
            assert_eq!(
                chat.prompt_config_chip_at(effort.x, effort.y),
                Some("effort")
            );
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|frame| render_full_frame(frame, &mut chat, false))
                .unwrap();
            for (_, area) in &chat.config_chip_areas {
                let cell = &terminal.backend().buffer()[(area.x, area.y)];
                assert_eq!(cell.fg, theme::palette().text);
                assert_eq!(cell.bg, theme::palette().selection);
            }
        });
    }
}

fn select_config(key: &str, current: &str, values: &[&'static str]) -> SessionConfigOption {
    SessionConfigOption::new(
        SessionConfigId::new(key),
        key,
        SessionConfigKind::Select(SessionConfigSelect::new(
            SessionConfigValueId::new(current),
            SessionConfigSelectOptions::Ungrouped(
                values
                    .iter()
                    .map(|value| SessionConfigSelectOption::new(*value, *value))
                    .collect(),
            ),
        )),
    )
}
// Hard-won: 6e7ecf3: title styling made hint descriptions bold and erased key emphasis
#[test]
fn prompt_hint_keys_are_bold_but_descriptions_are_not() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut terminal =
        Terminal::new(TestBackend::new(100, 24)).expect("test terminal supports drawing");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("chat draws");
    let buffer = terminal.backend().buffer();
    let (prompt_y, prompt_text) = (0..buffer.area.height)
        .map(|y| {
            let text = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            (y, text)
        })
        .find(|(_, text)| text.contains(" Enter  send "))
        .expect("the prompt hints are rendered");
    let enter_byte = prompt_text
        .find("Enter")
        .expect("the Enter hint is rendered");
    let enter_column = usize::from(buffer.area.x) + prompt_text[..enter_byte].chars().count();
    let slash_byte = prompt_text[enter_byte..]
        .find(" / ")
        .expect("the command hint is rendered")
        + enter_byte;
    let slash_column = usize::from(buffer.area.x) + prompt_text[..slash_byte].chars().count() + 1;
    let send_byte = prompt_text[enter_byte..]
        .find("send")
        .expect("the send description is rendered")
        + enter_byte;
    let send_column = usize::from(buffer.area.x) + prompt_text[..send_byte].chars().count();
    let commands_byte = prompt_text[slash_byte..]
        .find("commands")
        .expect("the commands description is rendered")
        + slash_byte;
    let commands_column = usize::from(buffer.area.x) + prompt_text[..commands_byte].chars().count();

    for column in enter_column..enter_column + "Enter".len() {
        let cell = &buffer[(column as u16, prompt_y)];
        assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
        assert_ne!(cell.bg, theme::palette().selection);
    }
    let slash = &buffer[(slash_column as u16, prompt_y)];
    assert!(slash.modifier.contains(ratatui::style::Modifier::BOLD));
    assert_ne!(slash.bg, theme::palette().selection);
    for (start, word) in [(send_column, "send"), (commands_column, "commands")] {
        for column in start..start + word.len() {
            let cell = &buffer[(column as u16, prompt_y)];
            assert!(!cell.modifier.contains(ratatui::style::Modifier::BOLD));
        }
    }
}
/// The title names the conversation you are in. The rule around it is
/// chrome and stays dim; the name draws bright white so it stands out.
/// The pane a conversation is drawn into is the whole of what its dialogs
/// may cover. A host with several panes gives each one a sub-rectangle of
/// the terminal, so a dialog centred in the frame would paint over its
/// neighbours.
/// The background-task dialog used to claim every pointer position on the
/// screen, which swallowed clicks on the host's own panes beside it. It now
/// answers only for the rectangle it drew into.
#[test]
fn the_task_dialog_claims_only_the_pointer_over_itself() {
    let pane = Rect::new(50, 2, 48, 26);
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.task_dialog_open = true;
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            render_in(
                frame,
                &mut chat,
                ChatRegions {
                    transcript: Rect::new(50, 2, 48, 19),
                    prompt: Rect::new(50, 21, 48, 7),
                    footer: None,
                    overlay: pane,
                    title_controls: 0,
                    title_lead: 0,
                    pane_focused: true,
                },
                false,
                false,
            )
        })
        .expect("draw the background-task dialog");

    let body = chat
        .frame_surfaces()
        .surface(SurfaceId::ModalBody)
        .expect("the dialog registers a modal body")
        .rect;
    let click = |column: u16, row: u16| MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert!(
        chat.component_handles_mouse(click(body.x, body.y)),
        "the dialog still answers for its own body"
    );
    assert!(
        !chat.component_handles_mouse(click(2, 6)),
        "a click on the host's navigator, far from the pane, is not the dialog's"
    );
    assert!(
        !chat.component_handles_mouse(click(pane.x - 1, pane.y + 4)),
        "a click just outside the pane is not the dialog's"
    );
}

/// A host that owns the rest of the frame gives the chat two rectangles;
/// nothing it draws may leak outside them.
#[test]
fn composer_border_holds_activity_without_moving_the_transcript_or_input() {
    for width in [16, 32, 48, 80] {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_spinner_style(crate::spinner::SpinnerStyle::Pulse);
        chat.set_input("my follow-up".into());
        let idle = drawn_transcript(&mut chat, width, 24);
        let input = chat
            .frame_surfaces()
            .surface(SurfaceId::PromptInput)
            .unwrap()
            .rect;

        let transcript = chat
            .frame_surfaces()
            .surface(SurfaceId::Transcript)
            .unwrap()
            .rect;
        chat.mark_prompt_submitted("continue");
        let running = drawn_transcript(&mut chat, width, 24);
        let prompt_top = usize::from(input.y.saturating_sub(1));
        let prompt_bottom = prompt_top + 1 + usize::from(input.height);
        let prompt_title = &running[prompt_top];
        assert!(
            prompt_title.contains(voice_button_glyph()),
            "{prompt_title}"
        );
        assert!(!prompt_title.contains("Prompt"), "{prompt_title}");
        assert!(!prompt_title.contains("Running"), "{prompt_title}");
        let spinner_width = prompt_title
            .chars()
            .filter(|ch| matches!(ch, '·' | '∙' | '•' | '●'))
            .count();
        assert_eq!(
            spinner_width,
            if width == 16 {
                1
            } else {
                crate::spinner::SPINNER_WIDTH
            },
            "{prompt_title}"
        );
        let last_spinner = prompt_title
            .chars()
            .enumerate()
            .filter_map(|(index, ch)| matches!(ch, '·' | '∙' | '•' | '●').then_some(index))
            .last()
            .expect("spinner frame on the prompt title");
        assert_eq!(
            last_spinner,
            prompt_title.chars().count() - 3,
            "spinner occupies the upper-right title: {prompt_title}"
        );
        // A 16-column composer has 14 cells of border, one short of the
        // whole hint; the border clips it there.
        let hint = if width > 16 {
            "Esc interrupts"
        } else {
            "Esc interrupt"
        };
        assert!(running[prompt_bottom].contains(hint), "{running:?}");
        assert_eq!(running[0], idle[0], "the transcript keeps its full height");
        assert_eq!(running[input.y as usize], idle[input.y as usize]);
        assert_eq!(
            chat.frame_surfaces()
                .surface(SurfaceId::Transcript)
                .unwrap()
                .rect,
            transcript,
            "activity must not consume a transcript row"
        );

        chat.phase = WorkerPhase::Idle;
        chat.prompt_in_flight = false;
        chat.turn_started_at_epoch_seconds = None;
        assert_eq!(drawn_transcript(&mut chat, width, 24), idle);
    }
}

/// The cursor belongs to whatever owns the keyboard, and the host decides
/// that, so the composer only shows one when it is told it has focus.
/// A conversation long enough that opening it converts the tail only.
fn long_session() -> MaterializedSession {
    let mut session = MaterializedSession::empty("session-long");
    session.transcript = (1..=300)
        .map(|position| {
            agent_message_item(
                &format!("agent:{position}"),
                position,
                &format!("message {position}"),
            )
        })
        .collect();
    session.applied_event_ordinal = 301;
    session
}

// Hard-won: 2b229f7: opening a long conversation blocked the event loop for seconds
#[test]
fn the_converted_history_completes_a_chat_opened_on_its_tail() {
    let session = long_session();
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let pending = chat.unconverted_prefix();
    assert!(pending > 0);
    let prefix = materialized_prefix_entries(
        &session.transcript[..pending],
        session.applied_event_ordinal,
    );

    let rebuild = apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::TranscriptPrefix {
            attempt: 1,
            result: Ok((prefix, Vec::new())),
        },
    );

    assert_eq!(rebuild, PrefixRebuild::NotNeeded);
    assert_eq!(chat.unconverted_prefix(), 0);
    assert_eq!(chat.entries.len(), session.transcript.len());
    assert_eq!(chat.entries[0].text, "message 1");
    assert_eq!(chat.notice(), None);
}

#[test]
fn history_that_no_longer_fits_the_tail_is_rebuilt_and_then_gives_up_with_a_notice() {
    let session = long_session();
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let pending = chat.unconverted_prefix();
    // History from a transcript compaction rewrote: it overlaps the tail.
    let stale = materialized_prefix_entries(
        &session.transcript[session.transcript.len() - pending..],
        session.applied_event_ordinal,
    );

    let rebuild = apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::TranscriptPrefix {
            attempt: 1,
            result: Ok((stale.clone(), Vec::new())),
        },
    );

    assert_eq!(rebuild, PrefixRebuild::Needed { attempt: 2 });
    assert_eq!(chat.unconverted_prefix(), pending);
    assert_eq!(chat.notice(), None);

    // A saved reading position in the unloaded prefix must stop waiting
    // when conversion gives up, so the reported recent history is readable.
    let saved = serde_json::from_value(serde_json::json!({
        "Row": {"entry": 0, "row": 0}
    }))
    .unwrap();
    chat.restore_transcript_position(saved);
    let exhausted = apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::TranscriptPrefix {
            attempt: MAX_PREFIX_CONVERSION_ATTEMPTS,
            result: Ok((stale, Vec::new())),
        },
    );

    assert_eq!(exhausted, PrefixRebuild::NotNeeded);
    assert_eq!(chat.unconverted_prefix(), pending);
    assert_eq!(chat.anchor, super::super::TranscriptAnchor::Bottom);
    assert!(
        chat.notice()
            .is_some_and(|notice| notice.contains("Earlier messages")),
        "giving up on the history has to be reported"
    );
}

#[test]
fn a_failed_history_conversion_is_reported_instead_of_dropped() {
    let session = long_session();
    let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
    let saved = serde_json::from_value(serde_json::json!({
        "Row": {"entry": 0, "row": 0}
    }))
    .unwrap();
    chat.restore_transcript_position(saved);

    let rebuild = apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::TranscriptPrefix {
            attempt: 1,
            result: Err("worker panicked".into()),
        },
    );

    assert_eq!(rebuild, PrefixRebuild::NotNeeded);
    assert_eq!(chat.anchor, super::super::TranscriptAnchor::Bottom);
    assert_eq!(
        chat.notice().as_deref(),
        Some("Earlier messages failed to load: worker panicked")
    );
}

#[test]
fn earlier_history_ignores_closed_readers_and_allows_retry_without_losing_the_draft() {
    let mut chat = ChatState::new(
        &mj_core::relay::WorkerSnapshot::summary("history".into(), WorkerPhase::Idle, 0),
        &[],
    );
    chat.input = "unfinished draft".into();
    chat.open_earlier_messages();
    let retired = chat.earlier.as_ref().unwrap().generation;
    chat.earlier_key(KeyCode::Esc);
    apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::EarlierMessages {
            generation: retired,
            result: Ok((None, vec![Line::raw("obsolete")])),
        },
    );
    assert!(chat.earlier.is_none());
    chat.open_earlier_messages();
    let current = chat.earlier.as_ref().unwrap().generation;
    apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::EarlierMessages {
            generation: retired,
            result: Ok((None, vec![Line::raw("obsolete")])),
        },
    );
    assert!(chat.earlier.as_ref().unwrap().lines.is_empty());
    chat.earlier.as_mut().unwrap().loading = true;
    apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::EarlierMessages {
            generation: current,
            result: Err("database unavailable".into()),
        },
    );
    assert!(!chat.earlier.as_ref().unwrap().loading);
    assert!(
        chat.earlier
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("database unavailable")
    );
    chat.earlier_key(KeyCode::Enter);
    assert!(chat.earlier.as_ref().unwrap().requested);
    chat.handle_terminal_paste("accidental paste");
    assert_eq!(chat.input, "unfinished draft");
    apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::EarlierMessages {
            generation: current,
            result: Ok((None, vec![Line::raw("old message")])),
        },
    );
    assert!(chat.earlier.as_ref().unwrap().loaded);
    assert!(chat.earlier.as_ref().unwrap().error.is_none());
    assert_eq!(
        chat.earlier.as_ref().unwrap().lines,
        vec![Line::raw("old message")]
    );
}

#[test]
fn clipboard_capability_follows_initialized_session_views() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    assert!(chat.clipboard_is_text_only());
    let mut view = managed_view(MaterializedSession::empty("clipboard-capabilities"));
    for (wire, text_only) in [
        (serde_json::json!({}), true),
        (
            serde_json::json!({"promptCapabilities": {"image": true}}),
            false,
        ),
        (
            serde_json::json!({"promptCapabilities": {"image": false}}),
            true,
        ),
    ] {
        view.snapshot
            .as_mut()
            .unwrap()
            .operational
            .agent_capabilities = Some(Box::new(serde_json::from_value(wire).unwrap()));
        apply_session_view(&mut chat, Ok(view.clone()));
        assert_eq!(chat.clipboard_is_text_only(), text_only);
    }
    view.snapshot
        .as_mut()
        .unwrap()
        .operational
        .agent_capabilities = None;
    apply_session_view(&mut chat, Ok(view));
    assert!(chat.clipboard_is_text_only());
}

fn clipboard_test_chat() -> ActiveChat {
    let fixture = mj_client::session::replacement_session_test_fixture("clipboard-admission", 72);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    // Hold processing at the queue boundary so tests control completion order
    // and never write to an actual session's attachment store.
    chat.attachment_tasks_in_flight = MAX_ATTACHMENT_TASKS;
    chat
}

fn clipboard_result(chat: &ActiveChat, content: ClipboardContent) -> ChatIoUpdate {
    ChatIoUpdate::Clipboard {
        generation: chat.state.input_generation(),
        target: chat.state.clipboard_target(),
        result: Ok(content),
    }
}

#[tokio::test]
async fn clipboard_completion_requires_image_support_and_preserves_plain_text() {
    let mut chat = clipboard_test_chat();
    assert!(!chat.state.prompt_images_supported);
    chat.apply_io_update(clipboard_result(
        &chat,
        ClipboardContent::Image(super::super::tests::test_image()),
    ));
    assert!(chat.draft().is_empty());
    assert!(chat.attachment_queue.is_empty());
    assert!(
        chat.state
            .notice()
            .unwrap()
            .contains("advertised image support")
    );
    chat.apply_io_update(clipboard_result(
        &chat,
        ClipboardContent::Text("ordinary text".into()),
    ));
    assert_eq!(chat.draft(), "ordinary text");

    chat.state.set_prompt_images_supported(true);
    let image = super::super::tests::test_image();
    chat.apply_io_update(clipboard_result(
        &chat,
        ClipboardContent::Image(image.clone()),
    ));
    let (sequence, _, _) = chat.attachment_queue.pop_front().unwrap();
    assert_eq!(chat.state.input, "ordinary text[image 1]");
    chat.apply_attachment_result(AttachmentResult {
        sequence,
        fallback: AttachmentFallback::FailedMarker,
        result: Ok(image.clone()),
    });
    assert_eq!(chat.state.input_images[0].image, image);
    assert_eq!(
        chat.state.submit_input(),
        ChatAction::Prompt("ordinary text[image 1]".into())
    );
    assert_eq!(chat.state.take_submitting_images()[0].image, image);
}

#[tokio::test]
async fn pasted_image_paths_attach_and_a_failed_one_returns_as_pasted_text() {
    let mut chat = clipboard_test_chat();
    chat.state.set_prompt_images_supported(true);
    let pasted = if cfg!(windows) {
        "\"C:\\shots\\one.png\"\nC:\\shots\\two.jpg"
    } else {
        "'/shots/one.png'\n/shots/two.jpg"
    };
    chat.handle_event(Event::Paste(pasted.into()));
    assert_eq!(chat.state.input, "[image 1]\n[image 2]");
    let (first, _, _) = chat.attachment_queue.pop_front().unwrap();
    let (second, _, _) = chat.attachment_queue.pop_front().unwrap();
    // The user keeps typing while both files are read.
    chat.state.paste_plain(" look");

    let image = super::super::tests::test_image();
    chat.apply_attachment_result(AttachmentResult {
        sequence: second,
        fallback: AttachmentFallback::Text(pasted.split('\n').nth(1).unwrap().into()),
        result: Ok(image.clone()),
    });
    chat.apply_attachment_result(AttachmentResult {
        sequence: first,
        fallback: AttachmentFallback::Text(pasted.split('\n').next().unwrap().into()),
        result: Err("no such file".into()),
    });
    let first_line = pasted.split('\n').next().unwrap();
    assert_eq!(chat.state.input, format!("{first_line}\n[image 2] look"));
    assert_eq!(chat.state.input_cursor, chat.state.input.len());
    assert_eq!(chat.state.input_images.len(), 1);
    assert_eq!(chat.state.input_images[0].image, image);
    assert!(chat.state.notice().unwrap().contains("no such file"));
}

#[tokio::test]
async fn stale_clipboard_results_do_not_cross_capability_or_input_context_changes() {
    use super::super::test_support::{ctrl, key};
    for change in 0..5 {
        let mut chat = clipboard_test_chat();
        chat.state.set_prompt_images_supported(true);
        let pending = clipboard_result(
            &chat,
            ClipboardContent::Image(super::super::tests::test_image()),
        );
        match change {
            0 => chat.state.set_prompt_images_supported(false),
            1 => {
                chat.state.handle_key(ctrl('r'));
            }
            2 => {
                chat.state
                    .handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL));
            }
            3 => {
                chat.state.turn_control_dialog_open = true;
            }
            _ => {
                chat.state.handle_key(key(KeyCode::Char('x')));
            }
        }
        let before = chat.state.draft_payload();
        chat.apply_io_update(pending);
        assert_eq!(chat.state.draft_payload(), before);
        assert!(chat.attachment_queue.is_empty());
        assert!(!chat.paste_in_flight);
    }
}

#[tokio::test]
async fn capability_loss_during_attachment_processing_keeps_draft_but_blocks_send() {
    let mut chat = clipboard_test_chat();
    chat.state.set_prompt_images_supported(true);
    let image = super::super::tests::test_image();
    chat.apply_io_update(clipboard_result(
        &chat,
        ClipboardContent::Image(image.clone()),
    ));
    let (sequence, _, _) = chat.attachment_queue.pop_front().unwrap();
    chat.state.set_prompt_images_supported(false);
    chat.apply_attachment_result(AttachmentResult {
        sequence,
        fallback: AttachmentFallback::FailedMarker,
        result: Ok(image.clone()),
    });
    let draft = chat.state.draft_payload();
    assert_eq!(chat.state.submit_input(), ChatAction::None);
    assert_eq!(chat.state.draft_payload(), draft);
    chat.state.set_prompt_images_supported(true);
    assert!(matches!(chat.state.submit_input(), ChatAction::Prompt(_)));
    assert_eq!(chat.state.take_submitting_images()[0].image, image);
}

#[tokio::test]
async fn clipboard_errors_keep_the_draft_and_report_the_underlying_failure() {
    let mut chat = clipboard_test_chat();
    chat.state.set_input("keep me".into());
    chat.apply_io_update(ChatIoUpdate::Clipboard {
        generation: chat.state.input_generation(),
        target: chat.state.clipboard_target(),
        result: Err("clipboard provider disconnected".into()),
    });
    assert_eq!(chat.draft(), "keep me");
    let notice = chat.state.notice().unwrap();
    assert!(notice.contains("clipboard provider disconnected"));
    assert!(notice.contains("only clipboard text"));
}

#[tokio::test]
async fn opening_a_chat_uses_the_initialized_image_capability() {
    let fixture = mj_client::session::replacement_session_test_fixture("clipboard-open", 1);
    let mut view = managed_view(MaterializedSession::empty("clipboard-open"));
    view.snapshot
        .as_mut()
        .unwrap()
        .operational
        .agent_capabilities = Some(Box::new(
        serde_json::from_value(serde_json::json!({"promptCapabilities": {"image": true}})).unwrap(),
    ));
    fixture.replacement_view.send_replace(view);
    let session = fixture.control.session("clipboard-open").await.unwrap();
    let chat = ActiveChat::open(
        session,
        "bundle-1",
        None,
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    assert!(!chat.state.clipboard_is_text_only());
}

#[tokio::test]
async fn a_question_opened_during_clipboard_read_receives_no_stray_content() {
    let mut chat = clipboard_test_chat();
    chat.state.set_prompt_images_supported(true);
    for content in [
        ClipboardContent::Text("wrong field".into()),
        ClipboardContent::Image(super::super::tests::test_image()),
    ] {
        let pending = clipboard_result(&chat, content);
        chat.state.restore_elicitation(ElicitationRequest {
            id: "question".into(),
            message: "Answer".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        });
        assert!(chat.state.clipboard_is_text_only());
        chat.apply_io_update(pending);
        assert!(chat.state.input_images.is_empty());
        assert!(chat.state.input.is_empty());
        assert!(chat.attachment_queue.is_empty());
        chat.state.elicitation = None;
    }
}

#[tokio::test]
async fn removed_or_failed_pending_attachments_do_not_reappear() {
    let mut chat = clipboard_test_chat();
    chat.state.set_prompt_images_supported(true);
    for result in [
        Ok(super::super::tests::test_image()),
        Err("invalid image encoding".into()),
    ] {
        assert!(chat.state.reserve_attachment(1));
        chat.state.clear_input();
        chat.apply_attachment_result(AttachmentResult {
            sequence: 1,
            fallback: AttachmentFallback::FailedMarker,
            result,
        });
        assert!(chat.state.input_images.is_empty());
        assert!(chat.state.input.is_empty());
    }
    assert!(chat.state.reserve_attachment(2));
    chat.apply_attachment_result(AttachmentResult {
        sequence: 2,
        fallback: AttachmentFallback::FailedMarker,
        result: Err("invalid image encoding".into()),
    });
    assert!(
        chat.state
            .notice()
            .unwrap()
            .contains("invalid image encoding")
    );
    assert_eq!(chat.state.submit_input(), ChatAction::None);
}

/// Launch campaign finding A-15: switching Setup's symbols to ASCII left an
/// open transcript drawn with `❯` until a restart, because its row cache
/// was kept across the change. The next draw uses the new symbols.
// Hard-won: 987af2e: an open transcript reused Unicode rows after switching to ASCII
#[test]
fn an_open_transcript_follows_a_symbol_set_change_on_the_next_draw() {
    use crate::theme::{SymbolSet, with_symbols};
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "hello there"));
    let unicode = with_symbols(SymbolSet::Unicode, || drawn_transcript(&mut chat, 60, 12));
    assert!(unicode.iter().any(|row| row.contains('❯')), "{unicode:#?}");
    let ascii = with_symbols(SymbolSet::Ascii, || drawn_transcript(&mut chat, 60, 12));
    assert!(ascii.iter().all(|row| !row.contains('❯')), "{ascii:#?}");
}

// Hard-won: b4adff5: empty image paste read an unsupported clipboard and showed the wrong failure
#[tokio::test]
async fn empty_paste_without_image_support_reports_it_without_reading_the_clipboard() {
    let mut chat = clipboard_test_chat();
    chat.state.set_prompt_images_supported(false);
    chat.state.input = "draft".into();
    chat.handle_event_result(crossterm::event::Event::Paste(String::new()));
    assert!(!chat.paste_in_flight, "the clipboard must not be read");
    assert_eq!(
        chat.state.notice().as_deref(),
        Some(super::super::input_state::IMAGE_PASTE_UNSUPPORTED_NOTICE)
    );
    assert_eq!(chat.state.input, "draft");
}

fn active_golden_buffer(chat: &mut ChatState, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render_full_frame(frame, chat, false))
        .expect("draw active chat");
    terminal.backend().buffer().clone()
}

fn append_active_golden_state(
    output: &mut String,
    label: &str,
    buffer: &ratatui::buffer::Buffer,
    details: &[String],
) {
    use std::fmt::Write as _;

    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(
        output,
        "=== {label} ({}x{}) ===",
        buffer.area.width, buffer.area.height
    )
    .expect("write state label");
    let lines = crate::golden::buffer_lines(buffer);
    output.push_str(&lines.join("\n"));
    output.push('\n');
    for detail in details {
        writeln!(output, "{detail}").expect("write state detail");
    }
}

fn active_golden_rect(chat: &ChatState, id: SurfaceId) -> String {
    let rect = chat
        .frame_surfaces()
        .surface(id)
        .map(|surface| surface.rect)
        .unwrap_or_default();
    format!(
        "{id:?} rect=({}, {}, {}, {})",
        rect.x, rect.y, rect.width, rect.height
    )
}

fn active_golden_style(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> String {
    let cell = &buffer[(x, y)];
    format!(
        "fg={:?} bg={:?} modifiers={:?}",
        cell.fg, cell.bg, cell.modifier
    )
}

#[tokio::test]
async fn golden_active_chat_render() {
    use mj_client::usage_format::SessionActivity;
    use mj_core::config::HarnessKind;
    use mj_core::relay::WorkerPhase;
    use ratatui::backend::Backend as _;
    use ratatui::layout::Rect;

    let mut output = String::new();

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Idle;
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    append_active_golden_state(&mut output, "empty conversation prompt", &buffer, &[]);
    chat.transcript_loading = true;
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    append_active_golden_state(&mut output, "transcript still loading", &buffer, &[]);
    chat.transcript_loading = false;
    chat.unconverted_prefix = 1;
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    append_active_golden_state(&mut output, "history prefix not converted", &buffer, &[]);
    chat.unconverted_prefix = 0;
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "Hello"));
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    append_active_golden_state(&mut output, "conversation has history", &buffer, &[]);
    chat.entries.clear();
    chat.phase = WorkerPhase::Running;
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    append_active_golden_state(&mut output, "running conversation follow-up", &buffer, &[]);

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("/".into());
    let buffer = active_golden_buffer(&mut chat, 80, 24);
    let popup = chat
        .frame_surfaces()
        .surface(SurfaceId::AutocompletePopup)
        .expect("autocomplete popup is registered");
    let popup_hit = chat
        .frame_surfaces()
        .surface_at(popup.rect.x, popup.rect.bottom() - 1)
        .map(|surface| surface.id);
    append_active_golden_state(
        &mut output,
        "autocomplete popup and hit target",
        &buffer,
        &[
            active_golden_rect(&chat, SurfaceId::Transcript),
            active_golden_rect(&chat, SurfaceId::PromptInput),
            format!("popup bottom hit={popup_hit:?}"),
        ],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    let buffer = active_golden_buffer(&mut chat, 80, 24);
    let transcript = chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript is registered")
        .rect;
    let prompt = chat
        .frame_surfaces()
        .surface(SurfaceId::PromptInput)
        .expect("prompt is registered")
        .rect;
    let prompt_hit = chat
        .frame_surfaces()
        .surface_at(prompt.x, prompt.y)
        .map(|surface| surface.id);
    append_active_golden_state(
        &mut output,
        "transcript and composer registered surfaces",
        &buffer,
        &[
            active_golden_rect(&chat, SurfaceId::Transcript),
            active_golden_rect(&chat, SurfaceId::PromptInput),
            format!(
                "transcript before prompt={} prompt hit={prompt_hit:?}",
                transcript.bottom() <= prompt.y
            ),
        ],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::Agent,
        "UNDERLYING CHAT SENTINEL",
    ));
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "question-1".into(),
            message: "Visible dialog message".into(),
            title: Some("Overlaid dialog".into()),
            description: None,
            fields: Vec::new(),
        },
    ));
    chat.task_dialog_open = true;
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    let transcript = chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("reduced transcript is registered")
        .rect;
    let message = chat
        .frame_surfaces()
        .surface(SurfaceId::ElicitationMessage)
        .expect("question message is registered")
        .rect;
    let dialog_hit = chat
        .frame_surfaces()
        .surface_at(message.x, message.y)
        .map(|surface| surface.id);
    let footer = crate::golden::buffer_lines(&buffer)
        .last()
        .cloned()
        .unwrap_or_default();
    append_active_golden_state(
        &mut output,
        "elicitation stays inside chat and leaves transcript visible",
        &buffer,
        &[
            active_golden_rect(&chat, SurfaceId::Transcript),
            active_golden_rect(&chat, SurfaceId::ModalBody),
            active_golden_rect(&chat, SurfaceId::ElicitationMessage),
            format!(
                "transcript above message={} prompt hidden={} message hit={dialog_hit:?}",
                transcript.bottom() <= message.y,
                chat.frame_surfaces()
                    .surface(SurfaceId::PromptInput)
                    .is_none()
            ),
            format!("footer={footer}"),
        ],
    );

    let mut short = ChatState::new(&snapshot(), &[]);
    short.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "short-question".into(),
            message: "One short question".into(),
            title: Some("Custom title".into()),
            description: None,
            fields: Vec::new(),
        },
    ));
    let buffer = active_golden_buffer(&mut short, 80, 24);
    let transcript = short
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript is registered")
        .rect;
    let message = short
        .frame_surfaces()
        .surface(SurfaceId::ElicitationMessage)
        .expect("question message is registered")
        .rect;
    let natural = short.elicitation.as_ref().unwrap().natural_height(80);
    let question_height = 23 - transcript.bottom() - 1;
    append_active_golden_state(
        &mut output,
        "short elicitation uses natural height",
        &buffer,
        &[
            active_golden_rect(&short, SurfaceId::Transcript),
            active_golden_rect(&short, SurfaceId::ElicitationMessage),
            format!(
                "natural question height={natural} actual={question_height} transcript taller than message={}",
                transcript.height > message.height
            ),
        ],
    );

    let mut tall = ChatState::new(&snapshot(), &[]);
    tall.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "tall-question".into(),
            message: "Question message".into(),
            title: Some("Tall question".into()),
            description: Some("This field description is intentionally long enough to wrap into many rows when it is shown in the focused form.".into()),
            fields: vec![ElicitationField {
                id: "answer".into(),
                title: "Answer".into(),
                description: Some("The focused answer description also consumes wrapped rows. ".repeat(20)),
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::Text {
                    default: None,
                    min_length: None,
                    max_length: None,
                    pattern: None,
                    format: None,
                },
            }],
        },
    ));
    let natural = tall.elicitation.as_ref().unwrap().natural_height(80);
    let buffer = active_golden_buffer(&mut tall, 80, 24);
    let transcript = tall
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript is registered")
        .rect;
    let message = tall
        .frame_surfaces()
        .surface(SurfaceId::ElicitationMessage)
        .expect("question message is registered")
        .rect;
    let question_height = 23 - transcript.bottom() - 1;
    append_active_golden_state(
        &mut output,
        "tall elicitation is capped without overlap",
        &buffer,
        &[
            active_golden_rect(&tall, SurfaceId::Transcript),
            active_golden_rect(&tall, SurfaceId::ElicitationMessage),
            format!(
                "natural question height={natural} actual={question_height} transcript before message={} prompt hidden={}",
                transcript.bottom() <= message.y,
                tall.frame_surfaces()
                    .surface(SurfaceId::PromptInput)
                    .is_none()
            ),
        ],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    let shared = Notices::default();
    chat.notices = shared.clone();
    shared.set("Background import finished");
    let buffer = active_golden_buffer(&mut chat, 120, 24);
    let footer_row = buffer.area.bottom() - 1;
    append_active_golden_state(
        &mut output,
        "shared warning notice in the composer footer",
        &buffer,
        &[format!(
            "footer text fg={:?}",
            buffer[(buffer.area.x, footer_row)].fg
        )],
    );
    shared.clear();
    let buffer = active_golden_buffer(&mut chat, 120, 24);
    append_active_golden_state(
        &mut output,
        "cleared notice restores normal footer hints",
        &buffer,
        &[format!(
            "footer text fg={:?}",
            buffer[(buffer.area.x, footer_row)].fg
        )],
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut footer_terminal = Terminal::new(TestBackend::new(200, 1)).expect("footer terminal");
    footer_terminal
        .draw(|frame| {
            let area = frame.area();
            render_chat_footer(frame, test_footer(area), &chat, true);
        })
        .expect("draw composer and host key hints");
    append_active_golden_state(
        &mut output,
        "composer and host key hints",
        footer_terminal.backend().buffer(),
        &[],
    );
    chat.queued_prompts.push_back(queued("queued-1", "next"));
    footer_terminal
        .draw(|frame| {
            let area = frame.area();
            render_chat_footer(frame, test_footer(area), &chat, true);
        })
        .expect("draw composer and host key hints with a queued prompt");
    append_active_golden_state(
        &mut output,
        "queued prompt keeps composer hints",
        footer_terminal.backend().buffer(),
        &[],
    );

    let chat = ChatState::new(&snapshot(), &[]);
    let mut footer_terminal = Terminal::new(TestBackend::new(32, 1)).expect("footer terminal");
    footer_terminal
        .draw(|frame| {
            let area = frame.area();
            render_chat_footer(frame, test_footer(area), &chat, true);
        })
        .expect("draw narrow footer");
    append_active_golden_state(
        &mut output,
        "narrow footer preserves palette and help hints",
        footer_terminal.backend().buffer(),
        &[],
    );

    let model_and_effort = vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "gpt-5.6-sol",
            SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                "gpt-5.6-sol",
                "Sol",
            )]),
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "effort",
            "Effort",
            "high",
            SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                "high", "High",
            )]),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ];
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Running;
    chat.set_prompt_in_flight(true);
    chat.activity_reachable = false;
    chat.set_config_options(&model_and_effort);
    let buffer = active_golden_buffer(&mut chat, 100, 24);
    append_active_golden_state(
        &mut output,
        "live model and effort during an in-flight prompt",
        &buffer,
        &[],
    );

    let mut fast = ChatState::new(&snapshot(), &[]);
    fast.set_config_options(&[fast_mode_option("off")]);
    let buffer = active_golden_buffer(&mut fast, 80, 24);
    append_active_golden_state(&mut output, "fast mode disabled", &buffer, &[]);
    fast.set_config_options(&[fast_mode_option("on")]);
    let buffer = active_golden_buffer(&mut fast, 80, 24);
    append_active_golden_state(&mut output, "fast mode confirmed active", &buffer, &[]);
    fast.set_config_options(&[]);
    let buffer = active_golden_buffer(&mut fast, 80, 24);
    append_active_golden_state(&mut output, "fast mode option removed", &buffer, &[]);

    let mut interrupt = ChatState::new(&snapshot(), &[]);
    interrupt.phase = WorkerPhase::Running;
    interrupt.activity_reachable = false;
    let buffer = active_golden_buffer(&mut interrupt, 80, 24);
    append_active_golden_state(&mut output, "running without our prompt", &buffer, &[]);
    interrupt.set_prompt_in_flight(true);
    let buffer = active_golden_buffer(&mut interrupt, 80, 24);
    append_active_golden_state(&mut output, "our prompt can be interrupted", &buffer, &[]);

    let mut plan = crate::chat::test_support::grok_chat();
    plan.finish_plan_mode_change(true);
    plan.phase = WorkerPhase::Running;
    let buffer = active_golden_buffer(&mut plan, 80, 24);
    append_active_golden_state(&mut output, "plan mode during a turn", &buffer, &[]);

    let mut goal = ChatState::new(&snapshot(), &[]);
    goal.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{"name": "goal", "description": "set a persistent goal"}]
        }),
    );
    goal.apply_event(&SequencedEvent {
        seq: 2,
        recorded_at_ms: None,
        request_id: Some("goal".into()),
        event: WorkerEvent::PromptAccepted {
            request_id: "goal".into(),
            text: "/goal ship the release".into(),
            attachments: Vec::new(),
        },
    });
    goal.activity_reachable = false;
    let buffer = active_golden_buffer(&mut goal, 100, 24);
    append_active_golden_state(&mut output, "advertised active goal title", &buffer, &[]);
    goal.apply_event(&SequencedEvent {
        seq: 3,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::TurnCompleted,
    });
    let buffer = active_golden_buffer(&mut goal, 100, 24);
    append_active_golden_state(
        &mut output,
        "completed goal clears composer title",
        &buffer,
        &[],
    );

    let mut ordinary = ChatState::new(&snapshot(), &[]);
    ordinary.mark_prompt_submitted("/goal ship the release");
    ordinary.set_prompt_in_flight(false);
    let buffer = active_golden_buffer(&mut ordinary, 100, 24);
    append_active_golden_state(
        &mut output,
        "unadvertised goal text stays an ordinary prompt",
        &buffer,
        &[],
    );
    ordinary.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{"name": "goal", "description": "set a persistent goal"}]
        }),
    );
    ordinary.mark_prompt_submitted("please ship the release");
    ordinary.set_prompt_in_flight(false);
    let buffer = active_golden_buffer(&mut ordinary, 100, 24);
    append_active_golden_state(
        &mut output,
        "ordinary text with goal advertised",
        &buffer,
        &[],
    );

    let mut title = ChatState::new(&snapshot(), &[]);
    let buffer = active_golden_buffer(&mut title, 80, 24);
    let title_column = crate::golden::buffer_lines(&buffer)[0]
        .find("Conversation")
        .expect("conversation title is visible") as u16;
    let rule_column = (0..buffer.area.width)
        .rfind(|x| buffer[(*x, 0)].symbol() == "─")
        .expect("quiet rule follows the title");
    append_active_golden_state(
        &mut output,
        "conversation title and quiet rule",
        &buffer,
        &[
            format!(
                "title style {}",
                active_golden_style(&buffer, title_column, 0)
            ),
            format!(
                "rule style {}",
                active_golden_style(&buffer, rule_column, 0)
            ),
        ],
    );

    for ui_theme in theme::UiTheme::ALL {
        let mut controls = ChatState::new(&snapshot(), &[]);
        controls.set_subagent_count(2);
        controls.set_session_activity(SessionActivity {
            pursuing_goal: Default::default(),
            checking_response: false,
            quota_recovery: None,
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            prompt_in_flight: false,
            idle_since_ms: None,
            execution: None,
            harness_turn_started_at_ms: None,
            state: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![mj_core::relay::BackgroundCommand {
                id: "test:task".into(),
                started_at_ms: 0,
                command: "cargo test".into(),
                can_stop: false,
            }],
            active_user_shells: Vec::new(),
        });
        controls.activity_reachable = false;
        let neutral = theme::with_theme(ui_theme, || active_golden_buffer(&mut controls, 100, 24));
        let subagent = controls
            .subagent_control_area
            .expect("subagent control is rendered");
        let task = controls
            .task_control_area
            .expect("task control is rendered");
        let subagent_label = (subagent.x..subagent.right())
            .map(|x| neutral[(x, subagent.y)].symbol())
            .collect::<String>();
        let task_label = (task.x..task.right())
            .map(|x| neutral[(x, task.y)].symbol())
            .collect::<String>();
        let neutral_styles = format!(
            "subagents={subagent_label:?} {} tasks={task_label:?} {}",
            active_golden_style(&neutral, subagent.x, subagent.y),
            active_golden_style(&neutral, task.x, task.y)
        );
        append_active_golden_state(
            &mut output,
            &format!("{} theme: neutral navigation controls", ui_theme.label()),
            &neutral,
            &[neutral_styles],
        );

        controls.subagent_control_focused = true;
        controls.task_control_focused = true;
        let focused = theme::with_theme(ui_theme, || active_golden_buffer(&mut controls, 100, 24));
        let focused_styles = format!(
            "subagents {} tasks {}",
            active_golden_style(&focused, subagent.x, subagent.y),
            active_golden_style(&focused, task.x, task.y)
        );
        append_active_golden_state(
            &mut output,
            &format!("{} theme: focused navigation controls", ui_theme.label()),
            &focused,
            &[focused_styles],
        );
    }

    let pane = Rect::new(50, 2, 48, 26);
    let regions = || ChatRegions {
        transcript: Rect::new(50, 2, 48, 19),
        prompt: Rect::new(50, 21, 48, 7),
        footer: None,
        overlay: pane,
        title_controls: 0,
        title_lead: 0,
        pane_focused: true,
    };
    let mut tasks = ChatState::new(&snapshot(), &[]);
    tasks.task_dialog_open = true;
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("overlay terminal");
    terminal
        .draw(|frame| render_in(frame, &mut tasks, regions(), false, false))
        .expect("draw task dialog in pane");
    let task_buffer = terminal.backend().buffer().clone();
    let task_body = tasks
        .frame_surfaces()
        .surface(SurfaceId::ModalBody)
        .expect("task dialog body is registered")
        .rect;
    append_active_golden_state(
        &mut output,
        "task dialog stays in host overlay",
        &task_buffer,
        &[format!(
            "modal body=({},{},{},{})",
            task_body.x, task_body.y, task_body.width, task_body.height
        )],
    );

    let fixture = mj_client::session::replacement_session_test_fixture("session-pane-modal", 91);
    let mut reviewed = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(chat_context(
            "session-pane-modal",
            &[("claude-1", HarnessKind::Claude)],
        )),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );
    reviewed.open_second_opinion(
        ElicitationRequest {
            id: "plan-pane".into(),
            message: "may I run this plan?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        },
        "the plan".into(),
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("review terminal");
    terminal
        .draw(|frame| reviewed.draw_in(frame, regions(), false, false))
        .expect("draw reviewer setup in pane");
    let review_buffer = terminal.backend().buffer().clone();
    let review_body = reviewed
        .state
        .frame_surfaces()
        .surface(SurfaceId::ModalBody)
        .expect("review setup body is registered")
        .rect;
    append_active_golden_state(
        &mut output,
        "review setup stays in host overlay",
        &review_buffer,
        &[format!(
            "modal body=({},{},{},{})",
            review_body.x, review_body.y, review_body.width, review_body.height
        )],
    );

    let mut regions_chat = ChatState::new(&snapshot(), &[]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("regions terminal");
    let custom_regions = ChatRegions {
        transcript: Rect::new(0, 4, 80, 12),
        prompt: Rect::new(0, 16, 80, 5),
        footer: None,
        overlay: Rect::new(0, 0, 80, 24),
        title_controls: 0,
        title_lead: 0,
        pane_focused: false,
    };
    terminal
        .draw(|frame| render_in(frame, &mut regions_chat, custom_regions, true, false))
        .expect("draw custom chat regions");
    let regions_buffer = terminal.backend().buffer().clone();
    let region_transcript = regions_chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("custom transcript is registered")
        .rect;
    let region_prompt = regions_chat
        .frame_surfaces()
        .surface(SurfaceId::PromptInput)
        .expect("custom prompt is registered")
        .rect;
    append_active_golden_state(
        &mut output,
        "transcript and prompt stay within host regions",
        &regions_buffer,
        &[
            format!("transcript rect={region_transcript:?}"),
            format!("prompt rect={region_prompt:?}"),
        ],
    );

    let mut cursor = ChatState::new(&snapshot(), &[]);
    let cursor_regions = ChatRegions {
        transcript: Rect::new(0, 0, 80, 16),
        prompt: Rect::new(0, 16, 80, 6),
        footer: Some(test_footer(Rect::new(0, 22, 80, 1))),
        overlay: Rect::new(0, 0, 80, 24),
        title_controls: 0,
        title_lead: 0,
        pane_focused: false,
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("cursor terminal");
    terminal
        .draw(|frame| render_in(frame, &mut cursor, cursor_regions, false, false))
        .expect("draw unfocused composer");
    let unfocused = terminal.backend().buffer().clone();
    let unfocused_cursor = terminal.backend_mut().get_cursor_position();
    append_active_golden_state(
        &mut output,
        "composer without keyboard focus",
        &unfocused,
        &[format!("cursor={unfocused_cursor:?}")],
    );
    terminal
        .draw(|frame| render_in(frame, &mut cursor, cursor_regions, true, false))
        .expect("draw focused composer");
    let focused = terminal.backend().buffer().clone();
    let focused_cursor = terminal.backend_mut().get_cursor_position();
    append_active_golden_state(
        &mut output,
        "composer with keyboard focus",
        &focused,
        &[format!("cursor={focused_cursor:?}")],
    );

    mj_core::golden::assert_platform_golden(
        env!("CARGO_MANIFEST_DIR"),
        "active-chat-render",
        &output,
    );
}
