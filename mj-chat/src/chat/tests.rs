use super::*;
use crate::chat::test_support::{
    advertise, alt, ctrl, drawn_transcript, fast_mode_option, grok_chat, key, mode_config_option,
    queued, select_config_option, snapshot,
};
use crate::selection::SurfaceId;
use base64::Engine;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use mj_client::review::RuntimeReviewView;
use mj_core::relay::ActivePrompt;
use mj_core::review::driver::TurnReviewPhase;
use mj_core::review::lanes::ReviewTier;

#[test]
fn activity_animation_stops_when_foreground_and_background_work_settle() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    assert!(!chat.needs_animation());
    chat.phase = WorkerPhase::Running;
    assert!(!chat.needs_animation());
    chat.turn_started_at_epoch_seconds = Some(1);
    assert!(chat.needs_animation());
    chat.activity_reachable = false;
    assert!(!chat.needs_animation());
    chat.activity_reachable = true;
    chat.turn_started_at_epoch_seconds = None;
    chat.phase = WorkerPhase::Idle;
    chat.session_activity.foreground_tool_started_at_ms = Some(1);
    assert!(chat.needs_animation());
    chat.session_activity = mj_client::usage_format::SessionActivity::default();
    assert!(!chat.needs_animation());
    chat.phase = WorkerPhase::Closing;
    assert!(chat.needs_animation());
    // A lifecycle snapshot can still carry the old primary activity when
    // the terminal has already delivered Closed. The settled phase wins.
    chat.phase = WorkerPhase::Closed;
    chat.session_activity.execution = Some(mj_core::relay::RelayExecutionState::Running);
    chat.session_activity.foreground_tool_started_at_ms = Some(1);
    assert!(!chat.needs_animation());
}

/// A standby composer edits exactly like the attached one — the readline
/// chords, paste with normalized line endings — and Enter on a plain prompt
/// queues it: the input clears, the text becomes a preview, and the host gets
/// a `Prompt` action. A command keeps the draft and explains, and command
/// completion stays closed.
#[test]
fn a_standby_composer_edits_like_the_real_one_and_queues_its_prompt() {
    let config: Config = serde_json::from_str(r#"{"version": 0}"#).expect("default config");
    let mut chat = ChatState::standby(
        "session-1",
        &config,
        SessionHeaderIdentity::default(),
        Notices::default(),
    );
    chat.set_draft("alpha beta".into());
    assert_eq!(chat.input_cursor, "alpha beta".len());

    // The readline set the type-ahead pane never answered.
    chat.handle_key(ctrl('a'));
    chat.handle_key(ctrl('k'));
    assert_eq!(chat.input, "");
    chat.handle_key(ctrl('y'));
    assert_eq!(chat.input, "alpha beta");

    chat.paste("…\r\nsecond");
    assert_eq!(chat.input, "alpha beta…\nsecond");

    // A command cannot be answered while the session is offline, so it is
    // consumed with an explanation and the draft stays put.
    chat.set_input("/help".into());
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert!(chat.notice().is_some());
    assert_eq!(chat.draft(), "/help");
    assert!(chat.queued_prompt_texts().is_empty());

    chat.set_input("alpha beta…\nsecond".into());
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt("alpha beta…\nsecond".into()),
        "Enter must hand a plain prompt to the host"
    );
    assert_eq!(chat.draft(), "");
    assert_eq!(chat.queued_prompt_texts(), vec!["alpha beta…\nsecond"]);
    assert!(chat.remove_queued_prompt_text("alpha beta…\nsecond"));
    assert!(chat.queued_prompt_texts().is_empty());

    chat.set_input("/mod".into());
    chat.update_autocomplete();
    assert!(chat.autocomplete.is_none());
}

#[test]
fn idle_background_work_and_working_review_keep_animation_independent() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.session_activity
        .background_commands
        .push(mj_core::relay::BackgroundCommand {
            id: "test:background".into(),
            started_at_ms: 1,
            command: "cargo test".into(),
            can_stop: false,
        });
    assert!(chat.needs_animation());

    chat.phase = WorkerPhase::Closed;
    assert!(!chat.needs_animation());

    chat.set_turn_review(Some(RuntimeReviewView {
        session_id: "session-1".into(),
        tier: ReviewTier::Quick,
        phase: TurnReviewPhase::CapturingDelta,
        roles: Vec::new(),
        status: "capturing the turn".into(),
        verdict: None,
    }));
    assert!(chat.needs_animation());
}

/// Mirrors what `ActiveChat::open` does for a session with no warm view:
/// build the state from the snapshot, then seed the saved draft.
fn freshly_opened_chat(saved_draft: &str) -> ChatState {
    let mut chat =
        ChatState::from_materialized(&MaterializedSession::empty("session-fresh"), &[], &[]);
    chat.set_history_context("bundle-1");
    chat.restore_draft(saved_draft.to_owned());
    chat
}

fn test_image() -> ClipboardImage {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ])
            .unwrap();
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    ClipboardImage::from_png_base64(encoded).unwrap()
}

fn background_task(id: &str, command: &str, can_stop: bool) -> mj_core::relay::BackgroundCommand {
    mj_core::relay::BackgroundCommand {
        id: id.into(),
        started_at_ms: mj_core::clock::epoch_millis() - 1_000,
        command: command.into(),
        can_stop,
    }
}

#[test]
fn image_paste_submits_markers_as_images_at_the_cursor() {
    let image = test_image();
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.handle_clipboard_content(ClipboardContent::Image(image.clone()));
    assert_eq!(chat.input, "[image 1]");
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt("[image 1]".into())
    );
    assert_eq!(chat.take_submitting_images()[0].image, image);
    assert!(chat.input_images.is_empty());

    chat.set_input("compare  and this".into());
    chat.input_cursor = "compare ".len();
    chat.handle_clipboard_content(ClipboardContent::Image(image.clone()));
    chat.handle_key(key(KeyCode::End));
    chat.handle_clipboard_content(ClipboardContent::Image(image.clone()));
    let payload = chat.draft_payload();
    assert_eq!(payload.text, "compare [image 2] and this[image 3]");
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt(payload.text.clone())
    );
    assert_eq!(chat.take_submitting_images(), payload.images);
    assert!(matches!(
        payload.content_blocks().as_slice(),
        [
            ContentBlock::Text(_),
            ContentBlock::Image(_),
            ContentBlock::Text(_),
            ContentBlock::Image(_)
        ]
    ));
}

#[test]
fn image_markers_move_and_delete_as_one_item() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("keep ".into());
    chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
    let end = chat.input_cursor;
    chat.handle_key(key(KeyCode::Left));
    assert_eq!(chat.input_cursor, 5);
    chat.handle_key(key(KeyCode::Right));
    assert_eq!(chat.input_cursor, end);
    chat.handle_key(key(KeyCode::Backspace));
    assert_eq!(chat.input, "keep ");
    assert!(chat.input_images.is_empty());
    chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
    chat.handle_key(key(KeyCode::Left));
    chat.handle_key(key(KeyCode::Delete));
    assert_eq!(chat.input, "keep ");
    assert!(chat.input_images.is_empty());
}

#[test]
fn kill_and_yank_preserve_images_and_renumber_copies() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
    chat.handle_key(ctrl('u'));
    assert!(chat.input.is_empty());
    assert!(chat.input_images.is_empty());
    chat.handle_key(ctrl('y'));
    chat.handle_key(ctrl('y'));
    assert_eq!(chat.input, "[image 1][image 2]");
    assert_eq!(chat.input_images.len(), 2);
    assert_eq!(chat.input_images[0].image, chat.input_images[1].image);
}

#[test]
fn composer_renders_numbered_images_and_advertises_control_v() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
    chat.feedback.clear();
    let screen = test_support::drawn_transcript(&mut chat, 160, 30).join("\n");
    assert!(screen.contains("[image 1]"));
    assert!(screen.contains("Ctrl-V paste"));
    chat.handle_key(key(KeyCode::Backspace));
    let screen = test_support::drawn_transcript(&mut chat, 160, 30).join("\n");
    assert!(!screen.contains("[image 1]"));
}

#[test]
fn legacy_failed_image_submission_remains_recoverable() {
    let image = test_image();
    let saved = serde_json::json!({
        "text": "", "image": null,
        "unsent": [{"kind": "Prompt", "text": "inspect ", "image": image,
            "error": "offline", "recorded_at_ms": 42}]
    });
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.restore_draft(format!("{CHAT_DRAFT_PREFIX}{saved}"));
    chat.restore_latest_unsent_prompt();
    assert_eq!(
        chat.draft_payload(),
        PromptPayload::with_image("inspect ", image)
    );
}

#[test]
fn image_draft_round_trips_and_plain_text_drafts_stay_compatible() {
    let payload = PromptPayload::with_image("describe this", test_image());
    let encoded = payload.encode_draft();
    assert!(encoded.starts_with(CHAT_DRAFT_PREFIX));
    assert_eq!(PromptPayload::decode_draft(&encoded).unwrap(), payload);
    assert_eq!(
        PromptPayload::decode_draft("plain draft").unwrap(),
        PromptPayload::text("plain draft")
    );

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.restore_draft(encoded);
    assert_eq!(chat.draft_payload(), payload);
}

