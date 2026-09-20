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
use ratatui::layout::{Position, Rect};
use std::collections::BTreeMap;

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
async fn client_review_state_restores_workflow_without_profile_defaults() {
    let (workflow, _) = ReviewWorkflow::start("proposal", "Review this plan", "context");
    let chat = prepare_storage_test_chat()
        .with_review_state(Ok(mj_client::session::ReviewState {
            review: Some(mj_core::storage::StoredReview {
                workflow,
                generation: 7,
                context_baseline: 12,
                native_lost: false,
                reviewer_transcript: Vec::new(),
            }),
        }))
        .open();
    assert_eq!(chat.reviewer_generation, 7);
    assert_eq!(
        chat.state.second_opinion().unwrap().captured().proposal,
        "Review this plan"
    );
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
                relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
                native_agents: Vec::new(),
                steering: None,
                cancelling_prompt_id: None,
                clear_context: false,
                clear_context_started_at_ms: None,
                native_agent_count: 0,
                expected_continuation: None,
                inferred_idle_since_ms: None,
                goal: Default::default(),
                capacity_retry: None,
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
                checkpoint_only: false,
                acp_ready: None,
                agent_capabilities: None,
                agent_info: None,
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
fn build_prompt_only_appears_without_session_history() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Idle;
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    let mut rendered = |chat: &mut ChatState| {
        terminal
            .draw(|frame| render_full_frame(frame, chat, false))
            .expect("draw chat");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    assert!(rendered(&mut chat).contains("What would you like to build?"));
    chat.transcript_loading = true;
    assert!(!rendered(&mut chat).contains("What would you like to build?"));
    chat.transcript_loading = false;
    chat.unconverted_prefix = 1;
    assert!(!rendered(&mut chat).contains("What would you like to build?"));
    chat.unconverted_prefix = 0;
    chat.entries
        .push(ChatEntry::plain(1, ChatRole::User, "Hello"));
    assert!(!rendered(&mut chat).contains("What would you like to build?"));
    chat.phase = WorkerPhase::Running;
    assert!(rendered(&mut chat).contains("Add a follow-up while the agent works…"));
}

#[test]
fn dictation_appends_to_existing_prompt_cleanly() {
    assert_eq!(append_dictation("please", "fix this"), "please fix this");
    assert_eq!(append_dictation("", "fix this"), "fix this");
    assert_eq!(append_dictation("please ", ""), "please");
}

#[test]
fn detaching_leaves_the_unsent_input_where_the_dashboard_saves_it_from() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("half typed thought".into());

    // Detaching keeps the composer intact: the warm chat goes on holding
    // it, and the surface reads it here to write it to the session row.
    detach_chat(&mut chat);
    assert_eq!(chat.input, "half typed thought");

    detach_chat(&mut chat);
    assert_eq!(chat.input, "half typed thought");
}

#[test]
fn detaching_an_empty_composer_leaves_an_empty_draft_to_save() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    detach_chat(&mut chat);

    assert_eq!(chat.input, "");
}

#[test]
fn an_elicitation_is_bounded_to_the_chat_content_area() {
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
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");

    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("draw elicitation");
    let buffer = terminal.backend().buffer();
    let lines = (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let row_of = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle} in {lines:#?}"))
    };

    let popup_top = row_of("Overlaid dialog");
    row_of("Visible dialog message");
    assert!(!lines.iter().any(|line| line.contains("Background tasks")));
    // The reduced transcript remains visible above the bottom-anchored
    // question, rather than being hidden behind it.
    let sentinel_top = row_of("UNDERLYING CHAT SENTINEL");
    assert!(sentinel_top < popup_top);
    // The footer remains outside the question even when width fitting
    // drops composer hints to make room for the host's function keys.
    assert_eq!(row_of("? keys"), lines.len() - 1);
    assert!(row_of("? keys") > popup_top);
}

