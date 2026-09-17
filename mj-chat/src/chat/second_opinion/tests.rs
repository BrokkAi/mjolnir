use super::*;
use crate::chat::test_support::{drawn_transcript, key, snapshot};
use crate::chat::{ChatAction, ChatState};
use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use mj_core::second_opinion::{HARNESS_DEFAULT_VALUE, ReviewerDefaults, ReviewerProfileChoice};
use mj_core::transcript::ChatRole;

fn plan_review() -> ElicitationRequest {
    mj_core::acp::normalized_plan_review(
        "plan-review-1".into(),
        &serde_json::json!({ "plan": "1. Read\n2. Change" }),
    )
}

fn captured() -> CapturedProposal {
    let request = plan_review();
    let proposal = mj_core::acp::plan_review_proposal(&request)
        .expect("a normalized plan review carries its proposal")
        .to_owned();
    CapturedProposal { request, proposal }
}

fn profiles() -> Vec<ReviewerProfileChoice> {
    vec![
        ReviewerProfileChoice {
            id: "codex".into(),
            harness: "codex".into(),
        },
        ReviewerProfileChoice {
            id: "claude".into(),
            harness: "claude".into(),
        },
    ]
}

fn config_option(id: &str, category: SessionConfigOptionCategory) -> SessionConfigOption {
    SessionConfigOption::select(
        id.to_owned(),
        id.to_owned(),
        id.to_owned(),
        SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
            id.to_owned(),
            id.to_owned(),
        )]),
    )
    .category(category)
}

fn chat_in_setup() -> ChatState {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.open_second_opinion(
        captured(),
        ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
    );
    chat
}

fn press(chat: &mut ChatState, code: KeyCode) -> ChatAction {
    chat.handle_key(key(code))
}

/// Answering one decision, from the dialog the user actually sees.
/// `steps` moves the highlight before the answer is accepted.
fn answer_plan_review(steps: usize) -> ChatAction {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.restore_elicitation(plan_review());
    press(&mut chat, KeyCode::Home);
    for _ in 0..steps {
        press(&mut chat, KeyCode::Down);
    }
    // Enter walks to the accept button and then presses it.
    for _ in 0..8 {
        let action = press(&mut chat, KeyCode::Enter);
        if !matches!(action, ChatAction::None) {
            return action;
        }
    }
    panic!("the decision dialog never produced an answer");
}

#[test]
fn cancelling_reviewer_setup_restores_the_unanswered_plan() {
    let mut chat = chat_in_setup();
    press(&mut chat, KeyCode::Enter);
    press(&mut chat, KeyCode::Esc);
    assert!(!chat.second_opinion_active());
    assert_eq!(
        chat.elicitation
            .as_ref()
            .expect("restored decision")
            .request(),
        &captured().request
    );
}

#[test]
fn choosing_the_second_opinion_never_answers_the_harness() {
    let mj_core::elicitation::ElicitationFieldKind::SingleSelect { options, .. } =
        &plan_review().fields[0].kind
    else {
        panic!("the decision is a single select");
    };
    // Every reachable decision is answered; exactly one of them is Hel's
    // own, and it must never become an elicitation response.
    let mut local = Vec::new();
    for steps in 0..options.len() {
        match answer_plan_review(steps) {
            ChatAction::StartSecondOpinion { request, proposal } => {
                assert_eq!(request.id, "plan-review-1");
                local.push(proposal);
            }
            ChatAction::RespondElicitation { response, .. } => {
                let mj_core::elicitation::ElicitationResponse::Accept { content } = response else {
                    panic!("accepting the dialog produces an accept");
                };
                assert_ne!(
                    content.get("action"),
                    Some(&mj_core::elicitation::ElicitationValue::String(
                        mj_core::acp::PLAN_REVIEW_SECOND_OPINION.to_owned()
                    )),
                    "a second opinion must never reach the harness"
                );
            }
            other => panic!("unexpected answer {other:?}"),
        }
    }
    assert_eq!(local, vec!["1. Read\n2. Change".to_owned()]);
}

#[test]
fn every_dialect_offers_the_second_opinion() {
    // Both plan-decision paths build their dialog through the same
    // normalizer, so the option is offered whatever the harness sent.
    for value in [
        serde_json::json!({ "plan": "standard permission plan" }),
        serde_json::json!({ "plan_content": "native feedback plan" }),
        serde_json::json!({ "planContent": "switch mode plan" }),
    ] {
        let request = mj_core::acp::normalized_plan_review("plan-review-1".into(), &value);
        let mj_core::elicitation::ElicitationFieldKind::SingleSelect { options, .. } =
            &request.fields[0].kind
        else {
            panic!("the decision is a single select");
        };
        assert!(
            options
                .iter()
                .any(|option| option.value == mj_core::acp::PLAN_REVIEW_SECOND_OPINION),
            "a plan decision must always offer a second opinion"
        );
    }
}