#[test]
fn failed_image_submission_preserves_newer_images_and_survives_reopening() {
    let original = PromptPayload::with_image("inspect old image ", test_image());
    let mut newer = test_image();
    newer.mime_type = "image/jpeg".into();
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.handle_clipboard_content(ClipboardContent::Image(newer.clone()));
    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::Prompt {
            command_id: "test-submit".into(),
            text: original.text.clone(),
            images: original.images.clone(),
            result: Err("offline".into()),
        },
    );
    assert_eq!(chat.input, "inspect old image [image 2]\n\n[image 1]");
    assert_eq!(chat.input_images[0].image, original.images[0].image);
    assert_eq!(chat.input_images[1].image, newer);
    let saved = chat.draft_payload();
    let mut reopened = ChatState::new(&snapshot(), &[]);
    reopened.restore_draft(chat.encoded_draft());
    assert_eq!(reopened.draft_payload(), saved);
    let retry = KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    reopened.handle_key(retry);
    assert_eq!(
        reopened.draft_payload(),
        saved,
        "retry must not overwrite a newer draft"
    );
    reopened.clear_input();
    reopened.handle_key(retry);
    assert_eq!(reopened.draft_payload(), original);
    assert_eq!(
        reopened.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt(original.text)
    );
    assert_eq!(reopened.take_submitting_images(), original.images);
}

#[test]
fn local_commands_cannot_silently_discard_an_attached_image() {
    for input in ["!pwd ", "/help ", "/model ", "/plan inspect "] {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input(input.into());
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        let before = chat.draft_payload();
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.draft_payload(), before);
    }
}

#[test]
fn attach_command_accumulates_after_existing_image_markers() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
    chat.handle_paste("/attach /tmp/second.png");

    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Attach {
            path: "/tmp/second.png".into(),
            command: "/attach /tmp/second.png".into(),
        }
    );
    assert_eq!(chat.input, "[image 1]");
    assert_eq!(chat.input_images.len(), 1);

    assert!(chat.reserve_attachment(0));
    assert_eq!(chat.input, "[image 1][image 2]");
    assert_eq!(chat.input_images.len(), 2);
}

#[test]
fn pending_attachment_is_failed_when_a_saved_draft_is_restored() {
    let payload = PromptPayload::with_image("inspect ", ClipboardImage::pending());
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.restore_draft(payload.encode_draft());

    assert_eq!(chat.input, "inspect [image 1]");
    assert!(chat.input_images[0].image.is_placeholder());
    assert!(!chat.input_images[0].image.is_pending());
}

#[test]
fn removed_pending_attachment_releases_visible_capacity() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for sequence in 0..MAX_IMAGES as u64 {
        assert!(chat.reserve_attachment(sequence));
    }
    assert!(!chat.reserve_attachment(MAX_IMAGES as u64));

    chat.handle_key(key(KeyCode::Backspace));
    assert!(chat.reserve_attachment(MAX_IMAGES as u64 + 1));
    assert_eq!(chat.input_images.len(), MAX_IMAGES);
}

#[test]
fn a_literal_draft_envelope_prefix_round_trips_as_text() {
    let payload = PromptPayload::text(format!(r#"{CHAT_DRAFT_PREFIX}{{"text":"literal"}}"#));
    assert_eq!(
        PromptPayload::decode_draft(&payload.encode_draft()).unwrap(),
        payload
    );
}

#[test]
fn dictation_toggle_is_inert_until_voice_is_available() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    assert_eq!(chat.dictation_toggle_action(), ChatAction::None);

    chat.set_voice_available(true);
    assert_eq!(chat.dictation_toggle_action(), ChatAction::ToggleVoice);

    // The key that used to start dictation is the host's now, so the composer
    // reads Alt-V as nothing at all.
    assert_eq!(
        chat.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)),
        ChatAction::None
    );
    assert!(chat.input.is_empty());
}

#[test]
fn active_voice_remains_stoppable_after_availability_is_lost() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.voice_active = true;
    chat.voice_button_area = Some(Rect::new(10, 8, 4, 1));

    assert_eq!(chat.dictation_toggle_action(), ChatAction::ToggleVoice);
    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }),
        ChatAction::ToggleVoice
    );
}

#[test]
fn disabled_voice_button_click_is_inert() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.voice_button_area = Some(Rect::new(10, 8, 4, 1));

    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }),
        ChatAction::None
    );
}

#[test]
fn enabled_voice_button_click_toggles_voice() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_voice_available(true);
    chat.voice_button_area = Some(Rect::new(10, 8, 4, 1));

    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 8,
            modifiers: KeyModifiers::NONE,
        }),
        ChatAction::ToggleVoice
    );
}

#[test]
fn enter_does_not_submit_while_voice_is_active() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.voice_active = true;
    chat.set_input("dictated draft".into());

    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(chat.input, "dictated draft");
}

#[test]
fn microphone_button_hitbox_occupies_prompt_upper_left() {
    let prompt = Rect::new(4, 2, 30, 5);
    let button = voice_button_area(prompt).expect("button fits");
    assert_eq!(button.y, prompt.y);
    assert_eq!(button.x, prompt.x + 1);
    assert_eq!(button.width, 3);
    assert!(voice_button_area(Rect::new(0, 0, 4, 3)).is_none());
    assert!(voice_button_area(Rect::new(0, 0, 5, 3)).is_some());
}

#[test]
fn regular_prompt_draws_microphone_at_its_upper_left() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_voice_available(true);
    chat.mark_prompt_submitted("continue");

    let rows = drawn_transcript(&mut chat, 80, 24);
    assert!(
        rows.iter().any(|row| row.contains("🎙︎")),
        "microphone button missing from prompt border: {rows:?}"
    );
    let button = chat.voice_button_area.expect("button hitbox");
    let border = &rows[usize::from(button.y)];
    let mic_offset = border.find("🎙︎").expect("microphone on top border");
    assert_eq!(
        rendering::display_width(&border[..mic_offset]),
        usize::from(button.x + 1)
    );
    assert_eq!(button.x, 1);
    assert_eq!(
        button.y,
        chat.frame_surfaces()
            .surface(SurfaceId::PromptInput)
            .unwrap()
            .rect
            .y
            .saturating_sub(1)
    );
    let press = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: button.x + 1,
        row: button.y,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(chat.handle_mouse(press), ChatAction::None);
    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        }),
        ChatAction::ToggleVoice
    );
}

#[test]
fn capacity_wait_displays_a_countdown_and_escape_cancels_it() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.header_target = "localhost".into();
    chat.header_profile = "codex".into();
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        capacity_retry: Some(mj_core::relay::CapacityRetry {
            attempt: 1,
            retry_at_ms: 120000,
            command_id: "capacity-retry-42".into(),
            submitted: false,
        }),
        ..Default::default()
    });
    assert!(chat.clock_text(60).contains("retrying in 1m00s"));
    assert!(matches!(
        chat.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ChatAction::Cancel
    ));
}

#[test]
fn clock_sampling_tracks_displayed_units_and_keeps_the_drawn_baseline() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.header_target.clear();
    chat.header_profile.clear();
    assert_eq!(chat.clock_text(100), chat.clock_text(101));
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: vec![mj_core::relay::BackgroundCommand {
            id: "test:clock".into(),
            started_at_ms: 0,
            command: "cargo test".into(),
            can_stop: false,
        }],
        ..mj_client::usage_format::SessionActivity::default()
    });
    // The task count is static until its elapsed-time dialog is opened.
    assert_eq!(chat.clock_text(100), chat.clock_text(101));
    chat.open_task_dialog();
    assert_ne!(chat.clock_text(100), chat.clock_text(101));
    assert_eq!(chat.clock_text(3_600), chat.clock_text(3_601));
    chat.last_clock_text = Some("previous frame".into());
    assert!(chat.clock_changed());
    assert!(
        chat.clock_changed(),
        "sampling must not acknowledge an undrawn frame"
    );
    assert_eq!(chat.last_clock_text.as_deref(), Some("previous frame"));
}