#[test]
fn drawing_the_chat_registers_the_transcript_and_prompt_interiors() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    drawn_transcript(&mut chat, 80, 24);

    let surfaces = chat.frame_surfaces();
    let transcript = surfaces
        .surface(SurfaceId::Transcript)
        .expect("transcript registered");
    let prompt = surfaces
        .surface(SurfaceId::PromptInput)
        .expect("prompt registered");
    // The registered rect is the text inside each border, which is what
    // the wheel already hit-tests against.
    assert_eq!(
        surfaces
            .surface_at(prompt.rect.x, prompt.rect.y)
            .map(|surface| surface.id),
        Some(SurfaceId::PromptInput)
    );
    assert!(
        transcript.rect.bottom() <= prompt.rect.y,
        "the transcript sits above the composer: {transcript:?} {prompt:?}"
    );
}

#[test]
fn the_autocomplete_popup_takes_the_cells_it_covers() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("/".into());

    drawn_transcript(&mut chat, 80, 24);

    let surfaces = chat.frame_surfaces();
    let popup = surfaces
        .surface(SurfaceId::AutocompletePopup)
        .expect("popup registered");
    assert_eq!(
        surfaces
            .surface_at(popup.rect.x, popup.rect.bottom() - 1)
            .map(|surface| surface.id),
        Some(SurfaceId::AutocompletePopup)
    );
}

#[test]
fn an_open_elicitation_keeps_the_upper_transcript_and_hides_the_prompt() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "question-1".into(),
            message: "Visible dialog message".into(),
            title: Some("Overlaid dialog".into()),
            description: None,
            fields: Vec::new(),
        },
    ));

    drawn_transcript(&mut chat, 80, 24);

    // The question owns the lower half, while the reduced transcript
    // remains independently selectable and scrollable above it.
    let surfaces = chat.frame_surfaces();
    let transcript = surfaces
        .surface(SurfaceId::Transcript)
        .expect("reduced transcript registered");
    let message = surfaces
        .surface(SurfaceId::ElicitationMessage)
        .expect("message pane registered");
    assert!(surfaces.surface(SurfaceId::ModalBody).is_some());
    assert!(surfaces.surface(SurfaceId::PromptInput).is_none());
    assert!(transcript.rect.bottom() <= message.rect.y);
    assert_eq!(
        surfaces
            .surface_at(message.rect.x, message.rect.y)
            .map(|surface| surface.id),
        Some(SurfaceId::ElicitationMessage)
    );
}

#[test]
fn short_question_stays_natural_and_leaves_more_than_half_to_transcript() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        ElicitationRequest {
            id: "short-question".into(),
            message: "One short question".into(),
            title: Some("Custom title".into()),
            description: None,
            fields: Vec::new(),
        },
    ));
    let natural = chat.elicitation.as_ref().unwrap().natural_height(80);

    drawn_transcript(&mut chat, 80, 24);

    let transcript = chat
        .frame_surfaces()
        .surface(SurfaceId::Transcript)
        .expect("transcript registered");
    let message = chat
        .frame_surfaces()
        .surface(SurfaceId::ElicitationMessage)
        .expect("question message registered");
    let question_height = 23 - transcript.rect.bottom() - 1;
    assert_eq!(question_height, natural);
    assert!(question_height < 23 / 2);
    assert!(transcript.rect.height > message.rect.height);
}

#[test]
fn tall_question_is_capped_at_half_and_keeps_both_surfaces_non_overlapping() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
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
    let natural = chat.elicitation.as_ref().unwrap().natural_height(80);
    assert!(natural > 23 / 2);

    drawn_transcript(&mut chat, 80, 24);

    let surfaces = chat.frame_surfaces();
    let transcript = surfaces
        .surface(SurfaceId::Transcript)
        .expect("transcript registered");
    let message = surfaces
        .surface(SurfaceId::ElicitationMessage)
        .expect("question message registered");
    let question_height = 23 - transcript.rect.bottom() - 1;
    assert_eq!(question_height, 23 / 2);
    assert!(transcript.rect.bottom() <= message.rect.y);
    assert!(surfaces.surface(SurfaceId::PromptInput).is_none());
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
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        id: id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        title: "work".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex-1".into(),
        bundle_id: "bundle-1".into(),
        project_directory: None,
        managed_worktree: None,
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
                        environment: BTreeMap::new(),
                        context_window_bytes: None,
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
    use mj_core::review::lanes::ReviewTier;

    let fixture = mj_client::session::replacement_session_test_fixture("session-review", 88);
    let mut context = chat_context("session-review", &[]);
    context.config.review = mj_core::config::ReviewConfig {
        enabled: true,
        tier: ReviewTier::Extended,
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
        &mj_core::config::ReviewConfig {
            enabled: true,
            tier: ReviewTier::Extended,
            profile: Some("reviewer-a".into()),
            model: None,
            effort: None,
        }
    );

    let mut reloaded = Config::default();
    reloaded.review.profile = Some("reviewer-b".into());
    chat.refresh_context(&reloaded, None, None);

    assert_eq!(chat.state.review_config(), &reloaded.review);
}