#[test]
fn the_waterfall_asks_the_session_to_probe_the_chosen_profile() {
    let mut chat = chat_in_setup();
    assert!(chat.second_opinion_active());

    chat.handle_key(key(KeyCode::Down));
    let action = press(&mut chat, KeyCode::Enter);
    let ChatAction::SecondOpinion(SecondOpinionIntent::Setup(requests)) = action else {
        panic!("confirming a profile probes it: {action:?}");
    };
    assert_eq!(
        requests,
        vec![SetupRequest::Probe {
            generation: 1,
            profile_id: "claude".into(),
        }]
    );
}

#[test]
fn immediate_default_model_advance_focuses_effort_options() {
    let mut chat = chat_in_setup();
    let _ = press(&mut chat, KeyCode::Enter);
    if let Some(SecondOpinion::Setup { setup, .. }) = chat.second_opinion_mut() {
        assert!(setup.probe_succeeded(1, &[]).is_none());
    } else {
        panic!("the reviewer setup remains open while discovery completes");
    }

    let _ = press(&mut chat, KeyCode::Tab);
    assert_eq!(press(&mut chat, KeyCode::Enter), ChatAction::None);
    let Some(SecondOpinion::Setup { setup, form, .. }) = chat.second_opinion() else {
        panic!("the reviewer setup remains open at the effort step");
    };
    assert_eq!(setup.stage(), SetupStage::Effort);
    assert_eq!(form.focused(), Some(SetupControl::Options));
}

#[test]
fn backing_to_profile_focuses_profile_options() {
    let mut chat = chat_in_setup();
    let _ = press(&mut chat, KeyCode::Enter);
    if let Some(SecondOpinion::Setup { setup, .. }) = chat.second_opinion_mut() {
        assert!(setup.probe_succeeded(1, &[]).is_none());
    } else {
        panic!("the reviewer setup remains open while discovery completes");
    }

    let _ = press(&mut chat, KeyCode::Tab);
    let _ = press(&mut chat, KeyCode::Tab);
    let _ = press(&mut chat, KeyCode::Enter);

    let Some(SecondOpinion::Setup { setup, form, .. }) = chat.second_opinion() else {
        panic!("backing up keeps the reviewer setup open");
    };
    assert_eq!(setup.stage(), SetupStage::Profile);
    assert_eq!(form.focused(), Some(SetupControl::Options));
}

#[test]
fn backing_to_model_focuses_model_options() {
    let mut chat = chat_in_setup();
    let _ = press(&mut chat, KeyCode::Enter);
    if let Some(SecondOpinion::Setup { setup, .. }) = chat.second_opinion_mut() {
        assert!(
            setup
                .probe_succeeded(
                    1,
                    &[config_option("model", SessionConfigOptionCategory::Model)]
                )
                .is_none()
        );
    } else {
        panic!("the reviewer setup remains open while discovery completes");
    }
    let _ = press(&mut chat, KeyCode::Enter);
    if let Some(SecondOpinion::Setup { setup, .. }) = chat.second_opinion_mut() {
        assert!(
            setup
                .model_applied(
                    1,
                    &[
                        config_option("model", SessionConfigOptionCategory::Model),
                        config_option("effort", SessionConfigOptionCategory::ThoughtLevel,),
                    ]
                )
                .is_none()
        );
    } else {
        panic!("the reviewer setup remains open while model configuration completes");
    }

    let _ = press(&mut chat, KeyCode::Tab);
    let _ = press(&mut chat, KeyCode::Tab);
    let _ = press(&mut chat, KeyCode::Enter);

    let Some(SecondOpinion::Setup { setup, form, .. }) = chat.second_opinion() else {
        panic!("backing up keeps the reviewer setup open");
    };
    assert_eq!(setup.stage(), SetupStage::Model);
    assert_eq!(form.focused(), Some(SetupControl::Options));
}

#[test]
fn cancelling_the_waterfall_leaves_the_captured_plan_alone() {
    let mut chat = chat_in_setup();
    let action = press(&mut chat, KeyCode::Esc);

    assert!(!chat.second_opinion_active());
    // Nothing was sent to the harness, so its own decision is still
    // pending and will be rebuilt from the projection.
    assert!(matches!(
        action,
        ChatAction::SecondOpinion(SecondOpinionIntent::Closed)
    ));
}