#[test]
fn background_tasks_use_the_prompt_border_and_open_a_task_dialog() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("keep this draft".into());
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: vec![mj_core::relay::BackgroundCommand {
            id: "test:dialog".into(),
            started_at_ms: mj_core::clock::epoch_millis() - 61_000,
            command: "cargo test --workspace".into(),
            can_stop: true,
        }],
        ..mj_client::usage_format::SessionActivity::default()
    });

    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(screen.contains("View tasks (1)"), "{screen}");
    assert!(!screen.contains("oldest"), "{screen}");
    assert!(!screen.contains("Background:"), "{screen}");

    let task_area = chat.task_control_area.expect("task control hitbox");
    chat.mark_prompt_submitted("continue");
    chat.steering_supported = Some(true);
    chat.queued_prompts.push_back(queued("next", "follow up"));
    let rows = drawn_transcript(&mut chat, 100, 24);
    let shifted_task_area = chat.task_control_area.expect("shifted task control hitbox");
    assert!(shifted_task_area.x > task_area.x);
    let border = &rows[usize::from(shifted_task_area.y)];
    assert!(border.contains("1 queued · Esc steers next"), "{border}");
    let task_offset = border.find("View tasks (1)").expect("task label");
    assert_eq!(
        rendering::display_width(&border[..task_offset]),
        usize::from(shifted_task_area.x + 1)
    );
    for width in [32, 48, 56, 80] {
        let rows = drawn_transcript(&mut chat, width, 24);
        let border = &rows[usize::from(shifted_task_area.y)];
        assert!(border.contains("1 queued · Esc steers next"), "{border}");
        if width == 32 {
            assert!(chat.task_control_area.is_none());
            assert!(!border.contains("View tasks"), "{border}");
        } else {
            assert!(chat.task_control_area.is_some());
            assert!(border.contains("View tasks (1)"), "{border}");
        }
    }
    drawn_transcript(&mut chat, 100, 24);
    let task_area = shifted_task_area;
    let cursor_before = chat.input_cursor;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: task_area.x,
        row: task_area.y,
        modifiers: KeyModifiers::NONE,
    };
    assert!(chat.component_handles_mouse(click));
    assert_eq!(chat.handle_mouse(click), ChatAction::None);
    assert!(chat.task_dialog_open());
    assert_eq!(chat.input_cursor, cursor_before);
    drawn_transcript(&mut chat, 100, 24);
    let dialog_inner = chat.task_dialog_area.expect("task dialog geometry");
    let dismiss = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: dialog_inner.x.saturating_add(1),
        row: dialog_inner.y.saturating_sub(1),
        modifiers: KeyModifiers::NONE,
    };
    assert!(chat.component_handles_mouse(dismiss));
    assert_eq!(chat.handle_mouse(dismiss), ChatAction::None);
    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..dismiss
        }),
        ChatAction::None
    );
    assert!(!chat.task_dialog_open());

    assert_eq!(chat.handle_key(key(KeyCode::Down)), ChatAction::None);
    assert!(chat.task_control_focused());
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert!(chat.task_dialog_open());
    assert_eq!(chat.input, "keep this draft");

    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(screen.contains("Background tasks"), "{screen}");
    assert!(screen.contains("cargo test --workspace"), "{screen}");
    assert!(screen.contains("Esc close"), "{screen}");

    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);
    assert!(!chat.task_dialog_open());
    assert_eq!(chat.input, "keep this draft");

    // The dialog remains drawable on a cramped terminal, and wrapping a
    // long command contributes rows to scrolling rather than disappearing
    // after one entry.
    chat.open_task_dialog();
    chat.session_activity.background_commands[0].command =
        "cargo test --all-targets --all-features --workspace".into();
    for height in [1, 4, 8] {
        let screen = drawn_transcript(&mut chat, 24, height).join("\n");
        if height >= 4 {
            assert!(screen.contains("×"), "height={height}: {screen}");
            assert!(
                screen.contains("Background task"),
                "height={height}: {screen}"
            );
        }
    }
    for _ in 0..20 {
        chat.handle_key(key(KeyCode::Down));
    }
    assert!(chat.task_dialog_scroll > 0);
    let tail = drawn_transcript(&mut chat, 24, 8).join("\n");
    assert!(tail.contains("ace"), "{tail}");

    chat.session_activity.background_commands[0].command = format!(
        "cargo test {}FINAL_ARGUMENT",
        "--feature example ".repeat(40)
    );
    drawn_transcript(&mut chat, 80, 12);
    for _ in 0..100 {
        chat.handle_key(key(KeyCode::Down));
    }
    let tail = drawn_transcript(&mut chat, 80, 12).join("\n");
    assert!(tail.contains("FINAL_ARGUMENT"), "{tail}");

    chat.set_session_activity(mj_client::usage_format::SessionActivity::default());
    let empty = drawn_transcript(&mut chat, 80, 12).join("\n");
    assert!(empty.contains("No background tasks remain."), "{empty}");
}

#[test]
fn subagents_use_the_prompt_border_and_activate_by_keyboard_or_mouse() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("keep this draft".into());
    chat.set_subagent_count(2);

    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(screen.contains("Sub-agents (2)"), "{screen}");
    let area = chat
        .subagent_control_area
        .expect("sub-agent control hitbox");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    };
    assert!(chat.component_handles_mouse(click));
    assert_eq!(chat.handle_mouse(click), ChatAction::OpenSubagents);

    assert_eq!(chat.handle_key(key(KeyCode::Down)), ChatAction::None);
    assert!(chat.subagent_control_focused());
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::OpenSubagents
    );
    assert_eq!(chat.input, "keep this draft");
}

#[test]
fn stoppable_background_task_keyboard_activation_is_deduplicated() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: vec![background_task("task-1", "cargo test", true)],
        ..mj_client::usage_format::SessionActivity::default()
    });
    chat.open_task_dialog();
    drawn_transcript(&mut chat, 80, 16);

    // Tab focuses the first Stop button; Enter submits it immediately.
    assert_eq!(chat.handle_key(key(KeyCode::Tab)), ChatAction::None);
    assert_eq!(chat.handle_key(key(KeyCode::BackTab)), ChatAction::None);
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::StopBackgroundTask {
            id: "task-1".into()
        }
    );
    assert!(chat.background_stop_pending("task-1"));
    drawn_transcript(&mut chat, 80, 16);
    assert!(
        drawn_transcript(&mut chat, 80, 16)
            .iter()
            .any(|line| line.contains("Stopping…"))
    );

    // The disabled pending control cannot submit a second request.
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
}

#[test]
fn read_only_background_rows_have_no_stop_control() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: vec![background_task("codex:1", "codex exec", false)],
        ..mj_client::usage_format::SessionActivity::default()
    });
    chat.open_task_dialog();
    let screen = drawn_transcript(&mut chat, 80, 16).join("\n");
    assert!(screen.contains("codex exec"), "{screen}");
    assert!(!screen.contains("[Stop]"), "{screen}");
    assert_eq!(chat.handle_key(key(KeyCode::Tab)), ChatAction::None);
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
}

#[test]
fn stoppable_background_task_mouse_activation_and_scroll_keep_dialog_state() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: (0..8)
            .map(|index| background_task(&format!("task-{index}"), &format!("work-{index}"), true))
            .collect(),
        ..mj_client::usage_format::SessionActivity::default()
    });
    chat.open_task_dialog();
    drawn_transcript(&mut chat, 40, 8);
    let inner = chat.task_dialog_area.expect("dialog geometry");
    chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: inner.x,
        row: inner.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(chat.task_dialog_scroll > 0);
    chat.handle_key(key(KeyCode::PageUp));
    assert_eq!(chat.task_dialog_scroll, 0);

    // The first row's button is right-aligned in the dialog inner area.
    let x = inner.right().saturating_sub(2);
    let y = inner.y;
    let press = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(chat.handle_mouse(press), ChatAction::None);
    assert_eq!(
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        }),
        ChatAction::StopBackgroundTask {
            id: "task-0".into()
        }
    );
}

#[test]
fn disappeared_background_task_clears_pending_stop_and_failure_reenables_it() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: vec![background_task("task-1", "cargo test", true)],
        ..mj_client::usage_format::SessionActivity::default()
    });
    assert_eq!(
        chat.request_background_stop("task-1".into()),
        ChatAction::StopBackgroundTask {
            id: "task-1".into()
        }
    );
    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::StopBackgroundTask {
            id: "task-1".into(),
            result: Ok(()),
        },
    );
    assert!(chat.background_stop_pending("task-1"));
    chat.set_session_activity(mj_client::usage_format::SessionActivity::default());
    assert!(!chat.background_stop_pending("task-1"));
    let notice_after_disappearance = chat.notice();
    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::StopBackgroundTask {
            id: "task-1".into(),
            result: Err("stale request".into()),
        },
    );
    assert_eq!(chat.notice(), notice_after_disappearance);

    chat.set_session_activity(mj_client::usage_format::SessionActivity {
        pursuing_goal: Default::default(),
        background_commands: vec![background_task("task-1", "cargo test", true)],
        ..mj_client::usage_format::SessionActivity::default()
    });
    assert_eq!(
        chat.request_background_stop("task-1".into()),
        ChatAction::StopBackgroundTask {
            id: "task-1".into()
        }
    );
    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::StopBackgroundTask {
            id: "task-1".into(),
            result: Err("provider unavailable".into()),
        },
    );
    assert!(!chat.background_stop_pending("task-1"));
    assert_eq!(
        chat.notice().as_deref(),
        Some("Background task could not be stopped: provider unavailable")
    );
    chat.open_task_dialog();
    drawn_transcript(&mut chat, 80, 16);
    assert!(
        drawn_transcript(&mut chat, 80, 16)
            .iter()
            .any(|line| line.contains("[Stop]"))
    );
}