/// A failed recovery copy is the one thing the user has to see on opening
/// the session, so it is raised after the connection notice a cold open
/// also sets: a notice is a single slot, and the last write wins.
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

/// A captured plan starts preparation directly; the controller resolves current settings.
#[tokio::test]
async fn a_second_opinion_opens_the_reviewer_preparation_from_the_context() {
    use mj_core::config::HarnessKind;

    let request = ElicitationRequest {
        id: "plan-1".into(),
        message: "may I run this plan?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };

    let fixture = mj_client::session::replacement_session_test_fixture("session-opinion", 83);
    let mut chat = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(chat_context(
            "session-opinion",
            &[("claude-1", HarnessKind::Claude)],
        )),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );

    chat.open_second_opinion(request.clone(), "the plan".into());

    let Some(SecondOpinion::Setup { setup, .. }) = chat.state.second_opinion() else {
        panic!("second opinion opens preparation, not a selector");
    };
    assert!(setup.failure.is_none());
    assert!(chat.reviewer_preparation.is_some());

    let fixture = mj_client::session::replacement_session_test_fixture("session-alone", 84);
    let mut alone = ActiveChat::open(
        fixture.stopped,
        "bundle-1",
        Some(chat_context("session-alone", &[])),
        fixture.control,
        SessionHeaderIdentity::default(),
        String::new(),
        Notices::default(),
    );

    alone.open_second_opinion(request, "the plan".into());

    assert!(matches!(
        alone.state.second_opinion(),
        Some(SecondOpinion::Setup { .. })
    ));
    assert!(alone.reviewer_preparation.is_some());
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
        ..Default::default()
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

    assert_eq!(chat.draft(), "still drafting");
    assert!(chat.state.notice().is_none());
}

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

#[test]
fn leaving_the_chat_reports_the_ordinal_the_user_has_read() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut session = MaterializedSession::empty("session-read");
    session.applied_event_ordinal = 12;
    session.transcript = vec![agent_transcript_item("first", 12)];
    apply_session_view(&mut chat, Ok(managed_view(session)));
    chat.queued_prompts.push_back(queued("queued-1", "queued"));

    assert_eq!(detach_chat(&mut chat), 12);
    // The transcript stays warm for the next visit; the interaction state
    // that belonged to the visit does not.
    assert_eq!(chat.entries.len(), 1);
    assert!(chat.queued_prompts.is_empty());
    assert_eq!(detach_chat(&mut chat), 12);
}

#[test]
fn a_notice_set_through_a_shared_handle_shows_in_the_chat_footer_in_yellow() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let shared = Notices::default();
    chat.notices = shared.clone();
    // Wide enough that the default hint line (over 100 columns) is not
    // truncated, so the footer text comparisons below are meaningful.
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");

    // Set from "outside", the way the surface's clone of the same handle
    // would.
    shared.set("Background import finished");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("draw chat");
    let buffer = terminal.backend().buffer();
    let footer_row = buffer.area.bottom() - 1;
    let footer_text = (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, footer_row)].symbol())
        .collect::<String>();
    assert!(footer_text.contains("Background import finished"));
    assert_eq!(
        buffer[(buffer.area.x, footer_row)].fg,
        theme::palette().warning
    );

    shared.clear();
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("draw chat");
    let buffer = terminal.backend().buffer();
    let footer_text = (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, footer_row)].symbol())
        .collect::<String>();
    assert!(footer_text.contains("Tab pane"), "{footer_text:?}");
    assert_eq!(
        buffer[(buffer.area.x, footer_row)].fg,
        theme::palette().text
    );
}

