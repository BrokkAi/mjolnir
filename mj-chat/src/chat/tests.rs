use super::*;
use crate::chat::test_support::{
    advertise, alt, ctrl, drawn_transcript, fast_mode_option, grok_chat, key, mode_config_option,
    queued, select_config_option, snapshot,
};
use base64::Engine;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use mj_client::review::RuntimeReviewView;
use mj_core::review::driver::TurnReviewPhase;

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
        questions: Vec::new(),
        phase: TurnReviewPhase::CapturingDelta,
        roles: Vec::new(),
        status: "capturing the turn".into(),
        verdict: None,
    }));
    assert!(chat.needs_animation());
}

#[test]
fn review_status_omits_the_deprecated_tier() {
    let review = mj_core::config::ReviewConfig {
        enabled: true,
        tier: Some("extended".into()),
        profile: Some("reviewer".into()),
        ..Default::default()
    };
    assert_eq!(
        review_status_line(&review, true),
        "Reviewing every completed turn with [review] profile \"reviewer\". A review is open now."
    );
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

pub(super) fn test_image() -> ClipboardImage {
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
    chat.set_prompt_images_supported(true);
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
    reopened.set_prompt_images_supported(true);
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
    chat.set_prompt_images_supported(true);
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
fn a_session_created_with_subagents_shows_a_dimmed_entry_before_the_first_child() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(!screen.contains("Subagents"), "{screen}");

    chat.set_subagents_enabled(true);
    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(screen.contains("Subagents · 0 "), "{screen}");
    // Launch finding R1-3: the ASCII symbol set draws no middle dot.
    let ascii = crate::theme::with_symbols(crate::theme::SymbolSet::Ascii, || {
        drawn_transcript(&mut chat, 100, 24).join("\n")
    });
    assert!(ascii.contains("Subagents - 0 "), "{ascii}");
    assert!(!ascii.contains('·'), "{ascii}");
    assert!(
        chat.subagent_control_area.is_none(),
        "the dimmed entry is not clickable"
    );

    chat.set_subagent_count(1);
    let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
    assert!(screen.contains("Subagents · 0/1"), "{screen}");
    assert!(chat.subagent_control_area.is_some());
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
            .any(|line| line.contains("Interrupting…"))
    );

    // The disabled pending control cannot submit a second request.
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
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

// Hard-won: 30de9e86: Typed composer text was lost when leaving and reopening a conversation.
#[test]
fn a_saved_draft_reopens_in_the_composer_with_the_cursor_at_its_end() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    chat.restore_draft("half typed thought".into());

    assert_eq!(chat.input, "half typed thought");
    assert_eq!(chat.input_cursor, "half typed thought".len());
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
// Hard-won: 7c56a0f4: Incidental keypresses erased notices before the user could see a frame.
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
// Hard-won: 3fbdd2ae: An open elicitation trapped users because detach keys were consumed.
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

    // A phase alone does not identify a running turn.
    chat.phase = WorkerPhase::Running;
    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);

    // Esc interrupts a Codex goal turn as well as a prompted turn.
    chat.session_activity.harness_turn_started_at_ms = Some(1_000);
    chat.session_activity.pursuing_goal = true;
    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::Cancel);

    // A turn Claude Code started on its own after a background task can be
    // stopped.
    chat.session_activity.pursuing_goal = false;
    assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::Cancel);
    chat.session_activity.harness_turn_started_at_ms = None;

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

/// A harness that advertises no selector cannot apply the change at all, so
/// the refusal belongs in the footer now rather than in a transcript line that
/// arrives seconds after "Configuration update accepted". The words are the
/// runtime's own, and the article follows the key's name. The composer clears
/// the same as it would for a command that was actually sent, so the next
/// command typed does not append to the refused one.
// Hard-won: 27140151: An unsupported selector was reported accepted before its late refusal arrived.
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
fn editing_queued_images_preserves_payload_and_recovers_failed_removal() {
    let mut source = ChatState::new(&snapshot(), &[]);
    source.set_prompt_images_supported(true);
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
fn a_current_mode_update_corrects_the_locally_tracked_plan_mode() {
    let mut chat = grok_chat();
    chat.set_input("/plan".into());
    chat.submit_input();
    assert!(chat.plan_mode_active());

    let mut session = MaterializedSession::empty("1234567890");
    session
        .configuration
        .values
        .insert("mode".into(), serde_json::Value::String("default".into()));
    chat.apply_materialized(&session, &[], &[]);

    assert!(!chat.plan_mode_active());
}

// Hard-won: 6a66a7d0: Refused slash commands remained in the draft and contaminated the next command.
#[test]
fn review_tier_slash_args_are_not_config_gestures() {
    for tier in ["quick", "extended"] {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input(format!("/review {tier}"));
        assert_eq!(
            chat.handle_key(key(crossterm::event::KeyCode::Enter)),
            ChatAction::None
        );
        assert_eq!(chat.notice().as_deref(), Some("usage: /review [status]"));
        assert!(chat.input.is_empty());
    }
}

#[test]
fn a_refused_slash_command_clears_the_draft_so_the_next_command_stands_alone() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.input = "/model".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert!(chat.input.is_empty(), "draft kept: {:?}", chat.input);
    assert!(chat.notice().unwrap().contains("/model"));
}