#[test]
fn a_saved_draft_reopens_in_the_composer_with_the_cursor_at_its_end() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    chat.restore_draft("half typed thought".into());

    assert_eq!(chat.input, "half typed thought");
    assert_eq!(chat.input_cursor, "half typed thought".len());
}

#[test]
fn an_empty_saved_draft_leaves_the_composer_untouched() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("typed since opening".into());

    chat.restore_draft(String::new());

    assert_eq!(chat.input, "typed since opening");
}

#[test]
fn a_fresh_chat_opens_with_the_session_s_saved_draft_in_the_composer() {
    let chat = freshly_opened_chat("half typed thought");

    assert_eq!(chat.input, "half typed thought");
    assert_eq!(chat.input_cursor, "half typed thought".len());
}

#[test]
fn a_fresh_chat_for_a_session_with_no_saved_draft_opens_empty() {
    assert_eq!(freshly_opened_chat("").input, "");
}

#[test]
fn enter_submits_to_the_worker_while_idle_or_running() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.handle_key(key(KeyCode::Char('h')));
    chat.handle_key(key(KeyCode::Char('i')));
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt("hi".into())
    );

    let mut running = snapshot();
    running.phase = WorkerPhase::Running;
    running.active_prompt = Some(ActivePrompt {
        request_id: "p".into(),
        text: "busy".into(),
        attachments: vec![],
    });
    let mut chat = ChatState::new(&running, &[]);
    chat.handle_key(key(KeyCode::Char('x')));
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt("x".into())
    );
    assert!(chat.queued_prompts.is_empty());
    assert!(chat.entries.is_empty());
}

#[test]
fn bang_prefix_submits_a_bash_command_without_starting_a_prompt() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("!printf '%s' hello | tr a-z A-Z".into());

    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::RunShell("printf '%s' hello | tr a-z A-Z".into())
    );
    assert!(chat.input.is_empty());
    assert_eq!(
        chat.prompt_history.last().map(String::as_str),
        Some("!printf '%s' hello | tr a-z A-Z")
    );
}

#[test]
fn empty_bang_command_stays_in_the_composer_and_shows_usage() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("!   ".into());

    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(chat.input, "!   ");
    assert_eq!(chat.notice().as_deref(), Some("usage: !<bash command>"));
}

#[test]
fn enter_does_not_send_a_prompt_while_the_worker_is_closing_or_closed() {
    for phase in [WorkerPhase::Closing, WorkerPhase::Closed] {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = phase;
        chat.input = "hello".into();
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(
            chat.feedback.current().as_deref(),
            Some("The worker is closing; this prompt was not sent")
        );
        assert_eq!(chat.input, "hello");
    }
}

#[test]
fn bootstrap_uses_snapshot_queue_without_duplicating_replayed_additions() {
    let worker: WorkerSnapshot = serde_json::from_value(serde_json::json!({
        "session_id": "1234567890",
        "phase": "running",
        "latest_seq": 1,
        "last_checkpoint_seq": null,
        "active_prompt": null,
        "config": {},
        "queued_prompts": [{
            "id": "queued-0001",
            "text": "next",
            "attachments": [],
            "created_at_ms": 1
        }],
        "handled_requests": {}
    }))
    .unwrap();
    let events = [SequencedEvent {
        seq: 1,
        recorded_at_ms: Some(1),
        request_id: Some("enqueue-1".into()),
        event: WorkerEvent::QueuedPromptAdded {
            prompt: mj_core::relay::QueuedPrompt {
                id: "queued-0001".into(),
                text: "next".into(),
                attachments: vec![],
                created_at_ms: 1,
            },
        },
    }];

    let chat = ChatState::new(&worker, &events);

    assert_eq!(chat.queued_prompts.len(), 1);
    assert_eq!(chat.queued_prompts[0].id, "queued-0001");
}

#[test]
fn submitting_a_prompt_clears_a_stale_queue_notice() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_notice("Queued 1: next");

    chat.mark_prompt_submitted("hello");

    assert_eq!(chat.phase, WorkerPhase::Running);
    assert!(chat.notice().is_none());
}

#[test]
fn notices_set_replace_if_and_clear() {
    let notices = Notices::default();
    assert_eq!(notices.current(), None);

    notices.set("first notice");
    assert_eq!(notices.current().as_deref(), Some("first notice"));

    assert!(!notices.replace_if("wrong expectation", "replaced"));
    assert_eq!(notices.current().as_deref(), Some("first notice"));

    assert!(notices.replace_if("first notice", "second notice"));
    assert_eq!(notices.current().as_deref(), Some("second notice"));

    notices.clear();
    assert_eq!(notices.current(), None);
}

#[test]
fn notices_keep_a_history_and_count_failures_that_stack() {
    let notices = Notices::default();
    notices.set("Profile quotas refreshed");
    notices.set_failure("Resume failed: archive missing");
    // A second failure before the first was readable: the bar counts them,
    // the log keeps each plain.
    notices.set_failure("Move failed: target unreachable");
    assert_eq!(
        notices.current().as_deref(),
        Some("2 failures · latest: Move failed: target unreachable")
    );
    let history = notices.history();
    assert_eq!(
        history
            .iter()
            .map(|record| (record.text.as_str(), record.failure))
            .collect::<Vec<_>>(),
        [
            ("Move failed: target unreachable", true),
            ("Resume failed: archive missing", true),
            ("Profile quotas refreshed", false),
        ]
    );
    // Clearing resets the count; the next failure stands alone.
    notices.clear();
    notices.set_failure("Stop failed");
    assert_eq!(notices.current().as_deref(), Some("Stop failed"));
    // The same text twice is one history entry.
    notices.clear();
    notices.set("Same");
    notices.set("Same");
    assert_eq!(
        notices
            .history()
            .iter()
            .filter(|record| record.text == "Same")
            .count(),
        1
    );
}

#[test]
fn a_fresh_failure_notice_survives_routine_background_notices() {
    let notices = Notices::default();
    notices.set_failure("Resume failed: archived transcript is invalid");

    notices.set("Profile quotas refreshed");
    assert_eq!(
        notices.current().as_deref(),
        Some("Resume failed: archived transcript is invalid")
    );

    // A newer failure replaces the protected one at once, and the bar says
    // that one was overwritten.
    notices.set_failure("Resume failed: target disconnected");
    assert_eq!(
        notices.current().as_deref(),
        Some("2 failures · latest: Resume failed: target disconnected")
    );

    let after_set = std::time::Instant::now();
    assert!(notices.dismiss(after_set + NOTICE_MINIMUM_DISPLAY));
    notices.set("Profile quotas refreshed");
    assert_eq!(
        notices.current().as_deref(),
        Some("Profile quotas refreshed")
    );
}

#[test]
fn cloned_notices_share_one_slot() {
    let notices = Notices::default();
    let clone = notices.clone();

    notices.set("set through the original");
    assert_eq!(clone.current().as_deref(), Some("set through the original"));

    clone.clear();
    assert_eq!(notices.current(), None);
}

/// Dismissal is what an incidental key press asks for, and a notice that
/// nobody has had time to read must survive it.
#[test]
fn a_notice_is_dismissed_only_once_it_has_been_showing_long_enough() {
    let notices = Notices::default();
    assert!(notices.dismiss(std::time::Instant::now()));

    notices.set("Credential sync failed");
    let after_set = std::time::Instant::now();
    assert!(!notices.dismiss(after_set));
    assert_eq!(notices.current().as_deref(), Some("Credential sync failed"));

    assert!(notices.dismiss(after_set + NOTICE_MINIMUM_DISPLAY));
    assert_eq!(notices.current(), None);
}

/// Draws are gated on a dirty flag that background work never sets, so a
/// renderer tells the bar moved by recording this counter with each frame.
#[test]
fn notice_generation_changes_only_when_displayed_text_changes() {
    let notices = Notices::default();
    let drawn = notices.generation();

    notices.set("Import failed");
    assert_ne!(notices.generation(), drawn);
    let drawn = notices.generation();

    // Repeating the same report updates its age without changing the
    // visible footer.
    notices.set("Import failed");
    assert_eq!(notices.generation(), drawn);

    assert!(notices.replace_if("Import failed", "Import failed: no space left"));
    assert_ne!(notices.generation(), drawn);
    let drawn = notices.generation();

    notices.clear();
    assert_ne!(notices.generation(), drawn);

    // Clearing an empty bar changes nothing on screen.
    let drawn = notices.generation();
    notices.clear();
    assert_eq!(notices.generation(), drawn);
}

fn text_elicitation() -> ElicitationRequest {
    ElicitationRequest {
        id: "ask-1".into(),
        message: "Which branch should I use?".into(),
        title: None,
        description: None,
        fields: vec![mj_core::elicitation::ElicitationField {
            id: "branch".into(),
            title: "Branch".into(),
            description: None,
            required: false,
            secret: false,
            custom_answer_for: None,
            custom_answer_option: None,
            kind: mj_core::elicitation::ElicitationFieldKind::Text {
                default: None,
                min_length: None,
                max_length: None,
                pattern: None,
                format: None,
            },
        }],
    }
}