/// The composer's own row is where a user typing in it learns the keys, so it
/// carries the same groups the dashboard's row does: the composer's own keys,
/// then the host's prefix chords.
#[test]
fn chat_footer_advertises_the_composer_keys_and_the_host_chords() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut terminal = Terminal::new(TestBackend::new(200, 24)).expect("terminal");
    let footer_of = |terminal: &Terminal<TestBackend>| {
        let buffer = terminal.backend().buffer();
        let row = buffer.area.bottom() - 1;
        (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
    };

    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, true))
        .expect("draw chat");
    let footer = footer_of(&terminal);
    for hint in [
        "Ctrl-R history",
        "│ ctrl+b then: b panes · q detach · : palette · ? keys",
    ] {
        assert!(footer.contains(hint), "{footer:?} omits {hint}");
    }
    // Transcript rendering is a registry command now, so the composer's own
    // row no longer names a key for it.
    assert!(!footer.contains("rendering"), "{footer:?}");
    assert!(!footer.contains("Ctrl-G"), "{footer:?}");

    assert!(!footer.contains("Ctrl-T"), "{footer:?}");

    // The queued-prompt variant is a different string and must say the
    // same things about the keys it still names.
    chat.queued_prompts.push_back(queued("queued-1", "next"));
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, true))
        .expect("draw chat with a queued prompt");
    let footer = footer_of(&terminal);
    for hint in [
        "Ctrl-R history",
        "│ ctrl+b then: b panes · q detach · : palette · ? keys",
    ] {
        assert!(footer.contains(hint), "{footer:?} omits {hint}");
    }
}

#[test]
fn narrow_chat_footer_keeps_complete_palette_and_help_hints_on_screen() {
    let chat = ChatState::new(&snapshot(), &[]);
    for width in [6, 20, 32, 40, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_chat_footer(frame, test_footer(area), &chat, true);
            })
            .expect("draw narrow footer");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.trim_end().ends_with("? keys"), "{width}: {text:?}");
        if width >= 20 {
            assert!(text.contains(": palette"), "{width}: {text:?}");
        }
        if width == 32 {
            // The chords give way before the composer's own keys, so what a
            // narrow row keeps is the key that works right here plus the two
            // hints that lead to everything else.
            assert_eq!(text.trim_end(), "Tab pane │ : palette · ? keys");
        }
    }
}

/// `symbols = "ascii"` is for a Linux console or a locale without UTF-8. The
/// composer's own hints were written out with a literal middle dot joining
/// them, so splitting on the glyph set's separator found nothing under the
/// ASCII set and the dot reached the screen anyway. Each branch that builds
/// those hints (idle, queued, and dictating) is checked here.
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
    assert!(idle.contains("Ctrl-V paste"), "{idle:?}");

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

#[test]
fn composer_title_shows_live_model_and_effort_without_outer_session_frame() {
    use agent_client_protocol::schema::v1::{
        SessionConfigSelectOption, SessionConfigSelectOptions,
    };

    let options = vec![
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
    chat.set_config_options(&options);
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");

    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("draw chat");
    let buffer = terminal.backend().buffer();
    let rendered = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert_eq!(rendered.matches("gpt-5.6-sol · high").count(), 1);
    assert!(rendered.contains("Esc cancels"));
    assert!(!rendered.contains("Running"));
    // No outer frame wraps the whole session: the transcript's own titled
    // border is the first thing on the frame, not a session title bar.
    assert!(!rendered.contains("HEL /"));
    assert!(rendered.starts_with("╭ Conversation "), "{rendered:?}");
}

#[test]
fn composer_title_shows_fast_only_while_the_confirmed_mode_is_active() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_config_options(&[fast_mode_option("off")]);
    assert!(!prompt_title(&chat).contains("Fast"));

    chat.set_config_options(&[fast_mode_option("on")]);
    assert_eq!(prompt_title(&chat), " Fast ");

    chat.set_config_options(&[]);
    assert!(!prompt_title(&chat).contains("Fast"));
}