#[test]
fn reviewer_setup_dismiss_glyph_survives_a_redraw_and_matches_escape() {
    let mut clicked = chat_in_setup();
    let rows = drawn_transcript(&mut clicked, 100, 24);
    let (row, column) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.chars()
                .position(|character| character == '×')
                .map(|column| (row as u16, column as u16))
        })
        .expect("reviewer setup dismiss glyph");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(clicked.handle_mouse(click), ChatAction::None);
    // The visual armed state causes a redraw before the release arrives.
    // The same form gesture must survive that frame boundary.
    drawn_transcript(&mut clicked, 100, 24);
    assert_eq!(
        clicked.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..click
        }),
        ChatAction::SecondOpinion(SecondOpinionIntent::Closed)
    );
    assert!(!clicked.second_opinion_active());

    let mut escaped = chat_in_setup();
    assert_eq!(
        press(&mut escaped, KeyCode::Esc),
        ChatAction::SecondOpinion(SecondOpinionIntent::Closed)
    );
    assert!(!escaped.second_opinion_active());
}

/// The split's actions are the whole keyboard: there is no composer,
/// because the revised plan is the planner's to write.
#[test]
fn the_split_cycles_its_actions_and_gates_transfer() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let captured = captured();
    let (workflow, _) =
        ReviewWorkflow::start(captured.id(), captured.proposal.clone(), "context-1");
    chat.open_second_opinion(
        captured,
        ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
    );
    chat.second_opinion_mut()
        .expect("the view is open")
        .begin_review(workflow, "waiting", 0);
    assert!(chat.second_opinion_split());

    // Transfer is first and refuses until the reviewer has answered.
    assert_eq!(press(&mut chat, KeyCode::Enter), ChatAction::None);
    assert!(chat.second_opinion_split(), "a refused transfer stays put");

    // Implementing the original needs no reviewer answer.
    press(&mut chat, KeyCode::Tab);
    let action = press(&mut chat, KeyCode::Enter);
    let ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests)) = action else {
        panic!("implementing the original is a workflow step: {action:?}");
    };
    let [
        WorkflowRequest::PromptPrimary { prompt, .. },
        WorkflowRequest::PauseReviewer,
    ] = requests.as_slice()
    else {
        panic!("implementing prompts the primary and pauses the reviewer");
    };
    assert!(prompt.contains("1. Read\n2. Change"));
    assert!(!chat.second_opinion_active(), "the split closes behind it");
}

#[test]
fn cancelling_the_split_asks_for_the_captured_decision_back() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let (workflow, _) = ReviewWorkflow::start("plan-review-1", "the plan", "context-1");
    chat.open_second_opinion(
        captured(),
        ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
    );
    chat.second_opinion_mut()
        .expect("the view is open")
        .begin_review(workflow, "waiting", 0);

    let action = press(&mut chat, KeyCode::Esc);
    let ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests)) = action else {
        panic!("cancelling is a workflow step: {action:?}");
    };
    assert!(requests.contains(&WorkflowRequest::PauseReviewer));
    assert!(requests.iter().any(|request| matches!(
        request,
        WorkflowRequest::RestoreDecision { proposal, .. } if proposal == "the plan"
    )));
    // Nothing was transferred.
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, WorkflowRequest::PromptPrimary { .. }))
    );
}

#[test]
fn reviewer_rows_adopt_the_new_theme_without_new_events() {
    let mut pane = pane_from_entries(vec![ChatEntry::plain(
        1,
        ChatRole::Agent,
        "Review the **changed behavior**.",
    )]);
    theme::with_theme(theme::UiTheme::Midnight, || pane.ensure_rows(60));
    let original = pane.rows.clone();
    theme::with_theme(theme::UiTheme::Light, || {
        pane.ensure_rows(60);
        assert_ne!(pane.rows, original);
        assert_eq!(
            pane.rows.iter().map(row_text).collect::<Vec<_>>(),
            original.iter().map(row_text).collect::<Vec<_>>()
        );
        assert!(
            pane.rows
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| { span.style.fg == Some(theme::palette().secondary) })
        );
    });
}