#[test]
fn restored_question_drafts_keep_distinct_answers_and_reject_changed_requests() {
    let request = text_elicitation();
    let drafts = ["first session", "second session"].map(|answer| {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.restore_elicitation(request.clone());
        chat.elicitation.as_mut().unwrap().paste(answer);
        chat.elicitation_draft().unwrap()
    });
    for (draft, answer) in drafts.into_iter().zip(["first session", "second session"]) {
        let mut chat = ChatState::new(&snapshot(), &[]);
        assert!(!chat.restore_elicitation_draft(draft.clone()));
        let mut changed = request.clone();
        changed.message = "A different question with the same id".into();
        chat.restore_elicitation(changed);
        assert!(!chat.restore_elicitation_draft(draft.clone()));
        chat.sync_elicitation(std::slice::from_ref(&request));
        assert!(chat.restore_elicitation_draft(draft.clone()));
        chat.handle_key(key(KeyCode::Enter));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::RespondElicitation {
                request: request.clone(),
                response: ElicitationResponse::Accept {
                    content: BTreeMap::from([(
                        "branch".into(),
                        mj_core::elicitation::ElicitationValue::String(answer.into())
                    )])
                },
            }
        );
        chat.sync_elicitation(&[]);
        assert!(!chat.restore_elicitation_draft(draft));
    }
}

#[test]
fn restored_question_drafts_cannot_change_the_answer_recipient() {
    let request = text_elicitation();
    let mut reviewer = ChatState::new(&snapshot(), &[]);
    reviewer.show_review_role_elicitation(Some("reviewer-a".into()), request.clone());
    let draft = reviewer.elicitation_draft().unwrap();
    let mut primary = ChatState::new(&snapshot(), &[]);
    primary.restore_elicitation(request.clone());
    assert!(!primary.restore_elicitation_draft(draft.clone()));
    let mut other_reviewer = ChatState::new(&snapshot(), &[]);
    other_reviewer.show_review_role_elicitation(Some("reviewer-b".into()), request);
    assert!(!other_reviewer.restore_elicitation_draft(draft));
}

#[test]
fn reviewer_form_reconciliation_drops_stale_forms_and_resurfaces_primary() {
    let request = mj_core::elicitation::ElicitationRequest {
        id: "reviewer-form-1".into(),
        message: "Allow reading /etc?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    let mut chat = ChatState::new(&snapshot(), &[]);
    assert!(chat.show_review_role_elicitation(Some("reviewer-a".into()), request.clone()));

    chat.reconcile_reviewer_elicitation(&[(Some("reviewer-a".into()), request.clone())]);
    assert!(chat.reviewer_elicitation_open());

    let changed = mj_core::elicitation::ElicitationRequest {
        message: "Allow reading /var?".into(),
        ..request.clone()
    };
    chat.reconcile_reviewer_elicitation(&[(Some("reviewer-a".into()), changed.clone())]);
    assert!(!chat.reviewer_elicitation_open());

    chat.sync_elicitation(std::slice::from_ref(&request));
    chat.elicitation = None;
    assert!(chat.show_review_role_elicitation(Some("reviewer-a".into()), changed));
    chat.reconcile_reviewer_elicitation(&[]);
    assert!(!chat.reviewer_elicitation_open());
    assert!(
        chat.elicitation.is_some(),
        "primary pending form resurfaced"
    );
    assert!(!chat.elicitation_is_reviewers);
}

/// A pending elicitation is durable projection state, rebuilt from the
/// session the next time it is opened, so leaving the view is a different
/// act from answering the agent. The moved-key notices are handled on the
/// same terms: they pass the open form without consuming it.
#[test]
fn control_g_and_control_q_pass_a_chat_whose_elicitation_is_still_open() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let request = text_elicitation();
    chat.restore_elicitation(request.clone());

    assert_eq!(chat.handle_key(ctrl('g')), ChatAction::None);
    assert_eq!(chat.handle_key(ctrl('q')), ChatAction::None);
    assert_eq!(
        chat.materialized_session().pending_elicitations,
        vec![request.clone()]
    );

    // Every other key still belongs to the form, and Escape still answers
    // the agent rather than leaving.
    assert_eq!(chat.handle_key(key(KeyCode::Char('q'))), ChatAction::None);
    assert_eq!(
        chat.handle_key(key(KeyCode::Esc)),
        ChatAction::RespondElicitation {
            request,
            response: ElicitationResponse::Cancel,
        }
    );
    assert!(chat.materialized_session().pending_elicitations.is_empty());
}

#[test]
fn escape_only_cancels_an_active_turn() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(chat.handle_key(control_c), ChatAction::None);
    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);

    // A turn the harness started on its own runs with no prompt of ours in
    // flight. The relay refuses to cancel that, so Esc must not offer to.
    chat.phase = WorkerPhase::Running;
    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);

    chat.set_prompt_in_flight(true);
    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::Cancel);
    assert_eq!(chat.handle_key(control_c), ChatAction::None);
}

#[test]
fn cancellation_waits_for_turn_completion_before_queue_can_drain() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Running;
    chat.queued_prompts.push_back(queued("queued-1", "next"));
    chat.apply_event(&SequencedEvent {
        seq: 1,
        recorded_at_ms: None,
        request_id: Some("cancel".into()),
        event: WorkerEvent::Cancelled,
    });
    assert_eq!(chat.phase, WorkerPhase::Running);

    chat.apply_event(&SequencedEvent {
        seq: 2,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::TurnCompleted,
    });
    assert_eq!(chat.phase, WorkerPhase::Idle);
    assert_eq!(chat.queued_prompts.front().unwrap().text, "next");
}

#[test]
fn alt_up_recovers_the_latest_queued_prompt_for_editing() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.queued_prompts.push_back(queued("queued-1", "first"));
    chat.queued_prompts.push_back(queued("queued-2", "second"));

    assert_eq!(
        chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
        ChatAction::RemoveQueuedPrompt {
            id: "queued-2".into(),
            text: "second".into(),
            kind: QueuedCommandKind::Prompt,
        }
    );

    assert_eq!(chat.input, "second");
    assert_eq!(chat.queued_prompts.len(), 1);
    assert_eq!(chat.queued_prompts[0].text, "first");
}

#[test]
fn up_and_control_p_peel_queued_prompts_back_into_the_editor() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for (id, text) in [
        ("queued-1", "first"),
        ("queued-2", "second"),
        ("queued-3", "third"),
    ] {
        chat.queued_prompts.push_back(queued(id, text));
    }

    chat.handle_key(key(KeyCode::Up));
    assert_eq!(chat.input, "third");
    assert_eq!(chat.queued_prompts.len(), 2);

    chat.clear_input();
    chat.handle_key(ctrl('p'));
    assert_eq!(chat.input, "second");
    assert_eq!(chat.queued_prompts.len(), 1);

    chat.clear_input();
    chat.handle_key(key(KeyCode::Up));
    assert_eq!(chat.input, "first");
    assert!(chat.queued_prompts.is_empty());

    chat.clear_input();
    chat.handle_key(key(KeyCode::Up));
    assert!(chat.input.is_empty());
}

#[test]
fn model_and_effort_slash_commands_change_live_session_config() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_config_options(&[
        select_config_option("model", "gpt-5.6", &["gpt-5.6", "gpt-5.6-luna"]),
        select_config_option("effort", "high", &["high", "xhigh"]),
    ]);
    chat.input = "/model gpt-5.6-luna".into();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::SetConfig {
            key: "model".into(),
            value: "gpt-5.6-luna".into(),
        }
    );

    chat.input = "/effort xhigh".into();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::SetConfig {
            key: "effort".into(),
            value: "xhigh".into(),
        }
    );
}

/// A harness that advertises no selector cannot apply the change at all, so
/// the refusal belongs in the footer now rather than in a transcript line that
/// arrives seconds after "Configuration update accepted". The words are the
/// runtime's own, and the article follows the key's name. The composer clears
/// the same as it would for a command that was actually sent, so the next
/// command typed does not append to the refused one.
#[test]
fn a_selector_the_harness_does_not_expose_is_refused_before_anything_is_sent() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    // A live session whose harness advertises effort but no model at all.
    chat.set_config_options(&[select_config_option("effort", "high", &["high", "low"])]);

    chat.input = "/model o3-mini".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(
        chat.feedback.current().as_deref(),
        Some("ACP bridge does not expose a model selector")
    );
    // Nothing was sent, but the command was still handled: the composer
    // clears exactly as it would for an accepted command, so the next
    // command typed does not append to the refused one.
    assert_eq!(chat.input, "");

    chat.set_config_options(&[]);
    chat.input = "/effort xhigh".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(
        chat.feedback.current().as_deref(),
        Some("ACP bridge does not expose an effort selector")
    );
    assert_eq!(chat.input, "");
}