// Hard-won: 6a66a7d0: Unknown slash commands were sent to the agent as ordinary prompts.
#[test]
fn an_unknown_slash_command_is_not_sent_to_the_agent() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.input = "/bogus thing".into();
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert!(chat.input.is_empty());
    assert!(chat.notice().unwrap().contains("/bogus"));

    // A path is a prompt, not a command.
    chat.input = "/tmp/log is empty".into();
    assert!(matches!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt(_)
    ));
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

// Hard-won: ac740841: Synchronous clipboard access could stall the chat event loop.
#[test]
fn ctrl_v_returns_paste_request_action() {
    let mut chat = ChatState::new(&snapshot(), &[]);

    assert_eq!(chat.handle_key(ctrl('v')), ChatAction::PasteFromClipboard);
    assert_eq!(
        chat.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)),
        ChatAction::PasteFromClipboard
    );
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

// Hard-won: f378854d: Every streamed token created a separate transcript entry.
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

fn unanswered_session(
    command_id: &str,
    position: u64,
    text: &str,
    stop: &str,
) -> MaterializedSession {
    let mut session = MaterializedSession::empty("1234567890");
    session.applied_event_ordinal = 9;
    session.transcript.push(
        TranscriptItem {
            stable_id: format!("user:{position}"),
            position,
            latest_content_event_ordinal: None,
            created_at_ms: 10,
            last_changed_at_ms: 10,
            body: TranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": text})],
            },
        }
        .into(),
    );
    let mut outcome = unanswered_outcome(stop);
    outcome.command_id = command_id.into();
    outcome.turn_start_position = Some(position);
    session.last_turn_outcome = Some(outcome);
    session
}

/// The notice helps once: later unanswered prompts in the same run add no
/// row, an answered prompt starts a new run, and a reattach (which re-reads
/// the last outcome and restores the saved draft) does not bring it back.
// Hard-won: c1169958: Reattach re-recorded an unanswered prompt after the person dismissed it.
#[test]
fn unanswered_prompts_are_reported_once_until_one_is_answered() {
    let unanswered = mj_core::acp::PROMPT_UNANSWERED_STOP_REASON;
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.apply_materialized(&unanswered_session("p1", 4, "first", unanswered), &[], &[]);
    chat.apply_materialized(&unanswered_session("p2", 6, "second", unanswered), &[], &[]);
    assert_eq!(chat.unsent_prompts.len(), 1);
    assert_eq!(chat.unsent_prompts[0].payload.text, "first");

    // Reattach: a new chat restores the saved draft, then sees the same outcome.
    let mut reattached = ChatState::new(&snapshot(), &[]);
    reattached.restore_draft(chat.encoded_draft());
    reattached.apply_materialized(&unanswered_session("p2", 6, "second", unanswered), &[], &[]);
    reattached.apply_materialized(&unanswered_session("p3", 8, "third", unanswered), &[], &[]);
    assert_eq!(reattached.unsent_prompts.len(), 1);

    // Once the person has dismissed the row, a reload does not restore it.
    reattached.unsent_prompts.clear();
    let mut later = ChatState::new(&snapshot(), &[]);
    later.restore_draft(reattached.encoded_draft());
    later.apply_materialized(&unanswered_session("p3", 8, "third", unanswered), &[], &[]);
    assert!(later.unsent_prompts.is_empty());

    // An answered prompt resets it, so a later failure is reported again.
    later.apply_materialized(
        &unanswered_session("p4", 10, "fourth", "end_turn"),
        &[],
        &[],
    );
    later.apply_materialized(&unanswered_session("p5", 12, "fifth", unanswered), &[], &[]);
    assert_eq!(later.unsent_prompts.len(), 1);
    assert_eq!(later.unsent_prompts[0].payload.text, "fifth");
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

#[test]
fn clear_requires_capability_and_idle_state_before_submission() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.phase = WorkerPhase::Idle;
    chat.set_input("/clear".into());
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert!(chat.notice().unwrap().contains("does not support"));
    chat.clear_context_supported = true;
    chat.rebuild_command_choices();
    assert!(chat.lists_command("clear"));
    chat.phase = WorkerPhase::Running;
    chat.set_input("/clear".into());
    assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    assert!(chat.notice().unwrap().contains("idle"));
    chat.phase = WorkerPhase::Idle;
    chat.set_input("/clear".into());
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter)),
        ChatAction::Prompt("/clear".into())
    );
}

#[test]
fn continuing_feed_failure_survives_dismissal_and_clears_only_on_recovery() {
    let notices = Notices::default();
    notices.set("Unrelated task finished");
    notices.set_persistent_failure(Some("Could not refresh sessions: connection closed".into()));
    let generation = notices.generation();
    notices.set_persistent_failure(Some("Could not refresh sessions: connection closed".into()));
    assert_eq!(notices.generation(), generation);
    assert_eq!(notices.history().len(), 2);
    assert!(!notices.dismiss(std::time::Instant::now() + NOTICE_MINIMUM_DISPLAY));
    assert!(
        notices
            .current()
            .unwrap()
            .contains("Could not refresh sessions")
    );
    notices.set_persistent_failure(None);
    assert_eq!(
        notices.current().as_deref(),
        Some("Unrelated task finished")
    );
    assert!(notices.generation() > generation);

    notices.set_persistent_failure(Some("Could not refresh sessions".into()));
    notices.clear();
    assert!(
        notices
            .current()
            .unwrap()
            .contains("Could not refresh sessions")
    );
    notices.set_persistent_failure(None);
    assert!(notices.current().is_none());
}