/// The relay cannot cancel a turn the harness started on its own, so the
/// composer offers Esc only while a prompt of ours is in flight.
#[test]
fn composer_bottom_offers_esc_only_while_a_prompt_of_ours_is_in_flight() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Running;
    assert!(prompt_bottom_queue_control(&chat).is_none());

    chat.set_prompt_in_flight(true);
    assert_eq!(
        prompt_bottom_queue_control(&chat).unwrap().to_string(),
        " Esc cancels"
    );
    assert!(!prompt_title(&chat).contains("Esc cancels"));
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
                "Esc cancels",
                "Interrupting turn…",
                "Cancellation requested",
            ),
            (
                Some(true),
                None,
                "Esc cancels",
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
            let hint = if targeted { hint } else { "Esc cancels" };
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
    assert!(screen.contains("View tasks (2)"), "{screen}");

    // The spinner represents a running turn, whatever it left behind.
    chat.phase = WorkerPhase::Running;
    assert!(!prompt_title(&chat).contains("Running"));
}

#[test]
fn composer_title_names_plan_mode_even_during_a_turn() {
    let mut chat = crate::chat::test_support::grok_chat();
    chat.finish_plan_mode_change(true);
    chat.phase = WorkerPhase::Running;

    assert!(prompt_title(&chat).contains("PLAN MODE"));
    assert!(!prompt_title(&chat).contains("Prompt"));
}

#[test]
fn effort_separator_between_prompt_chips_is_not_clickable() {
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
        format!("{} ", chat.current_effort().unwrap())
    );
    assert_eq!(title.spans[separator + 1].style, theme::selection(false));

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

#[test]
fn subagents_are_blue_highlighted_as_clickable_on_prompt_border() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_subagent_count(2);
    let mut terminal =
        Terminal::new(TestBackend::new(100, 24)).expect("test terminal supports drawing");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("chat draws");
    let area = chat
        .subagent_control_area
        .expect("the sub-agent control is rendered");
    let buffer = terminal.backend().buffer();
    let label = (area.x..area.right())
        .map(|x| buffer[(x, area.y)].symbol())
        .collect::<String>();
    assert_eq!(label, " Subagents · 0 working ");
    assert!((area.x..area.right()).all(|x| {
        let cell = &buffer[(x, area.y)];
        cell.bg == theme::palette().selection && cell.fg == theme::palette().text
    }));
}

#[test]
fn running_tasks_are_blue_highlighted_as_clickable_on_prompt_border() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
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
    let mut terminal =
        Terminal::new(TestBackend::new(100, 24)).expect("test terminal supports drawing");
    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("chat draws");
    let area = chat
        .task_control_area
        .expect("the running-task control is rendered");
    let buffer = terminal.backend().buffer();
    let label = (area.x..area.right())
        .map(|x| buffer[(x, area.y)].symbol())
        .collect::<String>();
    assert_eq!(label, " View tasks (1) ");
    assert!((area.x..area.right()).all(|x| {
        let cell = &buffer[(x, area.y)];
        cell.bg == theme::palette().selection && cell.fg == theme::palette().text
    }));
}

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

#[test]
fn composer_title_identifies_an_active_advertised_goal() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "goal", "description": "set a persistent goal"}
            ]
        }),
    );
    chat.apply_event(&SequencedEvent {
        seq: 2,
        recorded_at_ms: None,
        request_id: Some("goal".into()),
        event: WorkerEvent::PromptAccepted {
            request_id: "goal".into(),
            text: "/goal ship the release".into(),
            attachments: Vec::new(),
        },
    });

    assert!(prompt_title(&chat).contains("Pursuing goal"));
    assert!(!prompt_title(&chat).contains("Running"));

    chat.apply_event(&SequencedEvent {
        seq: 3,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::TurnCompleted,
    });
    assert!(prompt_title(&chat).is_empty());
    assert!(!prompt_title(&chat).contains("Pursuing goal"));
}