#[test]
fn the_reviewer_pane_scrolls_and_copies_from_its_own_rows() {
    let entries = (0..40)
        .map(|index| ChatEntry::plain(index + 1, ChatRole::Agent, format!("reviewer line {index}")))
        .collect::<Vec<_>>();
    let mut pane = pane_from_entries(entries);
    pane.ensure_rows(40);
    let total = pane.rows.len();
    assert!(total > 10, "the fixture must not fit on one screen");

    // Scrolling stops at the last full screen rather than running past it.
    pane.scroll_by(1_000, 10);
    assert_eq!(pane.viewport.top_row, total - 10);
    pane.scroll_by(-1_000, 10);
    assert_eq!(pane.viewport.top_row, 0);

    let text = pane
        .selection_text(&SelectionRange {
            start: crate::selection::ContentPos::new(0, 0),
            end: crate::selection::ContentPos::new(1, 39),
        })
        .expect("a selection over this pane's rows resolves here");
    assert!(
        text.contains("reviewer line 0"),
        "the pane resolves its own rows: {text:?}"
    );
}

/// A live split, drawn once so its panes and buttons have real rects.
fn drawn_split() -> (ChatState, ratatui::layout::Rect) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut chat = ChatState::new(&snapshot(), &[]);
    // Enough primary history to have somewhere to scroll to, so a wheel
    // that reaches the primary is visible in its anchor.
    chat.entries
        .extend((1..=60).map(|index| {
            ChatEntry::plain(index, ChatRole::Agent, format!("primary line {index}"))
        }));
    let captured = captured();
    let (mut workflow, _) =
        ReviewWorkflow::start(captured.id(), captured.proposal.clone(), "context-1");
    workflow.primary_context_completed("context-1", "context", "review-1");
    workflow.reviewer_turn_completed("review-1", "the plan misses error handling");
    chat.open_second_opinion(
        captured,
        ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
    );
    chat.second_opinion_mut()
        .expect("the view is open")
        .begin_review(workflow, "ready", 0);

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| crate::chat::active::render_full_frame(frame, &mut chat, false))
        .unwrap();
    let reviewer = chat.reviewer_area.expect("the split draws a reviewer pane");
    (chat, reviewer)
}

#[test]
fn the_wheel_scrolls_whichever_pane_it_is_over() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut chat, reviewer) = drawn_split();
    let primary_before = chat.anchor;

    let wheel = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    // Over the reviewer: only the reviewer moves.
    chat.handle_mouse(wheel(
        MouseEventKind::ScrollUp,
        reviewer.x + 1,
        reviewer.y + 1,
    ));
    assert_eq!(chat.anchor, primary_before);

    // Outside it: the primary transcript takes the wheel instead.
    chat.handle_mouse(wheel(MouseEventKind::ScrollUp, 1, reviewer.y + 1));
    assert_ne!(
        chat.anchor, primary_before,
        "a wheel outside the reviewer pane scrolls the primary"
    );

    let _ = MouseButton::Left;
}

#[test]
fn clicking_a_split_button_takes_the_same_action_as_the_keyboard() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut chat, _) = drawn_split();
    let (action, area) = chat
        .split_action_areas
        .iter()
        .find(|(action, _)| *action == SplitAction::Implement)
        .copied()
        .expect("the split draws its action buttons");
    assert_eq!(action, SplitAction::Implement);

    let press = chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 1,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(press, ChatAction::None);
    let outcome = chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: area.x + 1,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    });
    let ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests)) = outcome else {
        panic!("clicking a button acts on it: {outcome:?}");
    };
    assert!(requests.iter().any(|request| matches!(
        request,
        WorkflowRequest::PromptPrimary { prompt, .. } if prompt.contains("1. Read")
    )));
}

#[test]
fn clicking_beside_the_buttons_does_nothing() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut chat, reviewer) = drawn_split();
    let outcome = chat.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: reviewer.x + 1,
        row: reviewer.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(outcome, ChatAction::None);
    assert!(chat.second_opinion_split(), "the split stays up");
}

/// A reviewer's form must be answered, or the review stalls waiting on a
/// harness nobody is talking to. It is shown in the ordinary dialog and
/// its answer is routed back to the reviewer, never to the planner.
#[test]
fn a_reviewer_form_is_answered_back_to_the_reviewer() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    let form = mj_core::elicitation::ElicitationRequest {
        id: "reviewer-form-1".into(),
        message: "Allow reading /etc?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    assert!(chat.show_review_role_elicitation(None, form));
    assert!(chat.reviewer_elicitation_open());

    // The primary's own projection must not take a reviewer's form down.
    chat.sync_elicitation(&[]);
    assert!(chat.reviewer_elicitation_open());

    let action = press(&mut chat, KeyCode::Esc);
    let ChatAction::RespondReviewerElicitation { elicitation_id, .. } = action else {
        panic!("a reviewer's answer goes to the reviewer: {action:?}");
    };
    assert_eq!(elicitation_id, "reviewer-form-1");
    assert!(!chat.reviewer_elicitation_open());
}