#[test]
fn saved_image_drafts_require_current_capability_without_losing_content() {
    let payload = PromptPayload::with_image("inspect ", test_image());
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input_payload(payload.clone());
    let mut reopened = ChatState::new(&snapshot(), &[]);
    reopened.restore_draft(chat.encoded_draft());
    assert_eq!(reopened.submit_input(), ChatAction::None);
    assert_eq!(reopened.draft_payload(), payload);
    assert!(
        reopened
            .notice()
            .unwrap()
            .contains("advertised image support")
    );
    reopened.set_prompt_images_supported(true);
    assert!(matches!(reopened.submit_input(), ChatAction::Prompt(_)));
    assert_eq!(reopened.take_submitting_images(), payload.images);
}

// Hard-won: 6a66a7d0: A refused slash command remained in the draft and contaminated later input.
#[test]
fn attach_requires_capability_and_clears_the_command_on_refusal() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.set_input("/attach picture.png".into());
    assert_eq!(chat.submit_input(), ChatAction::None);
    assert!(chat.input.is_empty());
    let notice = chat.notice().unwrap();
    assert_eq!(notice, super::input_state::ATTACH_UNSUPPORTED_NOTICE);
    assert!(!notice.contains("marker"));
    assert!(!chat.reserve_attachment(1));
    assert!(chat.input_images.is_empty());
}

mod golden_cases {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::fmt::Write as _;

    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 24;

    fn rendered(chat: &mut ChatState, width: u16, height: u16) -> String {
        // The task dialog prints elapsed time. A future start keeps that
        // user-facing clock at 0s in every checked-in render.
        for command in &mut chat.session_activity.background_commands {
            command.started_at_ms = i64::MAX;
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| crate::chat::active::render_full_frame(frame, chat, false))
            .expect("draw chat surface");
        crate::golden::buffer_lines(terminal.backend().buffer()).join("\n")
    }

    fn state<F>(
        output: &mut String,
        label: &str,
        chat: &mut ChatState,
        width: u16,
        height: u16,
        action: Option<ChatAction>,
        details: F,
    ) where
        F: FnOnce(&ChatState) -> Vec<String>,
    {
        if !output.is_empty() {
            output.push('\n');
        }
        writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state label");
        output.push_str(&rendered(chat, width, height));
        output.push('\n');
        if let Some(action) = action {
            writeln!(output, "action: {action:?}").expect("write action");
        }
        for detail in details(chat) {
            writeln!(output, "{detail}").expect("write detail");
        }
    }