#[test]
fn fast_toggles_the_advertised_codex_configuration_without_arguments() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_config_options(&[fast_mode_option("off")]);
    chat.input = "/fast".into();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::SetConfig {
            key: "fast-mode".into(),
            value: "on".into(),
        }
    );

    chat.set_config_options(&[fast_mode_option("on")]);
    chat.input = "/fast".into();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::SetConfig {
            key: "fast-mode".into(),
            value: "off".into(),
        }
    );

    chat.input = "/fast on".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(chat.notice().as_deref(), Some("usage: /fast"));
}

#[test]
fn fast_stays_local_when_the_active_model_does_not_support_it() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.input = "/fast".into();

    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(chat.input, "/fast");
    assert_eq!(
        chat.notice().as_deref(),
        Some("Fast mode is unavailable for the active Codex model")
    );
}

#[test]
fn config_commands_are_queued_while_the_agent_is_busy() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Running;

    chat.input = "/model".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(
        chat.feedback.current().as_deref(),
        Some("The agent does not advertise model values; usage: /model <value>")
    );

    chat.set_config_options(&[select_config_option("model", "opus", &["opus", "sonnet"])]);
    chat.input = "/model sonnet".into();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        }
    );
    assert!(chat.input.is_empty());

    chat.phase = WorkerPhase::Closing;
    chat.input = "/model sonnet".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(
        chat.feedback.current().as_deref(),
        Some("The worker is closing; this configuration change was not sent")
    );
}

#[test]
fn a_queued_config_change_peels_back_into_the_composer() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut session = MaterializedSession::empty("1234567890");
    // The projection only rebuilds when its frontier moved.
    session.applied_event_ordinal = 5;
    session.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-config".into(),
        kind: QueuedCommandKind::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        },
        content: vec![serde_json::json!({"type": "text", "text": "/model sonnet"})],
        queued_at_ms: 10,
    });
    chat.apply_materialized(&session, &[], &[]);
    assert_eq!(chat.queued_prompts.len(), 1);
    assert_eq!(chat.queued_prompts[0].queue_label(), "queued config");

    assert_eq!(
        chat.handle_key(ctrl('p')),
        ChatAction::RemoveQueuedPrompt {
            id: "queued-config".into(),
            text: "/model sonnet".into(),
            kind: QueuedCommandKind::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            },
        }
    );
    assert_eq!(chat.input, "/model sonnet");
    assert!(chat.queued_prompts.is_empty());

    // Resubmitting the peeled-back text parses as the same change.
    chat.phase = WorkerPhase::Running;
    chat.set_config_options(&[select_config_option("model", "opus", &["opus", "sonnet"])]);
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        }
    );
}

#[test]
fn editing_queued_images_preserves_payload_and_recovers_failed_removal() {
    let mut source = ChatState::new(&snapshot(), &[]);
    source.set_input("compare ".into());
    source.handle_clipboard_content(ClipboardContent::Image(test_image()));
    source.handle_paste(" with ");
    source.handle_clipboard_content(ClipboardContent::Image(test_image()));
    let payload = source.draft_payload();
    let mut session = MaterializedSession::empty("image-queue");
    session.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-image".into(),
        kind: QueuedCommandKind::Prompt,
        content: prompt_content_blocks(&payload.text, &payload.images),
        queued_at_ms: 10,
    });
    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    assert!(matches!(
        chat.handle_key(key(KeyCode::Up)),
        ChatAction::RemoveQueuedPrompt { .. }
    ));
    assert_eq!(chat.draft_payload(), payload);
    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::RemoveQueuedPrompt {
            id: "queued-image".into(),
            text: payload.text.clone(),
            kind: QueuedCommandKind::Prompt,
            result: Err("offline".into()),
        },
    );
    assert_eq!(chat.queued_prompts.back().unwrap().images, payload.images);
    chat.handle_key(key(KeyCode::Backspace));
    assert_eq!(chat.input_images.len(), 1);
    assert_eq!(chat.input, "compare [image 1] with ");
}

#[test]
fn stale_projection_does_not_restore_a_queue_entry_being_edited() {
    let mut session = MaterializedSession::empty("session-queue-edit");
    session.applied_event_ordinal = 5;
    session.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-prompt".into(),
        kind: QueuedCommandKind::Prompt,
        content: vec![serde_json::json!({"type": "text", "text": "revise me"})],
        queued_at_ms: 10,
    });
    let mut chat = ChatState::from_materialized(&session, &[], &[]);

    assert_eq!(
        chat.handle_key(key(KeyCode::Up)),
        ChatAction::RemoveQueuedPrompt {
            id: "queued-prompt".into(),
            text: "revise me".into(),
            kind: QueuedCommandKind::Prompt,
        }
    );
    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::RemoveQueuedPrompt {
            id: "queued-prompt".into(),
            text: "revise me".into(),
            kind: QueuedCommandKind::Prompt,
            result: Ok(()),
        },
    );

    // The relay accepted the removal, but its previously published view
    // can still arrive before the projection containing that command.
    chat.apply_materialized(&session, &[], &[]);
    assert_eq!(chat.input, "revise me");
    assert!(chat.queued_prompts.is_empty());
    assert!(chat.pending_queue_removals.contains("queued-prompt"));

    session.applied_event_ordinal = 6;
    session.queued_prompts.clear();
    chat.apply_materialized(&session, &[], &[]);
    assert!(chat.pending_queue_removals.is_empty());
}

#[test]
fn failed_queue_removal_restores_the_peeled_entry() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.queued_prompts
        .push_back(queued("queued-prompt", "revise me"));
    chat.handle_key(ctrl('p'));

    remote::apply_chat_remote_result(
        &mut chat,
        remote::ChatRemoteResult::RemoveQueuedPrompt {
            id: "queued-prompt".into(),
            text: "revise me".into(),
            kind: QueuedCommandKind::Prompt,
            result: Err("relay rejected removal".into()),
        },
    );

    assert!(chat.pending_queue_removals.is_empty());
    assert_eq!(chat.queued_prompts.len(), 1);
    assert_eq!(chat.queued_prompts[0].id, "queued-prompt");
}

#[test]
fn plan_toggles_the_session_mode_for_a_harness_without_a_plan_command() {
    let mut chat = grok_chat();
    chat.set_input("/plan".into());

    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/plan".into(),
            control: PlanControl::SetSessionMode {
                mode_id: "plan".into()
            },
            requested_active: true,
            prompt: None,
        }
    );
    assert!(chat.input.is_empty());
    assert!(chat.plan_command_pending);

    chat.plan_command_pending = false;
    chat.set_input("/plan".into());
    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/plan".into(),
            control: PlanControl::SetSessionMode {
                mode_id: "default".into()
            },
            requested_active: false,
            prompt: None,
        }
    );
    assert!(chat.plan_command_pending);
}

#[test]
fn plan_accepts_explicit_on_and_off_arguments() {
    let mut chat = grok_chat();
    chat.set_input("/plan off".into());
    assert_eq!(chat.submit_input(), ChatAction::None);

    chat.set_input("/plan ON".into());
    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/plan ON".into(),
            control: PlanControl::SetSessionMode {
                mode_id: "plan".into()
            },
            requested_active: true,
            prompt: None,
        }
    );

    chat.plan_command_pending = false;
    chat.set_input("/plan sideways".into());
    assert_eq!(chat.submit_input(), ChatAction::Prompt("sideways".into()));
}

#[test]
fn plan_uses_an_advertised_mode_config_option() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_config_options(&[mode_config_option("default", &["default", "plan"])]);
    chat.set_input("/plan".into());

    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/plan".into(),
            control: PlanControl::SetConfig {
                key: "mode".into(),
                value: "plan".into()
            },
            requested_active: true,
            prompt: None,
        }
    );
}

#[test]
fn grok_uses_its_trusted_set_mode_fallback_even_with_an_unrelated_mode_config() {
    let mut chat = grok_chat();
    chat.set_config_options(&[mode_config_option("default", &["default", "act"])]);
    chat.set_input("/plan".into());

    assert!(matches!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            control: PlanControl::SetSessionMode { .. },
            ..
        }
    ));
}

#[test]
fn an_unchanged_mode_catalogue_does_not_undo_an_optimistic_toggle() {
    let options = [mode_config_option("default", &["default", "plan"])];
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_config_options(&options);
    chat.set_input("/plan".into());
    assert!(matches!(
        chat.submit_input(),
        ChatAction::PlanCommand { .. }
    ));

    chat.set_config_options(&options);

    assert!(chat.plan_mode_active());
}

#[test]
fn muse_plan_forwards_the_advertised_skill_without_changing_approvals() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_harness_kind(HarnessKind::Muse);
    advertise(&mut chat, 1, &["plan"]);
    chat.set_input("/plan the migration".into());
    assert_eq!(
        chat.submit_input(),
        ChatAction::Prompt("/plan the migration".into())
    );
    assert!(!chat.plan_command_pending);
}

#[test]
fn an_agent_plan_command_does_not_override_hels_unified_command() {
    let mut chat = grok_chat();
    advertise(&mut chat, 1, &["plan"]);
    chat.set_input("/plan the migration".into());

    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/plan the migration".into(),
            control: PlanControl::SetSessionMode {
                mode_id: "plan".into()
            },
            requested_active: true,
            prompt: Some("the migration".into()),
        }
    );
}