#[test]
fn the_primary_form_keeps_the_screen_over_a_reviewer_one() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.restore_elicitation(plan_review());
    let reviewer_form = mj_core::elicitation::ElicitationRequest {
        id: "reviewer-form-1".into(),
        message: "Allow reading /etc?".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    // An answer the planning harness is blocked on matters more than one
    // its reviewer is.
    assert!(!chat.show_review_role_elicitation(None, reviewer_form));
    assert!(!chat.reviewer_elicitation_open());
}

/// The prompts a review generates are Hel's, not the user's. Rendering
/// them as user messages would put words in their mouth and would make a
/// later resume replay them as if they had been typed.
#[test]
fn generated_review_prompts_never_read_as_the_user() {
    use mj_core::second_opinion::{
        PRIMARY_CONTEXT_REQUEST, implement_original_note, is_control_origin_prompt, review_request,
        transfer_note,
    };

    for generated in [
        PRIMARY_CONTEXT_REQUEST.to_owned(),
        review_request("context", "the plan"),
        transfer_note("the review"),
        implement_original_note("the plan"),
    ] {
        assert!(
            is_control_origin_prompt(&generated),
            "a generated prompt must be recognizable as Hel's: {generated:?}"
        );
    }
    // Something a person typed is not, even when it mentions one.
    assert!(!is_control_origin_prompt(
        "please add a [HARNESS NOTE: ...] to the docs"
    ));
    assert!(!is_control_origin_prompt("fix the parser"));

    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        1,
        ChatRole::System,
        PRIMARY_CONTEXT_REQUEST,
    ));
    assert_eq!(chat.entries[0].role, ChatRole::System);
}

/// A review whose target is gone still has to be readable: the reviewer's
/// own journal died with it, so the pane is rebuilt from the copy the
/// controller kept.
#[test]
fn a_reviewer_pane_rebuilds_from_a_stored_transcript() {
    let item = std::sync::Arc::new(mj_core::state::TranscriptItem {
        stable_id: "agent:1".into(),
        position: 1,
        latest_content_event_ordinal: Some(1),
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: mj_core::state::TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": "the plan misses error handling"}
            })],
            streaming: false,
        },
    });

    let mut pane = ReviewerPane::default();
    assert!(pane.is_empty());
    pane.restore("session-1-reviewer", vec![item]);

    assert!(!pane.is_empty());
    assert_eq!(
        pane.latest_answer().as_deref(),
        Some("the plan misses error handling"),
        "a restored review can still be read, and transferred from"
    );
    // Restoring nothing leaves the pane alone rather than clearing it.
    pane.restore("session-1-reviewer", Vec::new());
    assert!(!pane.is_empty());
}

#[test]
fn an_empty_reviewer_has_no_answer_to_transfer() {
    let pane = pane_from_entries(Vec::new());
    assert!(pane.is_empty());
    assert_eq!(pane.latest_answer(), None);
}

#[test]
fn a_harness_default_selection_is_stored_under_its_sentinel() {
    let selection = mj_core::second_opinion::ReviewerSelection {
        profile_id: "codex".into(),
        model: None,
        effort: Some("high".into()),
    };
    assert_eq!(
        selection.stored_values(),
        ("codex", HARNESS_DEFAULT_VALUE, "high")
    );

    let mut defaults = ReviewerDefaults::default();
    let (profile, model, effort) = selection.stored_values();
    defaults.restore("workspace-1", profile, model, effort);
    assert_eq!(defaults.profile("workspace-1"), Some("codex"));
    assert_eq!(
        defaults.model("workspace-1", "codex"),
        Some(HARNESS_DEFAULT_VALUE)
    );
    assert_eq!(
        defaults.effort("workspace-1", "codex", HARNESS_DEFAULT_VALUE),
        Some("high")
    );
}

#[test]
fn a_review_ignores_a_context_answer_that_predates_its_request() {
    let mut chat = ChatState::new(&snapshot(), &[]);
    chat.entries.push(ChatEntry::plain(
        3,
        ChatRole::Agent,
        "an answer from before the review",
    ));
    // The baseline is the frontier when the request went out, so only a
    // later message can be the answer to it.
    assert_eq!(chat.latest_agent_text_after(3), None);
    assert_eq!(
        chat.latest_agent_text_after(2).as_deref(),
        Some("an answer from before the review")
    );

    let _ = KeyModifiers::NONE;
}