    fn details(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn save(name: &str, output: &str) {
        mj_core::golden::assert_platform_golden(env!("CARGO_MANIFEST_DIR"), name, output);
    }

    #[test]
    fn golden_chat_image_prompt_composer() {
        let mut output = String::new();

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        chat.set_input("compare  and this".into());
        chat.input_cursor = "compare ".len();
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        state(
            &mut output,
            "paste image at cursor",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[
                    &format!("draft: {}", chat.input),
                    &format!("image count: {}", chat.input_images.len()),
                ])
            },
        );
        chat.handle_key(key(KeyCode::End));
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        let payload = chat.draft_payload();
        let action = chat.handle_key(key(KeyCode::Enter));
        let submitted = chat.take_submitting_images();
        let submitted_matches = submitted == payload.images;
        let marker_blocks_match = matches!(
            payload.content_blocks().as_slice(),
            [
                ContentBlock::Text(_),
                ContentBlock::Image(_),
                ContentBlock::Text(_),
                ContentBlock::Image(_)
            ]
        );
        state(
            &mut output,
            "submit image markers at their cursor positions",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| {
                details(&[
                    &format!("submitted image count: {}", submitted.len()),
                    &format!("submitted images match draft payload: {submitted_matches}"),
                    &format!("marker content-block layout preserved: {marker_blocks_match}"),
                    &format!("payload: {}", payload.text),
                ])
            },
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        chat.set_input("keep ".into());
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        state(
            &mut output,
            "image marker insertion",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("cursor: {}", chat.input_cursor)]),
        );
        let end = chat.input_cursor;
        chat.handle_key(key(KeyCode::Left));
        state(
            &mut output,
            "left skips the whole marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("cursor: {}", chat.input_cursor)]),
        );
        chat.handle_key(key(KeyCode::Right));
        state(
            &mut output,
            "right skips the whole marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[
                    &format!(
                        "cursor at original marker end: {}",
                        chat.input_cursor == end
                    ),
                    &format!("cursor: {}", chat.input_cursor),
                ])
            },
        );
        chat.handle_key(key(KeyCode::Backspace));
        state(
            &mut output,
            "backspace removes the whole marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[
                    &format!("cursor after deletion: {}", chat.input_cursor),
                    &format!("remaining images: {}", chat.input_images.len()),
                ])
            },
        );
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(key(KeyCode::Delete));
        state(
            &mut output,
            "delete removes the whole marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("remaining images: {}", chat.input_images.len())]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        chat.handle_key(ctrl('u'));
        state(
            &mut output,
            "kill removes image with its marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("images after kill: {}", chat.input_images.len())]),
        );
        chat.handle_key(ctrl('y'));
        chat.handle_key(ctrl('y'));
        state(
            &mut output,
            "yank copies and renumbers images",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[
                    &format!("image count: {}", chat.input_images.len()),
                    &format!(
                        "copies share image data: {}",
                        chat.input_images[0].image == chat.input_images[1].image
                    ),
                ])
            },
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        state(
            &mut output,
            "numbered marker and paste hint",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |_| details(&["clipboard action: PasteFromClipboard"]),
        );
        chat.handle_key(key(KeyCode::Backspace));
        state(
            &mut output,
            "marker removed from composer",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[&format!(
                    "marker remains: {}",
                    chat.input.contains("[image")
                )])
            },
        );

        for command in ["!pwd ", "/help ", "/model ", "/plan inspect "] {
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.set_prompt_images_supported(true);
            chat.set_input(command.into());
            chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
            let before = chat.draft_payload();
            let action = chat.handle_key(key(KeyCode::Enter));
            state(
                &mut output,
                &format!("image retained by {command:?}"),
                &mut chat,
                WIDTH,
                HEIGHT,
                Some(action),
                |chat| {
                    details(&[
                        &format!("draft unchanged: {}", chat.draft_payload() == before),
                        &format!("image count: {}", chat.input_images.len()),
                    ])
                },
            );
        }

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        let action = chat.handle_terminal_paste("");
        state(
            &mut output,
            "empty terminal paste requests clipboard",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        let action = chat.handle_key(key(KeyCode::Enter));
        let submitted = chat.take_submitting_images();
        state(
            &mut output,
            "clipboard image becomes a prompt attachment",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| details(&[&format!("submitted image count: {}", submitted.len())]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut actions = Vec::new();
        for text in ["hello", " \t\n", "world"] {
            actions.push(chat.handle_terminal_paste(text));
        }
        chat.handle_clipboard_content(ClipboardContent::Text(String::new()));
        state(
            &mut output,
            "text paste bypasses clipboard image lookup",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(ChatAction::None),
            |chat| {
                details(&[
                    &format!("text-paste actions: {actions:?}"),
                    &format!("text remains: {:?}", chat.input),
                ])
            },
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[select_config_option("model", "small", &["small", "large"])]);
        chat.open_config_picker("model");
        let action = chat.handle_terminal_paste("");
        state(
            &mut output,
            "empty paste stays in the active config picker",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| details(&[&format!("picker active: {}", chat.config_picker_active())]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        chat.handle_clipboard_content(ClipboardContent::Image(test_image()));
        chat.handle_paste("/attach /workspace/second.png");
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "attach command retains existing marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| {
                details(&[
                    &format!("markers before reservation: {}", chat.input_images.len()),
                    &format!("draft after command: {:?}", chat.input),
                ])
            },
        );
        chat.reserve_attachment(0);
        state(
            &mut output,
            "reserved attachment appends a numbered marker",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("markers: {}", chat.input_images.len())]),
        );

        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "plain text").expect("write text file");
        let error = super::attachments::install_path("attach-text", &path).unwrap_err();
        let message = format!("{error:#}");
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.handle_paste("/attach notes.txt");
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "text file attachment feedback",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| {
                details(&[
                    &format!(
                        "installer feedback includes image-only explanation: {}",
                        message.contains("/attach adds image files only")
                    ),
                    &format!("installer mentions marker: {}", message.contains("marker")),
                ])
            },
        );

        save("chat-image-prompt-composer", &output);
    }

    #[test]
    fn golden_chat_composer_draft_lifecycle() {
        let mut output = String::new();

        let mut saved = freshly_opened_chat("saved before opening");
        state(
            &mut output,
            "fresh chat restores saved draft",
            &mut saved,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("draft: {:?}", chat.draft())]),
        );
        let mut empty = freshly_opened_chat("");
        state(
            &mut output,
            "fresh chat without saved draft",
            &mut empty,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("draft empty: {}", chat.draft().is_empty())]),
        );

        let config: Config = serde_json::from_str(r#"{"version": 0}"#).expect("default config");
        let mut chat = ChatState::standby(
            "session-1",
            &config,
            SessionHeaderIdentity::default(),
            Notices::default(),
        );
        chat.set_draft("alpha beta".into());
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('k'));
        chat.handle_key(ctrl('y'));
        chat.paste("…\r\nsecond");
        state(
            &mut output,
            "standby composer readline and normalized paste",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[
                    &format!("draft: {:?}", chat.draft()),
                    &format!("cursor: {}", chat.input_cursor),
                ])
            },
        );
        chat.set_input("/help".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "standby command remains an editable draft",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| {
                details(&[
                    &format!("draft: {:?}", chat.draft()),
                    &format!("queued prompts: {}", chat.queued_prompt_texts().len()),
                ])
            },
        );
        chat.set_input("alpha beta…\nsecond".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "standby prompt is queued for the host",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| {
                details(&[
                    &format!("draft empty: {}", chat.draft().is_empty()),
                    &format!("queued prompts: {:?}", chat.queued_prompt_texts()),
                ])
            },
        );
        chat.remove_queued_prompt_text("alpha beta…\nsecond");
        chat.set_input("/mod".into());
        chat.update_autocomplete();
        state(
            &mut output,
            "standby composer does not offer slash autocomplete",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[&format!(
                    "autocomplete active: {}",
                    chat.autocomplete.is_some()
                )])
            },
        );

        save("chat-composer-draft-lifecycle", &output);
    }

    #[test]
    fn golden_chat_voice_prompt_control() {
        let mut output = String::new();

        let mut chat = ChatState::new(&snapshot(), &[]);
        let unavailable = chat.dictation_toggle_action();
        let key_action = chat.handle_key(alt('v'));
        state(
            &mut output,
            "voice unavailable",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(unavailable),
            |chat| {
                details(&[
                    &format!("Alt-V action: {key_action:?}"),
                    &format!("draft empty: {}", chat.input.is_empty()),
                ])
            },
        );
        chat.set_voice_available(true);
        let action = chat.dictation_toggle_action();
        let key_action = chat.handle_key(alt('v'));
        state(
            &mut output,
            "voice available, host owns Alt-V",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| details(&[&format!("Alt-V action: {key_action:?}")]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_voice_available(true);
        state(
            &mut output,
            "enabled microphone and rendered hitbox",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("button area: {:?}", chat.voice_button_area)]),
        );
        let area = chat.voice_button_area.expect("rendered voice button");
        let down = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        let up = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        state(
            &mut output,
            "mouse activates microphone on release",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(up),
            |_| details(&[&format!("press action: {down:?}")]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.voice_button_area = Some(Rect::new(10, 8, 4, 1));
        let disabled = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 8,
            modifiers: KeyModifiers::NONE,
        });
        state(
            &mut output,
            "disabled microphone click is inert",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(disabled),
            |chat| details(&[&format!("button area: {:?}", chat.voice_button_area)]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.voice_active = true;
        chat.set_input("dictated draft".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Enter preserves a draft during active voice capture",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| details(&[&format!("draft: {:?}", chat.input)]),
        );

        for (width, prompt) in [
            (4, Rect::new(0, 0, 4, 3)),
            (5, Rect::new(0, 0, 5, 3)),
            (30, Rect::new(4, 2, 30, 5)),
        ] {
            let button = voice_button_area(prompt);
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.set_voice_available(true);
            state(
                &mut output,
                &format!("prompt width {width} microphone geometry"),
                &mut chat,
                width,
                8,
                None,
                |_| details(&[&format!("button area: {button:?}")]),
            );
        }

        save("chat-voice-prompt-control", &output);
    }

    #[test]
    fn golden_chat_background_task_dialog() {
        let mut output = String::new();
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("keep this draft".into());
        chat.set_session_activity(mj_client::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            background_commands: vec![background_task("task-main", "cargo test --workspace", true)],
            ..mj_client::usage_format::SessionActivity::default()
        });
        let initial = rendered(&mut chat, 100, 24);
        let initial_task_x = chat.task_control_area.expect("initial task control").x;
        state(
            &mut output,
            "task count in prompt border",
            &mut chat,
            100,
            24,
            None,
            |chat| {
                details(&[
                    &format!("task label visible: {}", initial.contains("Tasks (1)")),
                    &format!("draft: {:?}", chat.input),
                    &format!("initial task control x: {initial_task_x}"),
                ])
            },
        );
        chat.mark_prompt_submitted("continue");
        chat.steering_supported = Some(true);
        chat.targeted_turn_control_supported = true;
        chat.queued_prompts.push_back(queued("next", "follow up"));
        for width in [32, 48, 56, 80] {
            state(
                &mut output,
                &format!("queued task border width {width}"),
                &mut chat,
                width,
                24,
                None,
                |chat| {
                    details(&[
                        &format!("task control area: {:?}", chat.task_control_area),
                        &format!("queued text: {}", chat.queued_prompt_texts().len()),
                    ])
                },
            );
        }
        state(
            &mut output,
            "queued task border width 100",
            &mut chat,
            100,
            24,
            None,
            |chat| {
                let area = chat.task_control_area.expect("task control after layout");
                details(&[
                    &format!("task control area: {area:?}"),
                    &format!("task control shifted right: {}", area.x > initial_task_x),
                ])
            },
        );

        let task_area = chat.task_control_area.expect("task control hitbox");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: task_area.x,
            row: task_area.y,
            modifiers: KeyModifiers::NONE,
        };
        let mouse_claimed = chat.component_handles_mouse(click);
        let action = chat.handle_mouse(click);
        state(
            &mut output,
            "mouse opens task dialog",
            &mut chat,
            100,
            24,
            Some(action),
            |chat| {
                details(&[
                    &format!("dialog open: {}", chat.task_dialog_open()),
                    &format!("draft cursor: {}", chat.input_cursor),
                    &format!("chat surface claims click: {mouse_claimed}"),
                ])
            },
        );
        let dialog_inner = chat.task_dialog_area.expect("task dialog geometry");
        let dismiss = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: dialog_inner.x.saturating_add(1),
            row: dialog_inner.y.saturating_sub(1),
            modifiers: KeyModifiers::NONE,
        };
        let outside_claimed = chat.component_handles_mouse(dismiss);
        let down = chat.handle_mouse(dismiss);
        let up = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..dismiss
        });
        state(
            &mut output,
            "outside mouse click dismisses task dialog",
            &mut chat,
            100,
            24,
            Some(up),
            |chat| {
                details(&[
                    &format!("press action: {down:?}"),
                    &format!("dialog open: {}", chat.task_dialog_open()),
                    &format!("chat surface claims outside click: {outside_claimed}"),
                ])
            },
        );
        let down = chat.handle_key(key(KeyCode::Down));
        let open = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "keyboard opens dialog without replacing draft",
            &mut chat,
            100,
            24,
            Some(open),
            |chat| {
                details(&[
                    &format!("focus action: {down:?}"),
                    &format!("draft: {:?}", chat.input),
                ])
            },
        );
        let close = chat.handle_key(key(KeyCode::Esc));
        state(
            &mut output,
            "Escape closes task dialog",
            &mut chat,
            100,
            24,
            Some(close),
            |chat| details(&[&format!("draft: {:?}", chat.input)]),
        );

        chat.open_task_dialog();
        chat.session_activity.background_commands[0].command =
            "cargo test --all-targets --all-features --workspace".into();
        for height in [1, 4, 8] {
            state(
                &mut output,
                &format!("narrow task dialog height {height}"),
                &mut chat,
                24,
                height,
                None,
                |chat| details(&[&format!("scroll: {}", chat.task_dialog_scroll)]),
            );
        }
        for _ in 0..20 {
            chat.handle_key(key(KeyCode::Down));
        }
        state(
            &mut output,
            "wrapped task command scroll tail",
            &mut chat,
            24,
            8,
            None,
            |chat| details(&[&format!("scroll: {}", chat.task_dialog_scroll)]),
        );
        chat.session_activity.background_commands[0].command = format!(
            "cargo test {}FINAL_ARGUMENT",
            "--feature example ".repeat(40)
        );
        rendered(&mut chat, 80, 12);
        for _ in 0..100 {
            chat.handle_key(key(KeyCode::Down));
        }
        state(
            &mut output,
            "long command retains its final argument after scrolling",
            &mut chat,
            80,
            12,
            None,
            |chat| details(&[&format!("scroll: {}", chat.task_dialog_scroll)]),
        );
        chat.set_session_activity(mj_client::usage_format::SessionActivity::default());
        state(
            &mut output,
            "empty task dialog",
            &mut chat,
            80,
            12,
            None,
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_session_activity(mj_client::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            background_commands: vec![background_task("read-only", "codex exec", false)],
            ..mj_client::usage_format::SessionActivity::default()
        });
        chat.open_task_dialog();
        let tab = chat.handle_key(key(KeyCode::Tab));
        let enter = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "read-only row has no Stop control",
            &mut chat,
            80,
            16,
            Some(enter),
            |chat| {
                details(&[
                    &format!("Tab action: {tab:?}"),
                    &format!("dialog remains open: {}", chat.task_dialog_open()),
                ])
            },
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_session_activity(mj_client::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            background_commands: (0..8)
                .map(|index| {
                    background_task(&format!("task-{index}"), &format!("work-{index}"), true)
                })
                .collect(),
            ..mj_client::usage_format::SessionActivity::default()
        });
        chat.open_task_dialog();
        rendered(&mut chat, 40, 8);
        let inner = chat.task_dialog_area.expect("dialog geometry");
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: inner.x,
            row: inner.y,
            modifiers: KeyModifiers::NONE,
        });
        state(
            &mut output,
            "mouse scrolls task rows",
            &mut chat,
            40,
            8,
            None,
            |chat| details(&[&format!("scroll: {}", chat.task_dialog_scroll)]),
        );
        chat.handle_key(key(KeyCode::PageUp));
        state(
            &mut output,
            "PageUp returns to first task rows",
            &mut chat,
            40,
            8,
            None,
            |chat| details(&[&format!("scroll: {}", chat.task_dialog_scroll)]),
        );
        let x = inner.right().saturating_sub(2);
        let y = inner.y;
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        let down = chat.handle_mouse(press);
        let stop = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..press
        });
        state(
            &mut output,
            "mouse stops first visible task",
            &mut chat,
            40,
            8,
            Some(stop),
            |_| details(&[&format!("press action: {down:?}")]),
        );

        save("chat-background-task-dialog", &output);
    }

    #[test]
    fn golden_chat_subagents_prompt_control() {
        let mut output = String::new();
        let mut chat = ChatState::new(&snapshot(), &[]);
        state(
            &mut output,
            "subagent control absent by default",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("control: {:?}", chat.subagent_control_area)]),
        );
        chat.set_subagents_enabled(true);
        state(
            &mut output,
            "dimmed zero count",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("control: {:?}", chat.subagent_control_area)]),
        );
        crate::theme::with_symbols(crate::theme::SymbolSet::Ascii, || {
            state(
                &mut output,
                "dimmed zero count in ASCII",
                &mut chat,
                WIDTH,
                HEIGHT,
                None,
                |_| details(&["symbol mode: ASCII"]),
            );
        });
        chat.set_subagent_count(1);
        state(
            &mut output,
            "first child enables prompt control",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[&format!(
                    "control available: {}",
                    chat.subagent_control_area.is_some()
                )])
            },
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("keep this draft".into());
        chat.set_subagent_count(2);
        state(
            &mut output,
            "active subagent count and preserved draft",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("control: {:?}", chat.subagent_control_area)]),
        );
        let area = chat.subagent_control_area.expect("subagent hitbox");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        let mouse_claimed = chat.component_handles_mouse(click);
        let mouse_action = chat.handle_mouse(click);
        state(
            &mut output,
            "mouse opens subagent panel",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(mouse_action),
            |_| details(&[&format!("chat surface claims click: {mouse_claimed}")]),
        );
        let down = chat.handle_key(key(KeyCode::Down));
        let enter = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "keyboard opens subagent panel",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(enter),
            |chat| {
                details(&[
                    &format!("focus action: {down:?}"),
                    &format!("draft: {:?}", chat.input),
                ])
            },
        );

        save("chat-subagents-prompt-control", &output);
    }

    #[test]
    fn golden_chat_queued_prompt_editing() {
        let mut output = String::new();
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.queued_prompts.push_back(queued("queued-1", "first"));
        chat.queued_prompts.push_back(queued("queued-2", "second"));
        let action = chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        state(
            &mut output,
            "Alt-Up peels newest queued prompt",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| {
                details(&[
                    &format!("draft: {:?}", chat.input),
                    &format!("remaining queue: {:?}", chat.queued_prompt_texts()),
                ])
            },
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        for (id, text) in [
            ("queued-1", "first"),
            ("queued-2", "second"),
            ("queued-3", "third"),
        ] {
            chat.queued_prompts.push_back(queued(id, text));
        }
        for (label, key_event) in [
            ("Up peels newest", key(KeyCode::Up)),
            ("Ctrl-P peels next", ctrl('p')),
            ("Up peels oldest", key(KeyCode::Up)),
            ("Up with empty queue", key(KeyCode::Up)),
        ] {
            let action = chat.handle_key(key_event);
            state(
                &mut output,
                label,
                &mut chat,
                WIDTH,
                HEIGHT,
                Some(action),
                |chat| {
                    details(&[
                        &format!("draft: {:?}", chat.input),
                        &format!("remaining queue: {:?}", chat.queued_prompt_texts()),
                    ])
                },
            );
            chat.clear_input();
        }

        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut session = MaterializedSession::empty("1234567890");
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
        state(
            &mut output,
            "queued config before editing",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[&format!(
                    "queue label: {}",
                    chat.queued_prompts[0].queue_label()
                )])
            },
        );
        let action = chat.handle_key(ctrl('p'));
        state(
            &mut output,
            "Ctrl-P peels queued config into composer",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| {
                details(&[
                    &format!("draft: {:?}", chat.input),
                    &format!("queue empty: {}", chat.queued_prompts.is_empty()),
                ])
            },
        );
        chat.phase = WorkerPhase::Running;
        chat.set_config_options(&[select_config_option("model", "opus", &["opus", "sonnet"])]);
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "resubmitted config retains its command",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        save("chat-queued-prompt-editing", &output);
    }

    #[test]
    fn golden_chat_config_slash_commands() {
        let mut output = String::new();
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[
            select_config_option("model", "gpt-5.6", &["gpt-5.6", "gpt-5.6-luna"]),
            select_config_option("effort", "high", &["high", "xhigh"]),
        ]);
        chat.set_input("/model gpt-5.6-luna".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "model selector",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.set_input("/effort xhigh".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "effort selector",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[fast_mode_option("off")]);
        chat.set_input("/fast".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "fast toggles on",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.set_config_options(&[fast_mode_option("on")]);
        chat.set_input("/fast".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "fast toggles off",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.set_input("/fast on".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "fast argument usage",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("/fast".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "fast unavailable for active model",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Running;
        chat.set_input("/model".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "busy model command usage",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.set_config_options(&[select_config_option("model", "opus", &["opus", "sonnet"])]);
        chat.set_input("/model sonnet".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "busy agent accepts queued config update",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| details(&[&format!("draft cleared: {}", chat.input.is_empty())]),
        );
        chat.phase = WorkerPhase::Closing;
        chat.set_input("/model sonnet".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "closing worker refuses config update",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("/model".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "missing model value shows usage",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        save("chat-config-slash-commands", &output);
    }

    #[test]
    fn golden_chat_plan_command_capabilities() {
        let mut output = String::new();

        let mut chat = grok_chat();
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Grok enters plan mode",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.plan_command_pending = false;
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Grok exits plan mode",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = grok_chat();
        chat.set_input("/plan off".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "explicit plan off",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.set_input("/plan ON".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "explicit plan on",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );
        chat.plan_command_pending = false;
        chat.set_input("/plan sideways".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "unrecognized plan suffix becomes prompt",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[mode_config_option("default", &["default", "plan"])]);
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "advertised mode config",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = grok_chat();
        chat.set_config_options(&[mode_config_option("default", &["default", "act"])]);
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Grok trusted mode fallback",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_harness_kind(HarnessKind::Muse);
        advertise(&mut chat, 1, &["plan"]);
        chat.set_input("/plan the migration".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Muse advertised plan skill",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = grok_chat();
        advertise(&mut chat, 1, &["plan"]);
        chat.set_input("/plan the migration".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "unified plan command wins over skill",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "no compatible plan surface",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_harness_kind(HarnessKind::Codex);
        chat.set_config_options(&[
            select_config_option("mode", "read-only", &["read-only", "full-access"]),
            select_config_option("collaboration_mode", "default", &["default", "plan"]),
        ]);
        chat.set_input("/plan inspect the migration".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Codex collaboration mode preserves permission mode",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| details(&[&format!("prompt history: {:?}", chat.prompt_history)]),
        );

        for harness in [HarnessKind::Claude, HarnessKind::Kimi] {
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.set_harness_kind(harness);
            chat.set_config_options(&[select_config_option(
                "mode",
                "default",
                &["default", "plan"],
            )]);
            chat.set_input("/plan".into());
            let action = chat.handle_key(key(KeyCode::Enter));
            state(
                &mut output,
                &format!("{harness:?} exact mode config"),
                &mut chat,
                WIDTH,
                HEIGHT,
                Some(action),
                |_| Vec::new(),
            );
        }

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_harness_kind(HarnessKind::Grok);
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "Grok set mode without catalogue",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = grok_chat();
        chat.set_harness_kind(HarnessKind::Muse);
        for command in ["/plan design it", "/implement"] {
            chat.set_input(command.into());
            let action = chat.handle_key(key(KeyCode::Enter));
            state(
                &mut output,
                &format!("unsupported plan command {command}"),
                &mut chat,
                WIDTH,
                HEIGHT,
                Some(action),
                |_| Vec::new(),
            );
        }

        let mut chat = grok_chat();
        chat.finish_plan_mode_change(true);
        chat.set_input("/implement start with the parser".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "implement exits plan and carries instruction",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        let mut chat = grok_chat();
        chat.finish_plan_mode_change(true);
        let request = ElicitationRequest {
            id: "plan-review-1".into(),
            message: "review".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        for (action_name, feedback) in [
            ("implement", None),
            ("revise", Some("add tests")),
            ("exit", None),
        ] {
            let mut content = std::collections::BTreeMap::new();
            content.insert(
                "action".into(),
                ElicitationValue::String(action_name.into()),
            );
            if let Some(feedback) = feedback {
                content.insert("feedback".into(), ElicitationValue::String(feedback.into()));
            }
            let response = ElicitationResponse::Accept { content };
            let followup = chat.plan_review_followup(&request, &response);
            state(
                &mut output,
                &format!("plan review choice {action_name}"),
                &mut chat,
                WIDTH,
                HEIGHT,
                None,
                |_| details(&[&format!("followup: {followup:?}")]),
            );
        }

        let mut chat = grok_chat();
        chat.phase = WorkerPhase::Running;
        chat.set_input("/plan".into());
        let action = chat.handle_key(key(KeyCode::Enter));
        state(
            &mut output,
            "plan waits for idle agent",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |_| Vec::new(),
        );

        save("chat-plan-command-capabilities", &output);
    }

    #[test]
    fn golden_chat_transcript_rendering() {
        let mut output = String::new();

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("inspect renderer".into());
        chat.toggle_render_mode();
        state(
            &mut output,
            "raw transcript and composer state",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("render mode: {:?}", chat.render_mode)]),
        );
        chat.toggle_render_mode();
        for key_event in [alt('t'), ctrl('t'), alt('v')] {
            let action = chat.handle_key(key_event);
            state(
                &mut output,
                "retired transcript shortcut remains local",
                &mut chat,
                WIDTH,
                HEIGHT,
                Some(action),
                |chat| details(&[&format!("render mode: {:?}", chat.render_mode)]),
            );
        }

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
                request_id: Some("prompt-1".into()),
                event: WorkerEvent::PromptAccepted {
                    request_id: "prompt-1".into(),
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
        let mut replayed = ChatState::new(&initial, &events);
        state(
            &mut output,
            "replayed user and agent transcript",
            &mut replayed,
            WIDTH,
            HEIGHT,
            None,
            |_| Vec::new(),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({"sessionUpdate":"tool_call", "toolCallId":"grep-config", "title":"grep config", "status":"pending"}),
        );
        state(
            &mut output,
            "pending tool call title",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |_| Vec::new(),
        );
        chat.apply_session_update(
            2,
            &serde_json::json!({"sessionUpdate":"tool_call_update", "toolCallId":"grep-config", "status":"completed", "content":[{"type":"content", "content":{"type":"text", "text":"noise"}}]}),
        );
        state(
            &mut output,
            "completed tool call keeps title and suppresses update noise",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| details(&[&format!("entry text: {:?}", chat.entries[0].text)]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        for (seq, status) in [(1, "pending"), (2, "completed")] {
            chat.apply_session_update(
                seq,
                &serde_json::json!({
                    "sessionUpdate":"plan",
                    "entries":[{"content":"inspect renderer", "priority":"high", "status":status}]
                }),
            );
            state(
                &mut output,
                &format!("plan entry {status}"),
                &mut chat,
                WIDTH,
                HEIGHT,
                None,
                |chat| {
                    details(&[
                        &format!("plan entries: {}", chat.entries[0].plan.len()),
                        &format!(
                            "completed: {}",
                            chat.entries[0].plan[0].status == PlanStatus::Completed
                        ),
                    ])
                },
            );
        }

        save("chat-transcript-rendering", &output);
    }

    #[test]
    fn golden_chat_unanswered_prompt_recovery() {
        let mut output = String::new();
        let unanswered = mj_core::acp::PROMPT_UNANSWERED_STOP_REASON;
        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut session = unanswered_session("prompt-1", 4, "rename the module", unanswered);
        let transcript_item = std::sync::Arc::make_mut(&mut session.transcript[0]);
        transcript_item.created_at_ms = i64::MIN;
        transcript_item.last_changed_at_ms = i64::MIN;
        chat.apply_materialized(&session, &[], &[]);
        chat.apply_materialized(&session, &[], &[]);
        chat.unsent_prompts[0].recorded_at_ms = i64::MIN;
        state(
            &mut output,
            "unanswered prompt appears once and can be restored",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[
                    &format!("unsent prompt count: {}", chat.unsent_prompts.len()),
                    &format!("headline: {}", chat.unsent_prompts[0].kind.headline()),
                ])
            },
        );
        let action = chat.handle_key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        state(
            &mut output,
            "Ctrl-Alt-R restores unanswered prompt to composer",
            &mut chat,
            WIDTH,
            HEIGHT,
            Some(action),
            |chat| details(&[&format!("restored draft: {:?}", chat.draft_payload().text)]),
        );

        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut finished = MaterializedSession::empty("1234567890");
        finished.applied_event_ordinal = 9;
        finished.last_turn_outcome = Some(unanswered_outcome("EndTurn"));
        chat.apply_materialized(&finished, &[], &[]);
        state(
            &mut output,
            "normally completed turn has no restore row",
            &mut chat,
            WIDTH,
            HEIGHT,
            None,
            |chat| {
                details(&[&format!(
                    "unsent prompt count: {}",
                    chat.unsent_prompts.len()
                )])
            },
        );

        save("chat-unanswered-prompt-recovery", &output);
    }
}