#[test]
fn plan_is_kept_local_without_a_compatible_mode_surface() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("/plan".into());

    assert_eq!(chat.submit_input(), ChatAction::None);
    assert_eq!(chat.input, "/plan");
    assert!(chat.feedback.current().unwrap().contains("does not expose"));
}

#[test]
fn codex_plan_uses_collaboration_mode_not_the_permission_mode() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_harness_kind(HarnessKind::Codex);
    chat.set_config_options(&[
        select_config_option("mode", "read-only", &["read-only", "full-access"]),
        select_config_option("collaboration_mode", "default", &["default", "plan"]),
    ]);
    chat.set_input("/plan inspect the migration".into());

    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/plan inspect the migration".into(),
            control: PlanControl::SetConfig {
                key: "collaboration_mode".into(),
                value: "plan".into(),
            },
            requested_active: true,
            prompt: Some("inspect the migration".into()),
        }
    );
    assert_eq!(
        chat.prompt_history.last().map(String::as_str),
        Some("/plan inspect the migration")
    );
}

#[test]
fn claude_and_kimi_prefer_the_exact_mode_config() {
    for harness in [HarnessKind::Claude, HarnessKind::Kimi] {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_harness_kind(harness);
        chat.set_config_options(&[select_config_option(
            "mode",
            "default",
            &["default", "plan"],
        )]);
        chat.set_input("/plan".into());
        assert!(matches!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                control: PlanControl::SetConfig { ref key, .. },
                ..
            } if key == "mode"
        ));
    }
}

#[test]
fn grok_uses_set_mode_without_advertising_modes() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_harness_kind(HarnessKind::Grok);
    chat.set_input("/plan".into());
    assert!(matches!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            control: PlanControl::SetSessionMode { ref mode_id },
            ..
        } if mode_id == "plan"
    ));
}

#[test]
fn a_harness_without_plan_mode_rejects_plan_and_implement_locally() {
    let mut chat = grok_chat();
    chat.set_harness_kind(HarnessKind::Muse);
    for command in ["/plan design it", "/implement"] {
        chat.set_input(command.into());
        assert_eq!(chat.submit_input(), ChatAction::None);
        assert_eq!(chat.input, command);
        assert!(
            chat.feedback
                .current()
                .unwrap()
                .contains("does not expose compatible plan/default modes")
        );
    }
}

#[test]
fn implement_exits_plan_mode_before_submitting_the_instruction() {
    let mut chat = grok_chat();
    chat.finish_plan_mode_change(true);
    chat.set_input("/implement start with the parser".into());
    assert_eq!(
        chat.submit_input(),
        ChatAction::PlanCommand {
            original: "/implement start with the parser".into(),
            control: PlanControl::SetSessionMode {
                mode_id: "default".into()
            },
            requested_active: false,
            prompt: Some("start with the parser".into()),
        }
    );
}

#[test]
fn plan_review_choices_have_distinct_followup_directions() {
    let mut chat = grok_chat();
    chat.finish_plan_mode_change(true);
    let standard = ElicitationRequest {
        id: "plan-review-1".into(),
        message: "review".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    let response = |action: &str, feedback: Option<&str>| {
        let mut content = BTreeMap::new();
        content.insert("action".into(), ElicitationValue::String(action.into()));
        if let Some(feedback) = feedback {
            content.insert("feedback".into(), ElicitationValue::String(feedback.into()));
        }
        ElicitationResponse::Accept { content }
    };

    assert_eq!(
        chat.plan_review_followup(&standard, &response("implement", None)),
        Some(PlanReviewFollowup {
            desired_active: false,
            control: None,
            prompt: None,
        })
    );
    assert_eq!(
        chat.plan_review_followup(&standard, &response("revise", Some("add tests"))),
        Some(PlanReviewFollowup {
            desired_active: true,
            control: None,
            prompt: Some("add tests".into()),
        })
    );
    assert!(matches!(
        chat.plan_review_followup(&standard, &response("exit", None)),
        Some(PlanReviewFollowup {
            desired_active: false,
            control: Some(PlanControl::SetSessionMode { .. }),
            prompt: None,
        })
    ));
}

#[test]
fn plan_waits_for_an_idle_agent() {
    let mut chat = grok_chat();
    chat.phase = WorkerPhase::Running;
    chat.set_input("/plan".into());

    assert_eq!(chat.submit_input(), ChatAction::None);
    assert!(chat.feedback.current().unwrap().contains("only available"));
}

#[test]
fn a_current_mode_update_corrects_the_locally_tracked_plan_mode() {
    let mut chat = grok_chat();
    chat.set_input("/plan".into());
    chat.submit_input();
    assert!(chat.plan_mode_active());

    let mut session = MaterializedSession::empty("1234567890");
    session
        .configuration
        .insert("mode".into(), serde_json::Value::String("default".into()));
    chat.apply_materialized(&session, &[], &[]);

    assert!(!chat.plan_mode_active());
}

#[test]
fn config_slash_command_without_value_shows_usage() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.input = "/model".into();

    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert_eq!(
        chat.notice().as_deref(),
        Some("The agent does not advertise model values; usage: /model <value>")
    );
}

#[test]
fn editor_preserves_uppercase_text_while_shortcuts_remain_case_insensitive() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    chat.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
    // Some terminals report the uppercase character without a Shift modifier.
    chat.handle_key(key(KeyCode::Char('I')));
    assert_eq!(chat.input, "HI");

    chat.handle_key(ctrl('r'));
    chat.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
    assert_eq!(chat.history_search.as_ref().unwrap().query, "N");
    chat.handle_key(key(KeyCode::Esc));

    // A shifted Alt chord still reaches the readline shortcut it names, so
    // Alt-Shift-B moves back a word rather than typing one.
    assert_eq!(chat.input_cursor, 2);
    chat.handle_key(KeyEvent::new(
        KeyCode::Char('B'),
        KeyModifiers::ALT | KeyModifiers::SHIFT,
    ));
    assert_eq!(chat.input_cursor, 0);
    assert_eq!(chat.input, "HI");
}

#[test]
fn empty_terminal_paste_requests_clipboard_and_accepts_an_image() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("describe ".into());
    assert_eq!(
        chat.handle_terminal_paste(""),
        ChatAction::PasteFromClipboard
    );
    assert_eq!(chat.input, "describe ");

    let image = test_image();
    chat.handle_clipboard_content(ClipboardContent::Image(image.clone()));
    assert_eq!(chat.input, "describe [image 1]");
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt("describe [image 1]".into())
    );
    assert_eq!(chat.take_submitting_images()[0].image, image);
}

#[test]
fn terminal_text_paste_does_not_request_the_clipboard() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for text in ["hello", " \t\n", "world"] {
        assert_eq!(chat.handle_terminal_paste(text), ChatAction::None);
    }
    assert_eq!(chat.input, "hello \t\nworld");
    chat.handle_clipboard_content(ClipboardContent::Text(String::new()));
    assert_eq!(chat.input, "hello \t\nworld");
}

#[test]
fn empty_terminal_paste_respects_the_config_picker() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_config_options(&[select_config_option("model", "small", &["small", "large"])]);
    assert!(chat.open_config_picker("model"));
    assert_eq!(chat.handle_terminal_paste(""), ChatAction::None);
    assert!(chat.input.is_empty());
    assert!(chat.config_picker_active());
}

#[test]
fn ctrl_v_returns_paste_request_action() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    assert_eq!(chat.handle_key(ctrl('v')), ChatAction::PasteFromClipboard);
    assert_eq!(
        chat.handle_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )),
        ChatAction::PasteFromClipboard
    );
    assert!(chat.input.is_empty());
}

#[test]
fn toggle_render_mode_flips_between_rich_and_raw() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    assert_eq!(chat.render_mode, TranscriptRenderMode::Rich);
    chat.toggle_render_mode();
    assert_eq!(chat.render_mode, TranscriptRenderMode::Raw);
    chat.toggle_render_mode();
    assert_eq!(chat.render_mode, TranscriptRenderMode::Rich);

    // The keys that used to run these toggles belong to the host's registry
    // now, so the composer answers none of them.
    for key in [alt('t'), ctrl('t'), alt('v')] {
        chat.handle_key(key);
    }
    assert_eq!(chat.render_mode, TranscriptRenderMode::Rich);
    assert!(chat.input.is_empty());
}

#[test]
fn replay_projects_user_and_agent_text() {
    let runtime = RuntimeEvent::SessionUpdate {
        update: serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "done"}
        }),
    };
    let events = vec![
        SequencedEvent {
            seq: 1,
            recorded_at_ms: None,
            request_id: Some("p".into()),
            event: WorkerEvent::PromptAccepted {
                request_id: "p".into(),
                text: "work".into(),
                attachments: vec![],
            },
        },
        SequencedEvent {
            seq: 2,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::to_value(runtime).unwrap(),
            },
        },
    ];
    let mut initial = snapshot();
    initial.latest_seq = 2;
    let chat = ChatState::new(&initial, &events);
    assert_eq!(chat.entries.len(), 2);
    assert_eq!(chat.entries[0].role, ChatRole::User);
    assert_eq!(chat.entries[1].text, "done");
}