#[test]
fn composer_title_does_not_label_ordinary_or_unadvertised_prompts_as_goals() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.mark_prompt_submitted("/goal ship the release");
    assert!(!prompt_title(&chat).contains("Running"));
    assert!(!prompt_title(&chat).contains("Pursuing goal"));

    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "goal", "description": "set a persistent goal"}
            ]
        }),
    );
    chat.mark_prompt_submitted("please ship the release");
    assert!(!prompt_title(&chat).contains("Running"));
    assert!(!prompt_title(&chat).contains("Pursuing goal"));
}

/// The title names the conversation you are in. The rule around it is
/// chrome and stays dim; the name draws bright white so it stands out.
#[test]
fn the_conversation_title_remains_readable_above_its_quiet_rule() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

    terminal
        .draw(|frame| render_full_frame(frame, &mut chat, false))
        .expect("draw chat");

    let buffer = terminal.backend().buffer();
    let cells: Vec<_> = (0..80)
        .map(|x| buffer[(x, 0)].symbol().to_owned())
        .collect();
    let title_start = cells
        .windows(1)
        .position(|cell| cell[0] == "C")
        .expect("the title is on the top row");
    for offset in 0.."Conversation".chars().count() {
        let column = u16::try_from(title_start + offset).unwrap();
        assert_eq!(
            buffer[(column, 0)].fg,
            theme::palette().text,
            "the title draws bright white: {}",
            cells.concat()
        );
    }
    let rule = cells
        .iter()
        .rposition(|cell| cell == "\u{2500}")
        .expect("the rule follows the title");
    assert_eq!(
        buffer[(u16::try_from(rule).unwrap(), 0)].fg,
        theme::palette().border,
        "the rule stays chrome"
    );
}