#[test]
fn hydrated_tail_continues_the_last_streamed_message() {
    let first = RuntimeEvent::SessionUpdate {
        update: serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "answer",
            "content": {"type": "text", "text": "hello"}
        }),
    };
    let event = SequencedEvent {
        seq: 1,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::Adapter {
            kind: "session_update".into(),
            payload: serde_json::to_value(first).unwrap(),
        },
    };
    let mut initial = snapshot();
    initial.latest_seq = 1;
    let full = ChatState::new(&initial, &[event]);
    let entries = full.bounded_entries(10, 512 * 1024);
    let mut tail =
        ChatState::from_tail(initial.session_id.clone(), WorkerPhase::Running, 1, entries);
    let second = RuntimeEvent::SessionUpdate {
        update: serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "answer",
            "content": {"type": "text", "text": " world"}
        }),
    };
    tail.apply_events(&[SequencedEvent {
        seq: 2,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::Adapter {
            kind: "session_update".into(),
            payload: serde_json::to_value(second).unwrap(),
        },
    }]);

    assert_eq!(tail.entries.len(), 1);
    assert_eq!(tail.entries[0].text, "hello world");
    let materialized = tail.materialized_session();
    assert_eq!(materialized.transcript[0].position, 1);
    assert_eq!(
        materialized.transcript[0].latest_content_event_ordinal,
        Some(2)
    );
    assert_eq!(materialized.unread_agent_messages_after(1), 1);
}

#[test]
fn streamed_message_chunks_coalesce_into_one_entry() {
    let mut initial = snapshot();
    initial.latest_seq = 0;
    let mut chat = ChatState::new(&initial, &[]);
    for (seq, text) in [(1, "gpt"), (2, "-5.6"), (3, "-terra")] {
        chat.apply_session_update(
            seq,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text}
            }),
        );
    }
    chat.apply_session_update(
        4,
        &serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "hmm"}
        }),
    );
    assert_eq!(chat.entries.len(), 2);
    assert_eq!(chat.entries[0].role, ChatRole::Agent);
    assert_eq!(chat.entries[0].text, "gpt-5.6-terra");
    assert_eq!(chat.entries[1].role, ChatRole::Thought);
}

#[test]
fn tool_calls_render_title_and_updates_stay_quiet() {
    let mut initial = snapshot();
    initial.latest_seq = 0;
    let mut chat = ChatState::new(&initial, &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({"sessionUpdate": "tool_call",
            "toolCallId": "grep-config",
            "title": "grep config", "status": "pending"}),
    );
    chat.apply_session_update(
        2,
        &serde_json::json!({"sessionUpdate": "tool_call_update",
            "toolCallId": "grep-config", "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "noise"}}]}),
    );
    assert_eq!(chat.entries.len(), 1);
    assert_eq!(chat.entries[0].role, ChatRole::Tool);
    assert_eq!(chat.entries[0].text, "grep config");
}

#[test]
fn partial_tool_updates_preserve_unchanged_structured_fields() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "inspect",
            "title": "inspect",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": "first result"}
            }],
            "locations": [{"path": "src/lib.rs", "line": 7}]
        }),
    );
    chat.apply_session_update(
        2,
        &serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "inspect",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": "replacement result"}
            }]
        }),
    );

    assert_eq!(chat.entries[0].tool_content, ["replacement result"]);
    assert_eq!(chat.entries[0].tool_locations, ["src/lib.rs:7"]);
}

#[test]
fn unknown_json_does_not_leak_nested_text_into_the_transcript() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_session_update(
        1,
        &serde_json::json!({"items": [{"text": "not an ACP message"}]}),
    );
    assert!(chat.entries.is_empty());
}

#[test]
fn message_ids_keep_adjacent_agent_messages_separate() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for (seq, id, text) in [(1, "one", "first"), (2, "two", "second")] {
        chat.apply_session_update(
            seq,
            &serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": id,
                "content": {"type": "text", "text": text}
            }),
        );
    }
    assert_eq!(chat.entries.len(), 2);
    assert_eq!(chat.entries[0].text, "first");
    assert_eq!(chat.entries[1].text, "second");
}

#[test]
fn plan_updates_replace_the_current_turn_plan() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    for (seq, status) in [(1, "pending"), (2, "completed")] {
        chat.apply_session_update(
            seq,
            &serde_json::json!({
                "sessionUpdate": "plan",
                "entries": [{
                    "content": "inspect renderer",
                    "priority": "high",
                    "status": status
                }]
            }),
        );
    }
    assert_eq!(chat.entries.len(), 1);
    assert_eq!(chat.entries[0].role, ChatRole::Plan);
    assert_eq!(chat.entries[0].plan[0].status, PlanStatus::Completed);
}

#[test]
fn same_ordinal_materialized_update_keeps_transcript_cache_but_refreshes_queue() {
    let mut session = MaterializedSession::empty("session-same-ordinal");
    session.applied_event_ordinal = 1;
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "user:1".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 10,
        last_changed_at_ms: 10,
        body: TranscriptBody::User {
            content: vec![serde_json::json!("first")],
        },
    }));

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    assert_eq!(chat.entries[0].text, "first");

    Arc::make_mut(&mut session.transcript[0]).body = TranscriptBody::User {
        content: vec![serde_json::json!("changed without new ordinal")],
    };
    session.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued".into(),
        kind: QueuedCommandKind::Prompt,
        content: vec![serde_json::json!("queued prompt")],
        queued_at_ms: 20,
    });
    chat.apply_materialized(&session, &[], &[]);

    assert_eq!(chat.entries[0].text, "first");
    assert_eq!(chat.queued_prompts.len(), 1);
    assert_eq!(chat.queued_prompts[0].text, "queued prompt");
}

#[test]
fn materialized_diff_counts_arrive_after_the_path_and_ignore_stale_revisions() {
    let mut session = MaterializedSession::empty("session-diffstats");
    session.applied_event_ordinal = 1;
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "tool:edit".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 10,
        last_changed_at_ms: 10,
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
    }));

    let mut chat = ChatState::from_materialized(&session, &[], &[]);
    assert_eq!(chat.entries[0].tool_diffstats, ["/workspace/src/lib.rs"]);
    let request = chat.take_diffstat_requests(1).pop().unwrap();
    let exact = request.clone().compute();
    chat.apply_diffstats("tool:edit", 9, exact.clone());
    assert_eq!(chat.entries[0].tool_diffstats, ["/workspace/src/lib.rs"]);
    chat.apply_diffstats("tool:edit", 10, exact);
    assert_eq!(
        chat.entries[0].tool_diffstats,
        ["/workspace/src/lib.rs  +1 −0"]
    );
}

/// A prompt the harness ended without answering keeps its text where
/// Ctrl-Alt-R can put it back, and says so in the transcript (#970).
#[test]
fn an_unanswered_prompt_is_marked_and_stays_restorable() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut session = MaterializedSession::empty("1234567890");
    session.applied_event_ordinal = 9;
    session.transcript.push(
        TranscriptItem {
            stable_id: "user:1".into(),
            position: 4,
            latest_content_event_ordinal: None,
            created_at_ms: 10,
            last_changed_at_ms: 10,
            body: TranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": "rename the module"})],
            },
        }
        .into(),
    );
    session.last_turn_outcome = Some(unanswered_outcome(
        mj_core::acp::PROMPT_UNANSWERED_STOP_REASON,
    ));
    chat.apply_materialized(&session, &[], &[]);
    // The same projection arriving again must not stack a second record.
    chat.apply_materialized(&session, &[], &[]);
    assert_eq!(chat.unsent_prompts.len(), 1);
    assert_eq!(chat.unsent_prompts[0].kind, UnsentKind::Unanswered);
    assert_eq!(
        chat.unsent_prompts[0].kind.headline(),
        "Prompt was not answered"
    );

    chat.restore_latest_unsent_prompt();
    assert_eq!(
        chat.draft_payload(),
        PromptPayload::text("rename the module")
    );
}

/// A turn that ended normally leaves nothing to restore.
#[test]
fn a_finished_turn_is_not_offered_for_restore() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let mut session = MaterializedSession::empty("1234567890");
    session.applied_event_ordinal = 9;
    session.last_turn_outcome = Some(unanswered_outcome("EndTurn"));
    chat.apply_materialized(&session, &[], &[]);
    assert!(chat.unsent_prompts.is_empty());
}

fn unanswered_outcome(stop_reason: &str) -> mj_core::state::MaterializedTurnOutcome {
    mj_core::state::MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: "prompt-1".into(),
        accepted_ordinal: Some(3),
        turn_start_position: Some(4),
        completed_ordinal: 8,
        completed_at_ms: 20,
        outcome: TurnOutcomeKind::Completed {
            stop_reason: stop_reason.into(),
        },
    }
}