/// The pane a conversation is drawn into is the whole of what its dialogs
/// may cover. A host with several panes gives each one a sub-rectangle of
/// the terminal, so a dialog centred in the frame would paint over its
/// neighbours.
#[tokio::test]
async fn the_task_dialog_and_setup_form_are_centred_within_their_overlay_rect() {
    use mj_core::config::HarnessKind;

    let pane = Rect::new(50, 2, 48, 26);
    let regions = || ChatRegions {
        transcript: Rect::new(50, 2, 48, 19),
        prompt: Rect::new(50, 21, 48, 7),
        footer: None,
        overlay: pane,
        title_controls: 0,
        pane_focused: true,
    };
    let outside_is_untouched = |terminal: &Terminal<TestBackend>, what: &str| {
        let buffer = terminal.backend().buffer();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                if pane.contains(Position::new(x, y)) {
                    continue;
                }
                assert_eq!(
                    buffer[(x, y)],
                    ratatui::buffer::Cell::default(),
                    "{what} touched ({x}, {y}) outside its pane"
                );
            }
        }
    };
    let modal_body = |chat: &ChatState, what: &str| {
        let body = chat
            .frame_surfaces()
            .surface(SurfaceId::ModalBody)
            .unwrap_or_else(|| panic!("{what} registers a modal body"))
            .rect;
        assert!(
            pane.contains(Position::new(body.x, body.y))
                && pane.contains(Position::new(body.right() - 1, body.bottom() - 1)),
            "{what} body {body:?} must lie inside the pane {pane:?}"
        );
        body
    };

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.task_dialog_open = true;
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");
    terminal
        .draw(|frame| render_in(frame, &mut chat, regions(), false, false))
        .expect("draw the background-task dialog");
    modal_body(&chat, "the background-task dialog");
    outside_is_untouched(&terminal, "the background-task dialog");

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
    assert!(
        reviewed.state.second_opinion().is_some(),
        "the reviewer setup form is open"
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("terminal");
    terminal
        .draw(|frame| reviewed.draw_in(frame, regions(), false, false))
        .expect("draw the reviewer setup form");
    modal_body(&reviewed.state, "the reviewer setup form");
    outside_is_untouched(&terminal, "the reviewer setup form");
}

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
fn draw_in_places_the_transcript_and_prompt_in_the_given_regions() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let regions = ChatRegions {
        transcript: Rect::new(0, 4, 80, 12),
        prompt: Rect::new(0, 16, 80, 5),
        footer: None,
        overlay: Rect::new(0, 0, 80, 24),
        title_controls: 0,
        pane_focused: false,
    };

    terminal
        .draw(|frame| render_in(frame, &mut chat, regions, true, false))
        .expect("draw chat");

    let buffer = terminal.backend().buffer();
    let row = |y: u16| -> String {
        (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    };
    for y in 0..4 {
        assert_eq!(
            row(y).trim(),
            "",
            "row {y} sits above the transcript region and must stay untouched"
        );
    }
    assert!(
        row(4).contains("Conversation"),
        "the transcript's titled border is the region's first row: {:?}",
        row(4)
    );
    assert!(row(16).contains(voice_button_glyph()), "{:?}", row(16));
    assert!(!row(16).contains("Prompt"), "{:?}", row(16));
    assert_eq!(
        row(17).chars().take(4).collect::<String>(),
        "│> W",
        "{:?}",
        row(17)
    );
    // Focus changes the border's color without changing its geometry.
    assert!(
        row(20).starts_with('╰'),
        "the prompt's bottom border closes the region: {:?}",
        row(20)
    );
    for y in 21..24 {
        assert_eq!(
            row(y).trim(),
            "",
            "row {y} sits below the prompt region and must stay untouched"
        );
    }
}

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
        assert!(
            running[prompt_bottom].contains("Esc cancels"),
            "{running:?}"
        );
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
#[test]
fn draw_in_draws_a_cursor_only_when_the_prompt_has_focus() {
    use ratatui::backend::Backend as _;

    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let regions = ChatRegions {
        transcript: Rect::new(0, 0, 80, 16),
        prompt: Rect::new(0, 16, 80, 6),
        footer: Some(test_footer(Rect::new(0, 22, 80, 1))),
        overlay: Rect::new(0, 0, 80, 24),
        title_controls: 0,
        pane_focused: false,
    };

    terminal
        .draw(|frame| render_in(frame, &mut chat, regions, false, false))
        .expect("draw chat");
    terminal.backend_mut().assert_cursor_position((0, 0));

    terminal
        .draw(|frame| render_in(frame, &mut chat, regions, true, false))
        .expect("draw chat");
    let cursor = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor position");
    assert!(
        cursor.y > 16 && cursor.y < 21,
        "the cursor sits inside the prompt region: {cursor:?}"
    );

    let boolean_question = ElicitationRequest {
        id: "boolean-question".into(),
        message: "Should I continue?".into(),
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
            kind: ElicitationFieldKind::Boolean { default: None },
        }],
    };
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        boolean_question,
    ));
    let mut question_terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    question_terminal
        .draw(|frame| render_in(frame, &mut chat, regions, true, false))
        .expect("draw question");
    question_terminal
        .backend_mut()
        .assert_cursor_position((0, 0));

    let text_question = ElicitationRequest {
        id: "text-question".into(),
        message: "What should I call it?".into(),
        title: None,
        description: None,
        fields: vec![ElicitationField {
            id: "name".into(),
            title: "Name".into(),
            description: None,
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
    };
    chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
        text_question,
    ));
    let mut text_terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    text_terminal
        .draw(|frame| render_in(frame, &mut chat, regions, true, false))
        .expect("draw text question");
    let question_cursor = text_terminal
        .backend_mut()
        .get_cursor_position()
        .expect("question cursor position");
    assert!(
        question_cursor.y < regions.prompt.bottom(),
        "the text question owns its cursor: {question_cursor:?}"
    );
    assert_ne!(question_cursor, Position::new(0, 0));
}

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

    let exhausted = apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::TranscriptPrefix {
            attempt: MAX_PREFIX_CONVERSION_ATTEMPTS,
            result: Ok((stale, Vec::new())),
        },
    );

    assert_eq!(exhausted, PrefixRebuild::NotNeeded);
    assert_eq!(chat.unconverted_prefix(), pending);
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

    let rebuild = apply_chat_io_update(
        &mut chat,
        ChatIoUpdate::TranscriptPrefix {
            attempt: 1,
            result: Err("worker panicked".into()),
        },
    );

    assert_eq!(rebuild, PrefixRebuild::NotNeeded);
    assert_eq!(
        chat.notice().as_deref(),
        Some("Earlier messages failed to load: worker panicked")
    );
}
